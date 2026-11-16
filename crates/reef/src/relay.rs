//! The pass-through for a desktop in reef mode: `/daemon/<host id>/ws`
//! opens that host's daemon as Reef reaches it (directly, or through ssh
//! from Reef's machine) and carries the desktop's WebSocket to it unchanged,
//! so a desktop needs to reach only Reef. It is the daemon's own protocol
//! end to end: the desktop says hello to the daemon and the daemon answers
//! it, as if Reef were not there. Reef changes one thing, the hello, which
//! gets the host's password when Reef has one and the desktop sent none.
//!
//! The device token is checked before the WebSocket opens, as for `/api/*`
//! (`Authorization: Bearer`). Whoever comes through holds the daemon's whole
//! protocol, terminals included: as much as reaching the daemon itself.
//!
//! Each desktop's connection is its own connection to the daemon; Reef's
//! own connection, which Skiff's views come from, is not shared.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// Where the pass-through is, before the host's id. `/api/hosts` gives it as
/// `relay`, so a desktop knows its Reef has one.
pub const PREFIX: &str = "/daemon/";

/// How long `pass` waits on a peer, as one value so a test can shorten it.
#[derive(Clone, Copy)]
struct Waits {
    /// A desktop says something at least this often: its heartbeat is every
    /// ten seconds. One silent for longer is gone, and so is its daemon socket.
    idle: Duration,
    /// How long a peer has to take one frame. One that stopped reading, a
    /// desktop whose network dropped without a reset or a daemon that hung,
    /// fills its socket and the send never ends, and the loop is stuck in it,
    /// the idle check too. A peer that takes nothing for as long as a silent
    /// desktop may be silent is as gone, so this is the same wait, and not
    /// shorter: a big image frame over a slow link is still on its way.
    send: Duration,
    /// How long a peer has to take Reef's close, and to answer it.
    close: Duration,
}

const WAITS: Waits = Waits {
    idle: Duration::from_secs(60),
    send: Duration::from_secs(60),
    close: Duration::from_secs(2),
};

/// A close frame's reason is at most 123 bytes.
const REASON_MAX: usize = 120;

/// A host's daemon as Reef reaches it.
#[derive(Debug, Clone, PartialEq)]
pub struct Daemon {
    pub endpoint: String,
    pub password: Option<String>,
}

/// The host id in a pass-through path: `/daemon/<id>/ws`, or `/daemon/<id>`,
/// the id percent-encoded. None for anything else.
pub(crate) fn host_in(path: &str) -> Option<String> {
    let rest = path.strip_prefix(PREFIX)?;
    let id = rest.strip_suffix("/ws").unwrap_or(rest);
    if id.is_empty() || id.contains('/') {
        return None;
    }
    crate::server::percent_decode(id).filter(|id| !id.is_empty())
}

/// The desktop's first frame, its hello, with the host's password added
/// when the desktop sent no `auth` of its own. Anything else as it came.
fn with_password(text: &str, password: Option<&str>) -> String {
    let Some(password) = password else {
        return text.to_string();
    };
    match serde_json::from_str::<Value>(text) {
        Ok(mut hello) if hello["type"] == "hello" && hello.get("auth").is_none() => {
            hello["auth"] = serde_json::json!({ "kind": "password", "password": password });
            hello.to_string()
        }
        _ => text.to_string(),
    }
}

fn close(code: CloseCode, reason: &str) -> Message {
    let mut end = reason.len().min(REASON_MAX);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    Message::Close(Some(CloseFrame {
        code,
        reason: reason[..end].to_string().into(),
    }))
}

/// Sends `frame`, true if the peer took it within `within`. Never waits
/// longer: see [`Waits::send`].
async fn send_within<T>(to: &mut T, frame: Message, within: Duration) -> bool
where
    T: futures_util::Sink<Message> + Unpin,
{
    matches!(
        tokio::time::timeout(within, to.send(frame)).await,
        Ok(Ok(()))
    )
}

