//! The daemons' side: one connection per host, the merged directory, and
//! the conversations phones have open. This is crusteo's `FleetStore`
//! without GPUI: the same bootstrap order, the same held-push replay, the
//! same timeline catch-up and seq gate (those live in `fleet`).
//!
//! Every request goes through [`guarded`] (`guard.rs`), Reef's allowlist.
//! The state machines it shares with crusteo are `fleet::sync`'s.

mod actions;
mod agents;
mod guard;
mod recording;
mod replies;
mod routing;

pub use actions::{QUEUE_MAX, Queued, Reply};
pub use agents::NewAgent;
pub use guard::{
    NAME_MAX, READ_REQUESTS, WRITE_REQUESTS, guarded, guarded_until, guarded_with_timeout,
    labels_write, name_write,
};

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fleet::sync::{Bootstrap, Fetch, HostSync, Loaded, Page, Step};
use fleet::timeline::{Follow, Timeline};
use fleet::{Connection, Fleet, HostConfig, requests};
use paseo_client::{Client, Config, ConnState, Event};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::host_id;

/// Work to do on the upstream state, sent back by request tasks.
pub type Apply = Box<dyn FnOnce(&mut Upstream) + Send>;

/// An event from one host's connection: host, link generation, event.
pub type HostEvent = (String, u64, Event);

struct Link {
    client: Client,
    /// A fresh value, from one counter for every link, on each connect,
    /// disconnect and removal. Bootstrap work from an older connection
    /// stops at its next step, even when the host was removed and added
    /// again under the same name. The task reads it without a round trip
    /// through Reef; `sync` holds the same value for the work Reef applies.
    epoch: Arc<AtomicU64>,
    /// Link generation, epoch, held pushes and timeline subscription.
    sync: HostSync,
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
    fetch: Fetch,
    viewers: usize,
    idle_since: Option<Instant>,
}

