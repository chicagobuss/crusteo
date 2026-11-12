//! The state machines between a daemon connection and [`Directory`]: what
//! Crusteo's `FleetStore` and Reef's `Upstream` both do, once, with no
//! socket, runtime or GPUI. Each adapter sends the requests these hand it,
//! feeds the replies back, and applies what comes out.
//!
//! - [`HostSync`]: one host's link. Which connection events still count
//!   (link generation), which bootstrap work still applies (epoch), and the
//!   live pushes held while the bootstrap replies are in flight, so a
//!   snapshot never erases a newer push.
//! - [`Bootstrap`]: the fixed order a host is read in (`session.events`,
//!   projects, workspaces, agents, labels), page by page.
//! - [`Page`] and [`Fetch`]: one conversation's timeline reads, one at a
//!   time, with a reload asked for meanwhile run when the current one lands.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::Host;
use crate::directory::Directory;
use crate::requests::{self, DirectoryRead};
use crate::timeline::{Follow, Timeline};

// ---- one host's link ------------------------------------------------------

/// One host's connection as the store sees it.
#[derive(Debug, Default)]
pub struct HostSync {
    /// Which `connect` made this link. Events from a replaced client are
    /// dropped by comparing against it ([`Self::accepts`]).
    generation: u64,
    /// A fresh value on every (re)connect; bootstrap work started under an
    /// older one is discarded ([`Self::is_current`]).
    epoch: u64,
    online: bool,
    /// Directory pushes that arrive while bootstrap replies are still in
    /// flight are held, and replayed after them.
    bootstrapping: bool,
    held: Vec<Value>,
    timeline_subscription: Option<String>,
    /// Agents whose timelines we want streamed on this host.
    pub watched: BTreeSet<String>,
}

impl HostSync {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            ..Self::default()
        }
    }

    /// The same link, streaming the timelines of `agents`.
    pub fn watching(mut self, agents: BTreeSet<String>) -> Self {
        self.watched = agents;
        self
    }

    /// Whether an event from link `generation` is from this link.
    pub fn accepts(&self, generation: u64) -> bool {
        self.generation == generation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn online(&self) -> bool {
        self.online
    }

    /// The epoch of the latest connection, kept after it drops.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Connected and past bootstrap: the directory is whole.
    pub fn settled(&self) -> bool {
        self.online && !self.bootstrapping
    }

    /// Whether work started for connection `epoch` still applies.
    pub fn is_current(&self, epoch: u64) -> bool {
        self.online && self.epoch == epoch
    }

    /// The socket is up under a new `epoch`: bootstrap begins, what was held
    /// for an earlier connection goes, and the daemon's subscriptions are
    /// new.
    pub fn connected(&mut self, epoch: u64) {
        self.online = true;
        self.epoch = epoch;
        self.timeline_subscription = None;
        self.bootstrapping = true;
        self.held.clear();
    }

    /// The socket is down; its subscriptions went with it.
    pub fn disconnected(&mut self) {
        self.online = false;
        self.timeline_subscription = None;
    }

    /// A directory push: held and `None` while bootstrapping, else handed
    /// back to apply now.
    pub fn hold(&mut self, message: Value) -> Option<Value> {
        if self.bootstrapping {
            self.held.push(message);
            None
        } else {
            Some(message)
        }
    }

    /// Bootstrap is over. The agent list is whole if `agents_complete`; the
    /// pushes held meanwhile, oldest first, are for the caller to apply now
    /// that the snapshots they must not precede have landed.
    pub fn finish_bootstrap(&mut self, dir: &mut Directory, agents_complete: bool) -> Vec<Value> {
        dir.agents_hydrated = agents_complete;
        self.bootstrapping = false;
        std::mem::take(&mut self.held)
    }

    /// The request that makes the daemon stream the timelines of `agents`
    /// (and only those) on this connection.
    pub fn timeline_subscription_request<'a>(
        agents: impl IntoIterator<Item = &'a String>,
    ) -> Value {
        let agents: Vec<&String> = agents.into_iter().collect();
        json!({ "type": "agent.timeline.set_subscription.request", "agentIds": agents })
    }

    /// Records the subscription the daemon just made. Each request makes a
    /// new one, so the one it replaces is returned, to release, or frames
    /// would come twice.
    pub fn swap_timeline_subscription(&mut self, id: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.timeline_subscription, id)
    }
}