/// Opens the daemon and carries frames both ways until either side ends,
/// or the desktop goes silent. A daemon that cannot be reached closes the
/// desktop's socket with why, which the desktop shows as the host's state.
/// Nothing waits on a peer for ever: a peer that takes no frame, or answers
/// no close, is given up on, and both sockets, and ssh, end with this.
pub(crate) async fn pass<S>(desktop: WebSocketStream<S>, daemon: Daemon)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pass_within(desktop, daemon, WAITS).await;
}

async fn pass_within<S>(desktop: WebSocketStream<S>, daemon: Daemon, waits: Waits)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut to_desktop, mut from_desktop) = desktop.split();
    let config = paseo_client::Config::new(daemon.endpoint.clone(), "reef-relay");
    let (socket, _ssh) = match paseo_client::open_socket(&config).await {
        Ok(opened) => opened,
        Err(error) => {
            let reason = format!("Reef could not reach it: {error}");
            let _ = send_within(
                &mut to_desktop,
                close(CloseCode::Error, &reason),
                waits.close,
            )
            .await;
            // Until the desktop closes too: a socket dropped at once is
            // reset, and the reason can be lost behind the reset.
            let drain = async { while let Some(Ok(_)) = from_desktop.next().await {} };
            let _ = tokio::time::timeout(waits.close, drain).await;
            return;
        }
    };
    let (mut to_daemon, mut from_daemon) = socket.split();
    let mut hello = true;
    let mut heard = Instant::now();
    let mut check = tokio::time::interval(waits.idle / 4);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    check.tick().await;
    loop {
        // In this order: what the desktop sent while a long send to either
        // side held the loop is read before the idle check can count it
        // silent, so a slow link that is still talking is not cut off.
        tokio::select! {
            biased;
            frame = from_desktop.next() => {
                let Some(Ok(frame)) = frame else { break };
                heard = Instant::now();
                let frame = match frame {
                    Message::Text(text) if hello => {
                        hello = false;
                        Message::text(with_password(text.as_str(), daemon.password.as_deref()))
                    }
                    Message::Text(_) | Message::Binary(_) => frame,
                    Message::Close(frame) => {
                        let _ = send_within(&mut to_daemon, Message::Close(frame), waits.close).await;
                        break;
                    }
                    // Pings and pongs are each side's own, answered there.
                    _ => continue,
                };
                if !send_within(&mut to_daemon, frame, waits.send).await {
                    let goodbye = close(CloseCode::Away, "the daemon went away");
                    let _ = send_within(&mut to_desktop, goodbye, waits.close).await;
                    break;
                }
            }
            frame = from_daemon.next() => {
                let frame = match frame {
                    Some(Ok(frame)) => frame,
                    _ => {
                        let goodbye = close(CloseCode::Away, "the daemon went away");
                        let _ = send_within(&mut to_desktop, goodbye, waits.close).await;
                        break;
                    }
                };
                match frame {
                    Message::Text(_) | Message::Binary(_) => {
                        if !send_within(&mut to_desktop, frame, waits.send).await {
                            break;
                        }
                    }
                    Message::Close(frame) => {
                        let _ = send_within(&mut to_desktop, Message::Close(frame), waits.close).await;
                        break;
                    }
                    _ => {}
                }
            }
            _ = check.tick() => {
                if heard.elapsed() > waits.idle {
                    break;
                }
            }
        }
    }
    // Both at once, in the one wait. Then both sockets are dropped.
    let _ = tokio::join!(
        tokio::time::timeout(waits.close, to_daemon.close()),
        tokio::time::timeout(waits.close, to_desktop.close()),
    );
}

#[cfg(test)]
mod tests {
    use tokio::net::{TcpListener, TcpSocket, TcpStream};

    use super::*;

    #[test]
    fn the_host_id_is_read_from_the_path() {
        assert_eq!(host_in("/daemon/devbox/ws").as_deref(), Some("devbox"));
        assert_eq!(host_in("/daemon/devbox").as_deref(), Some("devbox"));
        assert_eq!(
            host_in("/daemon/build%20box/ws").as_deref(),
            Some("build box")
        );
        for not in [
            "/daemon/",
            "/daemon//ws",
            "/daemon/a/b/ws",
            "/ws",
            "/daemon/%zz/ws",
        ] {
            assert_eq!(host_in(not), None, "{not}");
        }
    }

