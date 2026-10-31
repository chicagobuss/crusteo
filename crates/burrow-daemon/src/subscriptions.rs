//! Per-socket subscriptions ("owned subscriptions").
//!
//! A subscription belongs to the socket that opened it and dies with it. Its
//! id is assigned here. Store events reach every connection; each one turns
//! them into frames for the subscriptions it owns. A subscription remembers
//! the collection head it was opened at, and drops older events: the
//! snapshot it was opened with already reflects them. Because the store
//! publishes while it holds its lock, that is enough to make snapshot plus
//! pushes exact.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use burrow_protocol::directory::{AgentFilter, AgentScope, AgentSnapshot, ProjectPlacement};
use burrow_protocol::envelope::session;
use serde_json::{Map, Value, json};

use crate::ids;
use crate::labels::LabelChange;
use crate::store::{AgentChange, ProjectChange, StoreEvent, WorkspaceChange};

/// The event names `session.events.set_subscription.request` accepts.
pub const SESSION_EVENTS: &[&str] = &[
    "project.update",
    "providers_snapshot_update",
    "agent_attention_required",
    "agent_permission_request",
    "agent_permission_resolved",
    "checkout_status_update",
    "script_status_update",
    "workspace_setup_progress",
    "agent.provider_subagents.update",
    "terminal_attention_required",
    "status.server_info",
    "status.daemon_config_changed",
    "status.plugin_catalog_changed",
    "status.plugin_settings_changed",
    "activity_log",
    "hub.execution.agent.update",
    "hub.execution.agent.stream",
];

/// Which agents an agent-directory read or subscription covers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentView {
    pub statuses: Option<Vec<burrow_protocol::directory::AgentStatus>>,
    pub labels: Option<BTreeMap<String, String>>,
    pub project_keys: Option<Vec<String>>,
    pub requires_attention: Option<bool>,
}

impl AgentView {
    /// The store holds only active agents, so `scope: "active"` and the
    /// absent scope read the same set until archive lands (phase 1).
    pub fn new(_scope: Option<AgentScope>, filter: Option<&AgentFilter>) -> Self {
        let Some(filter) = filter else {
            return Self::default();
        };
        Self {
            statuses: filter.statuses.clone(),
            labels: filter.labels.clone(),
            project_keys: filter.project_keys.clone(),
            requires_attention: filter.requires_attention,
        }
    }

    pub fn matches(&self, agent: &AgentSnapshot, project: &ProjectPlacement) -> bool {
        self.statuses
            .as_ref()
            .is_none_or(|statuses| statuses.contains(&agent.status))
            && self.labels.as_ref().is_none_or(|labels| {
                labels
                    .iter()
                    .all(|(key, value)| agent.labels.get(key) == Some(value))
            })
            && self
                .project_keys
                .as_ref()
                .is_none_or(|keys| keys.contains(&project.project_key))
            && self
                .requires_attention
                .is_none_or(|wanted| agent.requires_attention == wanted)
    }
}

#[derive(Debug, Clone)]
pub enum Subscription {
    Agents {
        view: AgentView,
        from_seq: u64,
        sync: bool,
        /// Includes the agents of background workspaces.
        background: bool,
        /// Agents this subscription has shown, so a filter exit becomes a remove.
        members: HashSet<String>,
    },
    Workspaces {
        project_id: Option<String>,
        from_seq: u64,
        sync: bool,
        /// Includes background workspaces.
        background: bool,
    },
    Labels {
        from_seq: u64,
    },
    Events {
        events: HashSet<String>,
    },
    Timelines {
        agent_ids: BTreeSet<String>,
    },
    /// `workspace.setup.subscribe.request` (Paseo 0.13): one workspace's
    /// `workspace_setup_progress`.
    WorkspaceSetup {
        workspace_id: String,
    },
}

/// In an `agent_attention_required` event, the connection told to show a
/// notification ([`crate::push`]). Taken out before the event is sent.
pub const NOTIFY_CONNECTION: &str = "\u{0}notifyConnection";

#[derive(Debug, Default)]
pub struct Subscriptions {
    by_id: HashMap<String, Subscription>,
    /// The client reads compact providers snapshots.
    compact_provider_snapshots: bool,
    /// This socket, among every connection the daemon has had.
    connection: u64,
}

