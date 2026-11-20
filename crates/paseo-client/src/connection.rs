//! The socket loop: dial, hello, route frames, heartbeat, reconnect.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;

use burrow_protocol::envelope::{self, RpcFailure};

use crate::proto::ServerInfo;
use crate::{Command, Config, Error, Event};

#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    Connecting {
        attempt: u32,
    },
    Connected(ServerInfo),
    /// `auth` is set when the daemon refused the hello, or the handshake was
    /// answered 401 (a Reef refusing the token); the loop then stops
    /// retrying until told to reconnect.
    Disconnected {
        reason: String,
        auth: bool,
    },
}

// Explicitly retain tungstenite 0.28's existing transport limits. Oversized
// events can monopolize the queue budget, but incoming frames still have a
// stated bound, including the one being decoded outside that budget.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// As long as Paseo's desktop app gives an SSH connection to set up.
const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_TIMEOUT: Duration = Duration::from_millis(250);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long one write to the socket may stay pending before the connection is
/// given up as lost: [`Config::new`]'s default for [`Config::write_timeout`].
/// It is the heartbeat's own limit, and not a few seconds: while a write is
/// pending the loop reads nothing, a pong included, so a connection that
/// cannot write for this long is one the heartbeat would drop as well. A
/// shorter bound would cut a healthy slow link in the middle of a request
/// that carries a prompt with several MB of images.
pub(crate) const WRITE_TIMEOUT: Duration = HEARTBEAT_TIMEOUT;
const BACKOFF_BASE_MS: u64 = 1500;
const BACKOFF_MAX_MS: u64 = 30_000;

/// How long one address gets before the next is tried alongside it.
const DIAL_STAGGER: Duration = Duration::from_millis(250);

/// Host and port of a plain `ws://` URL; None for anything else.
fn authority(url: &str) -> Option<(&str, u16)> {
    let rest = url.strip_prefix("ws://")?;
    let authority = rest.split('/').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, port.parse().ok()?),
        _ => (authority, 80),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then_some((host, port))
}

/// Connects to `host:port`, racing its addresses the way browsers and Node
/// do ("happy eyeballs"): each address gets [`DIAL_STAGGER`] before the next
/// starts alongside it, a failure starts the next at once, and the first to
/// connect wins. Trying addresses one by one stalls on an address that drops
/// packets instead of refusing: Windows resolves `localhost` to `::1` first,
/// and a WSL daemon forwarded to `127.0.0.1` never answers there.
async fn dial(host: &str, port: u16) -> std::io::Result<tokio::net::TcpStream> {
    let mut addresses: std::collections::VecDeque<std::net::SocketAddr> =
        tokio::net::lookup_host((host, port)).await?.collect();
    let mut attempts = tokio::task::JoinSet::new();
    let mut last_error = None;
    loop {
        if let Some(address) = addresses.pop_front() {
            attempts.spawn(tokio::net::TcpStream::connect(address));
        }
        if attempts.is_empty() {
            return Err(last_error.unwrap_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses")
            }));
        }
        tokio::select! {
            Some(attempt) = attempts.join_next() => match attempt {
                Ok(Ok(stream)) => {
                    attempts.abort_all();
                    return Ok(stream);
                }
                Ok(Err(error)) => last_error = Some(error),
                Err(_) => {}
            },
            _ = tokio::time::sleep(DIAL_STAGGER), if !addresses.is_empty() => {}
        }
    }
}

/// A request waiting for its reply. One with `until` collects the messages
/// carrying its id until one of that type ends it.
struct Waiting {
    reply: oneshot::Sender<Result<Value, Error>>,
    until: Option<String>,
    updates: Vec<Value>,
}

type Pending = HashMap<String, Waiting>;

enum Ended {
    Closed,
    Lost(String),
    Refused(String),
    Redial,
}

/// Why a socket did not open. `Refused` is what asking again will not
/// change; `Lost` is tried again after the backoff.
#[derive(Debug, PartialEq)]
enum Failed {
    Lost(String),
    Refused(String),
}

impl From<String> for Failed {
    fn from(reason: String) -> Self {
        Failed::Lost(reason)
    }
}

