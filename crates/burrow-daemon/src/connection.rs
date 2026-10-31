//! One client socket: hello, dispatch, subscriptions, lease, backpressure.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use burrow_protocol::codes::{OUTBOUND_HIGH_WATER_BYTES, close};
use burrow_protocol::envelope::{
    Inbound, InboundError, RejectReason, hello_rejected, parse_inbound, pong, status_error,
};
use burrow_protocol::hello::{Hello, capability};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, sleep_until, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tracing::{debug, info, warn};

use crate::agent_requests::ClientShape;
use crate::daemon::Daemon;
use crate::handlers;
use crate::subscriptions::Subscriptions;

type Socket = WebSocketStream<TcpStream>;

enum Out {
    Text(String),
    Close(u16, &'static str),
}

/// The socket's outbound queue. Bytes count from enqueue until the frame is
/// written, which stands in for Node's `bufferedAmount`.
struct Outbox {
    tx: mpsc::UnboundedSender<Out>,
    queued: Arc<AtomicUsize>,
    overflowed: bool,
}

impl Outbox {
    fn send(&mut self, frame: &Value) {
        let text = frame.to_string();
        let len = text.len();
        if self.queued.load(Ordering::Acquire) + len > OUTBOUND_HIGH_WATER_BYTES {
            self.overflowed = true;
            return;
        }
        self.queued.fetch_add(len, Ordering::AcqRel);
        let _ = self.tx.send(Out::Text(text));
    }
}

fn spawn_writer(
    mut sink: SplitSink<Socket, Message>,
    mut rx: mpsc::UnboundedReceiver<Out>,
    queued: Arc<AtomicUsize>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            match out {
                Out::Text(text) => {
                    let len = text.len();
                    let sent = sink.send(Message::text(text)).await;
                    queued.fetch_sub(len, Ordering::AcqRel);
                    if sent.is_err() {
                        return;
                    }
                }
                Out::Close(code, reason) => {
                    let frame = CloseFrame {
                        code: code.into(),
                        reason: reason.into(),
                    };
                    let _ = sink.send(Message::Close(Some(frame))).await;
                    let _ = sink.close().await;
                    return;
                }
            }
        }
    })
}

