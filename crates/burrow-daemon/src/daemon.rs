//! Shared daemon state: configuration, the store and the change feed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use burrow_protocol::codes;
use burrow_protocol::server_info::ServerInfo;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::agents::Agents;
use crate::attachments::Attachments;
use crate::persist::{Persist, Writer};
use crate::store::{Store, StoreEvent};

/// The default listen address. Paseo's default port is 6767; Burrow uses
/// 6797 so both can run on one host.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:6797";

/// Store events a connection may fall behind by before it is dropped (it
/// reconnects and resynchronizes from its cursors).
const EVENT_BACKLOG: usize = 4096;

#[derive(Debug, Clone)]
pub struct Config {
    /// Bound at start; the daemon does not start without it.
    pub listen: SocketAddr,
    /// More addresses, each bound once it can be: one on an interface not
    /// up yet (a VPN's, just after boot) is tried again until it is.
    pub also_listen: Vec<SocketAddr>,
    /// Where persistent state lives. Phase 0 keeps only `server-id` there.
    pub home: Option<PathBuf>,
    /// Stable daemon identity (`srv_…`). Clients key hosts by it.
    pub server_id: String,
    pub hostname: Option<String>,
    /// `server_info.version`. The official app only displays it and compares
    /// it for equality with its own version.
    pub version: String,
    /// DNS names accepted in the `Host` header, besides localhost and IP literals.
    pub hostnames: Vec<String>,
    /// Origins accepted besides none and same-origin.
    pub allowed_origins: Vec<String>,
    pub timing: Timing,
    /// Where Burrow's git worktrees go. Defaults to `<home>/worktrees`,
    /// or a temporary directory without a home.
    pub worktrees_root: Option<PathBuf>,
}

/// Protocol timers. Paseo's values by default; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub hello_timeout: Duration,
    pub lease: Duration,
    pub lease_check: Duration,
    /// How often an address in [`Config::also_listen`] that could not be
    /// bound is tried again.
    pub listen_retry: Duration,
    /// How long an idle agent keeps its CLI before Burrow lets it go
    /// ([`crate::agents::release_idle`]); an hour, as Paseo 0.14. `None`
    /// keeps every CLI running.
    pub idle_release: Option<Duration>,
    /// How often idle agents are looked for.
    pub idle_check: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            hello_timeout: codes::HELLO_TIMEOUT,
            lease: codes::SOCKET_LEASE,
            lease_check: codes::LEASE_CHECK_EVERY,
            listen_retry: Duration::from_secs(5),
            idle_release: Some(Duration::from_secs(60 * 60)),
            idle_check: Duration::from_secs(5 * 60),
        }
    }
}

impl Config {
    /// A config for tests and embedding: ephemeral port, no home.
    pub fn ephemeral() -> Self {
        Self {
            listen: "127.0.0.1:0".parse().expect("literal address"),
            also_listen: Vec::new(),
            home: None,
            server_id: crate::ids::server_id(),
            hostname: Some("example-host".into()),
            version: default_version(),
            hostnames: Vec::new(),
            allowed_origins: Vec::new(),
            timing: Timing::default(),
            worktrees_root: None,
        }
    }

    /// The folder Burrow's worktrees go in, `<project hash>/<slug>`.
    pub fn worktrees_root(&self) -> PathBuf {
        match (&self.worktrees_root, &self.home) {
            (Some(root), _) => root.clone(),
            (None, Some(home)) => home.join("worktrees"),
            (None, None) => {
                std::env::temp_dir().join(format!("burrow-worktrees-{}", self.server_id))
            }
        }
    }
}

/// Where a client on this host reaches a daemon bound to `bound` and
/// `also`: a loopback address it listens on, else loopback on the port of
/// one bound to every interface, else the first. Never `0.0.0.0`, which a
/// client cannot connect to.
pub fn local_endpoint(bound: SocketAddr, also: &[SocketAddr]) -> String {
    let all = || std::iter::once(bound).chain(also.iter().copied());
    if let Some(loopback) = all().find(|addr| addr.ip().is_loopback()) {
        return loopback.to_string();
    }
    if let Some(any) = all().find(|addr| addr.ip().is_unspecified()) {
        let ip: std::net::IpAddr = match any {
            SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
            SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        };
        return SocketAddr::new(ip, any.port()).to_string();
    }
    bound.to_string()
}