// ---- bootstrap ------------------------------------------------------------

/// The pushes a host is subscribed to before its lists are read.
const SESSION_EVENTS: [&str; 6] = [
    "project.update",
    "script_status_update",
    "agent_permission_request",
    "agent_permission_resolved",
    "agent_attention_required",
    // Every Paseo since 0.1.48 sends it when a provider probe lands, and
    // burrow does too.
    "providers_snapshot_update",
];

const SUBAGENTS_EVENT: &str = "agent.provider_subagents.update";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Events,
    Projects,
    Workspaces,
    Agents,
    Labels,
    Done,
}

/// What [`Bootstrap::step`] wants next.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Send this request and give the result to [`Bootstrap::reply`].
    Send(Value),
    /// The daemon cannot list projects: the directory makes them from the
    /// workspaces. Apply that, then ask for the next step.
    ProjectsFromWorkspaces,
    Done,
}

/// A bootstrap reply, ready for the directory.
#[derive(Debug, Clone, PartialEq)]
pub enum Loaded {
    Events { subscription: Option<String> },
    Projects(Value),
    Workspaces { payload: Value, first: bool },
    Agents { payload: Value, first: bool },
    Labels(Value),
}

impl Loaded {
    /// Puts the reply in `dir`.
    pub fn apply(&self, dir: &mut Directory) {
        match self {
            Loaded::Events { subscription } => dir.subscriptions.events = subscription.clone(),
            Loaded::Projects(payload) => {
                dir.apply_projects_reply(payload);
            }
            Loaded::Workspaces { payload, first } => {
                dir.apply_workspaces_reply(payload, *first);
            }
            Loaded::Agents { payload, first } => {
                dir.apply_agents_reply(payload, *first);
            }
            Loaded::Labels(payload) => {
                dir.apply_labels_reply(payload);
            }
        }
    }

    /// The agent snapshots in an agents page, for whatever keeps an open
    /// conversation current from them.
    pub fn agent_snapshots(&self) -> impl Iterator<Item = &Value> {
        let entries = match self {
            Loaded::Agents { payload, .. } => payload["entries"].as_array(),
            _ => None,
        };
        entries.into_iter().flatten().map(|entry| &entry["agent"])
    }
}

/// The order a host is read in, one request at a time: `session.events`,
/// projects, workspaces and agents (every page when the daemon does not
/// sync), then labels. A failed step is noted ([`Self::failed`]) and the
/// rest still run; the caller reconnects after a pause so all of it is
/// retried.
#[derive(Debug)]
pub struct Bootstrap {
    read: DirectoryRead,
    events: Vec<&'static str>,
    project_list: bool,
    labels: bool,
    workspace_cursor: Option<paseo_client::proto::Cursor>,
    agent_cursor: Option<paseo_client::proto::Cursor>,
    project_cursor: Option<paseo_client::proto::Cursor>,
    stage: Stage,
    /// The next request is the first of its list.
    first: bool,
    /// The next page to ask for in a paged list.
    next_page: Option<String>,
    failed: bool,
    agents_complete: bool,
}

impl Bootstrap {
    /// Reads `host` from where its directory left off. `subagents` asks to
    /// hear provider subagent updates too, from a host that sends them.
    pub fn for_host(host: &Host, subagents: bool) -> Self {
        let mut events = SESSION_EVENTS.to_vec();
        if subagents && host.has("providerSubagents") {
            events.push(SUBAGENTS_EVENT);
        }
        let dir = &host.directory;
        Self {
            read: DirectoryRead::for_host(host),
            events,
            project_list: host.has("projectList"),
            labels: host.has("workspaceLabels"),
            workspace_cursor: dir.workspace_cursor.clone(),
            agent_cursor: dir.agent_cursor.clone(),
            project_cursor: dir.project_cursor.clone(),
            stage: Stage::Events,
            first: true,
            next_page: None,
            failed: false,
            agents_complete: false,
        }
    }