impl From<Failed> for Ended {
    fn from(failed: Failed) -> Self {
        match failed {
            Failed::Lost(reason) => Ended::Lost(reason),
            Failed::Refused(reason) => Ended::Refused(reason),
        }
    }
}

pub(crate) async fn run(
    config: Config,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: crate::events::EventSender,
) {
    let mut attempt: u32 = 0;
    loop {
        if events
            .send(Event::State(ConnState::Connecting { attempt }))
            .await
            .is_err()
        {
            return;
        }
        let ended = session(&config, &mut commands, &events, &mut attempt).await;
        let (reason, auth) = match ended {
            Ended::Closed => return,
            Ended::Redial => {
                attempt = 0;
                continue;
            }
            Ended::Lost(reason) => (reason, false),
            Ended::Refused(reason) => (reason, true),
        };
        if events
            .send(Event::State(ConnState::Disconnected { reason, auth }))
            .await
            .is_err()
        {
            return;
        }
        let delay = if auth {
            None
        } else {
            let ms = BACKOFF_BASE_MS
                .saturating_mul(1u64 << attempt.min(16))
                .min(BACKOFF_MAX_MS);
            Some(Duration::from_millis(ms))
        };
        attempt = attempt.saturating_add(1);
        if !idle(delay, &mut commands).await {
            return;
        }
    }
}

/// Waits out the backoff while refusing requests. Returns false on close.
async fn idle(delay: Option<Duration>, commands: &mut mpsc::UnboundedReceiver<Command>) -> bool {
    let deadline = delay.map(|delay| Instant::now() + delay);
    loop {
        let next = async {
            match deadline {
                Some(deadline) => timeout(deadline - Instant::now().min(deadline), commands.recv())
                    .await
                    .ok(),
                None => Some(commands.recv().await),
            }
        };
        match next.await {
            None => return true,
            Some(None) | Some(Some(Command::Close)) => return false,
            Some(Some(Command::Reconnect)) => return true,
            Some(Some(Command::Request { reply, .. })) => {
                let _ = reply.send(Err(Error::Disconnected));
            }
            Some(Some(_)) => {}
        }
    }
}

/// What the WebSocket runs over: a TCP connection, or ssh's standard streams.
pub trait Transport: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Transport for T {}

/// A daemon's WebSocket, open and before its hello.
pub type Socket = tokio_tungstenite::WebSocketStream<Box<dyn Transport>>;

/// Opens `config`'s daemon as this client does: dialled directly, or through
/// ssh for an `ssh://` endpoint, with the bearer token in the handshake if
/// there is one. Nothing is sent yet: no hello. The [`crate::ssh::Watch`],
/// for an `ssh://` endpoint, says why ssh failed once it has ended. Reef's
/// pass-through for a desktop opens daemons with this. When ssh fails, the
/// error is what ssh said, as for a client's own connection, and not what
/// the handshake made of ssh's closed streams.
pub async fn open_socket(config: &Config) -> Result<(Socket, Option<crate::ssh::Watch>), String> {
    let mut ssh = None;
    match connect(config, &mut ssh).await {
        Ok(socket) => Ok((socket, ssh)),
        Err(Failed::Lost(reason)) => Err(match ssh {
            Some(ssh) => ssh.failure().await.unwrap_or(reason),
            None => reason,
        }),
        // The daemon answered, over ssh or not: that is the reason.
        Err(Failed::Refused(reason)) => Err(reason),
    }
}

async fn connect(config: &Config, ssh: &mut Option<crate::ssh::Watch>) -> Result<Socket, Failed> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let url = config.url();
    let target = match crate::ssh::Target::parse(&config.endpoint) {
        None => None,
        Some(Ok(target)) => Some(target),
        Some(Err(err)) => return Err(err.into()),
    };
    // ssh may make two hops and a key exchange before the daemon answers.
    let within = if target.is_some() {
        SSH_CONNECT_TIMEOUT
    } else {
        CONNECT_TIMEOUT
    };
    let connect = async {
        let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        let mut request = (url.as_str().into_client_request()).map_err(|err| describe(&err))?;
        if let Some(token) = &config.bearer {
            let value = format!("Bearer {token}")
                .parse()
                .map_err(|_| "the token cannot go in a header".to_string())?;
            request.headers_mut().insert("authorization", value);
        }
        let transport: Box<dyn Transport> = match (&target, authority(&url)) {
            (Some(target), _) => {
                let (stream, watch) = crate::ssh::open(target, config.ssh_program.as_deref())?;
                *ssh = Some(watch);
                Box::new(stream)
            }
            (None, Some((host, port))) => {
                Box::new(dial(host, port).await.map_err(|e| e.to_string())?)
            }
            (None, None) => {
                return Err(Failed::Lost(format!(
                    "{url}: only host:port, ws:// and ssh:// addresses are supported"
                )));
            }
        };
        tokio_tungstenite::client_async_with_config(request, transport, Some(ws_config))
            .await
            .map(|(socket, _)| socket)
            .map_err(|err| handshake_failed(&err, config.bearer.is_some()))
    };
    match timeout(within, connect).await {
        Ok(result) => result,
        Err(_) => Err(Failed::Lost("connect timed out".into())),
    }
}

