//! The live fleet: one WebSocket per saved host, the merged directory, and
//! the agent conversations the user has open.
//!
//! Views observe [`FleetStore`] for directory changes and an [`AgentSession`]
//! for one conversation. Nothing else talks to a daemon.
//!
//! This file owns the connections: the store itself, its events and the
//! bootstrap of each host, and the request plumbing. The rest is split by
//! what it keeps: [`session`] (open conversations and the composer),
//! [`subagents`], [`terminals`] and [`actions`] (labels, images, archiving,
//! providers, creating things).

use std::collections::{BTreeSet, HashMap};

use fleet::sync::{Bootstrap, HostSync, Loaded, Step};
use fleet::timeline::Follow;
use fleet::{Connection, Fleet, HostConfig, HostId};
use gpui_kit::gpui::{App, AppContext, Context, Entity, EventEmitter, Task};
use paseo_client::{Client, Config, ConnState, Event};
use serde_json::Value;

mod actions;
mod session;
mod subagents;
mod terminals;

pub use session::{AgentSession, SessionPromptIndex};
use terminals::TerminalList;
pub use terminals::{TerminalSession, TerminalWatch};

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

/// A workspace [`FleetStore::create_workspace`] made.
#[derive(Debug, Clone)]
pub struct Created {
    pub workspace: String,
    /// Where it works on the host.
    pub directory: String,
    /// Its first agent, none when none was asked for, or why that failed
    /// although the workspace was made.
    pub agent: Result<Option<String>, String>,
}

/// Whether a host's providers are there to pick from, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvidersState {
    Ready,
    Loading,
    Offline,
    Failed(String),
}

impl ProvidersState {
    /// What a provider picker says when it has nothing to offer.
    pub fn label(&self) -> &str {
        match self {
            ProvidersState::Ready => "No providers",
            ProvidersState::Loading => "Loading providers…",
            ProvidersState::Offline => "Host offline",
            ProvidersState::Failed(_) => "Providers unavailable",
        }
    }
}

pub enum FleetEvent {
    /// Some host's directory or connection changed.
    Changed,
    /// A request failed in a way the user should see.
    Toast(String),
    /// An agent finished, failed, or needs permission.
    Attention {
        host: HostId,
        agent_id: String,
        workspace_id: Option<String>,
        reason: String,
        title: String,
        body: String,
    },
}

struct HostLink {
    client: Client,
    /// The link's generation and epoch, the conversations streamed on it,
    /// and the pushes held while it bootstraps.
    sync: HostSync,
    /// Terminals whose PTY we want attached. Slots are per connection.
    open_terminals: BTreeSet<String>,
    terminal_slots: HashMap<u8, String>,
    /// Frames for a slot not yet known, held a moment: a host can send a
    /// terminal's screen right behind the reply that names its slot, before
    /// the reply has been read. Replayed when the slot is.
    early_frames: Vec<(std::time::Instant, Vec<u8>)>,
}