    #[test]
    fn the_hello_gets_the_password_unless_it_has_auth() {
        let hello = r#"{"type":"hello","clientId":"c1"}"#;
        let added: Value = serde_json::from_str(&with_password(hello, Some("pw"))).unwrap();
        assert_eq!(
            added["auth"],
            serde_json::json!({"kind": "password", "password": "pw"})
        );
        assert_eq!(added["clientId"], "c1");
        // None to add, the desktop's own, or not a hello: as it came.
        assert_eq!(with_password(hello, None), hello);
        let own = r#"{"type":"hello","auth":{"kind":"password","password":"mine"}}"#;
        assert_eq!(with_password(own, Some("pw")), own);
        assert_eq!(
            with_password(r#"{"type":"ping"}"#, Some("pw")),
            r#"{"type":"ping"}"#
        );
        assert_eq!(with_password("not json", Some("pw")), "not json");
    }

    #[test]
    fn a_long_reason_is_cut_to_fit_a_close_frame() {
        let Message::Close(Some(frame)) = close(CloseCode::Error, &"é".repeat(100)) else {
            panic!("a close frame");
        };
        assert!(frame.reason.len() <= REASON_MAX);
        assert!(frame.reason.chars().all(|c| c == 'é'));
    }

    /// Short waits for the tests of a peer that is stuck, so they end soon;
    /// idle stays long, to show it is not what ends them.
    const SHORT: Waits = Waits {
        idle: Duration::from_secs(60),
        send: Duration::from_millis(300),
        close: Duration::from_millis(200),
    };

    /// A pass-through to the daemon at `endpoint` for the first desktop to
    /// connect to the address given back.
    async fn relay_to(endpoint: String) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let desktop = tokio_tungstenite::accept_async(stream).await.unwrap();
            let daemon = Daemon {
                endpoint,
                password: None,
            };
            pass_within(desktop, daemon, SHORT).await;
        });
        (addr, relay)
    }

    /// A socket that takes in only a few kilobytes of what it is sent before
    /// it is read from, so a peer that does not read stops the sender sooner.
    fn small_buffer() -> TcpSocket {
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        socket
    }

    /// The end of `task`, well after the short waits would have ended it.
    async fn ended<T>(task: tokio::task::JoinHandle<T>) -> T {
        (tokio::time::timeout(Duration::from_secs(10), task).await)
            .expect("it ended in time")
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_desktop_that_stops_reading_does_not_hold_the_relay() {
        // A daemon that sends big frames until its socket is dropped.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let daemon = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut daemon = tokio_tungstenite::accept_async(stream).await.unwrap();
            let frame = "x".repeat(64 * 1024);
            for _ in 0..4000 {
                if daemon.send(Message::text(frame.clone())).await.is_err() {
                    return true;
                }
            }
            false
        });
        let (addr, relay) = relay_to(endpoint).await;
        // A desktop that is connected, as one whose network dropped without
        // a reset is, and reads nothing: its socket fills.
        let stream = small_buffer().connect(addr).await.unwrap();
        let url = format!("ws://{addr}/daemon/devbox/ws");
        let (_desktop, _) = tokio_tungstenite::client_async(url, stream).await.unwrap();
        ended(relay).await;
        assert!(ended(daemon).await, "the daemon's socket was dropped");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_daemon_that_stops_reading_does_not_hold_the_relay() {
        // A daemon that takes the connection and reads nothing from it.
        let socket = small_buffer();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(1).unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let daemon = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        });
        let (addr, relay) = relay_to(endpoint).await;
        let stream = TcpStream::connect(addr).await.unwrap();
        let url = format!("ws://{addr}/daemon/devbox/ws");
        let (mut desktop, _) = tokio_tungstenite::client_async(url, stream).await.unwrap();
        // Held, not read, until the end.
        let _daemon = ended(daemon).await;
        // A desktop that sends big frames until its socket is dropped.
        let flood = tokio::spawn(async move {
            let frame = "x".repeat(64 * 1024);
            for _ in 0..4000 {
                if desktop.send(Message::text(frame.clone())).await.is_err() {
                    return true;
                }
            }
            false
        });
        ended(relay).await;
        assert!(ended(flood).await, "the desktop's socket was dropped");
    }
}