/// How a connection ends.
enum End {
    /// Send a close frame with this code and reason.
    Close((u16, &'static str)),
    /// Drop the TCP connection without a close frame; the client sees 1006.
    Terminate(&'static str),
    /// The client closed or the transport failed.
    Gone,
}

pub async fn run(
    daemon: Arc<Daemon>,
    socket: Socket,
    peer: SocketAddr,
    shutdown: watch::Receiver<bool>,
) {
    let (sink, mut stream) = socket.split();
    let queued = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::unbounded_channel();
    let writer = spawn_writer(sink, rx, queued.clone());
    let mut out = Outbox {
        tx,
        queued,
        overflowed: false,
    };

    let timing = daemon.config.timing;
    let end = match await_hello(&mut stream, &mut out, timing.hello_timeout).await {
        Ok(hello) => {
            info!(%peer, client_id = hello.client_id(), "client connected");
            let shape = ClientShape {
                compact_provider_snapshots: hello.has(capability::COMPACT_PROVIDER_SNAPSHOTS),
            };
            session(&daemon, &mut stream, &mut out, shape, shutdown).await
        }
        Err(end) => end,
    };
    match end {
        End::Close((code, reason)) => {
            debug!(%peer, code, reason, "closing");
            let _ = out.tx.send(Out::Close(code, reason));
            drop(out);
            let _ = timeout(std::time::Duration::from_secs(2), writer).await;
        }
        End::Terminate(why) => {
            warn!(%peer, why, "terminating socket");
            writer.abort();
        }
        End::Gone => {
            debug!(%peer, "client gone");
            drop(out);
            let _ = timeout(std::time::Duration::from_secs(2), writer).await;
        }
    }
}

/// Waits for the first frame, which must be a valid hello, and admits it.
async fn await_hello(
    stream: &mut SplitStream<Socket>,
    out: &mut Outbox,
    hello_timeout: std::time::Duration,
) -> Result<Hello, End> {
    let deadline = Instant::now() + hello_timeout;
    let hello = loop {
        let frame = tokio::select! {
            frame = stream.next() => frame,
            _ = sleep_until(deadline) => return Err(End::Close(close::HELLO_TIMEOUT)),
        };
        match frame {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return Err(End::Gone),
            Some(Ok(Message::Text(text))) => match parse_inbound(text.as_str()) {
                Ok(Inbound::Hello(hello)) => break *hello,
                Ok(_) | Err(InboundError::UnknownFrame(_)) => {
                    return Err(End::Close(close::MESSAGE_BEFORE_HELLO));
                }
                Err(_) => return Err(End::Close(close::INVALID_HELLO)),
            },
            Some(Ok(Message::Binary(_))) => return Err(End::Close(close::MESSAGE_BEFORE_HELLO)),
            Some(Ok(_)) => continue, // control frames
        }
    };
    // Paseo's order: protocol version, then auth, then the clientId.
    if hello.protocol_version < 1 {
        if hello.wants_rejection_frame() {
            out.send(&hello_rejected(RejectReason::IncompatibleProtocol));
        }
        return Err(End::Close(close::INCOMPATIBLE_PROTOCOL));
    }
    // Auth: phase 0 has none and binds to loopback by default (see docs/design.md).
    if hello.client_id().is_empty() {
        return Err(End::Close(close::INVALID_HELLO));
    }
    if hello.has_reserved_client_id() {
        return Err(End::Close(close::INVALID_PLUGIN_CLIENT_ID));
    }
    Ok(hello)
}

/// Forgets a connection's client ([`crate::push::Clients`]) when dropped.
struct Gone<'a>(&'a Daemon, u64);

impl Drop for Gone<'_> {
    fn drop(&mut self) {
        self.0.clients.gone(self.1);
    }
}

async fn session(
    daemon: &Arc<Daemon>,
    stream: &mut SplitStream<Socket>,
    out: &mut Outbox,
    shape: ClientShape,
    mut shutdown: watch::Receiver<bool>,
) -> End {
    // Replies to requests that finish later (agent operations).
    let (late, mut late_replies) = mpsc::unbounded_channel();
    // Subscribe to changes before anything reads the store.
    let mut changes = daemon.changes();
    let generation = daemon.read(|store| store.generation.clone());
    let mut subs = Subscriptions::for_client(shape.compact_provider_snapshots);
    // However the session ends, notifications forget this client.
    let _gone = Gone(daemon, subs.connection());
    out.send(&daemon.server_info().frame());

    // Claimed by the first bare ping; renewed by every inbound frame after.
    let mut lease: Option<Instant> = None;
    let timing = daemon.config.timing;
    let mut lease_check = tokio::time::interval(timing.lease_check);
    lease_check.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        if out.overflowed {
            return End::Terminate("outbound buffer over the high-water mark");
        }
        // Biased: store events published before a late reply was queued are
        // forwarded first, so a reply never overtakes the events it caused.
        tokio::select! {
            biased;
            frame = stream.next() => {
                let frame = match frame {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return End::Gone,
                    Some(Ok(frame)) => frame,
                };
                let data_frame = matches!(frame, Message::Text(_) | Message::Binary(_));
                if data_frame && let Some(last) = lease.as_mut() {
                    *last = Instant::now();
                }
                match frame {
                    Message::Text(text) => match parse_inbound(text.as_str()) {
                        Ok(Inbound::Session(message)) => {
                            for frame in handlers::handle(daemon, &mut subs, shape, &late, message) {
                                out.send(&frame);
                            }
                        }
                        Ok(Inbound::Ping) => {
                            lease.get_or_insert_with(Instant::now);
                            out.send(&pong());
                        }
                        Ok(Inbound::Hello(_)) => return End::Close(close::UNEXPECTED_HELLO),
                        Ok(Inbound::RecordingState(_)) => {}
                        Err(err) => out.send(&status_error(&format!("Invalid message: {err}"))),
                    },
                    // Terminal input (0x02) and resize (0x03) need a slot
                    // from subscribe_terminal_request; phase 0 has none, so
                    // they are dropped. Anything else is an error.
                    Message::Binary(bytes) if !matches!(bytes.first(), Some(0x02 | 0x03)) => {
                        out.send(&status_error("Invalid message: unknown binary frame"));
                    }
                    // tungstenite answers protocol pings itself. They do not
                    // renew the lease, as in Paseo.
                    _ => {}
                }
            }
            change = changes.recv() => match change {
                Ok(event) => {
                    for frame in subs.route(&event, &generation) {
                        out.send(&frame);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // The client resynchronizes from its cursors on reconnect.
                    return End::Terminate("fell behind the change feed");
                }
                Err(broadcast::error::RecvError::Closed) => return End::Close(close::SHUTTING_DOWN),
            },
            Some(reply) = late_replies.recv() => out.send(&reply),
            _ = lease_check.tick() => {
                if lease.is_some_and(|last| last.elapsed() > timing.lease) {
                    return End::Terminate("socket lease expired");
                }
            }
            _ = shutdown.changed() => return End::Close(close::SHUTTING_DOWN),
        }
    }
}