/// Writes can take a while on a busy daemon (archive waits for the provider
/// to shut down); the reference client allows a minute.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub struct FleetStore {
    pub fleet: Fleet,
    links: HashMap<HostId, HostLink>,
    sessions: HashMap<(HostId, String), Entity<AgentSession>>,
    /// Provider subagents of the agents with open conversations, by host and
    /// parent agent, oldest first.
    provider_subagents: HashMap<(HostId, String), Vec<fleet::subagents::Subagent>>,
    /// Provider subagents taken off their parent's track, as host, parent
    /// and id. Kept for this run only, as Paseo does.
    hidden_provider_subagents: std::collections::HashSet<(HostId, String, String)>,
    /// Open provider subagent conversations, by host, parent and subagent.
    subagent_sessions: HashMap<(HostId, String, String), Entity<AgentSession>>,
    subagent_viewers: HashMap<(HostId, String, String), usize>,
    /// Bumped whenever `provider_subagents` changes.
    pub subagents_revision: u64,
    /// Subagent tabs this client has told their host it has open.
    open_tab_marks: BTreeSet<(HostId, String)>,
    subagent_tabs: BTreeSet<(HostId, String)>,
    tab_mark_pending: BTreeSet<(HostId, String, bool)>,
    tab_mark_retry_at: HashMap<(HostId, String, bool), i64>,
    /// Changes only when a streamed conversation is added or removed.
    /// Store observers must not assume every notification changes the directory.
    sessions_revision: u64,
    terminals: HashMap<(HostId, String), Entity<TerminalSession>>,
    terminal_lists: HashMap<(HostId, String), TerminalList>,
    // Used only by tab reconciliation, never by lane or card cache keys.
    terminal_nav_revision: u64,
    terminal_watch_sets: HashMap<&'static str, Vec<TerminalWatch>>,
    terminal_watch_request: u64,
    /// Tabs the user closed; kept out of reconciliation until the daemon drops them.
    closing_terminals: HashMap<HostId, BTreeSet<String>>,
    client_id: String,
    /// Providers each host offers, loaded on first need, dropped when the
    /// host reconnects, and refreshed when the daemon says they changed.
    pub providers: HashMap<HostId, Vec<fleet::providers::Provider>>,
    providers_loading: BTreeSet<HostId>,
    /// Why a host's providers could not be loaded; the next ask retries.
    providers_failed: HashMap<HostId, String>,
    /// The `snapshotHash` of each host's providers as last loaded, so a
    /// `providers_snapshot_update` that only names a hash can say whether
    /// they changed.
    providers_hash: HashMap<HostId, String>,
    /// Hosts whose providers changed while a load was in flight: that
    /// load's answer may predate the change, so it is asked again.
    providers_stale: BTreeSet<HostId>,
    /// Each host's CLIs and models, to tell when one upgrades or a model
    /// comes ([fleet::cli_watch]), and whether a flush is already waiting.
    pub cli_watch: fleet::cli_watch::Watch,
    cli_flush_scheduled: bool,
    /// The fleet is `CRUSTEO_STRESS`'s, so a conversation opened starts
    /// with a short made-up exchange instead of asking a daemon.
    stress: bool,
    /// Pushes a synthetic daemon owes after answering a request; the churn
    /// loop delivers them.
    stress_pushes: std::cell::RefCell<Vec<(HostId, Value)>>,
    next_generation: u64,
    /// Bumped on each directory change; cheap change detection for views.
    pub revision: u64,
    /// Requests awaiting acknowledgement/directory confirmation, keyed by attempt.
    workspace_archives: fleet::workspace_archive::PendingArchives,
    /// What changed in which host's directory, oldest first, for views that
    /// patch what they derived. Entry `i` has sequence `change_base + i`.
    change_log: std::collections::VecDeque<FleetChange>,
    change_base: u64,
    /// Images read from daemons for the conversations on screen.
    pub images: crate::app::images::ImageCache,
    /// In reef mode, the Reef the hosts come from and how the last ask went.
    pub reef: Option<ReefStatus>,
}

/// Reef mode's state, for the Hosts screen.
#[derive(Debug, Clone)]
pub struct ReefStatus {
    pub url: String,
    /// Why the last ask failed; the hosts are then the last list Reef gave.
    pub error: Option<String>,
    /// When a list last arrived, as `now_millis`.
    pub synced_at: Option<i64>,
    pub link: crate::app::reef::ReefLink,
    /// Reef's "agents can message agents on other hosts", once read; a Reef
    /// from before the setting leaves it unknown.
    pub routing: Option<bool>,
    /// Why turning it on or off failed.
    pub routing_error: Option<String>,
    /// Asks the loop following Reef to ask again now.
    wake: flume::Sender<()>,
}

