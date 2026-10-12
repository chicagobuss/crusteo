//! The socket loop: dial, hello, route frames, heartbeat, reconnect.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;

use crate::proto::ServerInfo;
use crate::{Command, Config, Error, Event};

#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    Connecting {
        attempt: u32,
    },
    Connected(ServerInfo),
    /// `auth` is set when the daemon refused the hello; the loop then stops
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
const CLOSE_TIMEOUT: Duration = Duration::from_millis(250);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
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

async fn session(
    config: &Config,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &crate::events::EventSender,
    attempt: &mut u32,
) -> Ended {
    let url = config.url();
    let connect = async {
        let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        match authority(&url) {
            Some((host, port)) => {
                let stream = dial(host, port)
                    .await
                    .map_err(tokio_tungstenite::tungstenite::Error::Io)?;
                tokio_tungstenite::client_async_with_config(
                    url.as_str(),
                    tokio_tungstenite::MaybeTlsStream::Plain(stream),
                    Some(ws_config),
                )
                .await
            }
            None => {
                tokio_tungstenite::connect_async_with_config(url.as_str(), Some(ws_config), false)
                    .await
            }
        }
    };
    let socket = match timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(err)) => return Ended::Lost(describe(&err)),
        Err(_) => return Ended::Lost("connect timed out".into()),
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
    if let Err(err) = sink.send(Message::text(hello.to_string())).await {
        return Ended::Lost(describe(&err));
    }

    // The daemon answers a hello with a server_info status, or refuses it.
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let info = loop {
        let frame = match timeout(deadline - Instant::now().min(deadline), stream.next()).await {
            Err(_) => return Ended::Lost("no server_info after hello".into()),
            Ok(None) => return Ended::Lost("closed during hello".into()),
            Ok(Some(Err(err))) => return Ended::Lost(describe(&err)),
            Ok(Some(Ok(frame))) => frame,
        };
        let Message::Text(text) = frame else { continue };
        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        if value["type"] == "hello.rejected" {
            let reason = value["reason"].as_str().unwrap_or("rejected").to_string();
            return Ended::Refused(reason);
        }
        let message = unwrap_session(&value);
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
                                let _ = sink.send(Message::text(r#"{"type":"pong"}"#)).await;
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
                        let _ = sink.send(Message::Pong(bytes)).await;
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
                        let frame = json!({ "type": "session", "message": message });
                        match sink.send(Message::text(frame.to_string())).await {
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
                        let frame = json!({ "type": "session", "message": message });
                        if let Err(err) = sink.send(Message::text(frame.to_string())).await {
                            break Ended::Lost(describe(&err));
                        }
                    }
                    Some(Command::SendBinary(bytes)) => {
                        if let Err(err) = sink.send(Message::binary(bytes)).await {
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
                    if let Err(err) = sink.send(Message::text(r#"{"type":"ping"}"#)).await {
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
    let Some(id) = message
        .get("payload")
        .and_then(|payload| payload.get("requestId"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
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
        && kind != "rpc_error"
    {
        waiting.updates.push(payload);
        return None;
    }
    let waiting = pending.remove(&id)?;
    let result = if kind == "rpc_error" {
        Err(Error::Rpc {
            code: payload["code"].as_str().map(str::to_string),
            message: payload["error"]
                .as_str()
                .unwrap_or("request failed")
                .to_string(),
        })
    } else if waiting.until.is_some() {
        Ok(json!({ "updates": waiting.updates, "final": payload }))
    } else {
        Ok(payload)
    };
    let _ = waiting.reply.send(result);
    None
}

fn unwrap_session(value: &Value) -> &Value {
    if value["type"] == "session" {
        &value["message"]
    } else {
        value
    }
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
