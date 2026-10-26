//! The daemons' side: one connection per host, the merged directory, and
//! the conversations phones have open. This is crusteo's `FleetStore`
//! without GPUI: the same bootstrap order, the same held-push replay, the
//! same timeline catch-up and seq gate (those live in `fleet`).
//!
//! Every request goes through [`guarded`], which refuses any type not in
//! [`READ_REQUESTS`] or [`WRITE_REQUESTS`]. The writes are the phone's
//! actions (`actions.rs`, and `agents.rs` for creating, deleting and
//! relabelling), each added on purpose. Never on the list:
//! `clear_agent_attention`, which would clear "ready to review" for the
//! people at their desks, anything about terminals, and workspace changes
//! other than assigning a label.

mod actions;
mod agents;
mod replies;

pub use actions::{QUEUE_MAX, Queued, Reply};
pub use agents::NewAgent;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fleet::timeline::{Follow, Timeline};
use fleet::{Connection, Fleet, HostConfig, requests};
use paseo_client::{Client, Config, ConnState, Event};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::host_id;

/// The requests Reef sends a daemon to keep its picture current.
pub const READ_REQUESTS: &[&str] = &[
    "session.events.set_subscription.request",
    "project.list.request",
    "fetch_workspaces_request",
    "fetch_agents_request",
    "workspace.label.list.request",
    "agent.timeline.set_subscription.request",
    "fetch_agent_timeline_request",
    "subscription.release.request",
    // Each account's usage, for the `usage` view.
    usage_source::reports::REPORTS_REQUEST,
    // Providers, models and modes, for a phone's new-agent form; asked
    // only when a phone opens one.
    "get_providers_snapshot_request",
];

/// Work to do on the upstream state, sent back by request tasks.
pub type Apply = Box<dyn FnOnce(&mut Upstream) + Send>;

/// An event from one host's connection: host, link generation, event.
pub type HostEvent = (String, u64, Event);

/// A conversation timeline page, at most this many entries.
const PAGE: u32 = 40;

/// The only writes: what the phone's actions send, and nothing else.
pub const WRITE_REQUESTS: &[&str] = &[
    // Allow or deny a permission; answer or dismiss a question.
    "agent_permission_response",
    // Stop the running turn.
    "cancel_agent_request",
    // Send a message: to an idle agent, from the queue, or steering.
    "send_agent_message_request",
    // Archive an agent; the page asks first.
    "archive_agent_request",
    // Create an agent, in a workspace or a directory.
    "agent.create.request",
    // Delete an agent for good; the page asks first.
    "delete_agent_request",
    // Put a label on an agent's workspace, or take it off: the labels a
    // row shows are its workspace's, as in crusteo.
    "workspace.label.assignment.set.request",
];

/// Sends `message` if it is a known read or one of the phone's writes;
/// anything else is refused here.
/// The one `update_agent_request` Reef may send: an agent's labels and
/// nothing else ([`fleet::labels::set_request`]), checked field by field, so
/// a name, or any other label key, never rides along.
pub fn labels_write(message: &Value) -> bool {
    let Some(fields) = message.as_object() else {
        return false;
    };
    let only_labels = (message["labels"].as_object()).is_some_and(|labels| {
        labels.len() == 1 && labels.get(fleet::labels::KEY).is_some_and(Value::is_string)
    });
    message["type"] == "update_agent_request"
        && message["agentId"].is_string()
        && only_labels
        && (fields.keys())
            .all(|key| matches!(key.as_str(), "type" | "agentId" | "labels" | "requestId"))
}

pub fn guarded(
    client: &Client,
    message: Value,
) -> impl std::future::Future<Output = Result<Value, String>> + Send + 'static {
    guarded_with_timeout(client, message, paseo_client::DEFAULT_TIMEOUT)
}

/// [`guarded`] for a read the daemon answers in several messages, the last
/// of type `until`: the earlier messages' payloads, and the last's.
pub fn guarded_until(
    client: &Client,
    message: Value,
    until: &str,
) -> impl std::future::Future<Output = Result<(Vec<Value>, Value), String>> + Send + 'static {
    let kind = message["type"].as_str().unwrap_or_default().to_string();
    let allowed = READ_REQUESTS.contains(&kind.as_str());
    let reply =
        allowed.then(|| client.request_until(message, until, paseo_client::DEFAULT_TIMEOUT));
    async move {
        match reply {
            Some(reply) => reply.await.map_err(|err| err.to_string()),
            None => {
                eprintln!("reef: refused to send {kind:?}: not on Reef's list");
                Err(format!("{kind} is not on Reef's list"))
            }
        }
    }
}

pub fn guarded_with_timeout(
    client: &Client,
    message: Value,
    timeout: Duration,
) -> impl std::future::Future<Output = Result<Value, String>> + Send + 'static {
    let kind = message["type"].as_str().unwrap_or_default().to_string();
    let allowed = READ_REQUESTS.contains(&kind.as_str())
        || WRITE_REQUESTS.contains(&kind.as_str())
        || labels_write(&message);
    let reply = allowed.then(|| client.request_with_timeout(message, timeout));
    async move {
        match reply {
            Some(reply) => reply.await.map_err(|err| err.to_string()),
            None => {
                eprintln!("reef: refused to send {kind:?}: not on Reef's list");
                Err(format!("{kind} is not on Reef's list"))
            }
        }
    }
}