/// How a handshake failed. A 401 is refused: the token is wrong, and asking
/// again will not change that. Anything else is tried again, a 404 too: the
/// far end may know the host soon, as a Reef given it, or one restarting,
/// does. So is a 403: a proxy in front of the far end answers it for a while
/// as it reloads, or limits what it lets through. Reef and a daemon answer
/// 403 for an origin or host name they do not allow, and that is shown too.
fn handshake_failed(err: &tokio_tungstenite::tungstenite::Error, bearer: bool) -> Failed {
    use tokio_tungstenite::tungstenite::Error as WsError;
    let WsError::Http(response) = err else {
        return Failed::Lost(describe(err));
    };
    match response.status().as_u16() {
        401 if bearer => Failed::Refused("Reef refused this desktop's token (HTTP 401)".into()),
        401 => Failed::Refused("It wants a token this connection does not have (HTTP 401)".into()),
        403 => {
            // Reef says why: the origin, or the host name it was asked by.
            let why = (response.body().as_deref())
                .map(|body| String::from_utf8_lossy(body).trim().to_string())
                .filter(|why| !why.is_empty());
            Failed::Lost(match why {
                Some(why) => format!("{}: {why}", describe(err)),
                None => describe(err),
            })
        }
        _ => Failed::Lost(describe(err)),
    }
}

/// How long the daemon has to answer the hello. Through a Reef's
/// pass-through ([`Config::relayed`]) the hello waits on Reef opening the
/// daemon too, which through ssh may take [`SSH_CONNECT_TIMEOUT`].
fn hello_wait(config: &Config) -> Duration {
    if config.relayed {
        SSH_CONNECT_TIMEOUT + CONNECT_TIMEOUT
    } else {
        CONNECT_TIMEOUT
    }
}

async fn session(
    config: &Config,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &crate::events::EventSender,
    attempt: &mut u32,
) -> Ended {
    let mut ssh = None;
    let ended = session_over(config, commands, events, attempt, &mut ssh).await;
    // A connection through ssh that failed or dropped: what ssh said is
    // the reason worth showing ("Permission denied", "Could not resolve
    // hostname", the daemon's port refused on the far side).
    match (ended, ssh) {
        (Ended::Lost(reason), Some(ssh)) => Ended::Lost(ssh.failure().await.unwrap_or(reason)),
        (ended, _) => ended,
    }
}