/// One directory change on one host. A change the directories did not
/// journal (a host list or connection change) is logged as `Bulk` with an
/// empty host.
#[derive(Debug, Clone)]
pub struct FleetChange {
    pub host: HostId,
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
            providers_failed: HashMap::new(),
            providers_hash: HashMap::new(),
            providers_stale: BTreeSet::new(),
            cli_watch: fleet::cli_watch::Watch::default(),
            cli_flush_scheduled: false,
            stress: false,
            stress_pushes: Default::default(),
            next_generation: 0,
            revision: 0,
            workspace_archives: Default::default(),
            change_log: Default::default(),
            change_base: 0,
            images: Default::default(),
            reef: None,
        };
        if let Some(stress) = crate::app::stress::from_env() {
            store.fleet = stress.fleet(now_millis());
            store.stress = true;
            store.start_stress(stress, cx);
            return store;
        }
        let ids: Vec<HostId> = store.fleet.hosts.iter().map(|h| h.id().clone()).collect();
        for id in ids {
            store.connect(&id, cx);
        }
        if let Some(link) = crate::app::config::reef_link() {
            store.follow_reef(link, cx);
        }
        store
    }

    /// Reef mode: turns Reef's "agents can message agents on other hosts"
    /// on or off, for the whole fleet.
    pub fn set_agent_routing(&mut self, on: bool, cx: &mut Context<Self>) {
        let Some(status) = self.reef.as_mut() else {
            return;
        };
        let link = status.link.clone();
        status.routing = Some(on);
        status.routing_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let edit = serde_json::json!({ "agent_routing": on });
            let answer = cx
                .background_spawn(async move { link.settings(Some(&edit)) })
                .await;
            let _ = this.update(cx, |store, cx| {
                let Some(status) = store.reef.as_mut() else {
                    return;
                };
                match answer {
                    Ok(settings) => match settings["agent_routing"].as_bool() {
                        Some(now) => status.routing = Some(now),
                        None => {
                            status.routing = None;
                            status.routing_error =
                                Some("This Reef is too old for the setting: upgrade it".into());
                        }
                    },
                    Err(error) => {
                        status.routing = Some(!on);
                        status.routing_error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reef mode: asks Reef for its hosts now and every
    /// [`crate::app::reef::EVERY`], and makes them the fleet when they
    /// change. A failed ask keeps the hosts there are and says why.
    fn follow_reef(&mut self, link: crate::app::reef::ReefLink, cx: &mut Context<Self>) {
        let (wake, woken) = flume::bounded(1);
        self.reef = Some(ReefStatus {
            url: link.url.clone(),
            error: None,
            synced_at: None,
            link: link.clone(),
            routing: None,
            routing_error: None,
            wake: wake.clone(),
        });
        // The poll: a wake every EVERY, until the loop below is gone.
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            loop {
                executor.timer(crate::app::reef::EVERY).await;
                if wake.send_async(()).await.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                let ask = link.clone();
                let (answer, settings) = cx
                    .background_spawn(async move { (ask.fetch(), ask.settings(None).ok()) })
                    .await;
                let alive = this.update(cx, |store, cx| {
                    let status = store.reef.as_mut().expect("reef mode");
                    if let Some(on) = settings.as_ref().and_then(|s| s["agent_routing"].as_bool()) {
                        status.routing = Some(on);
                    }
                    match answer {
                        Ok((hosts, labels)) => {
                            status.error = None;
                            status.synced_at = Some(now_millis());
                            let current: Vec<HostConfig> = (store.fleet.hosts.iter())
                                .map(|h| h.config.clone())
                                .collect();
                            store.set_catalog(labels, cx);
                            if hosts != current {
                                store.set_hosts(hosts, cx);
                            } else {
                                cx.notify();
                            }
                        }
                        Err(error) => {
                            status.error = Some(error);
                            cx.notify();
                        }
                    }
                });
                if alive.is_err() {
                    break;
                }
                // The next poll, or sooner when an agent names a new label.
                if woken.recv_async().await.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Every saved host's id and name, in the user's order.
    pub fn host_labels(&self) -> Vec<(HostId, String)> {
        (self.fleet.hosts.iter())
            .map(|h| (h.id().clone(), h.name().to_string()))
            .collect()
    }

    fn connect(&mut self, name: &HostId, cx: &mut Context<Self>) {
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
        let watched: BTreeSet<String> = self
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
            name.clone(),
            HostLink {
                client,
                sync: HostSync::new(generation).watching(watched),
                open_terminals,
                terminal_slots: HashMap::new(),
                early_frames: Vec::new(),
            },
        );
        let name = name.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let alive = this
                    .update(cx, |store, cx| {
                        if store
                            .links
                            .get(&name)
                            .is_some_and(|l| l.sync.accepts(generation))
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
    /// Saves hosts as the fleet; one without an id gets one first.
    pub fn set_hosts(&mut self, mut hosts: Vec<HostConfig>, cx: &mut Context<Self>) {
        fleet::assign_ids(&mut hosts);
        crate::app::config::save_hosts(&hosts);
        let old: HashMap<HostId, HostConfig> = self
            .fleet
            .hosts
            .iter()
            .map(|h| (h.id().clone(), h.config.clone()))
            .collect();
        let mut next = Fleet::new(hosts.clone());
        for host in &mut next.hosts {
            if let Some(previous) = self.fleet.host(host.id())
                && previous.config == host.config
            {
                *host = previous.clone();
            }
        }
        self.fleet = next;
        for config in &hosts {
            if old.get(&config.id) != Some(config) {
                self.links.remove(&config.id);
                self.connect(&config.id, cx);
            }
        }
        self.links.retain(|id, _| hosts.iter().any(|h| &h.id == id));
        self.changed(cx);
    }

    pub fn reconnect(&mut self, host: &HostId) {
        if let Some(link) = self.links.get(host) {
            link.client.reconnect();
        }
    }

    /// The agents mid-turn on `host`, which a restart interrupts.
    pub fn running_on(&self, host: &HostId) -> usize {
        (self.fleet.host(host).into_iter())
            .flat_map(|h| h.directory.agents.values())
            .filter(|a| a.archived_at.is_none() && a.bucket() == fleet::Bucket::Running)
            .count()
    }

    /// Asks `host`'s daemon to restart (`restart_server_request`). The link
    /// drops while it does and reconnects once it listens again.
    pub fn restart_host(&mut self, host: &HostId, cx: &mut Context<Self>) {
        let name = self.fleet.host_name(host).to_string();
        let message = serde_json::json!({"type": "restart_server_request", "reason": "crusteo"});
        let task = self.write(host, message, cx);
        cx.spawn(async move |this, cx| {
            let said = match task.await {
                Ok(_) => format!("Restarting {name}. Crusteo reconnects once it is back."),
                Err(error) => format!("{name} did not restart: {error}"),
            };
            let _ = this.update(cx, |store, cx| store.toast(said, cx));
        })
        .detach();
    }

    /// Feeds synthetic pushes through `on_push`, the same path daemon pushes
    /// take, every 10ms for as long as the store lives.
    fn start_stress(&mut self, stress: crate::app::stress::Stress, cx: &mut Context<Self>) {
        let mut churn = stress.churn();
        self.seed_stress_shells();
        // The synthetic hosts' CLIs, and a few upgrading a while after start.
        let hosts: Vec<HostId> = self.fleet.hosts.iter().map(|h| h.id().clone()).collect();
        self.cli_watch.debounce = std::time::Duration::from_secs(3);
        for host in &hosts {
            self.ensure_providers(host, cx);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(20))
                .await;
            crate::app::stress::upgrade_hosts(3);
            for host in hosts.iter().take(3) {
                let push = serde_json::json!({"type": "providers_snapshot_update",
                    "payload": {"entries": [], "snapshotHash": "upgraded"}});
                let _ = this.update(cx, |store, cx| store.on_push(host, push, cx));
            }
        })
        .detach();
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
                        let owed = std::mem::take(&mut *store.stress_pushes.borrow_mut());
                        for (host, message) in owed.into_iter().chain(pushes) {
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
            let id = host.config.id.clone();
            for change in host.directory.journal.drain(..) {
                self.change_log.push_back(FleetChange {
                    host: id.clone(),
                    change,
                });
            }
        }
        if bulk_if_quiet && self.change_log.len() == before {
            // Nothing a directory journaled: be safe and say "everything".
            self.change_log.push_back(FleetChange {
                host: HostId::default(),
                change: fleet::DirChange::Bulk,
            });
        }
        self.ask_reef_for_new_labels(before);
        if self.change_log.len() > CHANGE_LOG_LIMIT {
            let dropped = self.change_log.len() - CHANGE_LOG_LIMIT / 2;
            self.change_log.drain(..dropped);
            self.change_base += dropped as u64;
        }
        cx.emit(FleetEvent::Changed);
        cx.notify();
    }

    /// Reef mode: when an agent changed since `from` (a change-log index)
    /// names a label the catalog has never had, such as one just made on a
    /// phone, asks Reef for the catalog now rather than at the next poll.
    fn ask_reef_for_new_labels(&self, from: usize) {
        let (Some(reef), Some(catalog)) = (&self.reef, &self.fleet.labels) else {
            return;
        };
        let unknown = (self.change_log.iter().skip(from)).any(|entry| {
            let (fleet::DirChange::AgentAdded(agent)
            | fleet::DirChange::AgentUpdated(agent)
            | fleet::DirChange::AgentMoved(agent)) = &entry.change
            else {
                return false;
            };
            let Some(found) = self.fleet.agent(&entry.host, agent) else {
                return false;
            };
            match fleet::labels::assignment(&found.labels) {
                fleet::labels::Assignment::Labels(ids) => ids
                    .iter()
                    .any(|id| catalog.get(id).is_none() && !catalog.tombstones.contains(id)),
                _ => false,
            }
        });
        if unknown {
            // One wake is enough however many ask.
            let _ = reef.wake.try_send(());
        }
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

    fn on_event(&mut self, name: &HostId, event: Event, cx: &mut Context<Self>) {
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
                    // What it ran while away is not known until bootstrap
                    // has listed every agent again (workspaces come first),
                    // so destructive actions wait for that.
                    host.directory.agents_hydrated = false;
                }
                let epoch = match self.links.get_mut(name) {
                    Some(link) => {
                        let epoch = link.sync.epoch() + 1;
                        link.sync.connected(epoch);
                        epoch
                    }
                    None => return,
                };
                // A daemon that reconnects may have restarted and probed its
                // providers again: what it offered before is no longer known.
                self.forget_providers(name);
                if self.providers_loading.contains(name) {
                    self.providers_stale.insert(name.clone());
                }
                self.bootstrap(name, epoch, cx);
                // Always read again: the CLIs on the host are watched for
                // upgrades and new models, not only read for a draft.
                self.ensure_providers(name, cx);
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
                    link.sync.disconnected();
                    link.terminal_slots.clear();
                    link.early_frames.clear();
                }
                self.mark_terminal_lists_stale(name);
                self.mark_terminals_offline(name, cx);
                self.changed(cx);
            }
            Event::Message(message) => self.on_push(name, message, cx),
            Event::Binary(bytes) => self.on_terminal_frame(name, &bytes, cx),
        }
    }

    fn on_push(&mut self, name: &HostId, message: Value, cx: &mut Context<Self>) {
        let kind = message["type"].as_str().unwrap_or_default().to_string();
        let payload = &message["payload"];
        match kind.as_str() {
            "agent_stream" => {
                let agent_id = payload["agentId"].as_str().unwrap_or_default();
                if let Some(session) = self
                    .sessions
                    .get(&(name.clone(), agent_id.to_string()))
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
                    .get(&(name.clone(), agent_id.to_string()))
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
                    host: name.clone(),
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
            "providers_snapshot_update" => self.on_providers_update(name, payload, cx),
            _ => {
                let message = match self.links.get_mut(name) {
                    Some(link) => match link.sync.hold(message) {
                        Some(message) => message,
                        None => return,
                    },
                    None => message,
                };
                let payload = &message["payload"];
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
    fn on_agent_snapshot(&mut self, host: &HostId, agent: &Value, cx: &mut Context<Self>) {
        let Some(agent_id) = agent["id"].as_str() else {
            return;
        };
        let Some(session) = self
            .sessions
            .get(&(host.clone(), agent_id.to_string()))
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
    fn is_current(&self, name: &HostId, epoch: u64) -> bool {
        self.links
            .get(name)
            .is_some_and(|link| link.sync.is_current(epoch))
    }

    /// Reads the host in [`Bootstrap`]'s order, then replays pushes that
    /// arrived meanwhile. Work from a superseded connection is dropped; a
    /// failed step reconnects after a pause so it is retried.
    fn bootstrap(&mut self, name: &HostId, epoch: u64, cx: &mut Context<Self>) {
        let Some(link) = self.links.get(name) else {
            return;
        };
        let Some(host) = self.fleet.host(name) else {
            return;
        };
        let client = link.client.clone();
        let mut plan = Bootstrap::for_host(host, true);
        let name = name.clone();

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
            loop {
                match plan.step() {
                    Step::Done => break,
                    Step::ProjectsFromWorkspaces => {
                        apply!(|store: &mut FleetStore, _cx: &mut Context<FleetStore>| {
                            if let Some(host) = store.fleet.host_mut(&name) {
                                host.directory.projects_from_workspaces = true;
                            }
                        });
                    }
                    Step::Send(request) => {
                        let result = client.request(request).await;
                        if let Err(err) = &result {
                            log_error(&name, plan.what(), err);
                        }
                        let result = result.map_err(|err| err.to_string());
                        let Some(loaded) = plan.reply(result) else {
                            continue;
                        };
                        apply!(|store: &mut FleetStore, cx: &mut Context<FleetStore>| {
                            if let Some(host) = store.fleet.host_mut(&name) {
                                loaded.apply(&mut host.directory);
                            }
                            for agent in loaded.agent_snapshots() {
                                store.on_agent_snapshot(&name, agent, cx);
                            }
                            // The events subscription shows nowhere.
                            if !matches!(loaded, Loaded::Events { .. }) {
                                store.changed(cx);
                            }
                        });
                    }
                }
            }

            let complete = plan.agents_complete();
            apply!(|store: &mut FleetStore, cx: &mut Context<FleetStore>| {
                let held = match (store.links.get_mut(&name), store.fleet.host_mut(&name)) {
                    (Some(link), Some(host)) => {
                        link.sync.finish_bootstrap(&mut host.directory, complete)
                    }
                    _ => Vec::new(),
                };
                for message in held {
                    store.on_push(&name, message, cx);
                }
                store.changed(cx);
            });

            if plan.failed() {
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
    pub fn request(&self, host: &HostId, message: Value, cx: &App) -> Task<Result<Value, String>> {
        self.request_with(host, message, paseo_client::DEFAULT_TIMEOUT, cx)
    }

    /// A request that changes something on the daemon, with a longer wait.
    pub fn write(&self, host: &HostId, message: Value, cx: &App) -> Task<Result<Value, String>> {
        self.request_with(host, message, WRITE_TIMEOUT, cx)
    }

    pub fn toast(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        cx.emit(FleetEvent::Toast(message.into()));
    }

    /// A request `host` answers in several messages, the last of type
    /// `until`: the earlier messages' payloads, and the last's.
    pub fn request_until(
        &self,
        host: &HostId,
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
            None => Task::ready(Err(self.not_connected(host))),
        }
    }

    /// Why a request to `host` cannot go: it has no connection.
    fn not_connected(&self, host: &HostId) -> String {
        format!("{} is not connected", self.fleet.host_name(host))
    }

    fn request_with(
        &self,
        host: &HostId,
        message: Value,
        timeout: std::time::Duration,
        cx: &App,
    ) -> Task<Result<Value, String>> {
        if self.stress {
            // A synthetic daemon answers what the new-workspace flow asks,
            // after a moment, as if over the network.
            let Some((reply, pushes)) = crate::app::stress::answer(host, &message, &self.fleet)
            else {
                return Task::ready(Err(self.not_connected(host)));
            };
            self.stress_pushes.borrow_mut().extend(pushes);
            let timer = cx
                .background_executor()
                .timer(std::time::Duration::from_millis(150));
            return cx.background_spawn(async move {
                timer.await;
                reply
            });
        }
        match self.links.get(host) {
            Some(link) => {
                let client = link.client.clone();
                cx.background_spawn(async move { send_request(&client, message, timeout).await })
            }
            None => Task::ready(Err(self.not_connected(host))),
        }
    }

    /// Fire a request and toast its failure; replies arrive as pushes anyway.
    pub fn act(&self, host: &HostId, message: Value, cx: &mut Context<Self>) {
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
    pub fn send_raw(&self, host: &HostId, message: Value) {
        if let Some(link) = self.links.get(host) {
            link.client.send(message);
        }
    }
}

fn log_error(host: &HostId, what: &str, err: &paseo_client::Error) {
    eprintln!("crusteo: {host}: loading {what} failed: {err}");
}

// The clock helpers moved to `fleet` so the headless hub shares them;
// views keep reaching them through the store.
pub use fleet::clock::{iso_from_millis, now_iso, now_millis, parse_iso, time_ago, uuid_v4};