struct Link {
    client: Client,
    /// Which `connect` made this link; events from a replaced one drop.
    generation: u64,
    /// A fresh value, from one counter for every link, on each connect,
    /// disconnect and removal. Bootstrap work from an older connection
    /// stops at its next step, even when the host was removed and added
    /// again under the same name.
    epoch: Arc<AtomicU64>,
    online: bool,
    /// Agents whose timelines we stream on this host.
    watched: BTreeSet<String>,
    timeline_subscription: Option<String>,
    /// Directory pushes that arrived while the bootstrap replies were in
    /// flight; replayed after them so a snapshot never erases newer pushes.
    bootstrapping: bool,
    held: Vec<Value>,
}

/// One open conversation.
#[derive(Default)]
pub struct Session {
    pub timeline: Timeline,
    pub loading: bool,
    pub error: Option<String>,
    /// Pages that landed or failed, so a phone waiting for older history
    /// knows when to look again.
    pub pages: u64,
    fetching: bool,
    reload_pending: bool,
    viewers: usize,
    idle_since: Option<Instant>,
}

impl Session {
    pub fn fetching(&self) -> bool {
        self.fetching
    }
}

pub struct Upstream {
    pub fleet: Fleet,
    links: HashMap<String, Link>,
    pub sessions: HashMap<(String, String), Session>,
    client_id: String,
    next_generation: u64,
    next_epoch: u64,
    events: mpsc::Sender<HostEvent>,
    apply: mpsc::UnboundedSender<Apply>,
    /// The directory or a connection changed since Reef last looked.
    pub dirty_fleet: bool,
    /// Conversations that changed since Reef last looked.
    pub dirty_sessions: HashSet<(String, String)>,
    /// Messages sent while their agent was busy, sent one per idle edge.
    pub queues: HashMap<(String, String), Vec<Queued>>,
    /// Answers for phones, by connection, for Reef to deliver.
    pub outbox: Vec<(u64, Value)>,
    /// Agents whose turn just ended, and whether it failed. Reef
    /// decides which become notifications.
    pub finished: Vec<(String, String, bool)>,
    /// Notifications ready to send every phone.
    pub notices: Vec<Value>,
    /// Until when an agent's turn ending is not reported: a few seconds
    /// after one was, so the same end seen two ways (directory and
    /// stream) is reported once, and a while after the phone stopped it.
    quiet_until: HashMap<(String, String), Instant>,
    /// Agents with a message on its way to their daemon.
    sending: HashSet<(String, String)>,
    /// Agents whose idle edge came while a message was on its way; their
    /// queue is looked at again when it lands.
    missed_edges: HashSet<(String, String)>,
    next_notice: u64,
    /// Each agent's latest reply, for list rows.
    pub replies: replies::Replies,
    /// Each host's connection state and when it began.
    host_since: HashMap<String, (String, String)>,
    /// Every usage source's latest reading: the feed's and each daemon's,
    /// keyed as crusteo keys them ([`crate::usage`]).
    pub usage: BTreeMap<String, usage_source::Reading>,
    /// A reading changed since Reef last looked.
    pub dirty_usage: bool,
    /// When each host was last asked for its usage, and how often to ask.
    usage_asked: HashMap<String, Instant>,
    pub usage_every: Duration,
    /// Each host's providers, read when a phone opens a new-agent form.
    providers: HashMap<String, agents::Providers>,
}

impl Upstream {
    pub fn new(
        hosts: Vec<HostConfig>,
        client_id: String,
        events: mpsc::Sender<HostEvent>,
        apply: mpsc::UnboundedSender<Apply>,
    ) -> Self {
        Self {
            fleet: Fleet::new(hosts),
            links: HashMap::new(),
            sessions: HashMap::new(),
            client_id,
            next_generation: 0,
            next_epoch: 0,
            events,
            apply,
            dirty_fleet: true,
            dirty_sessions: HashSet::new(),
            queues: HashMap::new(),
            outbox: Vec::new(),
            finished: Vec::new(),
            notices: Vec::new(),
            quiet_until: HashMap::new(),
            sending: HashSet::new(),
            missed_edges: HashSet::new(),
            next_notice: 0,
            replies: replies::Replies::default(),
            host_since: HashMap::new(),
            usage: BTreeMap::new(),
            dirty_usage: false,
            usage_asked: HashMap::new(),
            usage_every: usage_source::reports::ASK_EVERY,
            providers: HashMap::new(),
        }
    }

    /// Sets, or with `None` clears, the usage reading under `key`, as
    /// crusteo does: an account whose read failed keeps its last numbers,
    /// and every account its place ([`usage_source::merge::set`]).
    pub fn set_usage(&mut self, key: &str, reading: Option<usage_source::Reading>) {
        self.dirty_usage |= usage_source::merge::set(&mut self.usage, key, reading);
    }