impl Session {
    pub fn fetching(&self) -> bool {
        self.fetch.in_flight()
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
    /// What each host's coding-agent CLIs and models were, to tell when they
    /// change ([fleet::cli_watch]).
    pub cli: fleet::cli_watch::Watch,
    /// The fleet's recorder ([`crate::record`]), idle until configured.
    pub record: crate::record::Recorder,
    /// Whether Reef routes prompts between hosts, and the routes under way.
    routes: routing::Routes,
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
            record: crate::record::Recorder::new(None, apply.clone()),
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
            cli: fleet::cli_watch::Watch::default(),
            routes: routing::Routes::default(),
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
            let Some(link) = self.links.get(&name).filter(|link| link.sync.online()) else {
                continue;
            };
            self.usage_asked.insert(name.clone(), Instant::now());
            let generation = link.sync.generation();
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
                        .is_some_and(|l| l.sync.accepts(generation))
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
                        .is_some_and(|link| link.sync.settled())
            })
    }

    /// Whether every host connected now has its whole agent list. Hosts that
    /// are down do not count: one that is asleep will not answer in time.
    pub fn connected_synced(&self) -> bool {
        self.fleet.hosts.iter().all(|host| {
            (self.links.get(host.name()))
                .is_none_or(|link| link.sync.settled() && host.directory.agents_hydrated)
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
        self.cli.forget_host(name);
        self.set_usage(&crate::usage::host_key(name), None);
        self.forget_host(name);
        self.record.forget_host(name);
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
                epoch: Arc::new(AtomicU64::new(epoch)),
                sync: HostSync::new(generation),
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
            .is_some_and(|link| link.sync.accepts(generation))
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
                link.sync.connected(epoch);
                // The daemon may have restarted with other providers.
                self.forget_providers(name);
                self.watch_providers(name);
                self.bootstrap(name, epoch);
                self.resubscribe_timelines(name, true);
                self.ask_usage();
                self.announce_routing(name);
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
                    link.sync.disconnected();
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
                // Recording reads it soon, and at once when a turn ends.
                let ended = matches!(
                    payload["event"]["type"].as_str(),
                    Some("turn_completed" | "turn_failed" | "turn_canceled")
                );
                self.record.touch(key.clone(), ended);
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
            "providers_snapshot_update" => {
                self.forget_providers(name);
                self.watch_providers(name);
            }
            // A Burrow asks Reef to route a prompt to another host.
            kind if kind == paseo_client::routing::REQUEST => {
                let payload = payload.clone();
                self.on_fleet_request(name, &payload);
            }
            _ => {
                let message = match self.links.get_mut(name) {
                    Some(link) => match link.sync.hold(message) {
                        Some(message) => message,
                        None => return,
                    },
                    None => message,
                };
                let payload = &message["payload"];
                let agent_id = payload["agent"]["id"]
                    .as_str()
                    .or(payload["agentId"].as_str())
                    .unwrap_or_default()
                    .to_string();
                let was_running = (kind == "agent_update").then(|| self.running(name, &agent_id));
                let known = self.fleet.agent(&host_id(name), &agent_id).is_some();
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
                        let key = (name.to_string(), agent_id.clone());
                        self.record.forget(&key);
                        // The daemon says the same of both, so recording
                        // asks it which: only a deleted agent is forgotten.
                        self.record.suspect(key, true);
                    } else {
                        let ended = was && !self.running(name, &agent_id);
                        self.record
                            .touch((name.to_string(), agent_id.clone()), ended);
                        // A new agent is followed from its first turn.
                        if !known && self.record.following() {
                            self.resubscribe_timelines(name, false);
                        }
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
            .is_some_and(|link| link.sync.is_current(epoch))
    }

    /// Reads the host in [`Bootstrap`]'s order, then replays the pushes
    /// that arrived meanwhile. A failed step reconnects after a pause, so
    /// it is retried.
    fn bootstrap(&mut self, name: &str, epoch: u64) {
        let (Some(link), Some(host)) = (self.links.get(name), self.fleet.host(&host_id(name)))
        else {
            return;
        };
        let client = link.client.clone();
        let current = link.epoch.clone();
        // Reef streams no provider subagents.
        let mut plan = Bootstrap::for_host(host, false);
        let apply = self.apply.clone();
        let name = name.to_string();

        tokio::spawn(async move {
            let live = || current.load(Ordering::SeqCst) == epoch;
            // Applies a step's result only if this connection is still live.
            let send = |f: Apply| {
                let _ = apply.send(f);
            };
            while live() {
                match plan.step() {
                    Step::Done => break,
                    Step::ProjectsFromWorkspaces => {
                        let name = name.clone();
                        send(Box::new(move |up| {
                            if up.is_current(&name, epoch)
                                && let Some(host) = up.fleet.host_mut(&host_id(&name))
                            {
                                host.directory.projects_from_workspaces = true;
                            }
                        }));
                    }
                    Step::Send(request) => {
                        let result = guarded(&client, request).await;
                        if let Err(err) = &result {
                            log(&name, plan.what(), err);
                        }
                        let Some(loaded) = plan.reply(result) else {
                            continue;
                        };
                        let name = name.clone();
                        send(Box::new(move |up| {
                            if !up.is_current(&name, epoch) {
                                return;
                            }
                            // Turns that ended while Reef was away count as
                            // ended now.
                            let before = matches!(loaded, Loaded::Agents { .. })
                                .then(|| up.running_on(&name));
                            if let Some(host) = up.fleet.host_mut(&host_id(&name)) {
                                loaded.apply(&mut host.directory);
                            }
                            for agent in loaded.agent_snapshots() {
                                up.on_agent_snapshot(&name, agent);
                            }
                            if let Some(before) = before {
                                up.turn_edges(&name, &before);
                            }
                            // The events subscription shows nowhere.
                            up.dirty_fleet |= !matches!(loaded, Loaded::Events { .. });
                        }));
                    }
                }
            }

            let complete = plan.agents_complete();
            let failed = plan.failed();
            {
                let name = name.clone();
                send(Box::new(move |up| {
                    if !up.is_current(&name, epoch) {
                        return;
                    }
                    let held = match (up.links.get_mut(&name), up.fleet.host_mut(&host_id(&name))) {
                        (Some(link), Some(host)) => {
                            host.directory.journal.clear();
                            link.sync.finish_bootstrap(&mut host.directory, complete)
                        }
                        _ => Vec::new(),
                    };
                    for message in held {
                        up.on_push(&name, message);
                    }
                    // Recording follows the agents now listed.
                    if up.record.following() {
                        up.resubscribe_timelines(&name, false);
                        up.record_host_ready(&name);
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
            link.sync.watched.insert(agent.to_string());
        }
        self.resubscribe_timelines(host, false);
        self.fetch(host, agent, Page::Tail);
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
                link.sync.watched.remove(&agent);
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
        let Some(page) = Page::older(&session.timeline, fleet::sync::PAGE) else {
            return false;
        };
        if session.fetch.in_flight() {
            return false;
        }
        self.fetch(host, agent, page);
        true
    }

    fn resubscribe_timelines(&mut self, host: &str, catch_up: bool) {
        let Some(link) = self.links.get(host) else {
            return;
        };
        if !link.sync.online() {
            return;
        }
        let epoch = link.epoch.load(Ordering::SeqCst);
        let agents = self.timeline_agents(host);
        let reply = guarded(
            &link.client,
            HostSync::timeline_subscription_request(&agents),
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
                if let Some(previous) = link.sync.swap_timeline_subscription(id) {
                    tokio::spawn(guarded(&link.client, requests::release(&previous)));
                }
                if catch_up {
                    let agents: Vec<String> = link.sync.watched.iter().cloned().collect();
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
        let page = Page::catch_up(&session.timeline);
        self.fetch(host, agent, page);
    }

    fn follow(&mut self, host: &str, agent: &str, follow: Follow) {
        if let Some(page) = Page::following(follow) {
            self.fetch(host, agent, page);
        }
    }

    /// Reads an image `host` keeps in `file`, as crusteo does; `None` when
    /// the host is offline. The caller finds `file` in a conversation, so
    /// only files a prompt lists or a reply names are ever read.
    pub fn read_image(
        &self,
        host: &str,
        file: &fleet::images::ImageFile,
    ) -> Option<
        impl std::future::Future<Output = Result<fleet::prompt::ImageAttachment, String>> + Send + use<>,
    > {
        let link = self.links.get(host).filter(|link| link.sync.online())?;
        // Boxed: `guarded`'s future is 'static, but its type names the link.
        let reply: std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>> =
            Box::pin(guarded(&link.client, file.request()));
        Some(async move { fleet::images::image_from_reply(&reply.await?) })
    }

    /// Reads `page` of an open conversation ([`Fetch`] allows one read at a
    /// time).
    fn fetch(&mut self, host: &str, agent: &str, page: Page) {
        let key = (host.to_string(), agent.to_string());
        let Some(session) = self.sessions.get_mut(&key) else {
            return;
        };
        if !session.fetch.begin(&page) {
            return;
        }
        let Some(link) = self.links.get(host).filter(|link| link.sync.online()) else {
            session.fetch.abandon();
            session.loading = false;
            session.error = Some("The host is offline".into());
            self.dirty_sessions.insert(key);
            return;
        };
        let reply = guarded(&link.client, page.request(agent));
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
        session.pages += 1;
        let landed = session.fetch.land(&mut session.timeline, &result);
        session.error = landed.error;
        self.dirty_sessions.insert(key.clone());
        if let Some(page) = landed.next {
            self.fetch(&key.0, &key.1, page);
        }
    }
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
        up.links.get_mut("example-a").unwrap().sync.connected(old);
        let held = up.links["example-a"].epoch.clone();
        assert!(up.is_current("example-a", old));
        // Removed: that work sees it is stale at once.
        assert!(up.remove_host("example-a"));
        assert_ne!(held.load(Ordering::SeqCst), old);
        // Added again and connected: the old epoch still does not match.
        assert!(up.add_host(host()));
        let new = up.bump_epoch("example-a").unwrap();
        up.links.get_mut("example-a").unwrap().sync.connected(new);
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
}