impl Subscriptions {
    /// The subscriptions of a socket whose client reads compact providers
    /// snapshots, or not.
    pub fn for_client(compact_provider_snapshots: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            compact_provider_snapshots,
            connection: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ..Self::default()
        }
    }

    /// This socket's connection number.
    pub fn connection(&self) -> u64 {
        self.connection
    }

    pub fn open(&mut self, subscription: Subscription) -> String {
        let id = ids::subscription_id();
        self.by_id.insert(id.clone(), subscription);
        id
    }

    /// Releasing an unknown id is not an error: Paseo acks it too.
    pub fn release(&mut self, id: &str) {
        self.by_id.remove(id);
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// The frames `event` produces on this socket.
    pub fn route(&mut self, event: &StoreEvent, generation: &str) -> Vec<Value> {
        let mut frames = Vec::new();
        for (id, subscription) in &mut self.by_id {
            let compact = self.compact_provider_snapshots;
            let connection = self.connection;
            if let Some(frame) = route_one(id, subscription, event, generation, compact, connection)
            {
                frames.push(frame);
            }
        }
        frames
    }
}

fn route_one(
    id: &str,
    subscription: &mut Subscription,
    event: &StoreEvent,
    generation: &str,
    compact_snapshots: bool,
    connection: u64,
) -> Option<Value> {
    // A subscription that includes background workspaces is sequenced in
    // their own generation, as its read was.
    let sequenced = |mut payload: Map<String, Value>, seq: u64, sync: bool, background: bool| {
        payload.insert("subscriptionId".into(), id.into());
        if sync {
            let generation = crate::store::background_generation(generation, background);
            payload.insert("generation".into(), generation.into());
            payload.insert("seq".into(), seq.into());
        }
        Value::Object(payload)
    };
    match (subscription, event) {
        // Unsequenced, so it also reaches a subscription opened at the head
        // it was published at.
        (
            Subscription::Workspaces { from_seq, .. },
            StoreEvent::Workspace {
                seq,
                change:
                    WorkspaceChange::ProjectGone {
                        id: gone,
                        project_id,
                    },
            },
        ) if seq >= from_seq => {
            let mut payload =
                object(json!({"kind": "remove", "id": gone, "removedProjectId": project_id}));
            payload.insert("subscriptionId".into(), id.into());
            Some(session("workspace_update", Value::Object(payload)))
        }
        (
            Subscription::Agents {
                view,
                from_seq,
                sync,
                background,
                members,
            },
            StoreEvent::Agent { seq, change },
        ) if seq > from_seq => {
            let payload = match change {
                AgentChange::Upsert {
                    agent,
                    project,
                    background: in_background,
                } if view.matches(agent, project) && (*background || !in_background) => {
                    members.insert(agent.id.clone());
                    object(json!({"kind": "upsert", "agent": agent, "project": project}))
                }
                AgentChange::Upsert { agent, .. } => {
                    if !members.remove(&agent.id) {
                        return None;
                    }
                    object(json!({"kind": "remove", "agentId": agent.id}))
                }
                AgentChange::Remove { agent_id } => {
                    if !members.remove(agent_id) {
                        return None;
                    }
                    object(json!({"kind": "remove", "agentId": agent_id}))
                }
            };
            Some(session(
                "agent_update",
                sequenced(payload, *seq, *sync, *background),
            ))
        }
        (
            Subscription::Workspaces {
                project_id,
                from_seq,
                sync,
                background,
            },
            StoreEvent::Workspace { seq, change },
        ) if seq > from_seq => {
            let payload = match change {
                WorkspaceChange::Upsert(workspace) => {
                    if project_id
                        .as_ref()
                        .is_some_and(|wanted| *wanted != workspace.project_id)
                        || (workspace.background && !*background)
                    {
                        return None;
                    }
                    let mut workspace = workspace.clone();
                    if !*sync {
                        workspace.sync_seq = None;
                    }
                    object(json!({"kind": "upsert", "workspace": workspace}))
                }
                WorkspaceChange::ProjectGone { .. } => return None,
                WorkspaceChange::Remove {
                    id,
                    empty_project,
                    empty_project_without_background,
                    removed_project_id,
                } => {
                    let mut payload = object(json!({"kind": "remove", "id": id}));
                    if let Some(project_id) = removed_project_id {
                        payload.insert("removedProjectId".into(), project_id.clone().into());
                    }
                    let empty = if *background {
                        empty_project
                    } else {
                        empty_project_without_background
                    };
                    if let Some(project) = empty.as_ref().filter(|p| {
                        project_id
                            .as_ref()
                            .is_none_or(|wanted| *wanted == p.project_id)
                    }) {
                        let mut project = project.clone();
                        project.sync_seq = None;
                        payload.insert("emptyProject".into(), json!(project));
                    }
                    payload
                }
            };
            Some(session(
                "workspace_update",
                sequenced(payload, *seq, *sync, *background),
            ))
        }
        (Subscription::Labels { from_seq }, StoreEvent::Label { seq, change })
            if seq > from_seq =>
        {
            let payload = match change {
                LabelChange::Upsert {
                    label,
                    previous_name,
                } => {
                    let mut payload = object(json!({"kind": "upsert", "label": label}));
                    if let Some(previous) = previous_name {
                        payload.insert("previousName".into(), previous.clone().into());
                    }
                    payload
                }
                LabelChange::Remove { name } => object(json!({"kind": "remove", "name": name})),
            };
            // Label pushes always carry their place in the label journal.
            Some(session(
                "workspace.label.update",
                sequenced(payload, *seq, true, false),
            ))
        }
        (Subscription::Events { events }, StoreEvent::Project { seq, change })
            if events.contains("project.update") =>
        {
            let payload = match change {
                ProjectChange::Upsert(project) => {
                    object(json!({"kind": "upsert", "project": project}))
                }
                ProjectChange::Remove { project_id } => {
                    object(json!({"kind": "remove", "projectId": project_id}))
                }
            };
            Some(session(
                "project.update",
                sequenced(payload, *seq, true, false),
            ))
        }
        (
            Subscription::Timelines { agent_ids },
            StoreEvent::Timeline {
                agent_id,
                epoch,
                row,
            },
        ) if agent_ids.contains(agent_id) => {
            let mut event = json!({"type": "timeline", "provider": row.provider, "item": row.item});
            if let Some(turn_id) = &row.turn_id {
                event["turnId"] = turn_id.clone().into();
            }
            Some(session(
                "agent_stream",
                json!({
                    "subscriptionId": id,
                    "agentId": agent_id,
                    "event": event,
                    "timestamp": row.timestamp,
                    "seq": row.seq,
                    "epoch": epoch,
                }),
            ))
        }
        (
            Subscription::Timelines { agent_ids },
            StoreEvent::Stream {
                agent_id,
                event,
                timestamp,
            },
        ) if agent_ids.contains(agent_id) => Some(session(
            "agent_stream",
            json!({
                "subscriptionId": id,
                "agentId": agent_id,
                "event": event,
                "timestamp": timestamp,
            }),
        )),
        (Subscription::Events { events }, StoreEvent::Session { kind, payload })
            if events.contains(*kind) =>
        {
            let mut payload = object(payload.clone());
            if let Some(chosen) = payload.remove(NOTIFY_CONNECTION) {
                payload.insert("shouldNotify".into(), (chosen == connection).into());
            }
            payload.insert("subscriptionId".into(), id.into());
            Some(session(kind, Value::Object(payload)))
        }
        (Subscription::WorkspaceSetup { workspace_id }, StoreEvent::Session { kind, payload })
            if *kind == "workspace_setup_progress" && payload["workspaceId"] == *workspace_id =>
        {
            let mut payload = object(payload.clone());
            payload.insert("subscriptionId".into(), id.into());
            Some(session(kind, Value::Object(payload)))
        }
        (Subscription::Events { events }, StoreEvent::Providers { plain, compact })
            if events.contains("providers_snapshot_update") =>
        {
            let chosen = if compact_snapshots { compact } else { plain };
            let mut payload = object(chosen.clone());
            payload.insert("subscriptionId".into(), id.into());
            Some(session("providers_snapshot_update", Value::Object(payload)))
        }
        _ => None,
    }
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}