async fn session_over(
    config: &Config,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &crate::events::EventSender,
    attempt: &mut u32,
    ssh: &mut Option<crate::ssh::Watch>,
) -> Ended {
    let socket = match connect(config, ssh).await {
        Ok(socket) => socket,
        Err(failed) => return failed.into(),
    };
    let (mut sink, mut stream) = socket.split();

    let mut hello = json!({
        "type": "hello",
        "clientId": config.client_id,
        "clientType": "cli",
        "protocolVersion": crate::PROTOCOL_VERSION,
        "appVersion": config.app_version,
        "capabilities": crate::proto::capabilities(),
    });
    if let Some(password) = &config.password {
        hello["auth"] = json!({ "kind": "password", "password": password });
    }
    let write_timeout = config.write_timeout;
    if let Err(err) = send_within(&mut sink, Message::text(hello.to_string()), write_timeout).await
    {
        return Ended::Lost(describe(&err));
    }

    // The daemon answers a hello with a server_info status, or refuses it.
    let deadline = Instant::now() + hello_wait(config);
    let info = loop {
        let frame = match timeout(deadline - Instant::now().min(deadline), stream.next()).await {
            Err(_) => return Ended::Lost("no server_info after hello".into()),
            Ok(None) => return Ended::Lost("closed during hello".into()),
            Ok(Some(Err(err))) => return Ended::Lost(describe(&err)),
            Ok(Some(Ok(frame))) => frame,
        };
        let text = match frame {
            Message::Text(text) => text,
            // Closed with a reason: a Reef passing the daemon through says
            // why it could not reach it.
            Message::Close(Some(frame)) if !frame.reason.is_empty() => {
                return Ended::Lost(frame.reason.to_string());
            }
            _ => continue,
        };
        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        if value["type"] == "hello.rejected" {
            let reason = value["reason"].as_str().unwrap_or("rejected").to_string();
            return Ended::Refused(reason);
        }
        let message = envelope::unwrap_session(&value);
        if let Some(info) = ServerInfo::from_status(message) {
            break info;
        }
    };

    *attempt = 0;
    if events
        .send(Event::State(ConnState::Connected(info)))
        .await
        .is_err()
    {
        let _ = timeout(CLOSE_TIMEOUT, sink.send(Message::Close(None))).await;
        return Ended::Closed;
    }

    let mut pending: Pending = HashMap::new();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_EVERY);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;
    let mut awaiting_pong: Option<Instant> = None;

    let ended = loop {
        tokio::select! {
            frame = stream.next() => {
                let frame = match frame {
                    None => break Ended::Lost("connection closed".into()),
                    Some(Err(err)) => break Ended::Lost(describe(&err)),
                    Some(Ok(frame)) => frame,
                };
                // Traffic is evidence the link is alive. Clear before awaiting
                // the consumer, so a long UI stall cannot age a prior ping into
                // a false client-side timeout when socket polling resumes.
                awaiting_pong = None;
                match frame {
                    Message::Text(text) => {
                        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                        drop(text); // do not retain the wire copy during backpressure
                        let published = match value["type"].as_str() {
                            Some("ping") => {
                                let pong = Message::text(r#"{"type":"pong"}"#);
                                if let Err(err) = send_within(&mut sink, pong, write_timeout).await {
                                    break Ended::Lost(describe(&err));
                                }
                                Ok(())
                            }
                            Some("session") => route(value, &mut pending, events).await,
                            _ => Ok(()),
                        };
                        if published.is_err() {
                            break Ended::Closed;
                        }
                    }
                    Message::Binary(bytes) => {
                        if events.send(Event::Binary(bytes.to_vec())).await.is_err() {
                            break Ended::Closed;
                        }
                    }
                    Message::Ping(bytes) => {
                        if let Err(err) = send_within(&mut sink, Message::Pong(bytes), write_timeout).await {
                            break Ended::Lost(describe(&err));
                        }
                    }
                    Message::Close(frame) => {
                        let reason = frame
                            .map(|f| format!("closed ({}) {}", u16::from(f.code), f.reason))
                            .unwrap_or_else(|| "closed".into());
                        break Ended::Lost(reason);
                    }
                    _ => {}
                }
            }
            command = commands.recv() => {
                match command {
                    None | Some(Command::Close) => {
                        break Ended::Closed;
                    }
                    Some(Command::Reconnect) => break Ended::Redial,
                    Some(Command::Request { reply, deadline, .. }) if Instant::now() >= deadline => {
                        let _ = reply.send(Err(Error::Timeout));
                    }
                    Some(Command::Request { id, message, reply, until, .. }) => {
                        let frame = envelope::request_frame(message);
                        match send_within(&mut sink, Message::text(frame.to_string()), write_timeout).await {
                            Ok(()) => {
                                pending.insert(id, Waiting { reply, until, updates: Vec::new() });
                            }
                            Err(err) => {
                                let _ = reply.send(Err(Error::Disconnected));
                                break Ended::Lost(describe(&err));
                            }
                        }
                    }
                    Some(Command::Cancel(id)) => { pending.remove(&id); }
                    Some(Command::Send(message)) => {
                        let frame = envelope::request_frame(message);
                        if let Err(err) = send_within(&mut sink, Message::text(frame.to_string()), write_timeout).await {
                            break Ended::Lost(describe(&err));
                        }
                    }
                    Some(Command::SendBinary(bytes)) => {
                        if let Err(err) = send_within(&mut sink, Message::binary(bytes), write_timeout).await {
                            break Ended::Lost(describe(&err));
                        }
                    }
                }
            }
            _ = heartbeat.tick() => {
                if let Some(since) = awaiting_pong {
                    if since.elapsed() > HEARTBEAT_TIMEOUT {
                        break Ended::Lost("heartbeat timed out".into());
                    }
                } else {
                    awaiting_pong = Some(Instant::now());
                    let ping = Message::text(r#"{"type":"ping"}"#);
                    if let Err(err) = send_within(&mut sink, ping, write_timeout).await {
                        break Ended::Lost(describe(&err));
                    }
                }
            }
        }
    };

    if matches!(ended, Ended::Closed) {
        let _ = timeout(CLOSE_TIMEOUT, sink.send(Message::Close(None))).await;
    }
    for (_, waiting) in pending.drain() {
        let _ = waiting.reply.send(Err(Error::Disconnected));
    }
    ended
}

/// Sends one frame, or fails once `limit` has passed. On a half-open path (a
/// network that drops packets without resetting the connection) a write can
/// stay pending until TCP gives up, minutes later, and the loop that awaits it
/// reaches neither its heartbeat check nor a reconnect meanwhile. The timeout
/// is an error like any other write error: the caller ends the session as
/// lost, and the connection is dialled again after the backoff.
async fn send_within<S>(
    sink: &mut S,
    frame: Message,
    limit: Duration,
) -> Result<(), tokio_tungstenite::tungstenite::Error>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    match timeout(limit, sink.send(frame)).await {
        Ok(sent) => sent,
        Err(_) => Err(tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "write timed out"),
        )),
    }
}

