//! Session request handlers.
//!
//! [`handle`] takes one inbound session message and returns the frames to
//! send back. It may open or release subscriptions on the calling socket.
//! Requests Burrow does not implement get the answer Paseo gives a request
//! it does not know: `rpc_error` with code `unknown_schema`.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;

use burrow_protocol::codes::rpc;
use burrow_protocol::directory::{
    AgentEntry, AgentScope, AgentSnapshot, FetchAgentRequest, FetchAgentsRequest,
    FetchWorkspacesRequest, PageInfo, PageRequest, ProjectListRequest, SortDirection, SortKey,
    SubscribeRequest, SyncCursor, WorkspaceDescriptor,
};
use burrow_protocol::envelope::{request_id, rpc_error, session, status_error};
use burrow_protocol::labels::{LabelColor, LabelDefinition};
use burrow_protocol::timeline::{
    Direction, FetchTimelineRequest, FetchTimelineResponse, Projection,
    TimelineSubscriptionRequest, TimelineWindow,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::agent_requests::{self, ClientShape, Late};
use crate::clock;
use crate::daemon::Daemon;
use crate::labels::LabelError;
use crate::store::Store;
use crate::subscriptions::{AgentView, SESSION_EVENTS, Subscription, Subscriptions};
use crate::{daemon_requests, workspace_requests};

/// Session messages that never get a reply, and that Burrow ignores.
const FIRE_AND_FORGET: &[&str] = &[
    "terminal_input",
    "unsubscribe_terminal_request",
    "unsubscribe_terminals_request",
    "unsubscribe_checkout_diff_request",
    "voice_audio_chunk",
    "abort_request",
    "audio_played",
];

/// Why a request failed, before it is turned into a frame.
enum Failure {
    /// The message did not validate (or names no request Burrow knows).
    Schema,
    /// The handler refused it: `rpc_error` with this code and message.
    Rpc(&'static str, String),
}

type Outcome = Result<Vec<Value>, Failure>;

pub fn handle(
    daemon: &Arc<Daemon>,
    subs: &mut Subscriptions,
    shape: ClientShape,
    late: &Late,
    message: Value,
) -> Vec<Value> {
    let kind = message
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string);
    let id = request_id(&message).map(str::to_string);
    let Some(kind) = kind else {
        return vec![match &id {
            Some(id) => rpc_error(
                id,
                None,
                "Invalid message: missing type",
                rpc::INVALID_MESSAGE,
            ),
            None => status_error("Invalid message: missing type"),
        }];
    };
    let outcome = match kind.as_str() {
        "ping" => ping(&message),
        "fetch_agents_request" => parse(&message).and_then(|r| fetch_agents(daemon, subs, r)),
        "fetch_agent_request" => parse(&message).map(|r| fetch_agent(daemon, r)),
        "fetch_agent_history_request" => parse(&message).and_then(|r| agent_history(daemon, r)),
        "fetch_workspaces_request" => {
            parse(&message).and_then(|r| fetch_workspaces(daemon, subs, r))
        }
        "project.list.request" => parse(&message).map(|r| project_list(daemon, r)),
        "workspace.label.list.request" => parse(&message).and_then(|r| label_list(daemon, subs, r)),
        "workspace.label.assignment.set.request" => {
            parse(&message).and_then(|r| label_assignment(daemon, r))
        }
        "workspace.label.update.request" => parse(&message).and_then(|r| label_update(daemon, r)),
        "workspace.label.delete.request" => {
            parse(&message).and_then(|r| label_delete(daemon, r, false))
        }
        "workspace.label.delete.inspect.request" => {
            parse(&message).and_then(|r| label_delete(daemon, r, true))
        }
        "fetch_agent_timeline_request" => parse(&message).map(|r| fetch_timeline(daemon, r)),
        "agent.timeline.set_subscription.request" => {
            parse(&message).map(|r| timeline_subscription(subs, r))
        }
        "session.events.set_subscription.request" => {
            parse(&message).and_then(|r| events_subscription(daemon, subs, r))
        }
        "subscription.release.request" => parse(&message).map(|r| release(daemon, subs, r)),
        "workspace.setup.subscribe.request" => {
            parse(&message).map(|r| setup_subscription(daemon, subs, r))
        }
        "file_explorer_request" => parse(&message).map(|r| read_file(daemon, r)),
        "client_heartbeat" => parse(&message).map(|r| heartbeat(daemon, subs, r)),
        "register_push_token" => parse(&message).map(|r| register_push(daemon, subs, r)),
        "push.unregister.request" => parse(&message).map(|r| unregister_push(daemon, r)),
        other if agent_requests::KINDS.contains(&other) => {
            agent_requests::handle(daemon, other, &message, shape, late).ok_or(Failure::Schema)
        }
        other if workspace_requests::KINDS.contains(&other) => {
            workspace_requests::handle(daemon, other, &message, late).ok_or(Failure::Schema)
        }
        other if daemon_requests::KINDS.contains(&other) => {
            daemon_requests::handle(daemon, other, &message, late).ok_or(Failure::Schema)
        }
        other if crate::schedules::KINDS.contains(&other) => {
            crate::schedules::handle(daemon, other, &message, late).ok_or(Failure::Schema)
        }
        other if FIRE_AND_FORGET.contains(&other) => Ok(Vec::new()),
        _ => Err(Failure::Schema),
    };
    match (outcome, id) {
        (Ok(frames), _) => frames,
        (Err(Failure::Rpc(code, error)), Some(id)) => {
            vec![rpc_error(&id, Some(&kind), &error, code)]
        }
        (Err(Failure::Schema), Some(id)) => vec![rpc_error(
            &id,
            Some(&kind),
            &format!(
                "Unknown request, try upgrading the daemon (currently v{})",
                daemon.config.version
            ),
            rpc::UNKNOWN_SCHEMA,
        )],
        (Err(_), None) => vec![status_error(&format!(
            "Invalid message: unknown or malformed '{kind}' message"
        ))],
    }
}

fn parse<T: DeserializeOwned>(message: &Value) -> Result<T, Failure> {
    T::deserialize(message).map_err(|_| Failure::Schema)
}

fn ping(message: &Value) -> Outcome {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Ping {
        request_id: String,
        #[serde(default)]
        client_sent_at: Option<i64>,
    }
    let received = clock::now_ms();
    let ping: Ping = parse(message)?;
    let mut payload = json!({
        "requestId": ping.request_id,
        "serverReceivedAt": received,
        "serverSentAt": clock::now_ms(),
    });
    if let Some(sent) = ping.client_sent_at {
        payload["clientSentAt"] = sent.into();
    }
    Ok(vec![session("pong", payload)])
}

/// Modern clients send `subscribe: {}`; a client-chosen id is refused.
fn check_subscribe(subscribe: &Option<SubscribeRequest>) -> Result<bool, Failure> {
    match subscribe {
        Some(SubscribeRequest {
            subscription_id: Some(_),
        }) => Err(Failure::Rpc(
            rpc::HANDLER_ERROR,
            "Request failed: Subscription IDs are assigned by the host".into(),
        )),
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

/// Page cursors are opaque to clients; here they are an offset.
fn page_window(page: Option<&PageRequest>, total: usize) -> Result<(usize, usize), Failure> {
    let Some(page) = page else {
        return Ok((0, total));
    };
    let start = match &page.cursor {
        None => 0,
        Some(cursor) => cursor
            .strip_prefix("o")
            .and_then(|n| n.parse::<usize>().ok())
            .ok_or_else(|| Failure::Rpc(rpc::INVALID_REQUEST, "Invalid page cursor".into()))?,
    };
    let limit = (page.limit as usize).clamp(1, burrow_protocol::codes::DIRECTORY_PAGE_MAX);
    Ok((start.min(total), limit))
}

fn page_info(start: usize, shown: usize, total: usize) -> PageInfo {
    let end = start + shown;
    PageInfo {
        next_cursor: (end < total).then(|| format!("o{end}")),
        prev_cursor: (start > 0).then(|| format!("o{}", start.saturating_sub(shown.max(1)))),
        has_more: end < total,
    }
}

fn compare_by<T>(
    sort: &[SortKey],
    a: &T,
    b: &T,
    key: impl Fn(&str, &T, &T) -> Ordering,
) -> Ordering {
    for SortKey {
        key: name,
        direction,
    } in sort
    {
        let ordering = key(name, a, b);
        let ordering = match direction {
            SortDirection::Asc => ordering,
            SortDirection::Desc => ordering.reverse(),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn default_sort(key: &str) -> Vec<SortKey> {
    vec![SortKey {
        key: key.into(),
        direction: SortDirection::Desc,
    }]
}

fn agent_order(name: &str, a: &AgentSnapshot, b: &AgentSnapshot) -> Ordering {
    match name {
        "status_priority" => agent_priority(a).cmp(&agent_priority(b)),
        "created_at" => a.created_at.cmp(&b.created_at),
        "updated_at" => a.updated_at.cmp(&b.updated_at),
        "title" => a.title.cmp(&b.title),
        _ => Ordering::Equal,
    }
}

fn agent_priority(agent: &AgentSnapshot) -> u8 {
    agent.status.priority(
        !agent.pending_permissions.is_empty(),
        agent.requires_attention,
    )
}

fn fetch_agents(daemon: &Daemon, subs: &mut Subscriptions, request: FetchAgentsRequest) -> Outcome {
    let subscribe = check_subscribe(&request.subscribe)?;
    if request.sync.is_some()
        && (request.scope != Some(AgentScope::Active) || request.filter.is_some())
    {
        return Err(Failure::Rpc(
            rpc::INVALID_REQUEST,
            "Sequenced agent directory reads require scope=active and no filter.".into(),
        ));
    }
    let view = AgentView::new(request.scope, request.filter.as_ref());
    // As Paseo: asked for at the top level or in the filter.
    let background = request.include_background == Some(true)
        || (request.filter.as_ref()).is_some_and(|f| f.include_background == Some(true));
    let sort = request
        .sort
        .clone()
        .unwrap_or_else(|| default_sort("updated_at"));
    let (mut payload, from_seq, members) = daemon.read(|store| -> Result<_, Failure> {
        let plan = request
            .sync
            .as_ref()
            .map(|cursor| store.agents.plan(&store.generation_for(background), cursor));
        let mut matching: Vec<(AgentEntry, u64)> = store
            .agents
            .iter()
            .filter(|(agent, _)| background || !store.in_background(agent))
            .filter_map(|(agent, seq)| {
                let project = store.agent_placement(agent);
                view.matches(agent, &project).then(|| {
                    let entry = AgentEntry {
                        agent: agent.clone(),
                        project,
                        sync_seq: plan.as_ref().map(|_| seq),
                    };
                    (entry, seq)
                })
            })
            .collect();
        // `includeArchived` (`paseo ls -a`): archived agents too, which a
        // sequenced read never asks for.
        let archived = (request.filter.as_ref()).is_some_and(|f| f.include_archived == Some(true));
        if archived && plan.is_none() {
            matching.extend(
                (store.archived.values())
                    .filter(|agent| background || !store.in_background(agent))
                    .filter_map(|agent| {
                        let project = store.agent_placement(agent);
                        view.matches(agent, &project).then(|| {
                            let entry = AgentEntry {
                                agent: agent.clone(),
                                project,
                                sync_seq: None,
                            };
                            (entry, 0)
                        })
                    }),
            );
        }
        let members = matching.iter().map(|(e, _)| e.agent.id.clone()).collect();
        matching.sort_by(|(a, _), (b, _)| {
            compare_by(&sort, &a.agent, &b.agent, agent_order).then(a.agent.id.cmp(&b.agent.id))
        });
        let payload = match plan {
            // A sequenced read ignores `page` and returns the whole directory.
            Some(plan) => {
                let entries: Vec<_> = matching
                    .into_iter()
                    .filter(|(_, seq)| plan.after.is_none_or(|after| *seq > after))
                    .map(|(entry, _)| entry)
                    .collect();
                json!({"entries": entries, "pageInfo": PageInfo::default(), "sync": plan.meta})
            }
            None => {
                let total = matching.len();
                let (start, limit) = page_window(request.page.as_ref(), total)?;
                let entries: Vec<_> = matching
                    .into_iter()
                    .skip(start)
                    .take(limit)
                    .map(|(entry, _)| entry)
                    .collect();
                let info = page_info(start, entries.len(), total);
                json!({"entries": entries, "pageInfo": info})
            }
        };
        Ok((payload, store.agents.head_seq(), members))
    })?;
    payload["requestId"] = request.request_id.into();
    if subscribe {
        let id = subs.open(Subscription::Agents {
            view,
            from_seq,
            sync: request.sync.is_some(),
            background,
            members,
        });
        payload["subscriptionId"] = id.into();
    }
    Ok(vec![session("fetch_agents_response", payload)])
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetupSubscribeRequest {
    request_id: String,
    workspace_id: String,
}

/// `workspace.setup.subscribe.request` (Paseo 0.13's setup panel): the
/// setup's latest snapshot, then its `workspace_setup_progress` until the
/// subscription is released. Opened before the snapshot is read, so no
/// update between the two is lost.
fn setup_subscription(
    daemon: &Daemon,
    subs: &mut Subscriptions,
    request: SetupSubscribeRequest,
) -> Vec<Value> {
    let id = subs.open(Subscription::WorkspaceSetup {
        workspace_id: request.workspace_id.clone(),
    });
    let snapshot = (daemon.setups.lock().unwrap_or_else(|p| p.into_inner()))
        .get(&request.workspace_id)
        .cloned();
    vec![session(
        "workspace.setup.subscribe.response",
        json!({"requestId": request.request_id, "subscriptionId": id,
               "workspaceId": request.workspace_id, "snapshot": snapshot}),
    )]
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentHistoryRequest {
    request_id: String,
    #[serde(default)]
    page: Option<PageRequest>,
}

/// `fetch_agent_history_request`: every agent, archived ones and those of
/// background workspaces too, newest first, a page at a time. `paseo
/// delete --all` and `--cwd` read it.
fn agent_history(daemon: &Daemon, request: AgentHistoryRequest) -> Outcome {
    let mut payload = daemon.read(|store| -> Result<Value, Failure> {
        let mut agents: Vec<&AgentSnapshot> = (store.agents.iter())
            .map(|(agent, _)| agent)
            .chain(store.archived.values())
            .collect();
        agents.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        let total = agents.len();
        let (start, limit) = page_window(request.page.as_ref(), total)?;
        let entries: Vec<Value> = (agents.into_iter().skip(start).take(limit))
            .map(|agent| json!({"agent": agent, "project": store.agent_placement(agent)}))
            .collect();
        let info = page_info(start, entries.len(), total);
        Ok(json!({"entries": entries, "pageInfo": info}))
    })?;
    payload["requestId"] = request.request_id.into();
    Ok(vec![session("fetch_agent_history_response", payload)])
}

fn fetch_agent(daemon: &Daemon, request: FetchAgentRequest) -> Vec<Value> {
    let payload = daemon.read(|store| match store.find_agent(&request.agent_id) {
        Some(agent) => json!({
            "requestId": request.request_id,
            "agent": agent,
            "project": store.agent_placement(agent),
            "error": null,
        }),
        // Paseo answers "not found" with nulls and no error.
        None => json!({
            "requestId": request.request_id, "agent": null, "project": null, "error": null,
        }),
    });
    vec![session("fetch_agent_response", payload)]
}

fn workspace_order(name: &str, a: &WorkspaceDescriptor, b: &WorkspaceDescriptor) -> Ordering {
    let rank = |workspace: &WorkspaceDescriptor| workspace.status as u8;
    match name {
        "status_priority" => rank(a).cmp(&rank(b)),
        "activity_at" => a.activity_at.cmp(&b.activity_at),
        "name" => a.name.cmp(&b.name),
        "project_id" => a.project_id.cmp(&b.project_id),
        _ => Ordering::Equal,
    }
}

fn fetch_workspaces(
    daemon: &Daemon,
    subs: &mut Subscriptions,
    request: FetchWorkspacesRequest,
) -> Outcome {
    let subscribe = check_subscribe(&request.subscribe)?;
    if request.sync.is_some() && request.filter.is_some() {
        return Err(Failure::Rpc(
            rpc::INVALID_REQUEST,
            "Sequenced workspace directory reads require no filter.".into(),
        ));
    }
    let filter = request.filter.clone().unwrap_or_default();
    let background =
        request.include_background == Some(true) || filter.include_background == Some(true);
    let query = filter.query.as_deref().map(str::to_lowercase);
    let sort = request
        .sort
        .clone()
        .unwrap_or_else(|| default_sort("activity_at"));
    let (mut payload, from_seq) = daemon.read(|store| -> Result<_, Failure> {
        let plan = request.sync.as_ref().map(|cursor| {
            store
                .workspaces
                .plan(&store.generation_for(background), cursor)
        });
        let mut matching: Vec<(WorkspaceDescriptor, u64)> = store
            .workspaces
            .iter()
            .filter(|(workspace, _)| background || !workspace.background)
            .map(|(workspace, seq)| {
                let mut descriptor = store.workspace_descriptor(workspace);
                descriptor.sync_seq = plan.as_ref().map(|_| seq);
                (descriptor, seq)
            })
            .filter(|(workspace, _)| {
                filter
                    .project_id
                    .as_ref()
                    .is_none_or(|id| *id == workspace.project_id)
                    && query
                        .as_ref()
                        .is_none_or(|query| workspace.name.to_lowercase().contains(query))
            })
            .collect();
        matching.sort_by(|(a, _), (b, _)| {
            compare_by(&sort, a, b, workspace_order).then(a.id.cmp(&b.id))
        });
        let empty_projects = store.empty_projects(background);
        let payload = match plan {
            Some(plan) => {
                let entries: Vec<_> = matching
                    .into_iter()
                    .filter(|(_, seq)| plan.after.is_none_or(|after| *seq > after))
                    .map(|(workspace, _)| workspace)
                    .collect();
                json!({
                    "entries": entries, "emptyProjects": empty_projects,
                    "pageInfo": PageInfo::default(), "sync": plan.meta,
                })
            }
            None => {
                let total = matching.len();
                let (start, limit) = page_window(request.page.as_ref(), total)?;
                let entries: Vec<_> = matching
                    .into_iter()
                    .skip(start)
                    .take(limit)
                    .map(|(workspace, _)| workspace)
                    .collect();
                let info = page_info(start, entries.len(), total);
                // Only the first page carries the empty projects.
                let empty_projects = if start == 0 {
                    empty_projects
                } else {
                    Vec::new()
                };
                json!({"entries": entries, "emptyProjects": empty_projects, "pageInfo": info})
            }
        };
        Ok((payload, store.workspaces.head_seq()))
    })?;
    payload["requestId"] = request.request_id.into();
    if subscribe {
        let id = subs.open(Subscription::Workspaces {
            project_id: filter.project_id,
            from_seq,
            sync: request.sync.is_some(),
            background,
        });
        payload["subscriptionId"] = id.into();
    }
    Ok(vec![session("fetch_workspaces_response", payload)])
}

fn project_list(daemon: &Daemon, request: ProjectListRequest) -> Vec<Value> {
    let mut payload = daemon.read(|store| {
        let plan = request
            .sync
            .as_ref()
            .map(|cursor| store.projects.plan(&store.generation, cursor));
        let projects: Vec<_> = store
            .projects
            .iter()
            .filter(|(_, seq)| {
                plan.as_ref()
                    .and_then(|plan| plan.after)
                    .is_none_or(|after| *seq > after)
            })
            .map(|(project, seq)| {
                let mut descriptor = store.project_descriptor(project);
                descriptor.sync_seq = plan.as_ref().map(|_| seq);
                descriptor
            })
            .collect();
        match plan {
            Some(plan) => json!({"projects": projects, "sync": plan.meta}),
            None => json!({"projects": projects}),
        }
    });
    payload["requestId"] = request.request_id.into();
    vec![session("project.list.response", payload)]
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelListRequest {
    request_id: String,
    #[serde(default)]
    subscribe: Option<SubscribeRequest>,
    #[serde(default)]
    sync: Option<SyncCursor>,
}

fn label_list(daemon: &Daemon, subs: &mut Subscriptions, request: LabelListRequest) -> Outcome {
    let subscribe = check_subscribe(&request.subscribe)?;
    let (sync, generation) = daemon.read(|store: &Store| {
        let cursor = request.sync.clone().unwrap_or_default();
        (
            store.labels.synchronize(&store.generation, &cursor),
            store.generation.clone(),
        )
    });
    // Label removals are keyed by `name`, not `id`, and there is no `reason`.
    let removals: Vec<Value> = sync
        .removals
        .iter()
        .map(|(name, seq)| json!({"name": name, "seq": seq}))
        .collect();
    let mut payload = json!({
        "requestId": request.request_id,
        "labels": sync.labels,
        "sync": {
            "mode": sync.mode, "generation": generation,
            "headSeq": sync.head_seq, "removals": removals,
        },
    });
    if subscribe {
        let from_seq = sync.head_seq;
        payload["subscriptionId"] = subs.open(Subscription::Labels { from_seq }).into();
    }
    Ok(vec![session("workspace.label.list.response", payload)])
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelAssignmentRequest {
    request_id: String,
    workspace_id: String,
    label: LabelDefinition,
    assigned: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelUpdateRequest {
    request_id: String,
    name: String,
    #[serde(default)]
    new_name: Option<String>,
    #[serde(default)]
    color: Option<LabelColor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelNameRequest {
    request_id: String,
    name: String,
}

fn label_failure(error: LabelError) -> Failure {
    Failure::Rpc(error.code, error.message.into())
}

/// `workspace.label.assignment.set.request`: puts a label on a workspace or
/// takes it off. A label new to the catalog is made with the colour given.
fn label_assignment(daemon: &Daemon, request: LabelAssignmentRequest) -> Outcome {
    let (label, labels) = daemon
        .apply(|store| {
            store.set_label_assignment(&request.workspace_id, request.label, request.assigned)
        })
        .map_err(label_failure)?;
    Ok(vec![session(
        "workspace.label.assignment.set.response",
        json!({"requestId": request.request_id, "label": label, "workspaceLabels": labels}),
    )])
}

/// `workspace.label.update.request`: renames or recolours a label.
fn label_update(daemon: &Daemon, request: LabelUpdateRequest) -> Outcome {
    let (label, affected) = daemon
        .apply(|store| {
            store.update_label(&request.name, request.new_name.as_deref(), request.color)
        })
        .map_err(label_failure)?;
    Ok(vec![session(
        "workspace.label.update.response",
        json!({"requestId": request.request_id, "label": label,
               "affectedWorkspaceCount": affected}),
    )])
}

/// `workspace.label.delete.request`, and its dry run `.delete.inspect`.
fn label_delete(daemon: &Daemon, request: LabelNameRequest, inspect: bool) -> Outcome {
    let affected = if inspect {
        daemon.read(|store| store.count_label(&request.name))
    } else {
        daemon.apply(|store| store.delete_label(&request.name))
    }
    .map_err(label_failure)?;
    let kind = if inspect {
        "workspace.label.delete.inspect.response"
    } else {
        "workspace.label.delete.response"
    };
    Ok(vec![session(
        kind,
        json!({"requestId": request.request_id, "affectedWorkspaceCount": affected}),
    )])
}

fn fetch_timeline(daemon: &Daemon, request: FetchTimelineRequest) -> Vec<Value> {
    let direction = request.direction.unwrap_or(if request.cursor.is_some() {
        Direction::After
    } else {
        Direction::Tail
    });
    let limit = request.limit.map_or(
        if direction == Direction::After {
            0
        } else {
            burrow_protocol::codes::TIMELINE_DEFAULT_LIMIT
        },
        |limit| limit as usize,
    );
    // Timelines load from disk on first use.
    daemon.apply(|store| {
        if let Some(id) = store.find_agent(&request.agent_id).map(|a| a.id.clone()) {
            store.ensure_timeline(&id);
        }
        ((), Vec::new())
    });
    let projection = request.projection.unwrap_or_default();
    let response = daemon.read(|store| {
        let agent = store.find_agent(&request.agent_id)?;
        let page = store.timeline(&agent.id)?.fetch(
            direction,
            request.cursor.as_ref(),
            limit,
            request.whole_turns == Some(true),
            projection == Projection::Projected,
        );
        Some(FetchTimelineResponse {
            request_id: request.request_id.clone(),
            agent_id: request.agent_id.clone(),
            agent: Some(agent.clone()),
            direction,
            projection,
            epoch: page.epoch,
            reset: page.reset,
            stale_cursor: page.stale_cursor,
            gap: page.gap,
            window: page.window,
            start_cursor: page.start_cursor,
            end_cursor: page.end_cursor,
            has_older: page.has_older,
            has_newer: page.has_newer,
            entries: page.entries,
            error: None,
        })
    });
    let response = response.unwrap_or_else(|| FetchTimelineResponse {
        request_id: request.request_id.clone(),
        agent_id: request.agent_id.clone(),
        agent: None,
        direction,
        projection: Projection::Projected,
        epoch: String::new(),
        reset: false,
        stale_cursor: false,
        gap: false,
        window: TimelineWindow {
            min_seq: 0,
            max_seq: 0,
            next_seq: 0,
        },
        start_cursor: None,
        end_cursor: None,
        has_older: false,
        has_newer: false,
        entries: Vec::new(),
        error: Some(format!("Agent not found: {}", request.agent_id)),
    });
    let mut payload = serde_json::to_value(response).unwrap_or(Value::Null);
    if request.merge_window == Some(true) {
        payload["mergeWindow"] = true.into();
    }
    vec![session("fetch_agent_timeline_response", payload)]
}

fn timeline_subscription(
    subs: &mut Subscriptions,
    request: TimelineSubscriptionRequest,
) -> Vec<Value> {
    let agent_ids: BTreeSet<String> = request.agent_ids.into_iter().collect();
    let sorted: Vec<&String> = agent_ids.iter().collect();
    let payload = json!({"requestId": request.request_id, "agentIds": sorted});
    let mut payload = payload;
    payload["subscriptionId"] = subs
        .open(Subscription::Timelines {
            agent_ids: agent_ids.clone(),
        })
        .into();
    vec![session("agent.timeline.set_subscription.response", payload)]
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsRequest {
    request_id: String,
    events: Vec<String>,
    /// The client shows notifications for the events it asks for, so it
    /// may be the one told to ([`crate::push`]).
    #[serde(default)]
    notifications: Option<bool>,
}

fn events_subscription(
    daemon: &Daemon,
    subs: &mut Subscriptions,
    request: EventsRequest,
) -> Outcome {
    if !request
        .events
        .iter()
        .all(|event| SESSION_EVENTS.contains(&event.as_str()))
    {
        return Err(Failure::Schema);
    }
    let notifying = request.notifications == Some(true)
        && request
            .events
            .iter()
            .any(|e| e == "agent_attention_required");
    let id = subs.open(Subscription::Events {
        events: request.events.into_iter().collect(),
    });
    daemon.clients.subscribed(subs.connection(), &id, notifying);
    Ok(vec![session(
        "session.events.set_subscription.response",
        json!({"requestId": request.request_id, "subscriptionId": id}),
    )])
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Heartbeat {
    #[serde(default)]
    focused_agent_id: Option<String>,
    last_activity_at: String,
    app_visible: bool,
}

/// `client_heartbeat`: where the person is in the app, for notifications,
/// and a renewal of the push token this client registered.
fn heartbeat(daemon: &Daemon, subs: &Subscriptions, beat: Heartbeat) -> Vec<Value> {
    let presence = crate::push::Presence {
        app_visible: beat.app_visible,
        focused_agent_id: beat.focused_agent_id.filter(|id| !id.is_empty()),
        last_activity_ms: clock::parse_iso_ms(&beat.last_activity_at),
    };
    if let Some(token) = daemon.clients.heartbeat(subs.connection(), presence) {
        daemon.push_tokens.renew(&token);
    }
    Vec::new()
}

#[derive(Deserialize)]
struct PushToken {
    token: String,
}

/// `register_push_token`: a phone's Expo token, leased for 48 hours.
fn register_push(daemon: &Daemon, subs: &Subscriptions, push: PushToken) -> Vec<Value> {
    let token = push.token.trim().to_string();
    if !token.is_empty() {
        daemon.push_tokens.renew(&token);
        daemon.clients.set_token(subs.connection(), Some(token));
    }
    Vec::new()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushUnregister {
    request_id: String,
    token: String,
}

/// `push.unregister.request`: notifications turned off on the phone.
fn unregister_push(daemon: &Daemon, request: PushUnregister) -> Vec<Value> {
    daemon.push_tokens.revoke(&request.token);
    daemon.clients.clear_token(&request.token);
    vec![session(
        "push.unregister.response",
        json!({"requestId": request.request_id}),
    )]
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseRequest {
    request_id: String,
    subscription_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileExplorerRequest {
    request_id: String,
    cwd: String,
    #[serde(default)]
    path: Option<String>,
    mode: String,
    #[serde(default)]
    max_bytes: Option<u64>,
}

/// `file_explorer_request`, for the prompt images Burrow keeps and
/// nothing else; see [`crate::attachments`]. The image always comes back
/// base64 in the JSON reply, which Paseo's clients read whether or not
/// they offered to take binary frames.
fn read_file(daemon: &Daemon, request: FileExplorerRequest) -> Vec<Value> {
    let path = request.path.unwrap_or_else(|| ".".into());
    let read = match request.mode.as_str() {
        "file" => daemon
            .attachments
            .read(&request.cwd, &path, request.max_bytes),
        _ => Err("Burrow lists no directories".into()),
    };
    let (file, error) = match read {
        Ok(file) => (file, Value::Null),
        Err(error) => (Value::Null, error.into()),
    };
    vec![session(
        "file_explorer_response",
        json!({
            "cwd": request.cwd, "path": path, "mode": request.mode, "directory": null,
            "file": file, "error": error, "requestId": request.request_id,
        }),
    )]
}

fn release(daemon: &Daemon, subs: &mut Subscriptions, request: ReleaseRequest) -> Vec<Value> {
    subs.release(&request.subscription_id);
    daemon
        .clients
        .released(subs.connection(), &request.subscription_id);
    vec![session(
        "subscription.release.response",
        json!({"requestId": request.request_id, "subscriptionId": request.subscription_id}),
    )]
}