    /// Asks each connected host that keeps usage reports for them, at most
    /// every [`Self::usage_every`] (two minutes, as crusteo asks), and a
    /// host that just came up at once. A host that fails keeps its last
    /// reading.
    pub fn ask_usage(&mut self) {
        let every = self.usage_every;
        use usage_source::reports::{REPORTS_DONE, REPORTS_REQUEST, parse_reports};
        let due: Vec<String> = (self.fleet.hosts.iter())
            .filter(|host| host.connection.is_online() && host.has("usageSources"))
            .map(|host| host.name().to_string())
            .filter(|name| (self.usage_asked.get(name)).is_none_or(|at| at.elapsed() >= every))
            .collect();
        for name in due {
            let Some(link) = self.links.get(&name).filter(|link| link.online) else {
                continue;
            };
            self.usage_asked.insert(name.clone(), Instant::now());
            let generation = link.generation;
            let reply = guarded_until(
                &link.client,
                json!({ "type": REPORTS_REQUEST }),
                REPORTS_DONE,
            );
            let apply = self.apply.clone();
            tokio::spawn(async move {
                let Ok((updates, last)) = reply.await else {
                    return;
                };
                if !last["error"].is_null() {
                    return;
                }
                let reading = parse_reports(&name, &updates, usage_source::unix_now());
                let _ = apply.send(Box::new(move |up: &mut Upstream| {
                    // Not for a host removed, or added again, meanwhile.
                    if up
                        .links
                        .get(&name)
                        .is_some_and(|l| l.generation == generation)
                    {
                        up.set_usage(&crate::usage::host_key(&name), reading);
                    }
                }));
            });
        }
    }

    /// Whether every host is connected and has its whole agent list, so an
    /// agent missing from the fleet is really gone. False with no hosts.
    pub fn all_synced(&self) -> bool {
        !self.fleet.hosts.is_empty()
            && self.fleet.hosts.iter().all(|host| {
                host.directory.agents_hydrated
                    && self
                        .links
                        .get(host.name())
                        .is_some_and(|link| link.online && !link.bootstrapping)
            })
    }

    /// Whether the agent is in a turn now, as the directory says.
    pub fn running(&self, host: &str, agent: &str) -> bool {
        self.fleet
            .agent(&host_id(host), agent)
            .is_some_and(|a| a.status == "running" || a.turn_open())
    }