    /// What to do next. Asked again after each [`Self::reply`] and after a
    /// [`Step::ProjectsFromWorkspaces`].
    pub fn step(&mut self) -> Step {
        loop {
            match self.stage {
                Stage::Events => {
                    return Step::Send(requests::session_events(&self.events));
                }
                Stage::Projects if self.project_list => {
                    let sync = self.read.sync;
                    return Step::Send(requests::list_projects(sync, self.project_cursor.as_ref()));
                }
                Stage::Projects => {
                    self.advance(Stage::Workspaces);
                    return Step::ProjectsFromWorkspaces;
                }
                Stage::Workspaces => {
                    let request = match self.next_page.as_deref() {
                        Some(cursor) => requests::fetch_workspaces_page(self.read, cursor),
                        None => {
                            requests::fetch_workspaces(self.read, self.workspace_cursor.as_ref())
                        }
                    };
                    return Step::Send(request);
                }
                Stage::Agents => {
                    let request = match self.next_page.as_deref() {
                        Some(cursor) => requests::fetch_agents_page(self.read, cursor),
                        None => requests::fetch_agents(self.read, self.agent_cursor.as_ref()),
                    };
                    return Step::Send(request);
                }
                Stage::Labels if self.labels => {
                    return Step::Send(requests::list_labels());
                }
                Stage::Labels => self.advance(Stage::Done),
                Stage::Done => return Step::Done,
            }
        }
    }

    /// What was being read, for a log line when its request failed.
    pub fn what(&self) -> &'static str {
        match self.stage {
            Stage::Events => "session events",
            Stage::Projects => "projects",
            Stage::Workspaces => "workspaces",
            Stage::Agents => "agents",
            Stage::Labels | Stage::Done => "labels",
        }
    }

    /// The result of the request [`Self::step`] gave. `Some` is a reply for
    /// the directory; `None` means the step failed.
    pub fn reply(&mut self, result: Result<Value, String>) -> Option<Loaded> {
        let first = self.first;
        let stage = self.stage;
        let payload = match result {
            Ok(payload) => payload,
            Err(_) => {
                // Labels are a nicety: their failure does not reconnect.
                self.failed |= stage != Stage::Labels;
                self.fail(stage);
                return None;
            }
        };
        Some(match stage {
            Stage::Events => {
                self.advance(Stage::Projects);
                Loaded::Events {
                    subscription: payload["subscriptionId"].as_str().map(str::to_string),
                }
            }
            Stage::Projects => {
                self.advance(Stage::Workspaces);
                Loaded::Projects(payload)
            }
            Stage::Workspaces => {
                self.page(&payload, Stage::Agents);
                Loaded::Workspaces { payload, first }
            }
            Stage::Agents => {
                self.page(&payload, Stage::Labels);
                self.agents_complete = self.stage == Stage::Labels;
                Loaded::Agents { payload, first }
            }
            Stage::Labels => {
                self.advance(Stage::Done);
                Loaded::Labels(payload)
            }
            Stage::Done => return None,
        })
    }

    /// A list continues at its next page, or ends and `then` begins.
    fn page(&mut self, payload: &Value, then: Stage) {
        let more =
            payload["pageInfo"]["hasMore"].as_bool().unwrap_or(false) && payload["sync"].is_null();
        let cursor = payload["pageInfo"]["nextCursor"]
            .as_str()
            .map(str::to_string)
            .filter(|_| more);
        match cursor {
            Some(cursor) => {
                self.next_page = Some(cursor);
                self.first = false;
            }
            None => self.advance(then),
        }
    }

    fn advance(&mut self, stage: Stage) {
        self.stage = stage;
        self.first = true;
        self.next_page = None;
    }

    /// A request failed. The lists end where they are; the single reads
    /// pass to the next.
    fn fail(&mut self, stage: Stage) {
        self.advance(match stage {
            Stage::Events => Stage::Projects,
            Stage::Projects => Stage::Workspaces,
            Stage::Workspaces => Stage::Agents,
            Stage::Agents => Stage::Labels,
            Stage::Labels | Stage::Done => Stage::Done,
        });
    }

    /// Some request failed; the host should be asked again from the start.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Every page of the agent list landed.
    pub fn agents_complete(&self) -> bool {
        self.agents_complete
    }
}