pub fn default_version() -> String {
    format!("{}-burrow", env!("CARGO_PKG_VERSION"))
}

pub struct Daemon {
    pub config: Config,
    store: Mutex<Store>,
    events: broadcast::Sender<Arc<StoreEvent>>,
    /// The persistence writer, when there is a home directory.
    writer: Mutex<Option<Writer>>,
    /// Providers and running agents.
    pub agents: Agents,
    /// Prompt images, kept for every client.
    pub attachments: Attachments,
    /// Finished `workspace.create` replies by idempotency key, with the
    /// request they answered, so a retry gets the same workspace.
    pub creations: Mutex<HashMap<String, (Value, Value)>>,
    /// The last `workspace_setup_progress` snapshot per workspace.
    pub setups: Mutex<HashMap<String, Value>>,
    /// Where a client on this host reaches the daemon (`host:port`), once
    /// it listens: what its agents get as `PASEO_HOST`.
    pub local_endpoint: OnceLock<String>,
    /// When it started, as Paseo's `daemon.get_status` says it.
    pub started_at: String,
}

impl Daemon {
    /// A daemon whose state lives only in memory, whatever `config.home` says.
    pub fn new(config: Config) -> Arc<Self> {
        Self::with_store(config, Store::new(), None)
    }

    /// A daemon backed by `config.home`, loading what was saved there.
    /// Without a home it is the same as [`Daemon::new`].
    pub fn open(config: Config) -> std::io::Result<Arc<Self>> {
        let Some(home) = config.home.clone() else {
            return Ok(Self::new(config));
        };
        let (persist, writer, loaded) = Persist::open(&home)?;
        let store = Store::open(persist, loaded);
        Ok(Self::with_store(config, store, Some(writer)))
    }

    fn with_store(config: Config, mut store: Store, writer: Option<Writer>) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        let attachments = Attachments::new(config.home.as_deref(), &config.server_id);
        store.server_id = config.server_id.clone();
        Arc::new(Self {
            config,
            store: Mutex::new(store),
            events,
            writer: Mutex::new(writer),
            agents: Agents::default(),
            attachments,
            creations: Mutex::new(HashMap::new()),
            setups: Mutex::new(HashMap::new()),
            local_endpoint: OnceLock::new(),
            started_at: crate::clock::now_iso(),
        })
    }

    /// Waits until every change so far is on disk.
    pub fn flush(&self) {
        let persist = self.read(|store| store.persist().cloned());
        if let Some(persist) = persist {
            persist.flush();
        }
    }

    /// Stops every agent session, then flushes and stops the persistence
    /// writer. Later changes are not saved.
    pub fn close(&self) {
        self.agents.shutdown();
        self.flush();
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(writer);
    }

    pub fn server_info(&self) -> ServerInfo {
        ServerInfo::new(
            self.config.server_id.clone(),
            self.config.hostname.clone(),
            &self.config.version,
        )
    }

    fn lock(&self) -> MutexGuard<'_, Store> {
        // A panic while holding the lock leaves the store as the last
        // complete mutation left it; keep serving.
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn read<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        f(&self.lock())
    }

    /// Applies a change and publishes its events while still holding the
    /// lock, so every reader sees events in seq order relative to snapshots.
    pub fn apply<R>(&self, f: impl FnOnce(&mut Store) -> (R, Vec<StoreEvent>)) -> R {
        let mut store = self.lock();
        let (result, events) = f(&mut store);
        for event in events {
            // No receivers is fine: nobody is connected.
            let _ = self.events.send(Arc::new(event));
        }
        result
    }

    /// The change feed. Subscribe before reading a snapshot.
    pub fn changes(&self) -> broadcast::Receiver<Arc<StoreEvent>> {
        self.events.subscribe()
    }
}
