//! The live fleet: one WebSocket per saved host, the merged directory, and
//! the agent conversations the user has open.
//!
//! Views observe [`FleetStore`] for directory changes and an [`AgentSession`]
//! for one conversation. Nothing else talks to a daemon.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use fleet::prompt::Prompt;
use fleet::prompt_index::{JumpState, PageOutcome, PromptMark};
use fleet::timeline::{Follow, Timeline};
use fleet::{Connection, Fleet, HostConfig, requests};
use gpui_kit::gpui::{App, AppContext, Context, Entity, EventEmitter, Task};
use paseo_client::{Client, Config, ConnState, Event};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelChange {
    pub name: String,
    pub color: String,
    pub assigned: bool,
}

pub struct LabelBatch {
    pub applied: Vec<LabelChange>,
    pub error: Option<String>,
}

fn reply_error(payload: &Value) -> Option<String> {
    payload["error"]
        .as_str()
        .map(str::to_string)
        .or_else(|| (payload["accepted"] == false).then(|| "The host declined".to_string()))
}

async fn send_request(
    client: &Client,
    message: Value,
    timeout: std::time::Duration,
) -> Result<Value, String> {
    client
        .request_with_timeout(message, timeout)
        .await
        .map_err(|error| error.to_string())
}

pub enum FleetEvent {
    /// Some host's directory or connection changed.
    Changed,
    /// A request failed in a way the user should see.
    Toast(String),
    /// An agent finished, failed, or needs permission.
    Attention {
        host: String,
        agent_id: String,
        workspace_id: Option<String>,
        reason: String,
        title: String,
        body: String,
    },
}