// ---- one conversation's timeline reads ------------------------------------

/// A conversation timeline page, at most this many entries.
pub const PAGE: u32 = 40;

/// A read of an agent's timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    /// The newest entries; replaces what is held.
    Tail,
    /// What follows the cursor: a live gap, or more pages owed.
    After { epoch: String, seq: u64 },
    /// What precedes the cursor, for scrolling up.
    Before { epoch: String, seq: u64, limit: u32 },
}

impl Page {
    /// The read that follows what the timeline asked for, if any.
    pub fn following(follow: Follow) -> Option<Page> {
        match follow {
            Follow::Nothing => None,
            Follow::Reload => Some(Page::Tail),
            Follow::CatchUp { epoch, seq } => Some(Page::After { epoch, seq }),
        }
    }

    /// The read that brings a timeline up to date after a gap in its
    /// stream: from its end when it has a baseline, else its tail.
    pub fn catch_up(timeline: &Timeline) -> Page {
        match &timeline.cursor {
            Some(cursor) if timeline.baseline => Page::After {
                epoch: cursor.epoch.clone(),
                seq: cursor.end_seq,
            },
            _ => Page::Tail,
        }
    }

    /// The read of the page before what the timeline holds, if there is one.
    pub fn older(timeline: &Timeline, limit: u32) -> Option<Page> {
        let (Some(cursor), true) = (&timeline.cursor, timeline.has_older) else {
            return None;
        };
        Some(Page::Before {
            epoch: cursor.epoch.clone(),
            seq: cursor.start_seq,
            limit,
        })
    }

    /// A reload replaces the timeline, so one asked for while another read
    /// is in flight is kept for after it ([`Fetch::begin`]).
    pub fn reloads(&self) -> bool {
        matches!(self, Page::Tail)
    }

    pub fn is_older(&self) -> bool {
        matches!(self, Page::Before { .. })
    }

    pub fn request(&self, agent_id: &str) -> Value {
        match self {
            Page::Tail => json!({
                "type": "fetch_agent_timeline_request", "agentId": agent_id,
                "direction": "tail", "limit": PAGE, "projection": "projected",
            }),
            Page::After { epoch, seq } => json!({
                "type": "fetch_agent_timeline_request", "agentId": agent_id,
                "direction": "after", "cursor": { "epoch": epoch, "seq": seq },
                "limit": PAGE, "projection": "projected",
            }),
            Page::Before { epoch, seq, limit } => json!({
                "type": "fetch_agent_timeline_request", "agentId": agent_id,
                "direction": "before", "cursor": { "epoch": epoch, "seq": seq },
                "limit": limit, "projection": "projected",
            }),
        }
    }
}

/// What a page that landed did.
#[derive(Debug, Clone, PartialEq)]
pub struct Landed {
    /// The read to make next: a reload asked for meanwhile, else what the
    /// timeline says it still needs.
    pub next: Option<Page>,
    /// The page moved the timeline's cursor.
    pub progressed: bool,
    /// Why the page failed, none if it did not.
    pub error: Option<String>,
}

/// One timeline read per conversation at a time. A reload asked for
/// meanwhile runs when the current read lands; a catch-up asked for
/// meanwhile is redundant (the held frames drain with that page).
#[derive(Debug, Default)]
pub struct Fetch {
    in_flight: bool,
    reload_pending: bool,
}

impl Fetch {
    /// Whether to send `page` now. False means one is in flight: `page` is
    /// dropped, or kept for later if it reloads.
    pub fn begin(&mut self, page: &Page) -> bool {
        self.start(page.reloads())
    }

