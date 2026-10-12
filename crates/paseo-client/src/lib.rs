//! A WebSocket client for one Paseo daemon.
//!
//! The wire is JSON text frames. Session traffic travels as
//! `{"type":"session","message":{...}}` in both directions; `ping`/`pong`,
//! `hello` and `hello.rejected` travel bare. Requests carry a `requestId`
//! at the top level and every response echoes it in `payload.requestId`.
//!
//! Networking runs on a private tokio runtime. Everything this crate hands
//! back (request futures, the event receiver) is runtime-agnostic, so a GPUI
//! app can await them on its own executor.

mod connection;
mod events;
pub mod proto;
pub mod terminal;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

pub use connection::ConnState;
pub use events::EventReceiver;
pub use proto::ServerInfo;

/// What a daemon connection reports, in order.
#[derive(Debug, Clone)]
pub enum Event {
    State(ConnState),
    /// A session message that was not the reply to one of our requests:
    /// subscription pushes, status broadcasts, and so on.
    Message(Value),
    Binary(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The connection dropped before the reply arrived.
    Disconnected,
    Timeout,
    /// The daemon answered with `rpc_error`.
    Rpc {
        code: Option<String>,
        message: String,
    },
    /// The typed reply carried an `error` field.
    Rejected(String),
    Decode(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Disconnected => f.write_str("not connected"),
            Error::Timeout => f.write_str("timed out"),
            Error::Rpc {
                code: Some(code),
                message,
            } => write!(f, "{message} ({code})"),
            Error::Rpc {
                code: None,
                message,
            } => f.write_str(message),
            Error::Rejected(message) => f.write_str(message),
            Error::Decode(message) => write!(f, "unexpected reply: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// The version of the daemon protocol this client speaks, sent in `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct Config {
    /// `host:port`, `ws://…` or `wss://…`.
    pub endpoint: String,
    pub client_id: String,
    pub app_version: String,
    pub password: Option<String>,
}

impl Config {
    pub fn new(endpoint: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            client_id: client_id.into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            password: None,
        }
    }

    pub fn url(&self) -> String {
        let endpoint = self.endpoint.trim().trim_end_matches('/');
        if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
            if endpoint.ends_with("/ws") {
                endpoint.to_string()
            } else {
                format!("{endpoint}/ws")
            }
        } else {
            format!("ws://{endpoint}/ws")
        }
    }
}

pub(crate) enum Command {
    Request {
        id: String,
        message: Value,
        reply: oneshot::Sender<Result<Value, Error>>,
        /// For a request answered in several messages, the type of the last.
        until: Option<String>,
        /// Past this the caller has given up; never send it late.
        deadline: tokio::time::Instant,
    },
    Cancel(String),
    Send(Value),
    SendBinary(Vec<u8>),
    Reconnect,
    Close,
}

/// A handle to one daemon. Cloning shares the connection.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    commands: mpsc::UnboundedSender<Command>,
    events: EventReceiver,
    endpoint: String,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.events.shutdown();
        let _ = self.commands.send(Command::Close);
    }
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

impl Client {
    /// Starts connecting in the background and keeps reconnecting until the
    /// last handle is dropped.
    pub fn connect(config: Config) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = events::channel();
        let endpoint = config.endpoint.clone();
        runtime().spawn(connection::run(config, command_rx, event_tx));
        Self {
            inner: Arc::new(Inner {
                commands,
                events,
                endpoint,
            }),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }

    /// Every event from this connection. There is one queue; give it to one reader.
    /// Drain it independently of request futures; waiting for a reply inside
    /// this loop can block the socket behind event backpressure. See EventReceiver.
    pub fn events(&self) -> EventReceiver {
        self.inner.events.clone()
    }

    /// Sends a session request and waits for the reply's payload.
    pub fn request(
        &self,
        message: Value,
    ) -> impl std::future::Future<Output = Result<Value, Error>> + Send + 'static {
        self.request_with_timeout(message, DEFAULT_TIMEOUT)
    }

    pub fn request_with_timeout(
        &self,
        message: Value,
        timeout: Duration,
    ) -> impl std::future::Future<Output = Result<Value, Error>> + Send + 'static {
        self.send_request(message, timeout, None)
    }

    /// A request the daemon answers in several messages, each carrying its
    /// id, the last of type `until`: the earlier messages' payloads, and the
    /// last's.
    pub fn request_until(
        &self,
        message: Value,
        until: &str,
        timeout: Duration,
    ) -> impl std::future::Future<Output = Result<(Vec<Value>, Value), Error>> + Send + 'static
    {
        let reply = self.send_request(message, timeout, Some(until.to_string()));
        async move {
            let mut both = reply.await?;
            let updates = match both["updates"].take() {
                Value::Array(updates) => updates,
                _ => Vec::new(),
            };
            Ok((updates, both["final"].take()))
        }
    }

    fn send_request(
        &self,
        mut message: Value,
        timeout: Duration,
        until: Option<String>,
    ) -> impl std::future::Future<Output = Result<Value, Error>> + Send + 'static {
        let id = message
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(new_request_id);
        if let Some(object) = message.as_object_mut() {
            object.insert("requestId".into(), Value::String(id.clone()));
        }
        let (reply, rx) = oneshot::channel();
        let sent = self
            .inner
            .commands
            .send(Command::Request {
                id: id.clone(),
                message,
                reply,
                until,
                deadline: tokio::time::Instant::now() + timeout,
            })
            .is_ok();
        let commands = self.inner.commands.clone();
        async move {
            if !sent {
                return Err(Error::Disconnected);
            }
            let outcome = runtime_agnostic_timeout(timeout, rx).await;
            match outcome {
                Some(Ok(result)) => result,
                Some(Err(_)) => Err(Error::Disconnected),
                None => {
                    let _ = commands.send(Command::Cancel(id));
                    Err(Error::Timeout)
                }
            }
        }
    }

    /// Fire-and-forget session message (terminal input, heartbeats).
    pub fn send(&self, message: Value) {
        let _ = self.inner.commands.send(Command::Send(message));
    }

    pub fn send_binary(&self, frame: Vec<u8>) {
        let _ = self.inner.commands.send(Command::SendBinary(frame));
    }

    /// Drops the current socket and dials again now, skipping any backoff.
    pub fn reconnect(&self) {
        let _ = self.inner.commands.send(Command::Reconnect);
    }
}

/// A request id. Unique per process is enough; the daemon only echoes it.
pub fn new_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("crq_{}_{n}", std::process::id())
}

/// A stable-looking client id in Paseo's `cid_<hex>` shape.
pub fn generate_client_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::from("cid_");
    for _ in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        );
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("paseo-client")
            .enable_all()
            .build()
            .expect("paseo-client runtime")
    })
}

/// Waits for `rx` up to `limit` without requiring the caller to be inside tokio:
/// the timer runs on the client's own runtime.
async fn runtime_agnostic_timeout<T: Send + 'static>(
    limit: Duration,
    rx: oneshot::Receiver<T>,
) -> Option<Result<T, oneshot::error::RecvError>> {
    let (done_tx, done_rx) = oneshot::channel();
    runtime().spawn(async move {
        let outcome = tokio::time::timeout(limit, rx).await.ok();
        let _ = done_tx.send(outcome);
    });
    done_rx.await.ok().flatten()
}

/// Runs a future to completion on the client runtime. For tests and tools.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    runtime().block_on(future)
}