/// Daemon prompt metadata and history loading belong to the session driver.
#[derive(Default)]
pub struct SessionPromptIndex {
    marks: Vec<PromptMark>,
    key: Option<(String, Option<u64>)>,
    generation: u64,
    jump: JumpState,
    revision: u64,
}
impl SessionPromptIndex {
    pub fn marks(&self) -> &[PromptMark] {
        &self.marks
    }
    pub fn jump(&self) -> &JumpState {
        &self.jump
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn loading(&self) -> bool {
        matches!(self.jump, JumpState::Loading { .. })
    }
    pub fn error(&self) -> Option<&str> {
        match &self.jump {
            JumpState::Failed(error) => Some(error),
            _ => None,
        }
    }
}

/// One open conversation.
pub struct AgentSession {
    pub host: String,
    pub agent_id: String,
    pub timeline: Timeline,
    pub loading: bool,
    pub error: Option<String>,
    /// Messages typed while a turn ran, sent one per idle edge.
    pub queue: Vec<Prompt>,
    pub failed_prompts: BTreeMap<String, Prompt>,
    inflight_sends: BTreeSet<String>,
    pub prompt_index: SessionPromptIndex,
    /// A timeline fetch is in flight.
    fetching: bool,
    /// A reload was asked for while fetching; do it when that lands.
    reload_pending: bool,
}

/// One open PTY. The emulator is local; bytes come from the daemon's slot.
pub struct TerminalSession {
    pub emulator: term::Terminal,
    pub attaching: bool,
    pub offline: bool,
    pub error: Option<String>,
    pub title: Option<String>,
    rows: u32,
    cols: u32,
    claimed: bool,
    attached_size: Option<(u32, u32)>,
    subscription_id: Option<String>,
    attach_gen: u64,
}

impl TerminalSession {
    fn new(_host: &str, _id: &str) -> Self {
        Self {
            emulator: term::Terminal::new(24, 80),
            attaching: false,
            offline: false,
            error: None,
            title: None,
            rows: 0,
            cols: 0,
            claimed: false,
            attached_size: None,
            subscription_id: None,
            attach_gen: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TerminalRecord {
    id: String,
    name: String,
    title: Option<String>,
}

struct TerminalList {
    cwd: String,
    loaded: bool,
    terminals: Vec<TerminalRecord>,
    subscription: Option<String>,
    epoch: u64,
    request: u64,
    pending: bool,
    revision: u64,
}

#[derive(Clone)]
pub struct TerminalWatch {
    pub host: String,
    pub workspace: String,
    pub cwd: String,
}

#[derive(Clone, Copy)]
enum PageKind {
    Tail,
    After,
    Before,
}

impl AgentSession {
    pub fn display_error(&self) -> Option<String> {
        self.prompt_index
            .error()
            .map(str::to_string)
            .or_else(|| self.error.clone())
    }
    pub fn unsent_count(&self) -> usize {
        self.queue.len() + self.failed_prompts.len() + self.inflight_sends.len()
    }
    fn new(host: &str, agent_id: &str) -> Self {
        Self {
            host: host.to_string(),
            agent_id: agent_id.to_string(),
            timeline: Timeline::default(),
            loading: true,
            error: None,
            queue: Vec::new(),
            failed_prompts: BTreeMap::new(),
            inflight_sends: BTreeSet::new(),
            prompt_index: SessionPromptIndex::default(),
            fetching: false,
            reload_pending: false,
        }
    }
}

struct HostLink {
    client: Client,
    /// Which `connect()` made this link. Events from a replaced client are
    /// dropped by comparing against it.
    generation: u64,
    /// Bumped on every (re)connect; bootstrap work from an older session
    /// is discarded.
    epoch: u64,
    /// Agents whose timelines we want streamed on this host.
    watched: BTreeSet<String>,
    /// Terminals whose PTY we want attached. Slots are per connection.
    open_terminals: BTreeSet<String>,
    terminal_slots: HashMap<u8, String>,
    timeline_subscription: Option<String>,
    online: bool,
    /// Directory pushes that arrived while bootstrap replies were still in
    /// flight; replayed after them so a snapshot never erases newer pushes.
    bootstrapping: bool,
    held: Vec<Value>,
}

/// Writes can take a while on a busy daemon (archive waits for the provider
/// to shut down); the reference client allows a minute.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub struct FleetStore {
    pub fleet: Fleet,
    links: HashMap<String, HostLink>,
    sessions: HashMap<(String, String), Entity<AgentSession>>,
    /// Provider subagents of the agents with open conversations, by host and
    /// parent agent, oldest first.
    provider_subagents: HashMap<(String, String), Vec<fleet::subagents::Subagent>>,
    /// Provider subagents taken off their parent's track, as host, parent
    /// and id. Kept for this run only, as Paseo does.
    hidden_provider_subagents: std::collections::HashSet<(String, String, String)>,
    /// Open provider subagent conversations, by host, parent and subagent.
    subagent_sessions: HashMap<(String, String, String), Entity<AgentSession>>,
    subagent_viewers: HashMap<(String, String, String), usize>,
    /// Bumped whenever `provider_subagents` changes.
    pub subagents_revision: u64,
    /// Subagent tabs this client has told their host it has open.
    open_tab_marks: BTreeSet<(String, String)>,
    subagent_tabs: BTreeSet<(String, String)>,
    tab_mark_pending: BTreeSet<(String, String, bool)>,
    tab_mark_retry_at: HashMap<(String, String, bool), i64>,
    /// Changes only when a streamed conversation is added or removed.
    /// Store observers must not assume every notification changes the directory.
    sessions_revision: u64,
    terminals: HashMap<(String, String), Entity<TerminalSession>>,
    terminal_lists: HashMap<(String, String), TerminalList>,
    // Used only by tab reconciliation, never by lane or card cache keys.
    terminal_nav_revision: u64,
    terminal_watch_sets: HashMap<&'static str, Vec<TerminalWatch>>,
    terminal_watch_request: u64,
    /// Tabs the user closed; kept out of reconciliation until the daemon drops them.
    closing_terminals: HashMap<String, BTreeSet<String>>,
    client_id: String,
    /// Providers each host offers, loaded on first need.
    pub providers: HashMap<String, Vec<fleet::providers::Provider>>,
    providers_loading: BTreeSet<String>,
    next_generation: u64,
    /// Bumped on each directory change; cheap change detection for views.
    pub revision: u64,
    /// Requests awaiting acknowledgement/directory confirmation, keyed by attempt.
    workspace_archives: fleet::workspace_archive::PendingArchives,
    /// What changed in which host's directory, oldest first, for views that
    /// patch what they derived. Entry `i` has sequence `change_base + i`.
    change_log: std::collections::VecDeque<FleetChange>,
    change_base: u64,
}

/// One directory change on one host. A change the directories did not
/// journal (a host list or connection change) is logged as `Bulk` with an
/// empty host.
#[derive(Debug, Clone)]
pub struct FleetChange {
    pub host: String,
    pub change: fleet::DirChange,
}

/// The log keeps this many changes; a reader further behind rebuilds.
const CHANGE_LOG_LIMIT: usize = 8192;

impl EventEmitter<FleetEvent> for FleetStore {}

impl FleetStore {
    pub fn new(hosts: Vec<HostConfig>, cx: &mut Context<Self>) -> Self {
        let mut store = Self {
            fleet: Fleet::new(hosts),
            links: HashMap::new(),
            sessions: HashMap::new(),
            provider_subagents: HashMap::new(),
            hidden_provider_subagents: Default::default(),
            subagent_sessions: HashMap::new(),
            subagent_viewers: HashMap::new(),
            subagents_revision: 0,
            open_tab_marks: BTreeSet::new(),
            subagent_tabs: BTreeSet::new(),
            tab_mark_pending: BTreeSet::new(),
            tab_mark_retry_at: HashMap::new(),
            sessions_revision: 0,
            terminals: HashMap::new(),
            terminal_lists: HashMap::new(),
            terminal_nav_revision: 0,
            terminal_watch_sets: HashMap::new(),
            terminal_watch_request: 0,
            closing_terminals: HashMap::new(),
            client_id: crate::app::config::client_id(),
            providers: HashMap::new(),
            providers_loading: BTreeSet::new(),
            next_generation: 0,
            revision: 0,
            workspace_archives: Default::default(),
            change_log: Default::default(),
            change_base: 0,
        };
        if let Some(stress) = crate::app::stress::from_env() {
            store.fleet = stress.fleet(now_millis());
            store.start_stress(stress, cx);
            return store;
        }
        let names: Vec<String> = store
            .fleet
            .hosts
            .iter()
            .map(|h| h.config.name.clone())
            .collect();
        for name in names {
            store.connect(&name, cx);
        }
        store
    }

    pub fn host_names(&self) -> Vec<String> {
        self.fleet
            .hosts
            .iter()
            .map(|h| h.config.name.clone())
            .collect()
    }

    fn connect(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(host) = self.fleet.host_mut(name) else {
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
        config.password = host.config.password.clone();
        let client = Client::connect(config);
        let events = client.events();
        self.next_generation += 1;
        let generation = self.next_generation;
        // Conversations already open keep streaming on the new link.
        let watched = self
            .sessions
            .keys()
            .filter(|(h, _)| h == name)
            .map(|(_, agent)| agent.clone())
            .collect();
        let open_terminals = self
            .terminals
            .keys()
            .filter(|(h, _)| h == name)
            .map(|(_, id)| id.clone())
            .collect();
        self.links.insert(
            name.to_string(),
            HostLink {
                client,
                generation,
                epoch: 0,
                watched,
                open_terminals,
                terminal_slots: HashMap::new(),
                timeline_subscription: None,
                online: false,
                bootstrapping: false,
                held: Vec::new(),
            },
        );
        let name = name.to_string();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let alive = this
                    .update(cx, |store, cx| {
                        if store
                            .links
                            .get(&name)
                            .is_some_and(|l| l.generation == generation)
                        {
                            store.on_event(&name, event, cx);
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Replaces the saved host list, reconnecting what changed.
    pub fn set_hosts(&mut self, hosts: Vec<HostConfig>, cx: &mut Context<Self>) {
        crate::app::config::save_hosts(&hosts);
        let old: HashMap<String, HostConfig> = self
            .fleet
            .hosts
            .iter()
            .map(|h| (h.config.name.clone(), h.config.clone()))
            .collect();
        let mut next = Fleet::new(hosts.clone());
        for host in &mut next.hosts {
            if let Some(previous) = self.fleet.host(&host.config.name)
                && previous.config == host.config
            {
                *host = previous.clone();
            }
        }
        self.fleet = next;
        for config in &hosts {
            if old.get(&config.name) != Some(config) {
                self.links.remove(&config.name);
                self.connect(&config.name, cx);
            }
        }
        self.links
            .retain(|name, _| hosts.iter().any(|h| &h.name == name));
        self.changed(cx);
    }

    pub fn reconnect(&mut self, host: &str) {
        if let Some(link) = self.links.get(host) {
            link.client.reconnect();
        }
    }

    /// Feeds synthetic pushes through `on_push`, the same path daemon pushes
    /// take, every 10ms for as long as the store lives.
    fn start_stress(&mut self, stress: crate::app::stress::Stress, cx: &mut Context<Self>) {
        let mut churn = stress.churn();
        // A shell in every fifth workspace, so Lanes shows terminal rows.
        // Stress hosts never connect, so nothing replaces these lists.
        let shells: Vec<(String, String)> = (self.fleet.hosts.iter())
            .flat_map(|host| {
                (host.directory.workspaces.keys())
                    .step_by(5)
                    .map(|workspace| (host.name().to_string(), workspace.clone()))
            })
            .collect();
        for (n, key) in shells.into_iter().enumerate() {
            self.terminal_lists.insert(
                key,
                TerminalList {
                    cwd: String::new(),
                    loaded: true,
                    terminals: vec![TerminalRecord {
                        id: format!("stress-shell-{n}"),
                        name: "zsh".into(),
                        title: None,
                    }],
                    subscription: None,
                    epoch: 0,
                    request: 0,
                    pending: false,
                    revision: 0,
                },
            );
        }
        cx.spawn(async move |this, cx| {
            let mut last = std::time::Instant::now();
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(10))
                    .await;
                let now = std::time::Instant::now();
                let pushes = churn.tick(now - last, now_millis());
                last = now;
                let alive = this
                    .update(cx, |store, cx| {
                        for (host, message) in pushes {
                            store.on_push(&host, message, cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.publish(true, cx);
    }

    /// Moves the directories' journals into the change log and tells
    /// observers. A change made outside the reducers journals nothing, so
    /// `bulk_if_quiet` logs it as [`fleet::DirChange::Bulk`].
    fn publish(&mut self, bulk_if_quiet: bool, cx: &mut Context<Self>) {
        // Acknowledgement may precede the directory push. Keep successful
        // archives disabled until an authoritative update reflects the archive.
        self.workspace_archives.retain_unsettled(&self.fleet);
        self.revision += 1;
        let before = self.change_log.len();
        for host in &mut self.fleet.hosts {
            for change in host.directory.journal.drain(..) {
                self.change_log.push_back(FleetChange {
                    host: host.config.name.clone(),
                    change,
                });
            }
        }
        if bulk_if_quiet && self.change_log.len() == before {
            // Nothing a directory journaled: be safe and say "everything".
            self.change_log.push_back(FleetChange {
                host: String::new(),
                change: fleet::DirChange::Bulk,
            });
        }
        if self.change_log.len() > CHANGE_LOG_LIMIT {
            let dropped = self.change_log.len() - CHANGE_LOG_LIMIT / 2;
            self.change_log.drain(..dropped);
            self.change_base += dropped as u64;
        }
        cx.emit(FleetEvent::Changed);
        cx.notify();
    }

    /// Where the change log ends now; hand it to [`Self::changes_since`]
    /// later to get what happened in between.
    pub fn change_cursor(&self) -> u64 {
        self.change_base + self.change_log.len() as u64
    }

    /// Changes after `cursor`, oldest first, or `None` if some of them have
    /// already been dropped from the log, in which case rebuild from `fleet`.
    pub fn changes_since(&self, cursor: u64) -> Option<impl Iterator<Item = &FleetChange>> {
        let skip = cursor.checked_sub(self.change_base)?;
        Some(self.change_log.iter().skip(skip as usize))
    }

    // ---- events from a daemon -----------------------------------------

    fn on_event(&mut self, name: &str, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::State(ConnState::Connecting { .. }) => {
                if let Some(host) = self.fleet.host_mut(name)
                    && (!matches!(host.connection, Connection::Offline(_)) || host.server.is_none())
                {
                    host.connection = Connection::Connecting;
                }
                self.changed(cx);
            }
            Event::State(ConnState::Connected(info)) => {
                if let Some(host) = self.fleet.host_mut(name) {
                    host.connection = Connection::Online;
                    host.server = Some(info);
                }
                let epoch = match self.links.get_mut(name) {
                    Some(link) => {
                        link.online = true;
                        link.timeline_subscription = None;
                        link.epoch += 1;
                        link.bootstrapping = true;
                        link.held.clear();
                        link.epoch
                    }
                    None => return,
                };
                self.bootstrap(name, epoch, cx);
                self.resubscribe_timelines(name, true, cx);
                self.reload_subagents(name, cx);
                self.reattach_terminals(name, cx);
                self.rewatch_terminal_lists(name, cx);
                self.changed(cx);
            }
            Event::State(ConnState::Disconnected { reason, auth }) => {
                if let Some(host) = self.fleet.host_mut(name) {
                    host.connection = if auth {
                        Connection::Refused(reason)
                    } else {
                        Connection::Offline(reason)
                    };
                    host.directory.disconnected();
                }
                if let Some(link) = self.links.get_mut(name) {
                    link.online = false;
                    link.timeline_subscription = None;
                    link.terminal_slots.clear();
                }
                self.mark_terminal_lists_stale(name);
                let sessions: Vec<Entity<TerminalSession>> = self
                    .terminals
                    .iter()
                    .filter(|((h, _), _)| h == name)
                    .map(|(_, session)| session.clone())
                    .collect();
                for session in sessions {
                    session.update(cx, |session, cx| {
                        session.offline = true;
                        session.attaching = false;
                        session.subscription_id = None;
                        session.claimed = false;
                        session.attached_size = None;
                        cx.notify();
                    });
                }
                self.changed(cx);
            }
            Event::Message(message) => self.on_push(name, message, cx),
            Event::Binary(bytes) => self.on_terminal_frame(name, &bytes, cx),
        }
    }

    fn on_push(&mut self, name: &str, message: Value, cx: &mut Context<Self>) {
        let kind = message["type"].as_str().unwrap_or_default().to_string();
        let payload = &message["payload"];
        match kind.as_str() {
            "agent_stream" => {
                let agent_id = payload["agentId"].as_str().unwrap_or_default();
                if let Some(session) = self
                    .sessions
                    .get(&(name.to_string(), agent_id.to_string()))
                    .cloned()
                {
                    let follow = session.update(cx, |session, cx| {
                        let follow = session.timeline.apply_live(payload);
                        cx.notify();
                        follow
                    });
                    self.follow(name, agent_id, follow, cx);
                    if payload["event"]["item"]["type"] == "user_message" {
                        self.ensure_prompt_index(name, agent_id, cx);
                    }
                }
            }
            "agent_permission_request" | "agent_permission_resolved" => {
                let agent_id = payload["agentId"].as_str().unwrap_or_default();
                if let Some(session) = self
                    .sessions
                    .get(&(name.to_string(), agent_id.to_string()))
                    .cloned()
                {
                    session.update(cx, |session, cx| {
                        session.timeline.apply_permission_event(&kind, payload);
                        cx.notify();
                    });
                }
            }
            "agent_attention_required" => {
                let agent_id = payload["agentId"].as_str().unwrap_or_default().to_string();
                let notification = &payload["notification"];
                cx.emit(FleetEvent::Attention {
                    host: name.to_string(),
                    workspace_id: notification["data"]["workspaceId"]
                        .as_str()
                        .map(str::to_string),
                    reason: payload["reason"].as_str().unwrap_or_default().to_string(),
                    title: notification["title"]
                        .as_str()
                        .unwrap_or("Agent update")
                        .to_string(),
                    body: notification["body"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    agent_id,
                });
            }
            "terminals_changed" => self.on_terminals_changed(name, payload, cx),
            "terminal_stream_exit" => self.on_terminal_exit(name, payload, cx),
            "agent.timeline.replacement" => {
                let agent_id = payload["agentId"].as_str().unwrap_or_default().to_string();
                self.follow(name, &agent_id, Follow::Reload, cx);
            }
            "agent.provider_subagents.update" => self.on_subagent_update(name, payload, cx),
            _ => {
                if let Some(link) = self.links.get_mut(name)
                    && link.bootstrapping
                {
                    link.held.push(message);
                    return;
                }
                let Some(host) = self.fleet.host_mut(name) else {
                    return;
                };
                let changed = host.directory.apply_push(&message);
                if kind == "agent_update" && payload["kind"] == "upsert" {
                    self.on_agent_snapshot(name, &payload["agent"], cx);
                }
                if changed.any() {
                    // The reducer journaled whatever it changed; if it
                    // journaled nothing (a stale or repeated push), there
                    // is nothing for derived views to redo.
                    self.publish(false, cx);
                }
            }
        }
    }

    /// An agent snapshot arrived: keep its open session's turn and
    /// permissions current, and drain the send queue on the idle edge.
    fn on_agent_snapshot(&mut self, host: &str, agent: &Value, cx: &mut Context<Self>) {
        let Some(agent_id) = agent["id"].as_str() else {
            return;
        };
        let Some(session) = self
            .sessions
            .get(&(host.to_string(), agent_id.to_string()))
            .cloned()
        else {
            return;
        };
        let next = session.update(cx, |session, cx| {
            let was_running = session.timeline.is_running();
            session.timeline.sync_snapshot(agent);
            cx.notify();
            if was_running && !session.timeline.is_running() && !session.queue.is_empty() {
                Some(session.queue.remove(0))
            } else {
                None
            }
        });
        if let Some(prompt) = next {
            // Removing the queued prompt frees a slot before sending it.
            let _ = self.send_prompt(host, agent_id, prompt, None, cx);
        }
    }

    // ---- bootstrap ----------------------------------------------------

    /// Whether work started for connection `epoch` of `name` still applies.
    fn is_current(&self, name: &str, epoch: u64) -> bool {
        self.links
            .get(name)
            .is_some_and(|link| link.epoch == epoch && link.online)
    }

    /// Subscribes to everything the sidebar needs, in order, then replays
    /// pushes that arrived meanwhile. Work from a superseded connection is
    /// dropped; a failed step reconnects after a pause so it is retried.
    fn bootstrap(&mut self, name: &str, epoch: u64, cx: &mut Context<Self>) {
        let Some(link) = self.links.get(name) else {
            return;
        };
        let Some(host) = self.fleet.host(name) else {
            return;
        };
        let client = link.client.clone();
        let sync = host.has("directorySync");
        let project_list = host.has("projectList");
        let labels = host.has("workspaceLabels");
        let provider_subagents = host.has("providerSubagents");
        let dir = &host.directory;
        let workspace_cursor = dir.workspace_cursor.clone();
        let agent_cursor = dir.agent_cursor.clone();
        let project_cursor = dir.project_cursor.clone();
        let name = name.to_string();

        cx.spawn(async move |this, cx| {
            // Applies a step's result only if this connection is still live.
            macro_rules! apply {
                ($body:expr) => {{
                    let applied = this
                        .update(cx, |store: &mut FleetStore, cx| {
                            if !store.is_current(&name, epoch) {
                                return false;
                            }
                            #[allow(clippy::redundant_closure_call)]
                            ($body)(store, cx);
                            true
                        })
                        .unwrap_or(false);
                    if !applied {
                        return;
                    }
                }};
            }
            let mut failed = false;

            let mut events = vec![
                "project.update",
                "script_status_update",
                "agent_permission_request",
                "agent_permission_resolved",
                "agent_attention_required",
            ];
            if provider_subagents {
                events.push("agent.provider_subagents.update");
            }
            match client.request(requests::session_events(&events)).await {
                Ok(payload) => {
                    let id = payload["subscriptionId"].as_str().map(str::to_string);
                    apply!(|store: &mut FleetStore, _cx: &mut Context<FleetStore>| {
                        if let Some(host) = store.fleet.host_mut(&name) {
                            host.directory.subscriptions.events = id;
                        }
                    });
                }
                Err(err) => {
                    log_error(&name, "session events", &err);
                    failed = true;
                }
            }

            if project_list {
                match client
                    .request(requests::list_projects(sync, project_cursor.as_ref()))
                    .await
                {
                    Ok(payload) => {
                        apply!(|store: &mut FleetStore, cx: &mut Context<FleetStore>| {
                            if let Some(host) = store.fleet.host_mut(&name) {
                                host.directory.apply_projects_reply(&payload);
                            }
                            store.changed(cx);
                        })
                    }
                    Err(err) => {
                        log_error(&name, "projects", &err);
                        failed = true;
                    }
                }
            } else {
                apply!(|store: &mut FleetStore, _cx: &mut Context<FleetStore>| {
                    if let Some(host) = store.fleet.host_mut(&name) {
                        host.directory.projects_from_workspaces = true;
                    }
                });
            }

            // Workspaces, then every following page when not syncing.
            let mut request = requests::fetch_workspaces(sync, workspace_cursor.as_ref());
            let mut first = true;
            loop {
                match client.request(request).await {
                    Ok(payload) => {
                        let next = payload["pageInfo"]["nextCursor"]
                            .as_str()
                            .map(str::to_string);
                        let more = payload["pageInfo"]["hasMore"].as_bool().unwrap_or(false)
                            && payload["sync"].is_null();
                        let page_first = first;
                        apply!(|store: &mut FleetStore, cx: &mut Context<FleetStore>| {
                            if let Some(host) = store.fleet.host_mut(&name) {
                                host.directory.apply_workspaces_reply(&payload, page_first);
                            }
                            store.changed(cx);
                        });
                        match next.filter(|_| more) {
                            Some(cursor) => {
                                request = requests::fetch_workspaces_page(&cursor);
                                first = false;
                            }
                            None => break,
                        }
                    }
                    Err(err) => {
                        log_error(&name, "workspaces", &err);
                        failed = true;
                        break;
                    }
                }
            }

            let mut request = requests::fetch_agents(sync, agent_cursor.as_ref());
            let mut first = true;
            let mut agents_complete = false;
            loop {
                match client.request(request).await {
                    Ok(payload) => {
                        let next = payload["pageInfo"]["nextCursor"]
                            .as_str()
                            .map(str::to_string);
                        let more = payload["pageInfo"]["hasMore"].as_bool().unwrap_or(false)
                            && payload["sync"].is_null();
                        let page_first = first;
                        apply!(|store: &mut FleetStore, cx: &mut Context<FleetStore>| {
                            if let Some(host) = store.fleet.host_mut(&name) {
                                host.directory.apply_agents_reply(&payload, page_first);
                            }
                            let agents: Vec<Value> = payload["entries"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|e| e["agent"].clone())
                                .collect();
                            for agent in agents {
                                store.on_agent_snapshot(&name, &agent, cx);
                            }
                            store.changed(cx);
                        });
                        match next.filter(|_| more) {
                            Some(cursor) => {
                                request = requests::fetch_agents_page(&cursor);
                                first = false;
                            }
                            None => {
                                agents_complete = true;
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        log_error(&name, "agents", &err);
                        failed = true;
                        break;
                    }
                }
            }

            if labels {
                match client.request(requests::list_labels()).await {
                    Ok(payload) => {
                        apply!(|store: &mut FleetStore, cx: &mut Context<FleetStore>| {
                            if let Some(host) = store.fleet.host_mut(&name) {
                                host.directory.apply_labels_reply(&payload);
                            }
                            store.changed(cx);
                        })
                    }
                    Err(err) => log_error(&name, "labels", &err),
                }
            }

            apply!(|store: &mut FleetStore, cx: &mut Context<FleetStore>| {
                if let Some(host) = store.fleet.host_mut(&name)
                    && agents_complete
                {
                    host.directory.agents_hydrated = true;
                }
                let held = match store.links.get_mut(&name) {
                    Some(link) => {
                        link.bootstrapping = false;
                        std::mem::take(&mut link.held)
                    }
                    None => Vec::new(),
                };
                for message in held {
                    store.on_push(&name, message, cx);
                }
                store.changed(cx);
            });

            if failed {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(5))
                    .await;
                let still = this
                    .update(cx, |store, _| store.is_current(&name, epoch))
                    .unwrap_or(false);
                if still {
                    client.reconnect();
                }
            }
        })
        .detach();
    }

    // ---- requests -------------------------------------------------------

    /// Sends a request to one host; the task resolves with the reply payload.
    pub fn request(&self, host: &str, message: Value, cx: &App) -> Task<Result<Value, String>> {
        self.request_with(host, message, paseo_client::DEFAULT_TIMEOUT, cx)
    }

    /// A request that changes something on the daemon, with a longer wait.
    pub fn write(&self, host: &str, message: Value, cx: &App) -> Task<Result<Value, String>> {
        self.request_with(host, message, WRITE_TIMEOUT, cx)
    }

    /// The protocol has one assignment per request. Preserve successful writes
    /// on partial failure so the view can reconcile against the actual labels.
    pub fn set_workspace_labels(
        &self,
        host: &str,
        workspace: &str,
        changes: Vec<LabelChange>,
        cx: &App,
    ) -> Task<LabelBatch> {
        let Some(link) = self.links.get(host) else {
            return Task::ready(LabelBatch {
                applied: vec![],
                error: Some(format!("{host} is not connected")),
            });
        };
        let client = link.client.clone();
        let workspace = workspace.to_string();
        cx.background_spawn(async move {
            let mut batch = LabelBatch {
                applied: vec![],
                error: None,
            };
            for change in changes {
                let message =
                    requests::set_label(&workspace, &change.name, &change.color, change.assigned);
                let result = send_request(&client, message, WRITE_TIMEOUT).await;
                let failure = match result {
                    Err(error) => Some(error),
                    Ok(payload) => reply_error(&payload),
                };
                if let Some(error) = failure {
                    batch.error = Some(error);
                    break;
                }
                batch.applied.push(change);
            }
            batch
        })
    }

    pub fn toast(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        cx.emit(FleetEvent::Toast(message.into()));
    }

    /// A request `host` answers in several messages, the last of type
    /// `until`: the earlier messages' payloads, and the last's.
    pub fn request_until(
        &self,
        host: &str,
        message: Value,
        until: &str,
        cx: &App,
    ) -> Task<Result<(Vec<Value>, Value), String>> {
        match self.links.get(host) {
            Some(link) => {
                let (client, until) = (link.client.clone(), until.to_string());
                cx.background_spawn(async move {
                    client
                        .request_until(message, &until, WRITE_TIMEOUT)
                        .await
                        .map_err(|error| error.to_string())
                })
            }
            None => Task::ready(Err(format!("{host} is not connected"))),
        }
    }

    fn request_with(
        &self,
        host: &str,
        message: Value,
        timeout: std::time::Duration,
        cx: &App,
    ) -> Task<Result<Value, String>> {
        match self.links.get(host) {
            Some(link) => {
                let client = link.client.clone();
                cx.background_spawn(async move { send_request(&client, message, timeout).await })
            }
            None => Task::ready(Err(format!("{host} is not connected"))),
        }
    }

    /// Keep duplicate archive requests out; directory pushes own view removal.
    pub fn workspace_archive_pending(&self, host: &str, workspace: &str) -> bool {
        self.workspace_archives.contains(host, workspace)
    }

    pub fn archive_workspace(&mut self, host: &str, workspace: &str, cx: &mut Context<Self>) {
        let key = (host.to_string(), workspace.to_string());
        let Some(attempt) = self.workspace_archives.begin(host, workspace) else {
            return;
        };
        let task = self.write(host, requests::archive_workspace(workspace), cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let failure = match result {
                Err(error) => Some(error),
                Ok(payload) => reply_error(&payload),
            };
            let succeeded = failure.is_none();
            let _ = this.update(cx, |store, cx| {
                if let Some(error) = failure {
                    store.workspace_archives.release(&key.0, &key.1, attempt);
                    store.toast(error, cx);
                }
                cx.notify();
            });
            if succeeded {
                // One deadline per successful request, never a polling loop.
                // A late deadline must not release a newer attempt's guard.
                cx.background_executor().timer(std::time::Duration::from_secs(30)).await;
                let _ = this.update(cx, |store, cx| {
                    if store.workspace_archives.release(&key.0, &key.1, attempt) {
                        store.toast("Archive was acknowledged, but its directory update has not arrived. Reconnect the host to refresh its state.", cx);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// Fire a request and toast its failure; replies arrive as pushes anyway.
    pub fn act(&self, host: &str, message: Value, cx: &mut Context<Self>) {
        let task = self.write(host, message, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let failure = match result {
                Err(err) => Some(err),
                Ok(payload) => reply_error(&payload),
            };
            if let Some(err) = failure {
                let _ = this.update(cx, |_, cx| cx.emit(FleetEvent::Toast(err)));
            }
        })
        .detach();
    }

    /// Fire-and-forget session message.
    pub fn send_raw(&self, host: &str, message: Value) {
        if let Some(link) = self.links.get(host) {
            link.client.send(message);
        }
    }

    /// Loads a host's providers snapshot once; views observe the store.
    pub fn ensure_providers(&mut self, host: &str, cx: &mut Context<Self>) {
        if self.providers.contains_key(host) || !self.providers_loading.insert(host.to_string()) {
            return;
        }
        let task = self.request(
            host,
            json!({ "type": "get_providers_snapshot_request" }),
            cx,
        );
        let host = host.to_string();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |store, cx| {
                store.providers_loading.remove(&host);
                if let Ok(payload) = result {
                    store
                        .providers
                        .insert(host.clone(), fleet::providers::parse(&payload));
                    store.changed(cx);
                }
            });
        })
        .detach();
    }

    // ---- conversations ----------------------------------------------------

    /// Opens (or returns) a conversation and starts streaming it.
    pub fn open_session(
        &mut self,
        host: &str,
        agent_id: &str,
        cx: &mut Context<Self>,
    ) -> Entity<AgentSession> {
        let key = (host.to_string(), agent_id.to_string());
        if let Some(session) = self.sessions.get(&key) {
            return session.clone();
        }
        let session = cx.new(|_| AgentSession::new(host, agent_id));
        if let Some(agent) = self.fleet.agent(host, agent_id) {
            let raw = agent.raw.clone();
            session.update(cx, |session, _| session.timeline.sync_snapshot(&raw));
        }
        self.sessions.insert(key, session.clone());
        self.sessions_revision += 1;
        // Membership affects previews, but is not a directory change: do
        // not bump the directory revision or publish a Bulk projection reset.
        cx.notify();
        if let Some(link) = self.links.get_mut(host) {
            link.watched.insert(agent_id.to_string());
        }
        self.resubscribe_timelines(host, false, cx);
        self.fetch_tail(host, agent_id, cx);
        self.list_subagents(host, agent_id, cx);
        session
    }

    // ---- provider subagents --------------------------------------------

    /// The provider subagents of `parent`, oldest first, once an open
    /// conversation of it has listed them.
    pub fn provider_subagents(&self, host: &str, parent: &str) -> &[fleet::subagents::Subagent] {
        self.provider_subagents
            .get(&(host.to_string(), parent.to_string()))
            .map_or(&[], Vec::as_slice)
    }

    /// Takes provider subagents off `parent`'s track. They are the
    /// provider's, not Paseo agents, so there is nothing to archive.
    pub fn hide_provider_subagents(&mut self, host: &str, parent: &str, ids: &[String]) {
        for id in ids {
            self.hidden_provider_subagents.insert((
                host.to_string(),
                parent.to_string(),
                id.clone(),
            ));
        }
    }

    pub fn provider_subagent_hidden(&self, host: &str, parent: &str, id: &str) -> bool {
        self.hidden_provider_subagents.contains(&(
            host.to_string(),
            parent.to_string(),
            id.to_string(),
        ))
    }

    /// Lists `parent`'s provider subagents; updates keep the list current.
    fn list_subagents(&mut self, host: &str, parent: &str, cx: &mut Context<Self>) {
        if !self
            .fleet
            .host(host)
            .is_some_and(|record| record.has("providerSubagents"))
        {
            return;
        }
        let task = self.request(host, requests::list_provider_subagents(parent), cx);
        let (host, parent) = (host.to_string(), parent.to_string());
        cx.spawn(async move |this, cx| {
            let Ok(payload) = task.await else {
                return;
            };
            if payload["error"].is_string() {
                return;
            }
            let _ = this.update(cx, |store, cx| {
                if store.session(&host, &parent).is_none() {
                    return;
                }
                let list = fleet::subagents::list(&payload);
                for subagent in &list {
                    store.sync_subagent_status(&host, &parent, subagent, cx);
                }
                store
                    .provider_subagents
                    .insert((host.clone(), parent.clone()), list);
                store.subagents_revision += 1;
                cx.notify();
            });
        })
        .detach();
    }

    /// After a reconnect: relist the open conversations' subagents and
    /// reload the open subagent conversations, whose epochs may be new.
    fn reload_subagents(&mut self, host: &str, cx: &mut Context<Self>) {
        let parents: Vec<String> = self
            .sessions
            .keys()
            .filter(|(h, _)| h == host)
            .map(|(_, agent)| agent.clone())
            .collect();
        for parent in parents {
            self.list_subagents(host, &parent, cx);
        }
        let open: Vec<(String, String)> = self
            .subagent_sessions
            .keys()
            .filter(|(h, ..)| h == host)
            .map(|(_, parent, sub)| (parent.clone(), sub.clone()))
            .collect();
        for (parent, sub) in open {
            self.fetch_subagent(host, &parent, &sub, "tail", None, cx);
        }
    }

    /// Brings an open subagent conversation's turn in line with its status.
    fn sync_subagent_status(
        &mut self,
        host: &str,
        parent: &str,
        subagent: &fleet::subagents::Subagent,
        cx: &mut Context<Self>,
    ) {
        let key = (host.to_string(), parent.to_string(), subagent.id.clone());
        if let Some(session) = self.subagent_sessions.get(&key) {
            session.update(cx, |session, cx| {
                if session.timeline.is_running() != subagent.running() {
                    let event =
                        fleet::subagents::status_event(subagent.status, &subagent.updated_at);
                    session.timeline.apply_live(&event);
                    cx.notify();
                }
            });
        }
    }

    fn on_subagent_update(&mut self, host: &str, payload: &Value, cx: &mut Context<Self>) {
        match payload["kind"].as_str() {
            Some("upsert") => {
                let value = &payload["subagent"];
                let Some(parent) = value["parentAgentId"].as_str() else {
                    return;
                };
                let key = (host.to_string(), parent.to_string());
                // Only conversations that are open keep a list.
                let Some(list) = self.provider_subagents.get_mut(&key) else {
                    return;
                };
                if fleet::subagents::upsert(list, value).is_none() {
                    return;
                }
                let id = value["id"].as_str().unwrap_or_default();
                if let Some(subagent) = list.iter().find(|sub| sub.id == id).cloned() {
                    self.sync_subagent_status(host, parent, &subagent, cx);
                }
                self.subagents_revision += 1;
                cx.notify();
            }
            Some("remove") => {
                let parent = payload["parentAgentId"].as_str().unwrap_or_default();
                let id = payload["subagentId"].as_str().unwrap_or_default();
                if let Some(list) = self
                    .provider_subagents
                    .get_mut(&(host.to_string(), parent.to_string()))
                {
                    list.retain(|sub| sub.id != id);
                    self.subagents_revision += 1;
                    cx.notify();
                }
            }
            Some("timeline") => {
                let parent = payload["parentAgentId"].as_str().unwrap_or_default();
                let id = payload["subagentId"].as_str().unwrap_or_default();
                let key = (host.to_string(), parent.to_string(), id.to_string());
                let Some(session) = self.subagent_sessions.get(&key).cloned() else {
                    return;
                };
                let follow = session.update(cx, |session, cx| {
                    let follow = session
                        .timeline
                        .apply_live(&fleet::subagents::live(payload));
                    cx.notify();
                    follow
                });
                match follow {
                    Follow::Nothing => {}
                    Follow::Reload => self.fetch_subagent(host, parent, id, "tail", None, cx),
                    Follow::CatchUp { epoch, seq } => {
                        self.fetch_subagent(host, parent, id, "after", Some((epoch, seq)), cx)
                    }
                }
            }
            _ => {}
        }
    }

    /// Opens a provider subagent's conversation, read-only, streaming.
    pub fn open_subagent(
        &mut self,
        host: &str,
        parent: &str,
        subagent: &str,
        cx: &mut Context<Self>,
    ) -> Entity<AgentSession> {
        let key = (host.to_string(), parent.to_string(), subagent.to_string());
        *self.subagent_viewers.entry(key.clone()).or_default() += 1;
        if let Some(session) = self.subagent_sessions.get(&key) {
            return session.clone();
        }
        let session = cx.new(|_| AgentSession::new(host, subagent));
        self.subagent_sessions.insert(key, session.clone());
        if let Some(found) = self
            .provider_subagents(host, parent)
            .iter()
            .find(|sub| sub.id == subagent)
            .cloned()
        {
            self.sync_subagent_status(host, parent, &found, cx);
        }
        self.fetch_subagent(host, parent, subagent, "tail", None, cx);
        session
    }

    /// Stops keeping a subagent conversation no view shows.
    pub fn close_subagent(&mut self, host: &str, parent: &str, subagent: &str) {
        let key = (host.to_string(), parent.to_string(), subagent.to_string());
        if release_viewer(&mut self.subagent_viewers, &key) {
            self.subagent_sessions.remove(&key);
            let parent_key = (host.to_string(), parent.to_string());
            if !self.sessions.contains_key(&parent_key)
                && !self
                    .subagent_viewers
                    .keys()
                    .any(|(h, p, _)| h == host && p == parent)
            {
                self.provider_subagents.remove(&parent_key);
            }
        }
    }

    /// Loads the page before what a subagent conversation shows.
    pub fn fetch_older_subagent(
        &mut self,
        host: &str,
        parent: &str,
        subagent: &str,
        cx: &mut Context<Self>,
    ) {
        let key = (host.to_string(), parent.to_string(), subagent.to_string());
        let Some(session) = self.subagent_sessions.get(&key) else {
            return;
        };
        let timeline = &session.read(cx).timeline;
        let (Some(cursor), true) = (timeline.cursor.clone(), timeline.has_older) else {
            return;
        };
        self.fetch_subagent(
            host,
            parent,
            subagent,
            "before",
            Some((cursor.epoch, cursor.start_seq)),
            cx,
        );
    }

    fn fetch_subagent(
        &mut self,
        host: &str,
        parent: &str,
        subagent: &str,
        direction: &str,
        cursor: Option<(String, u64)>,
        cx: &mut Context<Self>,
    ) {
        let key = (host.to_string(), parent.to_string(), subagent.to_string());
        let Some(session) = self.subagent_sessions.get(&key).cloned() else {
            return;
        };
        let tail = direction == "tail";
        let started = session.update(cx, |session, _| {
            if session.fetching {
                session.reload_pending |= tail;
                false
            } else {
                session.fetching = true;
                true
            }
        });
        if !started {
            return;
        }
        let limit = if direction == "after" { 0 } else { 40 };
        let message = requests::provider_subagent_timeline(
            parent,
            subagent,
            direction,
            cursor.as_ref().map(|(epoch, seq)| (epoch.as_str(), *seq)),
            limit,
        );
        let task = self.request(host, message, cx);
        let (host, parent, subagent) = (host.to_string(), parent.to_string(), subagent.to_string());
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |store, cx| {
                let key = (host.clone(), parent.clone(), subagent.clone());
                let Some(session) = store.subagent_sessions.get(&key).cloned() else {
                    return;
                };
                let (follow, reload) = session.update(cx, |session, cx| {
                    session.loading = false;
                    session.fetching = false;
                    let reload = std::mem::take(&mut session.reload_pending);
                    let follow = match &result {
                        Ok(payload) if payload["error"].is_string() => {
                            session.error = payload["error"].as_str().map(str::to_string);
                            Follow::Nothing
                        }
                        Ok(payload) => {
                            session.error = None;
                            session
                                .timeline
                                .apply_page(&fleet::subagents::page(payload))
                        }
                        Err(error) => {
                            session.error = Some(error.to_string());
                            Follow::Nothing
                        }
                    };
                    cx.notify();
                    (follow, reload)
                });
                match (reload, follow) {
                    (true, _) | (_, Follow::Reload) => {
                        store.fetch_subagent(&host, &parent, &subagent, "tail", None, cx)
                    }
                    (_, Follow::CatchUp { epoch, seq }) => store.fetch_subagent(
                        &host,
                        &parent,
                        &subagent,
                        "after",
                        Some((epoch, seq)),
                        cx,
                    ),
                    _ => {}
                }
            });
        })
        .detach();
    }

    /// Says to each host which subagents this client has open in tabs, as
    /// Paseo does, so the daemon keeps them when their parent is archived.
    pub fn mark_open_subagent_tabs(
        &mut self,
        tabs: &BTreeSet<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let label = format!("paseo.open-agent-tab.{}", self.client_id);
        let desired: BTreeSet<_> = tabs
            .iter()
            .filter(|(host, agent)| {
                self.fleet.agent(host, agent).is_some_and(|record| {
                    record.parent_agent_id.is_some() && record.archived_at.is_none()
                })
            })
            .cloned()
            .collect();
        let marked: BTreeSet<_> = desired
            .iter()
            .filter(|(host, agent)| {
                self.fleet.agent(host, agent).is_some_and(|record| {
                    record.labels.get(&label).and_then(Value::as_str) == Some("true")
                })
            })
            .cloned()
            .collect();
        let online: BTreeSet<_> = self
            .links
            .iter()
            .filter(|(_, link)| link.online)
            .map(|(host, _)| host.clone())
            .collect();
        self.subagent_tabs = desired.clone();
        prune_tab_marks(
            &mut self.open_tab_marks,
            &self.fleet,
            &self.links.keys().cloned().collect(),
        );
        self.tab_mark_retry_at.retain(|(host, agent, _), _| {
            self.open_tab_marks.contains(&(host.clone(), agent.clone()))
        });
        let actions = subagent_tab_actions(&mut self.open_tab_marks, &desired, &marked, &online);
        for (host, agent, open) in actions {
            let attempt = (host.clone(), agent.clone(), open);
            if self
                .tab_mark_retry_at
                .get(&attempt)
                .is_some_and(|at| now_millis() < *at)
                || !self.tab_mark_pending.insert(attempt.clone())
            {
                continue;
            }
            let task = self.request(
                &host,
                requests::set_open_agent_tab(&agent, &self.client_id, open),
                cx,
            );
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |store, cx| {
                    store.tab_mark_pending.remove(&attempt);
                    let delivered = result.as_ref().is_ok_and(|payload| {
                        let error = payload["error"].as_str().unwrap_or("").to_ascii_lowercase();
                        error.is_empty()
                            || (!open
                                && (error.contains("no such agent")
                                    || error.contains("agent not found")))
                    });
                    if delivered {
                        store.tab_mark_retry_at.remove(&attempt);
                    } else {
                        store
                            .tab_mark_retry_at
                            .insert(attempt.clone(), now_millis() + 30_000);
                    }
                    if delivered && !open {
                        let key = (host.clone(), agent.clone());
                        if store.subagent_tabs.contains(&key) {
                            // The tab reopened while its close was in flight.
                            store.act(
                                &host,
                                requests::set_open_agent_tab(&agent, &store.client_id, true),
                                cx,
                            );
                        } else {
                            store.open_tab_marks.remove(&key);
                        }
                    }
                });
            })
            .detach();
        }
    }

    pub fn sessions_revision(&self) -> u64 {
        self.sessions_revision
    }

    pub fn session(&self, host: &str, agent_id: &str) -> Option<Entity<AgentSession>> {
        self.sessions
            .get(&(host.to_string(), agent_id.to_string()))
            .cloned()
    }

    /// Stops streaming conversations no tab shows any more.
    pub fn retain_sessions(&mut self, keep: &BTreeSet<(String, String)>, cx: &mut Context<Self>) {
        let before = self.sessions.len();
        self.sessions.retain(|key, _| keep.contains(key));
        // A parent's subagents go with its conversation.
        // Provider sessions are owned by their live Conversation views, not
        // by the parent's navigation tab. Two views may share one session.
        self.provider_subagents.retain(|key, _| {
            keep.contains(key)
                || self
                    .subagent_viewers
                    .keys()
                    .any(|(host, parent, _)| key == &(host.clone(), parent.clone()))
        });
        if self.sessions.len() == before {
            return;
        }
        self.sessions_revision += 1;
        // Membership affects previews, but is not a directory change: do
        // not bump the directory revision or publish a Bulk projection reset.
        cx.notify();
        let hosts: Vec<String> = self.links.keys().cloned().collect();
        for host in hosts {
            let wanted: BTreeSet<String> = keep
                .iter()
                .filter(|(h, _)| *h == host)
                .map(|(_, a)| a.clone())
                .collect();
            if let Some(link) = self.links.get_mut(&host)
                && link.watched != wanted
            {
                link.watched = wanted;
                self.resubscribe_timelines(&host, false, cx);
            }
        }
    }

    fn resubscribe_timelines(&mut self, host: &str, catch_up: bool, cx: &mut Context<Self>) {
        let Some(link) = self.links.get(host) else {
            return;
        };
        if !link.online {
            return;
        }
        let epoch = link.epoch;
        let agents: Vec<String> = link.watched.iter().cloned().collect();
        let message =
            json!({ "type": "agent.timeline.set_subscription.request", "agentIds": agents });
        let task = self.request(host, message, cx);
        let host = host.to_string();
        cx.spawn(async move |this, cx| {
            if let Ok(payload) = task.await {
                let id = payload["subscriptionId"].as_str().map(str::to_string);
                let _ = this.update(cx, |store, cx| {
                    if !store.is_current(&host, epoch) {
                        return;
                    }
                    // Each request makes a new subscription on the daemon;
                    // release the one it replaces so frames are not doubled.
                    let previous = store
                        .links
                        .get_mut(&host)
                        .and_then(|link| std::mem::replace(&mut link.timeline_subscription, id));
                    if let Some(previous) = previous {
                        store
                            .request(&host, requests::release(&previous), cx)
                            .detach();
                    }
                    if catch_up {
                        let agents: Vec<String> = store
                            .links
                            .get(&host)
                            .map(|l| l.watched.iter().cloned().collect())
                            .unwrap_or_default();
                        for agent in agents {
                            store.catch_up(&host, &agent, cx);
                        }
                    }
                });
            }
        })
        .detach();
    }

    fn catch_up(&mut self, host: &str, agent_id: &str, cx: &mut Context<Self>) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        let cursor = session.read(cx).timeline.cursor.clone();
        match cursor {
            Some(cursor) if session.read(cx).timeline.baseline => {
                self.fetch_after(host, agent_id, &cursor.epoch, cursor.end_seq, cx)
            }
            _ => self.fetch_tail(host, agent_id, cx),
        }
    }

    fn follow(&mut self, host: &str, agent_id: &str, follow: Follow, cx: &mut Context<Self>) {
        match follow {
            Follow::Nothing => {}
            Follow::Reload => self.fetch_tail(host, agent_id, cx),
            Follow::CatchUp { epoch, seq } => self.fetch_after(host, agent_id, &epoch, seq, cx),
        }
    }

    /// One timeline fetch per conversation at a time. A reload asked for
    /// meanwhile runs when the current fetch lands; a catch-up asked for
    /// meanwhile is redundant (the held frames drain with that page).
    fn begin_fetch(
        &mut self,
        host: &str,
        agent_id: &str,
        reload: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.session(host, agent_id) else {
            return false;
        };
        session.update(cx, |session, _| {
            if session.fetching {
                if reload {
                    session.reload_pending = true;
                }
                false
            } else {
                session.fetching = true;
                true
            }
        })
    }

    fn fetch_tail(&mut self, host: &str, agent_id: &str, cx: &mut Context<Self>) {
        let message = json!({
            "type": "fetch_agent_timeline_request",
            "agentId": agent_id,
            "direction": "tail",
            "limit": 40,
            "projection": "projected",
        });
        self.fetch_page(host, agent_id, PageKind::Tail, message, cx);
    }

    fn fetch_after(
        &mut self,
        host: &str,
        agent_id: &str,
        epoch: &str,
        seq: u64,
        cx: &mut Context<Self>,
    ) {
        let message = json!({
            "type": "fetch_agent_timeline_request",
            "agentId": agent_id,
            "direction": "after",
            "cursor": { "epoch": epoch, "seq": seq },
            "limit": 40,
            "projection": "projected",
        });
        self.fetch_page(host, agent_id, PageKind::After, message, cx);
    }

    /// Loads the page before what is shown, for scrolling up.
    pub fn fetch_older(&mut self, host: &str, agent_id: &str, cx: &mut Context<Self>) {
        self.fetch_before(host, agent_id, 40, cx);
    }

    fn fetch_before(&mut self, host: &str, agent_id: &str, limit: u32, cx: &mut Context<Self>) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        let timeline = &session.read(cx).timeline;
        let (Some(cursor), true) = (timeline.cursor.clone(), timeline.has_older) else {
            return;
        };
        self.fetch_page(host, agent_id, PageKind::Before, json!({
            "type":"fetch_agent_timeline_request", "agentId":agent_id, "direction":"before",
            "cursor":{"epoch":cursor.epoch, "seq":cursor.start_seq}, "limit":limit, "projection":"projected",
        }), cx);
    }

    fn fetch_page(
        &mut self,
        host: &str,
        agent_id: &str,
        kind: PageKind,
        message: Value,
        cx: &mut Context<Self>,
    ) {
        if !self.begin_fetch(host, agent_id, matches!(kind, PageKind::Tail), cx) {
            return;
        }
        let older = matches!(kind, PageKind::Before);
        let task = self.request(host, message, cx);
        let host = host.to_string();
        let agent_id = agent_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = task.await.and_then(|payload| {
                if let Some(error) = payload["error"].as_str() {
                    Err(error.to_string())
                } else {
                    Ok(payload)
                }
            });
            let _ = this.update(cx, |store, cx| {
                let Some(session) = store.session(&host, &agent_id) else {
                    return;
                };
                let (follow, reload, outcome) = session.update(cx, |session, cx| {
                    session.loading = false;
                    session.fetching = false;
                    let reload = std::mem::take(&mut session.reload_pending);
                    let previous = session.timeline.cursor.clone();
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
                    cx.notify();
                    let outcome = match &result {
                        Ok(_) => PageOutcome::Success {
                            older,
                            progressed: session.timeline.cursor != previous,
                        },
                        Err(error) => PageOutcome::Failure(error.clone()),
                    };
                    (follow, reload, outcome)
                });
                if reload {
                    store.fetch_tail(&host, &agent_id, cx);
                } else {
                    store.follow(&host, &agent_id, follow, cx);
                }
                store.continue_prompt_load(&host, &agent_id, Some(&outcome), cx);
                store.ensure_prompt_index(&host, &agent_id, cx);
            });
        })
        .detach();
    }

    fn ensure_prompt_index(&mut self, host: &str, agent_id: &str, cx: &mut Context<Self>) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        let timeline = &session.read(cx).timeline;
        let Some(cursor) = &timeline.cursor else {
            return;
        };
        let latest = timeline
            .items()
            .filter_map(|item| match &item.kind {
                fleet::timeline::ItemKind::User { seq, .. } => *seq,
                _ => None,
            })
            .max();
        let key = (cursor.epoch.clone(), latest);
        if session.read(cx).prompt_index.key.as_ref() == Some(&key) {
            return;
        }
        let generation = session.update(cx, |s, cx| {
            let index = &mut s.prompt_index;
            if index.key.as_ref().is_some_and(|(epoch, _)| epoch != &key.0) {
                index.marks.clear();
            }
            index.key = Some(key.clone());
            index.generation += 1;
            index.revision += 1;
            cx.notify();
            index.generation
        });
        if !self
            .fleet
            .host(host)
            .is_some_and(|h| h.has("agentTimelinePromptIndex"))
        {
            return;
        }
        let task = self.request(host, requests::prompt_index(agent_id), cx);
        cx.spawn(async move |_, cx| {
            let result = task.await;
            session.update(cx, |s, cx| {
                if s.prompt_index.generation != generation {
                    return;
                }
                if let Ok(payload) = result
                    && payload["epoch"].as_str() == Some(&key.0)
                    && payload["error"].as_str().is_none()
                    && s.timeline.cursor.as_ref().is_some_and(|c| c.epoch == key.0)
                    && let Ok(mut marks) =
                        serde_json::from_value::<Vec<PromptMark>>(payload["prompts"].clone())
                {
                    marks.sort_by_key(|p| p.seq);
                    s.prompt_index.marks = marks;
                    s.prompt_index.revision += 1;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Prepend contiguous history through the normal paging gate, keeping
    /// the live tail, held events and optimistic sends intact.
    pub fn load_prompt(&mut self, host: &str, agent_id: &str, seq: u64, cx: &mut Context<Self>) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        session.update(cx, |s, cx| {
            s.prompt_index.jump = match &s.timeline.cursor {
                Some(cursor) => JumpState::Loading {
                    epoch: cursor.epoch.clone(),
                    seq,
                },
                None => JumpState::Failed("Conversation is still loading; try again.".into()),
            };
            s.prompt_index.revision += 1;
            cx.notify();
        });
        self.continue_prompt_load(host, agent_id, None, cx);
    }

    fn continue_prompt_load(
        &mut self,
        host: &str,
        agent_id: &str,
        page: Option<&PageOutcome>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        session.update(cx, |s, cx| {
            let previous = &s.prompt_index.jump;
            let next = previous.advance(&s.timeline, s.fetching, page);
            if &next != previous {
                s.prompt_index.jump = next;
                s.prompt_index.revision += 1;
                cx.notify();
            }
        });
        let state = session.read(cx);
        if let JumpState::Loading { epoch, seq } = state.prompt_index.jump()
            && fleet::prompt_index::history_load(&state.timeline, epoch, *seq, state.fetching)
                == fleet::prompt_index::HistoryLoad::FetchOlder
        {
            self.fetch_before(host, agent_id, 200, cx);
        }
    }

    // ---- composer actions ---------------------------------------------------

    pub fn can_submit(&self, host: &str, agent_id: &str, cx: &App) -> Result<(), String> {
        let Some(session) = self.session(host, agent_id) else {
            return Err("Conversation is not ready; try again.".into());
        };
        if session.read(cx).unsent_count() >= 32 {
            return Err(
                "Restore failed messages or wait for queued messages before sending more.".into(),
            );
        }
        Ok(())
    }

    pub fn send_prompt(
        &mut self,
        host: &str,
        agent_id: &str,
        prompt: Prompt,
        behavior: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.can_submit(host, agent_id, cx)?;
        let message_id = uuid_v4();
        let now = now_iso();
        if let Some(session) = self.session(host, agent_id) {
            session.update(cx, |session, cx| {
                session.inflight_sends.insert(message_id.clone());
                session
                    .timeline
                    .add_local_prompt(&message_id, &prompt, &now);
                cx.notify();
            });
        }
        let message = requests::send_message(agent_id, &message_id, &prompt, behavior);
        let task = self.write(host, message, cx);
        let host = host.to_string();
        let agent_id = agent_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let failure = match result {
                Err(err) => Some(err),
                Ok(payload) if payload["accepted"] == false || payload["error"].is_string() => {
                    Some(
                        payload["error"]
                            .as_str()
                            .unwrap_or("Message was not accepted")
                            .to_string(),
                    )
                }
                Ok(_) => None,
            };
            let _ = this.update(cx, |store, cx| {
                if let Some(session) = store.session(&host, &agent_id) {
                    session.update(cx, |session, cx| {
                        session.inflight_sends.remove(&message_id);
                        if failure.is_some() {
                            session.timeline.remove_local_user(&message_id);
                            session.failed_prompts.insert(message_id, prompt);
                        }
                        cx.notify();
                    });
                }
                if let Some(error) = failure {
                    cx.emit(FleetEvent::Toast(error));
                }
            });
        })
        .detach();
        Ok(())
    }

    pub fn failed_prompt(&self, host: &str, agent_id: &str, id: &str, cx: &App) -> Option<Prompt> {
        self.session(host, agent_id)?
            .read(cx)
            .failed_prompts
            .get(id)
            .cloned()
    }
    pub fn take_failed(
        &mut self,
        host: &str,
        agent_id: &str,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Option<Prompt> {
        self.session(host, agent_id)?.update(cx, |s, cx| {
            let prompt = s.failed_prompts.remove(id);
            if prompt.is_some() {
                cx.notify();
            }
            prompt
        })
    }

    pub fn queue_message(
        &mut self,
        host: &str,
        agent_id: &str,
        prompt: Prompt,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.can_submit(host, agent_id, cx)?;
        if let Some(session) = self.session(host, agent_id) {
            session.update(cx, |session, cx| {
                session.queue.push(prompt);
                cx.notify();
            });
        }
        Ok(())
    }

    pub fn cancel(&mut self, host: &str, agent_id: &str, cx: &mut Context<Self>) {
        self.act(
            host,
            json!({ "type": "cancel_agent_request", "agentId": agent_id }),
            cx,
        );
    }

    pub fn answer_permission(
        &mut self,
        host: &str,
        agent_id: &str,
        permission_id: &str,
        response: Value,
        cx: &mut Context<Self>,
    ) {
        self.send_raw(
            host,
            json!({
                "type": "agent_permission_response",
                "agentId": agent_id,
                "requestId": permission_id,
                "response": response,
            }),
        );
        // The card stays until the daemon confirms with
        // agent_permission_resolved, so an answer lost to a reconnect can be
        // given again.
        let _ = cx;
    }

    pub fn clear_attention(&mut self, host: &str, agent_id: &str, cx: &mut Context<Self>) {
        let needs = self.fleet.agent(host, agent_id).is_some_and(|a| {
            a.requires_attention && a.attention_reason.as_deref() != Some("permission")
        });
        if needs {
            let task = self.request(
                host,
                json!({ "type": "clear_agent_attention", "agentId": agent_id }),
                cx,
            );
            task.detach();
        }
    }

    /// Creates an agent in a workspace and returns its id when ready.
    pub fn create_agent(
        &mut self,
        host: &str,
        workspace_id: &str,
        config: Value,
        prompt: Option<Prompt>,
        cx: &mut Context<Self>,
    ) -> Task<Result<String, String>> {
        let message = requests::create_agent(workspace_id, config, prompt, &uuid_v4());
        let task = self.write(host, message, cx);
        cx.background_spawn(async move {
            let payload = task.await?;
            if let Some(error) = payload["error"].as_str() {
                return Err(error.to_string());
            }
            payload["agent"]["id"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "The host did not return an agent".to_string())
        })
    }

    // ---- terminals --------------------------------------------------------

    /// Membership generation used only by terminal tab reconciliation.
    pub fn terminal_nav_revision(&self) -> u64 {
        self.terminal_nav_revision
    }

    /// Terminal ids per workspace for tab reconciliation. `None` until the
    /// live list has arrived on this connection.
    pub fn terminal_nav_lists(&self) -> BTreeMap<String, Option<Vec<String>>> {
        let mut out = BTreeMap::new();
        for ((host, workspace), list) in &self.terminal_lists {
            let key = format!("{host}:{workspace}");
            let value = list.loaded.then(|| {
                list.terminals
                    .iter()
                    .filter(|t| !self.terminal_is_closing(host, &t.id))
                    .map(|t| t.id.clone())
                    .collect()
            });
            out.insert(key, value);
        }
        out
    }

    /// The workspace a listed terminal belongs to.
    pub fn terminal_workspace(&self, host: &str, id: &str) -> Option<String> {
        self.terminal_lists
            .iter()
            .find(|((h, _), list)| h == host && list.terminals.iter().any(|t| t.id == id))
            .map(|((_, workspace), _)| workspace.clone())
    }

    fn terminal_is_closing(&self, host: &str, id: &str) -> bool {
        self.closing_terminals
            .get(host)
            .is_some_and(|ids| ids.contains(id))
    }

    /// The terminals of a workspace whose list has loaded, in the host's
    /// order, without those being closed.
    pub fn workspace_terminal_ids<'a>(
        &'a self,
        host: &'a str,
        workspace: &str,
    ) -> impl Iterator<Item = &'a String> {
        self.terminal_lists
            .get(&(host.to_string(), workspace.to_string()))
            .filter(|list| list.loaded)
            .into_iter()
            .flat_map(|list| list.terminals.iter())
            .filter(move |record| !self.terminal_is_closing(host, &record.id))
            .map(|record| &record.id)
    }

    pub fn workspace_terminals_revision(&self, host: &str, workspace: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        if let Some(list) = self
            .terminal_lists
            .get(&(host.to_string(), workspace.to_string()))
        {
            (list.loaded, list.request, list.revision).hash(&mut hash);
        }
        hash.finish()
    }

    pub fn terminal_session(&self, host: &str, id: &str) -> Option<Entity<TerminalSession>> {
        self.terminals
            .get(&(host.to_string(), id.to_string()))
            .cloned()
    }

    pub fn terminal_label(&self, host: &str, id: &str, cx: &App) -> String {
        if let Some(session) = self.terminal_session(host, id) {
            let title = session.read(cx).title.clone();
            if let Some(title) = title.filter(|t| !t.is_empty()) {
                return title;
            }
        }
        for ((h, _), list) in &self.terminal_lists {
            if h != host {
                continue;
            }
            if let Some(record) = list.terminals.iter().find(|t| t.id == id) {
                if let Some(title) = record.title.clone().filter(|t| !t.is_empty()) {
                    return title;
                }
                if !record.name.is_empty() {
                    return record.name.clone();
                }
            }
        }
        "Terminal".into()
    }

    /// Subscribes to the standalone terminals of the workspaces on screen.
    pub fn ensure_terminal_watches(
        &mut self,
        owner: &'static str,
        wanted: Vec<TerminalWatch>,
        cx: &mut Context<Self>,
    ) {
        self.terminal_watch_sets.insert(owner, wanted);
        let wanted = terminal_watch_union(&self.terminal_watch_sets);
        let stale: Vec<_> = self
            .terminal_lists
            .keys()
            .filter(|key| !wanted.contains_key(*key))
            .cloned()
            .collect();
        for key in stale {
            if let Some(list) = self.terminal_lists.remove(&key)
                && let Some(subscription) = list.subscription
            {
                self.request(&key.0, requests::release(&subscription), cx)
                    .detach();
            }
        }
        self.closing_terminals.retain(|host, _| {
            self.terminal_lists
                .keys()
                .any(|(listed_host, _)| listed_host == host)
        });
        for watch in wanted.into_values() {
            let key = (watch.host.clone(), watch.workspace.clone());
            let online = self.links.get(&watch.host).is_some_and(|link| link.online);
            let epoch = self
                .links
                .get(&watch.host)
                .map(|link| link.epoch)
                .unwrap_or(0);
            let fresh = self.terminal_lists.get(&key).is_some_and(|list| {
                (list.subscription.is_some() || list.pending)
                    && list.epoch == epoch
                    && list.cwd == watch.cwd
            });
            if !online || fresh {
                self.terminal_lists
                    .entry(key)
                    .or_insert_with(|| TerminalList {
                        cwd: watch.cwd,
                        loaded: false,
                        terminals: Vec::new(),
                        subscription: None,
                        epoch: 0,
                        request: 0,
                        pending: false,
                        revision: 0,
                    });
                continue;
            }
            self.watch_terminals(watch, epoch, cx);
        }
    }

    pub fn open_terminal(
        &mut self,
        host: &str,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalSession> {
        let key = (host.to_string(), id.to_string());
        if let Some(session) = self.terminals.get(&key) {
            if let Some(link) = self.links.get_mut(host) {
                link.open_terminals.insert(id.to_string());
            }
            return session.clone();
        }
        let session = cx.new(|_| TerminalSession::new(host, id));
        self.terminals.insert(key, session.clone());
        if let Some(link) = self.links.get_mut(host) {
            link.open_terminals.insert(id.to_string());
        }
        session
    }

    pub fn retain_terminals(&mut self, keep: &BTreeSet<(String, String)>, cx: &mut Context<Self>) {
        let drop_keys: Vec<(String, String)> = self
            .terminals
            .keys()
            .filter(|key| !keep.contains(key))
            .cloned()
            .collect();
        for (host, id) in drop_keys {
            self.detach_terminal(&host, &id, cx);
            self.terminals.remove(&(host, id));
        }
    }

    pub fn create_terminal(
        &mut self,
        host: &str,
        workspace: &str,
        cwd: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<String, String>> {
        let task = self.write(host, requests::create_terminal(cwd, workspace, 24, 80), cx);
        let host = host.to_string();
        let workspace = workspace.to_string();
        cx.spawn(async move |this, cx| {
            let payload = task.await?;
            if let Some(error) = payload["error"].as_str().filter(|e| !e.is_empty()) {
                return Err(error.to_string());
            }
            let id = payload["terminal"]["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() {
                return Err("The host did not create a terminal".into());
            }
            let record = TerminalRecord {
                id: id.clone(),
                name: payload["terminal"]["name"]
                    .as_str()
                    .unwrap_or("Terminal")
                    .to_string(),
                title: payload["terminal"]["title"].as_str().map(str::to_string),
            };
            let _ = this.update(cx, |store, cx| {
                store.note_created_terminal(&host, &workspace, record);
                store.open_terminal(&host, &id, cx);
                store.changed(cx);
            });
            Ok(id)
        })
    }

    pub fn kill_terminal(&mut self, host: &str, id: &str, cx: &mut Context<Self>) {
        self.closing_terminals
            .entry(host.to_string())
            .or_default()
            .insert(id.to_string());
        for ((h, _), list) in &mut self.terminal_lists {
            if h == host {
                let before = list.terminals.len();
                list.terminals.retain(|t| t.id != id);
                if list.terminals.len() != before {
                    list.revision += 1;
                }
            }
        }
        self.detach_terminal(host, id, cx);
        self.act(host, requests::kill_terminal(id), cx);
        self.changed(cx);
    }

    pub fn terminal_write(&self, host: &str, id: &str, data: &str) {
        if data.is_empty() {
            return;
        }
        self.send_raw(host, requests::terminal_input(id, data));
    }

    pub fn terminal_resize(
        &mut self,
        host: &str,
        id: &str,
        rows: u32,
        cols: u32,
        cx: &mut Context<Self>,
    ) {
        if rows == 0 || cols == 0 {
            return;
        }
        let Some(session) = self.terminal_session(host, id) else {
            return;
        };
        let (attaching, subscribed, same) = session.update(cx, |session, cx| {
            session.emulator.resize(rows as usize, cols as usize);
            session.rows = rows;
            session.cols = cols;
            cx.notify();
            (
                session.attaching,
                session.subscription_id.is_some(),
                session.attached_size == Some((rows, cols)) && session.claimed,
            )
        });
        if same || attaching {
            return;
        }
        let online = self.links.get(host).is_some_and(|link| link.online);
        if !online {
            return;
        }
        if subscribed {
            self.send_raw(host, requests::terminal_resize(id, rows, cols, "update"));
            session.update(cx, |session, _| {
                session.attached_size = Some((rows, cols));
                session.claimed = true;
            });
        } else {
            self.attach_terminal(host, id, cx);
        }
    }

    fn note_created_terminal(&mut self, host: &str, workspace: &str, record: TerminalRecord) {
        let Some(list) = self
            .terminal_lists
            .get_mut(&(host.to_string(), workspace.to_string()))
        else {
            return;
        };
        if !list.terminals.iter().any(|t| t.id == record.id) {
            list.terminals.insert(0, record);
            list.revision += 1;
        }
    }

    fn watch_terminals(&mut self, watch: TerminalWatch, epoch: u64, cx: &mut Context<Self>) {
        let key = (watch.host.clone(), watch.workspace.clone());
        let previous = self
            .terminal_lists
            .get(&key)
            .and_then(|list| list.subscription.clone());
        let (loaded, terminals) = self
            .terminal_lists
            .get(&key)
            .map(|list| (list.loaded, list.terminals.clone()))
            .unwrap_or((false, Vec::new()));
        self.terminal_watch_request += 1;
        let request = self.terminal_watch_request;
        self.terminal_lists.insert(
            key,
            TerminalList {
                cwd: watch.cwd.clone(),
                loaded,
                terminals,
                subscription: None,
                epoch,
                request,
                pending: true,
                revision: 0,
            },
        );
        if let Some(previous) = previous {
            self.request(&watch.host, requests::release(&previous), cx)
                .detach();
        }
        let task = self.request(
            &watch.host,
            requests::subscribe_terminals(&watch.cwd, &watch.workspace),
            cx,
        );
        let host = watch.host.clone();
        let workspace = watch.workspace.clone();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |store, cx| {
                if !store.is_current(&host, epoch) {
                    return;
                }
                let key = (host.clone(), workspace.clone());
                let current = store
                    .terminal_lists
                    .get(&key)
                    .is_some_and(|list| list.request == request);
                if !current {
                    if let Ok(payload) = result
                        && let Some(subscription) = payload["subscriptionId"].as_str()
                    {
                        store
                            .request(&host, requests::release(subscription), cx)
                            .detach();
                    }
                    return;
                }
                if let Some(list) = store.terminal_lists.get_mut(&key) {
                    list.pending = false;
                }
                if let Ok(payload) = result {
                    store.apply_terminal_list(&host, &workspace, &payload, cx);
                }
            });
        })
        .detach();
    }

    fn rewatch_terminal_lists(&mut self, host: &str, cx: &mut Context<Self>) {
        let epoch = self.links.get(host).map(|link| link.epoch).unwrap_or(0);
        let watches: Vec<TerminalWatch> = self
            .terminal_lists
            .iter()
            .filter(|((h, _), _)| h == host)
            .map(|((_, workspace), list)| TerminalWatch {
                host: host.to_string(),
                workspace: workspace.clone(),
                cwd: list.cwd.clone(),
            })
            .collect();
        for ((h, _), list) in &mut self.terminal_lists {
            if h == host {
                list.subscription = None;
                list.loaded = false;
                list.epoch = 0;
            }
        }
        for watch in watches {
            self.watch_terminals(watch, epoch, cx);
        }
    }

    fn apply_terminal_list(
        &mut self,
        host: &str,
        workspace: &str,
        payload: &Value,
        cx: &mut Context<Self>,
    ) {
        let records = parse_terminal_records(&payload["terminals"]);
        let changed = self
            .terminal_lists
            .get(&(host.to_string(), workspace.to_string()))
            .is_some_and(|list| !list.loaded || list.terminals != records);
        let membership_changed = self
            .terminal_lists
            .get(&(host.to_string(), workspace.to_string()))
            .is_some_and(|list| {
                !list.loaded
                    || !list
                        .terminals
                        .iter()
                        .map(|t| &t.id)
                        .eq(records.iter().map(|t| &t.id))
            });
        if membership_changed {
            self.terminal_nav_revision += 1;
        }
        let ids: BTreeSet<String> = records.iter().map(|r| r.id.clone()).collect();
        if let Some(closing) = self.closing_terminals.get_mut(host) {
            closing.retain(|id| ids.contains(id));
        }
        let previous: BTreeSet<String> = self
            .terminal_lists
            .get(&(host.to_string(), workspace.to_string()))
            .map(|list| list.terminals.iter().map(|r| r.id.clone()).collect())
            .unwrap_or_default();
        if let Some(list) = self
            .terminal_lists
            .get_mut(&(host.to_string(), workspace.to_string()))
        {
            list.loaded = true;
            if changed {
                list.revision += 1;
            }
            list.terminals = records;
            if let Some(subscription) = payload["subscriptionId"].as_str() {
                list.subscription = Some(subscription.to_string());
            }
            if let Some(link) = self.links.get(host) {
                list.epoch = link.epoch;
            }
        }
        for record in self
            .terminal_lists
            .get(&(host.to_string(), workspace.to_string()))
            .map(|list| list.terminals.clone())
            .unwrap_or_default()
        {
            if let Some(session) = self.terminal_session(host, &record.id) {
                session.update(cx, |session, cx| {
                    if session.title.is_none() {
                        session.title = record.title.clone();
                        cx.notify();
                    }
                });
            }
        }
        let gone: Vec<String> = previous
            .into_iter()
            .filter(|id| !ids.contains(id))
            .filter(|id| !self.terminal_is_closing(host, id))
            .filter(|id| self.terminal_session(host, id).is_some())
            .collect();
        for id in gone {
            self.fail_terminal(host, &id, "This terminal is gone".into(), cx);
        }
        if changed {
            cx.notify();
        }
    }

    fn on_terminals_changed(&mut self, host: &str, payload: &Value, cx: &mut Context<Self>) {
        let Some(workspace) = payload["workspaceId"].as_str() else {
            return;
        };
        if !self
            .terminal_lists
            .contains_key(&(host.to_string(), workspace.to_string()))
        {
            return;
        }
        self.apply_terminal_list(host, workspace, payload, cx);
    }

    fn on_terminal_exit(&mut self, host: &str, payload: &Value, cx: &mut Context<Self>) {
        let Some(id) = payload["terminalId"].as_str() else {
            return;
        };
        let message = payload["error"]
            .as_str()
            .filter(|e| !e.is_empty())
            .unwrap_or("This terminal is gone")
            .to_string();
        self.fail_terminal(host, id, message, cx);
    }

    fn on_terminal_frame(&mut self, host: &str, bytes: &[u8], cx: &mut Context<Self>) {
        let Some(frame) = paseo_client::terminal::decode_frame(bytes) else {
            return;
        };
        let Some(id) = self
            .links
            .get(host)
            .and_then(|link| link.terminal_slots.get(&frame.slot))
            .cloned()
        else {
            return;
        };
        let Some(session) = self.terminal_session(host, &id) else {
            return;
        };
        let title_changed = session.update(cx, |session, cx| {
            match frame.opcode {
                paseo_client::terminal::Opcode::Output => session.emulator.feed(&frame.payload),
                paseo_client::terminal::Opcode::Restore => session.emulator.restore(&frame.payload),
                paseo_client::terminal::Opcode::Snapshot => {
                    if let Some(state) = paseo_client::terminal::decode_snapshot(&frame.payload) {
                        if session.title.is_none() {
                            session.title = state.title.clone();
                        }
                        let ansi = paseo_client::terminal::snapshot_to_ansi(&state);
                        session.emulator.restore(&ansi);
                    }
                }
                _ => {}
            }
            let title = session.emulator.title();
            let changed = title != session.title && title.is_some();
            if title.is_some() {
                session.title = title;
            }
            cx.notify();
            changed
        });
        if title_changed {
            self.changed(cx);
        }
    }

    fn attach_terminal(&mut self, host: &str, id: &str, cx: &mut Context<Self>) {
        let Some(link) = self.links.get(host) else {
            return;
        };
        if !link.online {
            return;
        }
        let epoch = link.epoch;
        let Some(session) = self.terminal_session(host, id) else {
            return;
        };
        let (rows, cols, generation) = session.update(cx, |session, _| {
            session.attach_gen += 1;
            session.attaching = true;
            session.error = None;
            session.offline = false;
            (session.rows, session.cols, session.attach_gen)
        });
        if rows > 0 && cols > 0 {
            self.send_raw(host, requests::terminal_resize(id, rows, cols, "claim"));
        }
        let restore = self.links.get(host).and_then(|_| {
            self.fleet.host(host).and_then(|h| {
                h.has("terminal-restore-modes").then(|| {
                    let mut value = json!({
                        "mode": "visible-snapshot",
                        "scrollbackLines": 200,
                    });
                    if rows > 0 && cols > 0 {
                        value["size"] = json!({ "rows": rows, "cols": cols });
                    }
                    value
                })
            })
        });
        let task = self.request(host, requests::subscribe_terminal(id, restore), cx);
        let host = host.to_string();
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |store, cx| {
                if !store.is_current(&host, epoch) {
                    return;
                }
                let Some(session) = store.terminal_session(&host, &id) else {
                    return;
                };
                if session.read(cx).attach_gen != generation {
                    return;
                }
                match result {
                    Ok(payload) => {
                        if let Some(error) = payload["error"].as_str().filter(|e| !e.is_empty()) {
                            store.fail_terminal(&host, &id, error.to_string(), cx);
                            return;
                        }
                        let Some(slot) = payload["slot"].as_u64() else {
                            store.fail_terminal(&host, &id, "The host did not attach".into(), cx);
                            return;
                        };
                        let slot = slot as u8;
                        let subscription = payload["subscriptionId"].as_str().map(str::to_string);
                        if let Some(link) = store.links.get_mut(&host) {
                            link.terminal_slots.retain(|_, terminal| terminal != &id);
                            link.terminal_slots.insert(slot, id.clone());
                        }
                        let previous = session.update(cx, |session, cx| {
                            session.attaching = false;
                            session.offline = false;
                            session.error = None;
                            session.claimed = rows > 0 && cols > 0;
                            if rows > 0 && cols > 0 {
                                session.attached_size = Some((rows, cols));
                            }
                            let previous = session.subscription_id.take();
                            session.subscription_id = subscription;
                            cx.notify();
                            previous
                        });
                        if let Some(previous) = previous {
                            store
                                .request(&host, requests::release(&previous), cx)
                                .detach();
                        }
                        let current = session.read(cx);
                        if current.rows > 0
                            && current.cols > 0
                            && current.attached_size != Some((current.rows, current.cols))
                        {
                            let (rows, cols) = (current.rows, current.cols);
                            store.send_raw(
                                &host,
                                requests::terminal_resize(&id, rows, cols, "update"),
                            );
                            session.update(cx, |session, _| {
                                session.attached_size = Some((rows, cols));
                                session.claimed = true;
                            });
                        }
                    }
                    Err(err) => store.fail_terminal(&host, &id, err, cx),
                }
            });
        })
        .detach();
    }

    fn reattach_terminals(&mut self, host: &str, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .links
            .get(host)
            .map(|link| link.open_terminals.iter().cloned().collect())
            .unwrap_or_default();
        for id in ids {
            if let Some(session) = self.terminal_session(host, &id) {
                session.update(cx, |session, _| {
                    session.subscription_id = None;
                    session.claimed = false;
                    session.attached_size = None;
                    session.attaching = false;
                    session.offline = false;
                });
            }
            self.attach_terminal(host, &id, cx);
        }
    }

    fn fail_terminal(&mut self, host: &str, id: &str, message: String, cx: &mut Context<Self>) {
        if let Some(link) = self.links.get_mut(host) {
            link.terminal_slots.retain(|_, terminal| terminal != id);
        }
        if let Some(session) = self.terminal_session(host, id) {
            session.update(cx, |session, cx| {
                session.attaching = false;
                session.offline = false;
                session.error = Some(message);
                session.subscription_id = None;
                cx.notify();
            });
        }
    }

    fn detach_terminal(&mut self, host: &str, id: &str, cx: &mut Context<Self>) {
        if let Some(link) = self.links.get_mut(host) {
            link.open_terminals.remove(id);
            link.terminal_slots.retain(|_, terminal| terminal != id);
        }
        if let Some(session) = self.terminal_session(host, id) {
            let subscription = session.update(cx, |session, _| session.subscription_id.take());
            if let Some(subscription) = subscription.filter(|id| !id.is_empty()) {
                self.request(host, requests::release(&subscription), cx)
                    .detach();
            }
        }
    }

    fn mark_terminal_lists_stale(&mut self, host: &str) {
        for ((h, _), list) in &mut self.terminal_lists {
            if h == host {
                list.subscription = None;
                list.loaded = false;
                list.epoch = 0;
            }
        }
    }
}

fn prune_tab_marks(
    tracked: &mut BTreeSet<(String, String)>,
    fleet: &Fleet,
    configured: &BTreeSet<String>,
) {
    tracked.retain(|(host, agent)| {
        configured.contains(host)
            && !fleet.host(host).is_some_and(|host| {
                host.connection.is_online()
                    && host.directory.agents_hydrated
                    && !host.directory.agents.contains_key(agent)
            })
    });
}

fn subagent_tab_actions(
    tracked: &mut BTreeSet<(String, String)>,
    desired: &BTreeSet<(String, String)>,
    marked: &BTreeSet<(String, String)>,
    online: &BTreeSet<String>,
) -> Vec<(String, String, bool)> {
    tracked.extend(desired.iter().cloned());
    desired
        .difference(marked)
        .filter(|(host, _)| online.contains(host))
        .map(|(host, agent)| (host.clone(), agent.clone(), true))
        .chain(
            tracked
                .difference(desired)
                .filter(|(host, _)| online.contains(host))
                .map(|(host, agent)| (host.clone(), agent.clone(), false)),
        )
        .collect()
}

fn terminal_watch_union(
    owners: &HashMap<&'static str, Vec<TerminalWatch>>,
) -> HashMap<(String, String), TerminalWatch> {
    owners
        .values()
        .flatten()
        .map(|watch| ((watch.host.clone(), watch.workspace.clone()), watch.clone()))
        .collect()
}

fn release_viewer<K: std::hash::Hash + Eq>(viewers: &mut HashMap<K, usize>, key: &K) -> bool {
    let Some(count) = viewers.get_mut(key) else {
        return false;
    };
    if *count > 1 {
        *count -= 1;
        false
    } else {
        viewers.remove(key);
        true
    }
}

fn parse_terminal_records(value: &Value) -> Vec<TerminalRecord> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            if entry["agentId"].as_str().is_some() {
                return None;
            }
            let id = entry["id"].as_str()?.to_string();
            Some(TerminalRecord {
                id,
                name: entry["name"].as_str().unwrap_or("Terminal").to_string(),
                title: entry["title"].as_str().map(str::to_string),
            })
        })
        .collect()
}