    /// [`Self::begin`] for a read the caller makes in its own way, one that
    /// `reloads` the timeline or not.
    pub fn start(&mut self, reloads: bool) -> bool {
        if self.in_flight {
            self.reload_pending |= reloads;
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// The read could not be made at all (the host is offline): nothing is
    /// in flight, and nothing is pending.
    pub fn abandon(&mut self) {
        self.in_flight = false;
    }

    /// The read `result` finished: feeds `timeline` the page and says what
    /// to read next.
    pub fn land(&mut self, timeline: &mut Timeline, result: &Result<Value, String>) -> Landed {
        self.in_flight = false;
        let reload = std::mem::take(&mut self.reload_pending);
        let previous = timeline.cursor.clone();
        let (follow, error) = match result {
            Ok(payload) => {
                if payload["agent"].is_object() {
                    timeline.sync_snapshot(&payload["agent"]);
                }
                (timeline.apply_page(payload), None)
            }
            Err(error) => (Follow::Nothing, Some(error.clone())),
        };
        Landed {
            next: if reload {
                Some(Page::Tail)
            } else {
                Page::following(follow)
            },
            progressed: timeline.cursor != previous,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostConfig;

    fn ws(id: &str, status: &str) -> Value {
        json!({"id": id, "projectId": "p", "projectDisplayName": "p",
               "projectRootPath": "/p/p", "projectKind": "git",
               "workspaceKind": "worktree", "name": id, "status": status})
    }

    fn host(features: &[&str]) -> Host {
        let mut host = Host::new(HostConfig {
            name: "a".into(),
            ..Default::default()
        });
        host.server = Some(paseo_client::ServerInfo {
            features: features
                .iter()
                .map(|f| (f.to_string(), json!(true)))
                .collect(),
            ..Default::default()
        });
        host
    }

    fn kind(request: &Value) -> &str {
        request["type"].as_str().unwrap()
    }

    // ---- HostSync ----

    #[test]
    fn pushes_wait_for_the_snapshots_and_replay_after_them_in_order() {
        let mut sync = HostSync::new(1);
        sync.connected(7);
        assert!(!sync.settled());
        let push = |status: &str| {
            json!({"type": "workspace_update", "payload": {"kind": "upsert",
                   "workspace": ws("w1", status)}})
        };
        // Both arrive before the workspaces reply; neither is applied.
        assert!(sync.hold(push("running")).is_none());
        assert!(sync.hold(push("done")).is_none());

        let mut dir = Directory::default();
        // The reply is older than the pushes held behind it.
        dir.apply_workspaces_reply(
            &json!({"entries": [ws("w1", "idle")],
                    "sync": {"generation": "g", "headSeq": 1, "mode": "snapshot", "removals": []}}),
            true,
        );
        let held = sync.finish_bootstrap(&mut dir, true);
        assert_eq!(held.len(), 2);
        assert!(sync.settled());
        for message in &held {
            dir.apply_push(message);
        }
        // Replayed oldest first: the newest push is what stands.
        assert_eq!(dir.workspaces["w1"].status, "done");
        assert!(dir.agents_hydrated);

        // Settled, a push is handed straight back.
        let live = push("running");
        assert_eq!(sync.hold(live.clone()), Some(live));
    }

    #[test]
    fn a_new_connection_drops_what_the_old_one_held() {
        let mut sync = HostSync::new(1);
        sync.connected(1);
        assert!(sync.hold(json!({"type": "agent_update"})).is_none());
        sync.disconnected();
        assert!(!sync.online());
        sync.connected(2);
        let held = sync.finish_bootstrap(&mut Directory::default(), false);
        assert!(held.is_empty(), "{held:?}");
    }

    #[test]
    fn work_from_an_older_connection_or_link_is_stale() {
        let mut sync = HostSync::new(4);
        assert!(sync.accepts(4));
        assert!(!sync.accepts(3), "events from a replaced client drop");

        assert!(!sync.is_current(1), "offline: nothing applies");
        sync.connected(1);
        assert!(sync.is_current(1));
        sync.disconnected();
        assert!(!sync.is_current(1));
        sync.connected(2);
        assert!(!sync.is_current(1), "bootstrap of the first connection");
        assert!(sync.is_current(2));
    }

    #[test]
    fn each_timeline_subscription_hands_back_the_one_it_replaces() {
        let mut sync = HostSync::new(1);
        sync.connected(1);
        assert_eq!(sync.swap_timeline_subscription(Some("s1".into())), None);
        assert_eq!(
            sync.swap_timeline_subscription(Some("s2".into()))
                .as_deref(),
            Some("s1")
        );
        // A new connection starts with none to release.
        sync.disconnected();
        assert_eq!(sync.swap_timeline_subscription(Some("s3".into())), None);

        sync.watched.insert("b".into());
        sync.watched.insert("a".into());
        let request = HostSync::timeline_subscription_request(&sync.watched);
        assert_eq!(request["agentIds"], json!(["a", "b"]));
    }

    // ---- Bootstrap ----

    /// Runs a bootstrap against `answer`, returning the kinds asked in order.
    fn run(
        bootstrap: &mut Bootstrap,
        dir: &mut Directory,
        mut answer: impl FnMut(&Value) -> Result<Value, String>,
    ) -> Vec<String> {
        let mut asked = Vec::new();
        loop {
            match bootstrap.step() {
                Step::Done => return asked,
                Step::ProjectsFromWorkspaces => {
                    asked.push("projects-from-workspaces".into());
                    dir.projects_from_workspaces = true;
                }
                Step::Send(request) => {
                    asked.push(kind(&request).to_string());
                    if let Some(loaded) = bootstrap.reply(answer(&request)) {
                        loaded.apply(dir);
                    }
                }
            }
        }
    }

    fn page(entries: Value, next: Option<&str>) -> Value {
        json!({"entries": entries, "pageInfo": {"hasMore": next.is_some(), "nextCursor": next}})
    }

    #[test]
    fn a_host_is_read_in_order_page_by_page() {
        let mut bootstrap = Bootstrap::for_host(&host(&["projectList", "workspaceLabels"]), false);
        let mut dir = Directory::default();
        let mut cursors = Vec::new();
        let asked = run(&mut bootstrap, &mut dir, |request| {
            Ok(match kind(request) {
                "session.events.set_subscription.request" => json!({"subscriptionId": "ev"}),
                "project.list.request" => json!({"projects": []}),
                "fetch_workspaces_request" => {
                    let cursor = request["page"]["cursor"].as_str();
                    cursors.push(cursor.map(str::to_string));
                    match cursor {
                        None => page(json!([ws("w1", "idle")]), Some("c1")),
                        _ => page(json!([ws("w2", "idle")]), None),
                    }
                }
                "fetch_agents_request" => page(
                    json!([{"agent": {"id": "a1", "workspaceId": "w1", "status": "idle"}}]),
                    None,
                ),
                _ => json!({"labels": []}),
            })
        });
        assert_eq!(
            asked,
            [
                "session.events.set_subscription.request",
                "project.list.request",
                "fetch_workspaces_request",
                "fetch_workspaces_request",
                "fetch_agents_request",
                "workspace.label.list.request",
            ]
        );
        assert_eq!(cursors, [None, Some("c1".to_string())]);
        assert!(!bootstrap.failed());
        assert!(bootstrap.agents_complete());
        assert_eq!(dir.subscriptions.events.as_deref(), Some("ev"));
        assert_eq!(dir.workspaces.len(), 2, "the second page adds to the first");
        assert!(dir.agents.contains_key("a1"));
    }

    #[test]
    fn a_host_without_a_project_list_makes_projects_from_workspaces() {
        let mut bootstrap = Bootstrap::for_host(&host(&[]), false);
        let mut dir = Directory::default();
        let asked = run(&mut bootstrap, &mut dir, |request| {
            Ok(match kind(request) {
                "session.events.set_subscription.request" => json!({"subscriptionId": "ev"}),
                _ => page(json!([]), None),
            })
        });
        assert_eq!(
            asked,
            [
                "session.events.set_subscription.request",
                "projects-from-workspaces",
                "fetch_workspaces_request",
                "fetch_agents_request",
            ],
            "and no labels read, as the host has none"
        );
        assert!(dir.projects_from_workspaces);
    }

    #[test]
    fn a_failed_step_is_noted_and_the_rest_still_run() {
        let mut bootstrap = Bootstrap::for_host(&host(&["workspaceLabels"]), false);
        let mut dir = Directory::default();
        let asked = run(&mut bootstrap, &mut dir, |request| match kind(request) {
            "fetch_workspaces_request" => Err("timed out".into()),
            "workspace.label.list.request" => Err("nope".into()),
            "session.events.set_subscription.request" => Ok(json!({"subscriptionId": "ev"})),
            _ => Ok(page(json!([]), None)),
        });
        assert!(
            asked.contains(&"fetch_agents_request".to_string()),
            "{asked:?}"
        );
        assert!(bootstrap.failed());
        assert!(bootstrap.agents_complete(), "agents still read in full");
        assert_eq!(dir.subscriptions.events.as_deref(), Some("ev"));

        // A lost label list alone is not worth a reconnect.
        let mut bootstrap = Bootstrap::for_host(&host(&["workspaceLabels"]), false);
        run(
            &mut bootstrap,
            &mut Directory::default(),
            |request| match kind(request) {
                "workspace.label.list.request" => Err("nope".into()),
                _ => Ok(page(json!([]), None)),
            },
        );
        assert!(!bootstrap.failed());
    }

    #[test]
    fn a_failed_agent_page_leaves_the_list_incomplete() {
        let mut bootstrap = Bootstrap::for_host(&host(&[]), false);
        let mut dir = Directory::default();
        run(&mut bootstrap, &mut dir, |request| match kind(request) {
            "fetch_agents_request" if request["page"]["cursor"].is_null() => Ok(page(
                json!([{"agent": {"id": "a1", "status": "idle"}}]),
                Some("c1"),
            )),
            "fetch_agents_request" => Err("dropped".into()),
            _ => Ok(page(json!([]), None)),
        });
        assert!(bootstrap.failed());
        assert!(!bootstrap.agents_complete());
        let mut held = HostSync::new(1);
        held.connected(1);
        held.finish_bootstrap(&mut dir, bootstrap.agents_complete());
        assert!(!dir.agents_hydrated, "destructive actions keep waiting");
    }

    #[test]
    fn a_sync_reply_is_one_page_whatever_it_says_of_more() {
        let mut bootstrap = Bootstrap::for_host(&host(&["directorySync"]), false);
        let mut dir = Directory::default();
        let mut agent_reads = 0;
        run(&mut bootstrap, &mut dir, |request| {
            Ok(match kind(request) {
                "fetch_agents_request" => {
                    agent_reads += 1;
                    json!({"entries": [], "pageInfo": {"hasMore": true, "nextCursor": "x"},
                           "sync": {"generation": "g", "headSeq": 1, "mode": "snapshot",
                                    "removals": []}})
                }
                _ => page(json!([]), None),
            })
        });
        assert_eq!(agent_reads, 1);
        assert!(bootstrap.agents_complete());
    }

    #[test]
    fn subagent_updates_are_asked_for_only_by_a_store_that_wants_them() {
        let events = |features: &[&str], wants| {
            let mut bootstrap = Bootstrap::for_host(&host(features), wants);
            match bootstrap.step() {
                Step::Send(request) => request["events"].to_string(),
                other => panic!("{other:?}"),
            }
        };
        assert!(events(&["providerSubagents"], true).contains(SUBAGENTS_EVENT));
        assert!(!events(&["providerSubagents"], false).contains(SUBAGENTS_EVENT));
        assert!(!events(&[], true).contains(SUBAGENTS_EVENT));
    }

    // ---- timeline reads ----

    fn tail_page(seqs: std::ops::RangeInclusive<u64>, has_older: bool) -> Value {
        let (first, last) = (*seqs.start(), *seqs.end());
        let entries: Vec<Value> = seqs
            .map(|seq| {
                json!({"seqStart": seq, "seqEnd": seq, "timestamp": "t",
                       "item": {"type": "user_message", "text": format!("m{seq}")}})
            })
            .collect();
        json!({"direction": "tail", "epoch": "e1", "entries": entries,
               "startCursor": {"seq": first}, "endCursor": {"seq": last},
               "hasOlder": has_older, "window": {"maxSeq": last}})
    }

    #[test]
    fn one_read_at_a_time_and_a_reload_waits_its_turn() {
        let mut fetch = Fetch::default();
        let mut timeline = Timeline::default();
        assert!(fetch.begin(&Page::Tail));
        assert!(fetch.in_flight());
        // A catch-up meanwhile is redundant; a reload is kept.
        assert!(!fetch.begin(&Page::After {
            epoch: "e1".into(),
            seq: 3
        }));
        assert!(!fetch.begin(&Page::Tail));
        let landed = fetch.land(&mut timeline, &Ok(tail_page(1..=3, false)));
        assert_eq!(
            landed.next,
            Some(Page::Tail),
            "the reload asked for meanwhile"
        );
        assert_eq!(landed.error, None);
        assert!(!fetch.in_flight());

        // Nothing pending the second time.
        assert!(fetch.begin(&Page::Tail));
        let landed = fetch.land(&mut timeline, &Ok(tail_page(1..=3, false)));
        assert_eq!(landed.next, None);
        assert!(!landed.progressed, "the same page moves nothing");
    }

    #[test]
    fn a_failed_read_reports_why_and_frees_the_slot() {
        let mut fetch = Fetch::default();
        let mut timeline = Timeline::default();
        assert!(fetch.begin(&Page::Tail));
        let landed = fetch.land(&mut timeline, &Err("The host is offline".into()));
        assert_eq!(landed.error.as_deref(), Some("The host is offline"));
        assert_eq!(landed.next, None);
        assert!(fetch.begin(&Page::Tail), "a retry may go");

        // A read that could not even be sent frees it too.
        fetch.abandon();
        assert!(!fetch.in_flight());
    }

    #[test]
    fn a_page_moves_the_cursor_and_catch_up_reads_from_its_end() {
        let mut fetch = Fetch::default();
        let mut timeline = Timeline::default();
        assert_eq!(Page::catch_up(&timeline), Page::Tail, "no baseline yet");
        assert!(fetch.begin(&Page::Tail));
        let landed = fetch.land(&mut timeline, &Ok(tail_page(1..=3, true)));
        assert!(landed.progressed);
        assert_eq!(
            Page::catch_up(&timeline),
            Page::After {
                epoch: "e1".into(),
                seq: 3
            }
        );
        assert_eq!(
            Page::older(&timeline, 200),
            Some(Page::Before {
                epoch: "e1".into(),
                seq: 1,
                limit: 200
            })
        );
    }

    #[test]
    fn a_timeline_that_asks_to_follow_is_read_again() {
        assert_eq!(Page::following(Follow::Nothing), None);
        assert_eq!(Page::following(Follow::Reload), Some(Page::Tail));
        assert_eq!(
            Page::following(Follow::CatchUp {
                epoch: "e".into(),
                seq: 9
            }),
            Some(Page::After {
                epoch: "e".into(),
                seq: 9
            })
        );
    }

    #[test]
    fn pages_ask_the_daemon_for_what_the_wire_says() {
        let tail = Page::Tail.request("a1");
        assert_eq!(tail["direction"], "tail");
        assert_eq!(tail["limit"], PAGE);
        assert_eq!(tail["agentId"], "a1");
        let after = Page::After {
            epoch: "e".into(),
            seq: 4,
        }
        .request("a1");
        assert_eq!(after["direction"], "after");
        assert_eq!(after["cursor"], json!({"epoch": "e", "seq": 4}));
        let before = Page::Before {
            epoch: "e".into(),
            seq: 2,
            limit: 200,
        }
        .request("a1");
        assert_eq!(before["direction"], "before");
        assert_eq!(before["limit"], 200);
        assert!(
            Page::Before {
                epoch: "e".into(),
                seq: 2,
                limit: 1
            }
            .is_older()
        );
        assert!(!Page::Tail.is_older());
    }
}