    fn running_on(&self, host: &str) -> HashMap<String, bool> {
        self.fleet
            .host(&host_id(host))
            .map(|h| {
                h.directory
                    .agents
                    .values()
                    .map(|a| (a.id.clone(), a.status == "running" || a.turn_open()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Agents that were running before a change and are not now: their
    /// turn ended. Each gets its queue drained by one message and is
    /// reported as finished.
    fn turn_edges(&mut self, host: &str, before: &HashMap<String, bool>) {
        let ended: Vec<(String, bool)> = before
            .iter()
            .filter(|(_, was)| **was)
            .filter_map(|(id, _)| {
                let agent = self.fleet.agent(&host_id(host), id)?;
                let still = agent.status == "running" || agent.turn_open();
                (!still).then(|| (id.clone(), agent.status == "error"))
            })
            .collect();
        for (agent, failed) in ended {
            self.turn_ended(host, &agent, failed);
        }
    }

    fn turn_ended(&mut self, host: &str, agent: &str, failed: bool) {
        let key = (host.to_string(), agent.to_string());
        let now = Instant::now();
        let quiet = self.quiet_until.get(&key).is_some_and(|until| now < *until);
        if !quiet {
            self.quiet_until.insert(key, now + Duration::from_secs(5));
            self.finished
                .push((host.to_string(), agent.to_string(), failed));
        }
        self.drain_queue(host, agent);
    }

    /// Dials every saved host. Call once, inside Reef's runtime.
    pub fn connect_all(&mut self) {
        let names: Vec<String> = self
            .fleet
            .hosts
            .iter()
            .map(|h| h.name().to_string())
            .collect();
        for name in names {
            self.connect(&name);
        }
    }

    /// Adds a host and dials it; false when one has that name already.
    pub fn add_host(&mut self, config: HostConfig) -> bool {
        if self.fleet.host(&host_id(&config.name)).is_some() {
            return false;
        }
        let name = config.name.clone();
        self.fleet.hosts.push(fleet::Host::new(config));
        self.connect(&name);
        self.dirty_fleet = true;
        true
    }

    /// Removes a host: its connection closes and everything kept for its
    /// agents goes. False when there is no such host.
    pub fn remove_host(&mut self, name: &str) -> bool {
        let before = self.fleet.hosts.len();
        self.fleet.hosts.retain(|host| host.name() != name);
        if self.fleet.hosts.len() == before {
            return false;
        }
        // Work still running for the old connection stops at its next
        // step; dropping the link closes the connection once that work
        // lets go of it. Events on their way no longer match a link.
        self.bump_epoch(name);
        self.links.remove(name);
        self.sessions.retain(|key, _| key.0 != name);
        self.dirty_sessions.retain(|key| key.0 != name);
        self.queues.retain(|key, _| key.0 != name);
        self.quiet_until.retain(|key, _| key.0 != name);
        self.sending.retain(|key| key.0 != name);
        self.missed_edges.retain(|key| key.0 != name);
        self.finished.retain(|(host, _, _)| host != name);
        self.host_since.remove(name);
        self.usage_asked.remove(name);
        self.forget_providers(name);
        self.set_usage(&crate::usage::host_key(name), None);
        self.forget_host(name);
        self.dirty_fleet = true;
        true
    }

    fn connect(&mut self, name: &str) {
        let Some(host) = self.fleet.host_mut(&host_id(name)) else {
            return;
        };
        let Some(endpoint) = host
            .config
            .endpoint
            .clone()
            .filter(|e| !e.trim().is_empty())
        else {
            host.connection = Connection::Offline("No address saved".into());
            return;
        };
        host.connection = Connection::Connecting;
        let mut config = Config::new(endpoint, self.client_id.clone());
        config.app_version = format!("reef/{}", env!("CARGO_PKG_VERSION"));
        config.password = host.config.password.clone();
        let client = Client::connect(config);
        let events = client.events();
        self.next_generation += 1;
        let generation = self.next_generation;
        self.next_epoch += 1;
        let epoch = self.next_epoch;
        self.links.insert(
            name.to_string(),
            Link {
                client,
                generation,
                epoch: Arc::new(AtomicU64::new(epoch)),
                online: false,
                watched: BTreeSet::new(),
                timeline_subscription: None,
                bootstrapping: false,
                held: Vec::new(),
            },
        );
        // Drain the connection on its own task, as the client asks, so a
        // busy Reef never stalls the socket behind a request reply.
        let tx = self.events.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if tx.send((name.clone(), generation, event)).await.is_err() {
                    break;
                }
            }
        });
    }

    fn mark_session(&mut self, host: &str, agent: &str) {
        self.dirty_sessions
            .insert((host.to_string(), agent.to_string()));
    }

    // ---- events from a daemon -----------------------------------------

    pub fn on_event(&mut self, name: &str, generation: u64, event: Event) {
        if !self
            .links
            .get(name)
            .is_some_and(|link| link.generation == generation)
        {
            return;
        }
        match event {
            Event::State(ConnState::Connecting { .. }) => {
                if let Some(host) = self.fleet.host_mut(&host_id(name))
                    && (!matches!(host.connection, Connection::Offline(_)) || host.server.is_none())
                {
                    host.connection = Connection::Connecting;
                }
                self.dirty_fleet = true;
            }
            Event::State(ConnState::Connected(info)) => {
                if let Some(host) = self.fleet.host_mut(&host_id(name)) {
                    host.connection = Connection::Online;
                    host.server = Some(info);
                }
                let Some(epoch) = self.bump_epoch(name) else {
                    return;
                };
                let Some(link) = self.links.get_mut(name) else {
                    return;
                };
                link.online = true;
                link.timeline_subscription = None;
                link.bootstrapping = true;
                link.held.clear();
                // The daemon may have restarted with other providers.
                self.forget_providers(name);
                self.bootstrap(name, epoch);
                self.resubscribe_timelines(name, true);
                self.ask_usage();
                self.dirty_fleet = true;
            }
            Event::State(ConnState::Disconnected { reason, auth }) => {
                if let Some(host) = self.fleet.host_mut(&host_id(name)) {
                    host.connection = if auth {
                        Connection::Refused(reason)
                    } else {
                        Connection::Offline(reason)
                    };
                    host.directory.disconnected();
                }
                self.bump_epoch(name);
                if let Some(link) = self.links.get_mut(name) {
                    link.online = false;
                    link.timeline_subscription = None;
                }
                self.dirty_fleet = true;
            }
            Event::Message(message) => self.on_push(name, message),
            // Terminal frames; Reef opens no terminals.
            Event::Binary(_) => {}
        }
        self.note_host(name);
    }

    /// Records when a host entered its connection state, so phones can
    /// tell a blip from an outage.
    fn note_host(&mut self, name: &str) {
        let Some(host) = self.fleet.host(&host_id(name)) else {
            return;
        };
        // Up or not: a host retrying goes between connecting and offline,
        // and that is one outage.
        let state = if host.connection.is_online() {
            "up"
        } else {
            "down"
        }
        .to_string();
        let changed = self
            .host_since
            .get(name)
            .is_none_or(|(known, _)| *known != state);
        if changed {
            let since = (state, fleet::clock::now_iso());
            self.host_since.insert(name.to_string(), since);
            self.dirty_fleet = true;
        }
    }

    /// When each host entered its current connection state.
    pub fn host_since(&self) -> HashMap<String, String> {
        self.host_since
            .iter()
            .map(|(name, (_, at))| (name.clone(), at.clone()))
            .collect()
    }

    fn on_push(&mut self, name: &str, message: Value) {
        let kind = message["type"].as_str().unwrap_or_default().to_string();
        let payload = &message["payload"];
        match kind.as_str() {
            "agent_stream" => {
                let agent = payload["agentId"].as_str().unwrap_or_default().to_string();
                let key = (name.to_string(), agent.clone());
                if let Some(session) = self.sessions.get_mut(&key) {
                    let follow = session.timeline.apply_live(payload);
                    self.mark_session(name, &agent);
                    self.follow(name, &agent, follow);
                    // An open conversation hears its turn end before the
                    // directory does.
                    match payload["event"]["type"].as_str() {
                        // A stopped turn has ended too: its queue goes on,
                        // as it does from the directory's idle edge.
                        Some("turn_completed" | "turn_canceled") => {
                            self.turn_ended(name, &agent, false)
                        }
                        Some("turn_failed") => self.turn_ended(name, &agent, true),
                        // Its row shows the reply as it streams.
                        Some("timeline")
                            if payload["event"]["item"]["type"] == "assistant_message" =>
                        {
                            self.reply_from_session(name, &agent)
                        }
                        _ => {}
                    }
                }
            }
            "agent_permission_request" | "agent_permission_resolved" => {
                let agent = payload["agentId"].as_str().unwrap_or_default().to_string();
                let key = (name.to_string(), agent.clone());
                if let Some(session) = self.sessions.get_mut(&key) {
                    session.timeline.apply_permission_event(&kind, payload);
                    self.mark_session(name, &agent);
                }
            }
            "agent.timeline.replacement" => {
                let agent = payload["agentId"].as_str().unwrap_or_default().to_string();
                self.follow(name, &agent, Follow::Reload);
            }
            // Push notifications come later; the directory push that
            // goes with it already moves the agent into needs-you.
            "agent_attention_required" => {}
            // Read again the next time a phone asks, not kept ten minutes.
            "providers_snapshot_update" => self.forget_providers(name),
            _ => {
                if let Some(link) = self.links.get_mut(name)
                    && link.bootstrapping
                {
                    link.held.push(message);
                    return;
                }
                let agent_id = payload["agent"]["id"]
                    .as_str()
                    .or(payload["agentId"].as_str())
                    .unwrap_or_default()
                    .to_string();
                let was_running = (kind == "agent_update").then(|| self.running(name, &agent_id));
                let Some(host) = self.fleet.host_mut(&host_id(name)) else {
                    return;
                };
                let changed = host.directory.apply_push(&message);
                // Nothing in Reef reads the journal; keep it from growing.
                host.directory.journal.clear();
                if kind == "agent_update" && payload["kind"] == "upsert" {
                    self.on_agent_snapshot(name, &payload["agent"]);
                }
                if let Some(was) = was_running {
                    let gone = self.fleet.agent(&host_id(name), &agent_id).is_none();
                    let before = HashMap::from([(agent_id.clone(), was)]);
                    self.turn_edges(name, &before);
                    if gone {
                        // Archived or removed: nothing kept for it stays.
                        self.forget_agent(name, &agent_id);
                    }
                }
                if changed.any() {
                    self.dirty_fleet = true;
                }
            }
        }
    }

    /// Drops what Reef keeps for an agent that is gone: its queue, its
    /// quiet time, its latest reply.
    fn forget_agent(&mut self, host: &str, agent: &str) {
        let key = (host.to_string(), agent.to_string());
        if self.queues.remove(&key).is_some() {
            self.dirty_sessions.insert(key.clone());
        }
        self.quiet_until.remove(&key);
        self.missed_edges.remove(&key);
        self.forget_agent_reply(&key);
    }

    /// An agent snapshot arrived: keep its open conversation's turn and
    /// permissions current.
    fn on_agent_snapshot(&mut self, host: &str, agent: &Value) {
        let Some(agent_id) = agent["id"].as_str() else {
            return;
        };
        let key = (host.to_string(), agent_id.to_string());
        if let Some(session) = self.sessions.get_mut(&key) {
            session.timeline.sync_snapshot(agent);
            self.dirty_sessions.insert(key);
        }
    }

    // ---- bootstrap ----------------------------------------------------

    /// Gives the host's link a fresh epoch, unique across every link the
    /// Reef has had, and returns it.
    fn bump_epoch(&mut self, name: &str) -> Option<u64> {
        let link = self.links.get(name)?;
        self.next_epoch += 1;
        link.epoch.store(self.next_epoch, Ordering::SeqCst);
        Some(self.next_epoch)
    }

    /// Whether `epoch` is the host's live connection. Epochs are never
    /// reused, so one from a removed link cannot match its replacement.
    fn is_current(&self, name: &str, epoch: u64) -> bool {
        self.links
            .get(name)
            .is_some_and(|link| link.online && link.epoch.load(Ordering::SeqCst) == epoch)
    }

    /// Subscribes to everything the views need, in order, then replays the
    /// pushes that arrived meanwhile. A failed step reconnects after a
    /// pause, so it is retried.
    fn bootstrap(&mut self, name: &str, epoch: u64) {
        let (Some(link), Some(host)) = (self.links.get(name), self.fleet.host(&host_id(name)))
        else {
            return;
        };
        let client = link.client.clone();
        let current = link.epoch.clone();
        let read = requests::DirectoryRead::for_host(host);
        let sync = read.sync;
        let project_list = host.has("projectList");
        let labels = host.has("workspaceLabels");
        let dir = &host.directory;
        let workspace_cursor = dir.workspace_cursor.clone();
        let agent_cursor = dir.agent_cursor.clone();
        let project_cursor = dir.project_cursor.clone();
        let apply = self.apply.clone();
        let name = name.to_string();

        tokio::spawn(async move {
            let live = || current.load(Ordering::SeqCst) == epoch;
            // Applies a step's result only if this connection is still live.
            let send = |f: Apply| {
                let _ = apply.send(f);
            };
            let mut failed = false;

            let events = [
                "project.update",
                "script_status_update",
                "agent_permission_request",
                "agent_permission_resolved",
                "agent_attention_required",
                // A host's tools or models changed (a CLI upgraded under
                // its Burrow): the phones' new-agent form reads them again.
                "providers_snapshot_update",
            ];
            match guarded(&client, requests::session_events(&events)).await {
                Ok(payload) => {
                    let name = name.clone();
                    send(Box::new(move |up| {
                        if up.is_current(&name, epoch)
                            && let Some(host) = up.fleet.host_mut(&host_id(&name))
                        {
                            host.directory.subscriptions.events =
                                payload["subscriptionId"].as_str().map(str::to_string);
                        }
                    }));
                }
                Err(err) => {
                    log(&name, "session events", &err);
                    failed = true;
                }
            }

            if live() && project_list {
                match guarded(
                    &client,
                    requests::list_projects(sync, project_cursor.as_ref()),
                )
                .await
                {
                    Ok(payload) => {
                        let name = name.clone();
                        send(Box::new(move |up| {
                            if up.is_current(&name, epoch)
                                && let Some(host) = up.fleet.host_mut(&host_id(&name))
                            {
                                host.directory.apply_projects_reply(&payload);
                                up.dirty_fleet = true;
                            }
                        }));
                    }
                    Err(err) => {
                        log(&name, "projects", &err);
                        failed = true;
                    }
                }
            } else if live() {
                let name = name.clone();
                send(Box::new(move |up| {
                    if up.is_current(&name, epoch)
                        && let Some(host) = up.fleet.host_mut(&host_id(&name))
                    {
                        host.directory.projects_from_workspaces = true;
                    }
                }));
            }

            // Workspaces, then every following page when not syncing.
            let mut request = requests::fetch_workspaces(read, workspace_cursor.as_ref());
            let mut first = true;
            while live() {
                match guarded(&client, request).await {
                    Ok(payload) => {
                        let next = next_page(&payload);
                        let page_first = first;
                        let name = name.clone();
                        send(Box::new(move |up| {
                            if up.is_current(&name, epoch)
                                && let Some(host) = up.fleet.host_mut(&host_id(&name))
                            {
                                host.directory.apply_workspaces_reply(&payload, page_first);
                                up.dirty_fleet = true;
                            }
                        }));
                        match next {
                            Some(cursor) => {
                                request = requests::fetch_workspaces_page(read, &cursor);
                                first = false;
                            }
                            None => break,
                        }
                    }
                    Err(err) => {
                        log(&name, "workspaces", &err);
                        failed = true;
                        break;
                    }
                }
            }

            let mut request = requests::fetch_agents(read, agent_cursor.as_ref());
            let mut first = true;
            let mut agents_complete = false;
            while live() {
                match guarded(&client, request).await {
                    Ok(payload) => {
                        let next = next_page(&payload);
                        let page_first = first;
                        let name = name.clone();
                        send(Box::new(move |up| {
                            if !up.is_current(&name, epoch) {
                                return;
                            }
                            // Turns that ended while Reef was away
                            // count as ended now.
                            let before = up.running_on(&name);
                            if let Some(host) = up.fleet.host_mut(&host_id(&name)) {
                                host.directory.apply_agents_reply(&payload, page_first);
                            }
                            for entry in payload["entries"].as_array().into_iter().flatten() {
                                up.on_agent_snapshot(&name, &entry["agent"]);
                            }
                            up.turn_edges(&name, &before);
                            up.dirty_fleet = true;
                        }));
                        match next {
                            Some(cursor) => {
                                request = requests::fetch_agents_page(read, &cursor);
                                first = false;
                            }
                            None => {
                                agents_complete = true;
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        log(&name, "agents", &err);
                        failed = true;
                        break;
                    }
                }
            }

            if live() && labels {
                match guarded(&client, requests::list_labels()).await {
                    Ok(payload) => {
                        let name = name.clone();
                        send(Box::new(move |up| {
                            if up.is_current(&name, epoch)
                                && let Some(host) = up.fleet.host_mut(&host_id(&name))
                            {
                                host.directory.apply_labels_reply(&payload);
                                up.dirty_fleet = true;
                            }
                        }));
                    }
                    Err(err) => log(&name, "labels", &err),
                }
            }

            {
                let name = name.clone();
                send(Box::new(move |up| {
                    if !up.is_current(&name, epoch) {
                        return;
                    }
                    if let Some(host) = up.fleet.host_mut(&host_id(&name)) {
                        host.directory.agents_hydrated = agents_complete;
                        host.directory.journal.clear();
                    }
                    let held = match up.links.get_mut(&name) {
                        Some(link) => {
                            link.bootstrapping = false;
                            std::mem::take(&mut link.held)
                        }
                        None => Vec::new(),
                    };
                    for message in held {
                        up.on_push(&name, message);
                    }
                    up.dirty_fleet = true;
                }));
            }

            if failed {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if live() {
                    client.reconnect();
                }
            }
        });
    }

    // ---- conversations --------------------------------------------------

    /// A phone opened this conversation: stream it.
    pub fn open(&mut self, host: &str, agent: &str) {
        let key = (host.to_string(), agent.to_string());
        if let Some(session) = self.sessions.get_mut(&key) {
            session.viewers += 1;
            session.idle_since = None;
            return;
        }
        let mut session = Session {
            loading: true,
            viewers: 1,
            ..Default::default()
        };
        if let Some(agent) = self.fleet.agent(&host_id(host), agent) {
            session.timeline.sync_snapshot(&agent.raw);
        }
        self.sessions.insert(key.clone(), session);
        self.dirty_sessions.insert(key);
        if let Some(link) = self.links.get_mut(host) {
            link.watched.insert(agent.to_string());
        }
        self.resubscribe_timelines(host, false);
        self.fetch_tail(host, agent);
    }

    /// A phone closed it. It keeps streaming a while in case it reopens.
    pub fn close(&mut self, host: &str, agent: &str) {
        let key = (host.to_string(), agent.to_string());
        if let Some(session) = self.sessions.get_mut(&key) {
            session.viewers = session.viewers.saturating_sub(1);
            if session.viewers == 0 {
                session.idle_since = Some(Instant::now());
            }
        }
    }

    /// Stops streaming conversations nobody has looked at for `grace`.
    pub fn sweep(&mut self, grace: Duration) {
        let now = Instant::now();
        let stale: Vec<(String, String)> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.idle_since.is_some_and(|since| now - since >= grace))
            .map(|(key, _)| key.clone())
            .collect();
        let mut hosts = BTreeSet::new();
        for (host, agent) in stale {
            self.sessions.remove(&(host.clone(), agent.clone()));
            if let Some(link) = self.links.get_mut(&host) {
                link.watched.remove(&agent);
            }
            hosts.insert(host);
        }
        for host in hosts {
            self.resubscribe_timelines(&host, false);
        }
    }

    /// Loads the page before what the conversation holds, if there is one.
    /// False when there is nothing older or a fetch is already running.
    pub fn fetch_older(&mut self, host: &str, agent: &str) -> bool {
        let Some(session) = self.sessions.get(&(host.to_string(), agent.to_string())) else {
            return false;
        };
        let (Some(cursor), true) = (session.timeline.cursor.clone(), session.timeline.has_older)
        else {
            return false;
        };
        if session.fetching {
            return false;
        }
        let message = json!({
            "type": "fetch_agent_timeline_request", "agentId": agent, "direction": "before",
            "cursor": { "epoch": cursor.epoch, "seq": cursor.start_seq }, "limit": PAGE,
            "projection": "projected",
        });
        self.fetch_page(host, agent, false, message);
        true
    }

    fn resubscribe_timelines(&mut self, host: &str, catch_up: bool) {
        let Some(link) = self.links.get(host) else {
            return;
        };
        if !link.online {
            return;
        }
        let epoch = link.epoch.load(Ordering::SeqCst);
        let agents: Vec<String> = link.watched.iter().cloned().collect();
        let reply = guarded(
            &link.client,
            json!({ "type": "agent.timeline.set_subscription.request", "agentIds": agents }),
        );
        let apply = self.apply.clone();
        let host = host.to_string();
        tokio::spawn(async move {
            let Ok(payload) = reply.await else {
                return;
            };
            let id = payload["subscriptionId"].as_str().map(str::to_string);
            let _ = apply.send(Box::new(move |up: &mut Upstream| {
                if !up.is_current(&host, epoch) {
                    return;
                }
                // Each request makes a new subscription on the daemon;
                // release the one it replaces so frames are not doubled.
                let Some(link) = up.links.get_mut(&host) else {
                    return;
                };
                if let Some(previous) = std::mem::replace(&mut link.timeline_subscription, id) {
                    tokio::spawn(guarded(&link.client, requests::release(&previous)));
                }
                if catch_up {
                    let agents: Vec<String> = link.watched.iter().cloned().collect();
                    for agent in agents {
                        up.catch_up(&host, &agent);
                    }
                }
            }));
        });
    }

    fn catch_up(&mut self, host: &str, agent: &str) {
        let Some(session) = self.sessions.get(&(host.to_string(), agent.to_string())) else {
            return;
        };
        match session.timeline.cursor.clone() {
            Some(cursor) if session.timeline.baseline => {
                self.fetch_after(host, agent, &cursor.epoch, cursor.end_seq)
            }
            _ => self.fetch_tail(host, agent),
        }
    }

    fn follow(&mut self, host: &str, agent: &str, follow: Follow) {
        match follow {
            Follow::Nothing => {}
            Follow::Reload => self.fetch_tail(host, agent),
            Follow::CatchUp { epoch, seq } => self.fetch_after(host, agent, &epoch, seq),
        }
    }

    fn fetch_tail(&mut self, host: &str, agent: &str) {
        let message = json!({
            "type": "fetch_agent_timeline_request", "agentId": agent, "direction": "tail",
            "limit": PAGE, "projection": "projected",
        });
        self.fetch_page(host, agent, true, message);
    }

    fn fetch_after(&mut self, host: &str, agent: &str, epoch: &str, seq: u64) {
        let message = json!({
            "type": "fetch_agent_timeline_request", "agentId": agent, "direction": "after",
            "cursor": { "epoch": epoch, "seq": seq }, "limit": PAGE, "projection": "projected",
        });
        self.fetch_page(host, agent, false, message);
    }

    /// One timeline fetch per conversation at a time. A reload asked for
    /// meanwhile runs when the current fetch lands; a catch-up asked for
    /// meanwhile is redundant (the held frames drain with that page).
    fn fetch_page(&mut self, host: &str, agent: &str, reload: bool, message: Value) {
        let key = (host.to_string(), agent.to_string());
        let Some(session) = self.sessions.get_mut(&key) else {
            return;
        };
        if session.fetching {
            session.reload_pending |= reload;
            return;
        }
        let Some(link) = self.links.get(host).filter(|link| link.online) else {
            session.loading = false;
            session.error = Some("The host is offline".into());
            self.dirty_sessions.insert(key);
            return;
        };
        session.fetching = true;
        let reply = guarded(&link.client, message);
        let apply = self.apply.clone();
        tokio::spawn(async move {
            let result = reply
                .await
                .and_then(|payload| match payload["error"].as_str() {
                    Some(error) => Err(error.to_string()),
                    None => Ok(payload),
                });
            let _ = apply.send(Box::new(move |up: &mut Upstream| up.on_page(key, result)));
        });
    }

    fn on_page(&mut self, key: (String, String), result: Result<Value, String>) {
        let Some(session) = self.sessions.get_mut(&key) else {
            return;
        };
        session.loading = false;
        session.fetching = false;
        session.pages += 1;
        let reload = std::mem::take(&mut session.reload_pending);
        let follow = match &result {
            Ok(payload) => {
                session.error = None;
                if payload["agent"].is_object() {
                    session.timeline.sync_snapshot(&payload["agent"]);
                }
                session.timeline.apply_page(payload)
            }
            Err(error) => {
                session.error = Some(error.clone());
                Follow::Nothing
            }
        };
        self.dirty_sessions.insert(key.clone());
        let (host, agent) = key;
        if reload {
            self.fetch_tail(&host, &agent);
        } else {
            self.follow(&host, &agent, follow);
        }
    }
}

fn next_page(payload: &Value) -> Option<String> {
    let more =
        payload["pageInfo"]["hasMore"].as_bool().unwrap_or(false) && payload["sync"].is_null();
    payload["pageInfo"]["nextCursor"]
        .as_str()
        .map(str::to_string)
        .filter(|_| more)
}

fn log(host: &str, what: &str, err: &str) {
    eprintln!("reef: {host}: loading {what} failed: {err}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_removed_hosts_work_cannot_touch_its_replacement() {
        let (events, _events) = mpsc::channel(8);
        let (apply, _apply) = mpsc::unbounded_channel();
        let mut up = Upstream::new(Vec::new(), "cid_test".into(), events, apply);
        let host = || HostConfig {
            id: host_id("example-a"),
            name: "example-a".into(),
            endpoint: Some("127.0.0.1:9".into()),
            password: None,
        };
        // Connected once: bootstrap work holds this epoch and its handle.
        assert!(up.add_host(host()));
        let old = up.bump_epoch("example-a").unwrap();
        up.links.get_mut("example-a").unwrap().online = true;
        let held = up.links["example-a"].epoch.clone();
        assert!(up.is_current("example-a", old));
        // Removed: that work sees it is stale at once.
        assert!(up.remove_host("example-a"));
        assert_ne!(held.load(Ordering::SeqCst), old);
        // Added again and connected: the old epoch still does not match.
        assert!(up.add_host(host()));
        let new = up.bump_epoch("example-a").unwrap();
        up.links.get_mut("example-a").unwrap().online = true;
        assert!(!up.is_current("example-a", old));
        assert!(up.is_current("example-a", new));
    }

    #[tokio::test]
    async fn an_open_conversation_hears_every_kind_of_turn_end() {
        let (events, _events) = mpsc::channel(8);
        let (apply, _apply) = mpsc::unbounded_channel();
        let mut up = Upstream::new(Vec::new(), "cid_test".into(), events, apply);
        for (agent, event, failed) in [
            ("done", "turn_completed", false),
            ("broke", "turn_failed", true),
            ("stopped", "turn_canceled", false),
        ] {
            let key = ("example-a".to_string(), agent.to_string());
            up.sessions.insert(key, Session::default());
            up.on_push(
                "example-a",
                json!({"type": "agent_stream", "payload": {"agentId": agent,
                    "event": {"type": event, "provider": "claude", "turnId": "t1"}}}),
            );
            assert_eq!(
                up.finished.pop(),
                Some(("example-a".into(), agent.into(), failed)),
                "{event}"
            );
        }
    }

    #[test]
    fn the_one_usage_read_is_on_the_list() {
        // Reports, not the older provider usage or anything that sets up
        // or changes a usage source.
        let usage: Vec<&&str> = (READ_REQUESTS.iter())
            .filter(|kind| kind.contains("usage"))
            .collect();
        assert_eq!(usage, [&"usage.list_reports.request"]);
        let client = Client::connect(Config::new("127.0.0.1:9", "cid_test"));
        let older = json!({"type": "provider.usage.list.request"});
        let result = paseo_client::block_on(guarded_until(&client, older, "x"));
        assert!(result.unwrap_err().contains("not on Reef's list"));
    }

    #[test]
    fn the_only_agent_update_is_its_labels_key() {
        let id = fleet::labels::LabelId::new("lbl_a");
        assert!(labels_write(&fleet::labels::set_request("a", &[id])));
        for refused in [
            json!({"type": "update_agent_request", "agentId": "a", "name": "x",
                   "labels": {fleet::labels::KEY: "v1:"}}),
            json!({"type": "update_agent_request", "agentId": "a",
                   "labels": {fleet::labels::KEY: "v1:", "paseo.parent-agent-id": "b"}}),
            json!({"type": "update_agent_request", "agentId": "a", "labels": {"other": "x"}}),
            json!({"type": "update_agent_request", "agentId": "a", "labels": {fleet::labels::KEY: 1}}),
            json!({"type": "send_agent_message_request", "agentId": "a",
                   "labels": {fleet::labels::KEY: "v1:"}}),
        ] {
            assert!(!labels_write(&refused), "{refused}");
        }
    }

    #[test]
    fn writes_off_the_list_are_refused_before_they_leave() {
        // A client that never connects: a refused request must fail at
        // once, without being queued for the socket.
        let client = Client::connect(Config::new("127.0.0.1:9", "cid_test"));
        for write in [
            json!({"type": "clear_agent_attention", "agentId": "a"}),
            json!({"type": "update_agent_request", "agentId": "a", "name": "x"}),
            json!({"type": "create_terminal_request", "cwd": "/"}),
            json!({"type": "create_agent_request", "config": {}}),
            json!({"type": "workspace.label.delete.request", "name": "x"}),
            json!({"type": "archive_workspace_request", "workspaceId": "w"}),
        ] {
            let result = paseo_client::block_on(guarded(&client, write));
            assert!(result.unwrap_err().contains("not on Reef's list"));
        }
    }
}