fn log_error(host: &str, what: &str, err: &paseo_client::Error) {
    eprintln!("crusteo: {host}: loading {what} failed: {err}");
}

pub fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    iso_from_millis(now.as_millis() as i64)
}

/// Formats epoch millis as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC).
pub fn iso_from_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parses `YYYY-MM-DDTHH:MM:SS(.fff)Z` into epoch millis.
pub fn parse_iso(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let millis = if bytes.get(19) == Some(&b'.') {
        let frac: String = text[20..]
            .chars()
            .take_while(char::is_ascii_digit)
            .take(3)
            .collect();
        format!("{frac:0<3}").parse::<i64>().ok()?
    } else {
        0
    };
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj.rem_euclid(400);
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + s) * 1000) + millis)
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// "3m ago" style.
pub fn time_ago(iso: &str) -> String {
    let Some(then) = parse_iso(iso) else {
        return String::new();
    };
    let secs = (now_millis() - then) / 1000;
    match secs {
        s if s < 45 => "just now".into(),
        s if s < 3600 => format!("{}m ago", (s / 60).max(1)),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

pub fn uuid_v4() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_i64(now_millis());
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_marks_wait_for_authoritative_directory_and_drop_removed_hosts() {
        let key = ("mac".to_string(), "child".to_string());
        let configured = BTreeSet::from(["mac".to_string()]);
        let mut fleet = Fleet::new([HostConfig {
            name: "mac".into(),
            ..Default::default()
        }]);
        let mut tracked = BTreeSet::from([key.clone(), ("removed".into(), "child".into())]);
        prune_tab_marks(&mut tracked, &fleet, &configured);
        assert_eq!(tracked, BTreeSet::from([key.clone()]));
        fleet.hosts[0].connection = Connection::Online;
        prune_tab_marks(&mut tracked, &fleet, &configured);
        assert!(tracked.contains(&key));
        fleet.hosts[0].directory.agents_hydrated = true;
        prune_tab_marks(&mut tracked, &fleet, &configured);
        assert!(tracked.is_empty());
        tracked.insert(key.clone());
        fleet.hosts[0].connection = Connection::Offline("test".into());
        prune_tab_marks(&mut tracked, &fleet, &configured);
        assert!(tracked.contains(&key));
    }

    #[test]
    fn restored_subagent_tab_clears_its_existing_mark_after_offline_close() {
        let key = ("mac".to_string(), "child".to_string());
        let mut tracked = BTreeSet::new();
        let desired = BTreeSet::from([key.clone()]);
        let online = BTreeSet::from(["mac".to_string()]);
        assert!(subagent_tab_actions(&mut tracked, &desired, &desired, &online).is_empty());
        assert!(tracked.contains(&key));
        assert!(
            subagent_tab_actions(&mut tracked, &BTreeSet::new(), &desired, &BTreeSet::new())
                .is_empty()
        );
        assert!(tracked.contains(&key));
        assert_eq!(
            subagent_tab_actions(&mut tracked, &BTreeSet::new(), &desired, &online),
            vec![("mac".into(), "child".into(), false)]
        );
    }

    #[test]
    fn terminal_watch_owners_preserve_shared_workspaces_and_drop_scrolled_out_ones() {
        let watch = |workspace: &str| TerminalWatch {
            host: "mac".into(),
            workspace: workspace.into(),
            cwd: format!("/code/{workspace}"),
        };
        let mut owners = HashMap::from([
            ("paseo", vec![watch("shared")]),
            ("lanes", vec![watch("shared"), watch("old")]),
        ]);
        assert_eq!(terminal_watch_union(&owners).len(), 2);
        owners.insert("lanes", vec![watch("new")]);
        let union = terminal_watch_union(&owners);
        assert!(union.contains_key(&("mac".into(), "shared".into())));
        assert!(union.contains_key(&("mac".into(), "new".into())));
        assert!(!union.contains_key(&("mac".into(), "old".into())));
        owners.insert("lanes", Vec::new());
        assert_eq!(terminal_watch_union(&owners).len(), 1);
        owners.insert("paseo", Vec::new());
        assert!(terminal_watch_union(&owners).is_empty());
    }

    #[test]
    fn shared_subagent_survives_until_the_last_view_closes() {
        let mut viewers = HashMap::from([("subagent", 2)]);
        assert!(!release_viewer(&mut viewers, &"subagent"));
        assert_eq!(viewers["subagent"], 1);
        assert!(release_viewer(&mut viewers, &"subagent"));
        assert!(!release_viewer(&mut viewers, &"subagent"));
        assert!(viewers.is_empty());
    }

    #[test]
    fn iso_round_trip() {
        let ms = parse_iso("2026-10-02T06:57:24.852Z").unwrap();
        assert_eq!(iso_from_millis(ms), "2026-10-02T06:57:24.852Z");
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
    }
}