/// Hands a reply to its waiting request, or publishes the message as an event.
async fn route(
    value: Value,
    pending: &mut Pending,
    events: &crate::events::EventSender,
) -> Result<(), crate::events::ConsumerGone> {
    let Value::Object(mut envelope) = value else {
        return Ok(());
    };
    let Some(message) = envelope.remove("message") else {
        return Ok(());
    };
    match settle(pending, message) {
        Some(message) => events.send(Event::Message(message)).await,
        None => Ok(()),
    }
}

/// Gives `message` to the request it answers, if one is waiting, and hands
/// it back otherwise. A request with `until` keeps the payloads of messages
/// before the one that ends it, and gets them with it as
/// `{"updates": [...], "final": ...}`.
fn settle(pending: &mut Pending, mut message: Value) -> Option<Value> {
    let Some(id) = envelope::reply_request_id(&message).map(str::to_string) else {
        return Some(message);
    };
    let Some(waiting) = pending.get_mut(&id) else {
        return Some(message);
    };
    let kind = message["type"].as_str().unwrap_or_default().to_string();
    let payload = message
        .get_mut("payload")
        .map(Value::take)
        .unwrap_or(Value::Null);
    if let Some(until) = &waiting.until
        && kind != *until
        && kind != envelope::RPC_ERROR
    {
        waiting.updates.push(payload);
        return None;
    }
    let waiting = pending.remove(&id)?;
    let result = if kind == envelope::RPC_ERROR {
        let RpcFailure { code, message } = RpcFailure::from_payload(&payload);
        Err(Error::Rpc { code, message })
    } else if waiting.until.is_some() {
        Ok(json!({ "updates": waiting.updates, "final": payload }))
    } else {
        Ok(payload)
    };
    let _ = waiting.reply.send(result);
    None
}

fn describe(err: &tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error as WsError;
    match err {
        WsError::Io(io) => io.to_string(),
        WsError::Http(response) => format!("HTTP {}", response.status()),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(until: Option<&str>) -> (Waiting, oneshot::Receiver<Result<Value, Error>>) {
        let (reply, rx) = oneshot::channel();
        let until = until.map(str::to_string);
        (
            Waiting {
                reply,
                until,
                updates: Vec::new(),
            },
            rx,
        )
    }

    #[test]
    fn a_plain_request_takes_the_first_reply() {
        let mut pending = Pending::new();
        let (entry, mut rx) = waiting(None);
        pending.insert("r1".into(), entry);
        let reply = json!({"type": "x.response", "payload": {"requestId": "r1", "ok": true}});
        assert!(settle(&mut pending, reply).is_none());
        assert_eq!(rx.try_recv().unwrap().unwrap()["ok"], true);
        let stray = json!({"type": "x.response", "payload": {"requestId": "r1"}});
        assert!(
            settle(&mut pending, stray).is_some(),
            "a late copy is an event"
        );
    }

    #[test]
    fn a_streamed_request_collects_updates_until_its_final_message() {
        let mut pending = Pending::new();
        let (entry, mut rx) = waiting(Some("usage.list_reports.response"));
        pending.insert("r2".into(), entry);
        for n in 1..=2 {
            let update = json!({"type": "usage.list_reports.update", "payload": {"requestId": "r2", "n": n}});
            assert!(settle(&mut pending, update).is_none());
        }
        assert!(
            rx.try_recv().is_err(),
            "not answered before the final message"
        );
        let done = json!({"type": "usage.list_reports.response", "payload": {"requestId": "r2", "error": null}});
        assert!(settle(&mut pending, done).is_none());
        let result = rx.try_recv().unwrap().unwrap();
        assert_eq!(result["updates"].as_array().unwrap().len(), 2);
        assert_eq!(result["updates"][1]["n"], 2);
        assert!(result["final"]["error"].is_null());
        assert!(pending.is_empty());
    }

    #[test]
    fn an_error_ends_a_streamed_request() {
        let mut pending = Pending::new();
        let (entry, mut rx) = waiting(Some("x.response"));
        pending.insert("r3".into(), entry);
        let error = json!({"type": "rpc_error", "payload": {"requestId": "r3", "error": "nope"}});
        assert!(settle(&mut pending, error).is_none());
        assert!(rx.try_recv().unwrap().is_err());
    }

    #[test]
    fn a_refused_token_stops_the_retries_and_what_may_pass_does_not() {
        use tokio_tungstenite::tungstenite::{Error as WsError, http::Response};
        let answered = |status: u16, body: &str| {
            let response = Response::builder()
                .status(status)
                .body(Some(body.as_bytes().to_vec()))
                .unwrap();
            WsError::Http(Box::new(response))
        };
        assert_eq!(
            handshake_failed(&answered(401, "Unauthorized"), true),
            Failed::Refused("Reef refused this desktop's token (HTTP 401)".into())
        );
        assert_eq!(
            handshake_failed(&answered(401, "Unauthorized"), false),
            Failed::Refused("It wants a token this connection does not have (HTTP 401)".into())
        );
        // A 403 may be a proxy in front for a moment: again, and why is shown.
        assert_eq!(
            handshake_failed(&answered(403, "Origin not allowed\n"), true),
            Failed::Lost("HTTP 403 Forbidden: Origin not allowed".into())
        );
        assert_eq!(
            handshake_failed(&answered(403, ""), true),
            Failed::Lost("HTTP 403 Forbidden".into())
        );
        assert_eq!(
            handshake_failed(&answered(403, "Origin not allowed"), false),
            Failed::Lost("HTTP 403 Forbidden: Origin not allowed".into())
        );
        // A host the far end does not know yet, or one in trouble: again.
        for status in [404, 500, 502, 503] {
            let failed = handshake_failed(&answered(status, "No such host"), true);
            assert!(matches!(failed, Failed::Lost(_)), "{status}: {failed:?}");
        }
        let refused = WsError::Io(std::io::ErrorKind::ConnectionRefused.into());
        assert!(matches!(handshake_failed(&refused, true), Failed::Lost(_)));
    }

    /// A write that never completes (a half-open path leaves one pending) is
    /// given up on after the limit, as a timeout; one that completes is left
    /// alone. A client's default bound is the heartbeat's own limit.
    #[test]
    fn a_write_that_never_completes_is_bounded_and_reported_as_a_timeout() {
        type Failure = tokio_tungstenite::tungstenite::Error;
        crate::block_on(async {
            let stuck = futures_util::sink::unfold((), |(), _: Message| async {
                std::future::pending::<Result<(), Failure>>().await
            });
            let mut stuck = Box::pin(stuck);
            // The outer limit makes an unbounded send fail the test, not hang it.
            let err = timeout(
                Duration::from_secs(5),
                send_within(&mut stuck, Message::text("x"), Duration::from_millis(50)),
            )
            .await
            .expect("the send is not bounded")
            .unwrap_err();
            assert!(
                matches!(&err, Failure::Io(io) if io.kind() == std::io::ErrorKind::TimedOut),
                "{err:?}"
            );
            assert_eq!(describe(&err), "write timed out");

            let open =
                futures_util::sink::unfold((), |(), _: Message| async { Ok::<(), Failure>(()) });
            let mut open = Box::pin(open);
            send_within(&mut open, Message::text("x"), Duration::from_secs(5))
                .await
                .unwrap();
        });
        assert_eq!(Config::new("host", "cid").write_timeout, HEARTBEAT_TIMEOUT);
    }

    #[test]
    fn authority_reads_plain_websocket_urls() {
        assert_eq!(
            authority("ws://localhost:6767/ws"),
            Some(("localhost", 6767))
        );
        assert_eq!(authority("ws://10.0.0.1/ws"), Some(("10.0.0.1", 80)));
        assert_eq!(authority("ws://[::1]:6767/ws"), Some(("::1", 6767)));
        assert_eq!(authority("ws://[::1]/ws"), Some(("::1", 80)));
        assert_eq!(authority("wss://example.com/ws"), None);
    }

    /// A daemon listening only on IPv4 is reached through `localhost` even
    /// where `localhost` resolves to `::1` first, and quickly.
    #[test]
    fn localhost_reaches_an_ipv4_only_listener() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let accept = tokio::spawn(async move { listener.accept().await });
            let stream = timeout(Duration::from_secs(2), dial("localhost", port))
                .await
                .expect("dial took too long")
                .expect("dial failed");
            assert!(stream.peer_addr().unwrap().is_ipv4());
            accept.await.unwrap().unwrap();
        });
    }

    /// Review #6 (0.2.1): through Reef's pass-through the daemon answers the
    /// hello only once Reef has opened it, which through ssh takes up to
    /// [`SSH_CONNECT_TIMEOUT`], and then the daemon answers. A relayed
    /// connection waits for both; a direct one still gives up after
    /// [`CONNECT_TIMEOUT`].
    #[test]
    fn a_relayed_hello_waits_out_reefs_ssh_connect() {
        let mut config = Config::new("192.0.2.10:6798", "relay-test");
        assert_eq!(hello_wait(&config), CONNECT_TIMEOUT);
        config.relayed = true;
        assert_eq!(hello_wait(&config), SSH_CONNECT_TIMEOUT + CONNECT_TIMEOUT);
        assert!(hello_wait(&config) > SSH_CONNECT_TIMEOUT);
    }

    #[test]
    fn socket_flood_stalls_at_the_queue_limit_and_last_client_drop_unblocks_it() {
        crate::block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                ws.next().await.unwrap().unwrap(); // hello
                ws.send(Message::text(
                    json!({
                        "type": "status",
                        "payload": {"status": "server_info", "serverId": "flood-test"}
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
                for n in 0..512 {
                    let push = json!({"type": "session", "message": {
                        "type": "agent_update", "payload": {"n": n, "text": "x".repeat(32 * 1024)}
                    }});
                    if ws.send(Message::text(push.to_string())).await.is_err() {
                        return;
                    }
                }
                // With a bounded consumer, the client must also be able to
                // shut down while the socket task is blocked publishing.
                while ws.next().await.is_some_and(|frame| frame.is_ok()) {}
            });
            let client =
                crate::Client::connect(Config::new(format!("127.0.0.1:{port}"), "flood-test"));
            let events = client.events();
            timeout(Duration::from_secs(2), async {
                while events.len() < crate::events::EVENT_LIMIT {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(events.len(), crate::events::EVENT_LIMIT);
            let mut expected = 0;
            for _ in 0..crate::events::EVENT_LIMIT {
                if let Event::Message(value) = events.recv().await.unwrap() {
                    assert_eq!(value["payload"]["n"], expected);
                    expected += 1;
                }
            }
            assert!(expected > 0);
            drop(client); // receiver is intentionally still alive and may be full
            timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
        });
    }
}
