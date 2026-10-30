//! Agent requests: parsing, running them through [`crate::agents`], and the
//! reply shapes Paseo uses for each.
//!
//! Requests that wait (create, send, cancel, archive, delete, refresh) run as tasks
//! and send their reply through the connection's `late` channel; the rest
//! answer at once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use burrow_protocol::codes::rpc;
use burrow_protocol::directory::AgentSnapshot;
use burrow_protocol::envelope::{rpc_error, session};
use burrow_provider::Image;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::warn;

use crate::agents::{self, CreateAgent, Setting};
use crate::daemon::Daemon;
use crate::git;
use crate::workspace_requests::{self, WorktreeSpec, make_worktree_workspace};
use crate::worktrees;

/// Where a request that finishes later sends its reply.
pub type Late = mpsc::UnboundedSender<Value>;

/// What the connection knows about its client that changes reply shapes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientShape {
    pub compact_provider_snapshots: bool,
}

/// The kinds this module answers.
pub const KINDS: &[&str] = &[
    "agent.create.request",
    "create_agent_request",
    "send_agent_message_request",
    "wait_for_finish_request",
    "agent.detach.request",
    "cancel_agent_request",
    "archive_agent_request",
    "delete_agent_request",
    "refresh_agent_request",
    "agent_permission_response",
    "clear_agent_attention",
    "update_agent_request",
    "set_agent_mode_request",
    "set_agent_model_request",
    "set_agent_thinking_request",
    "get_providers_snapshot_request",
    "refresh_providers_snapshot_request",
    "list_commands_request",
];

#[derive(Debug, Deserialize)]
pub(crate) struct ImagePayload {
    data: String,
    #[serde(rename = "mimeType")]
    mime_type: String,
}

pub(crate) fn images(images: Option<Vec<ImagePayload>>) -> Vec<Image> {
    images
        .unwrap_or_default()
        .into_iter()
        .map(|i| Image {
            data: i.data,
            mime_type: i.mime_type,
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentConfig {
    pub provider: String,
    pub cwd: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub mode_id: Option<String>,
    #[serde(default)]
    pub thinking_option_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Set in the agent's environment (`paseo run --env`).
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Create {
    request_id: String,
    config: AgentConfig,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    initial_prompt: Option<String>,
    #[serde(default)]
    client_message_id: Option<String>,
    #[serde(default)]
    images: Option<Vec<ImagePayload>>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    internal: Option<bool>,
    #[serde(default)]
    background: Option<bool>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    caller_agent_id: Option<String>,
    /// The legacy form of `git: {createWorktree, createNewBranch, ...}`.
    #[serde(default)]
    worktree_name: Option<String>,
    #[serde(default)]
    git: Option<GitOptions>,
    #[serde(default)]
    worktree: Option<WorktreeTarget>,
}

/// `GitSetupOptionsSchema`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitOptions {
    #[serde(default)]
    base_branch: Option<String>,
    #[serde(default)]
    create_new_branch: Option<bool>,
    #[serde(default)]
    new_branch_name: Option<String>,
    #[serde(default)]
    create_worktree: Option<bool>,
    #[serde(default)]
    worktree_slug: Option<String>,
    #[serde(default)]
    ref_name: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    checkout_source: Option<Value>,
    #[serde(default)]
    github_pr_number: Option<u64>,
}

/// `CreateAgentWorktreeTargetSchema`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub(crate) enum WorktreeTarget {
    #[serde(rename_all = "camelCase")]
    BranchOff {
        new_branch: String,
        #[serde(default)]
        base: Option<String>,
    },
    CheckoutBranch {
        branch: String,
    },
    #[serde(rename_all = "camelCase")]
    CheckoutPr {
        pr_number: u64,
    },
}

/// `normalizeAgentCreationGitOptions`: what the git options ask for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GitIntent {
    pub base_branch: Option<String>,
    pub create_new_branch: bool,
    pub new_branch_name: Option<String>,
    pub create_worktree: bool,
    pub worktree_slug: Option<String>,
    pub ref_name: Option<String>,
    pub action: Option<String>,
    pub change_request: bool,
}

pub(crate) fn git_intent(
    git: Option<GitOptions>,
    legacy_name: Option<String>,
    target: Option<WorktreeTarget>,
) -> Result<Option<GitIntent>, String> {
    use crate::worktrees::{assert_safe_ref, slugify, validate_slug};
    if target.is_some() && git.is_some() {
        return Err("create_agent_request worktree cannot be combined with git options".into());
    }
    let from_target = target.map(|target| match target {
        WorktreeTarget::BranchOff { new_branch, base } => GitOptions {
            create_worktree: Some(true),
            create_new_branch: Some(true),
            new_branch_name: Some(new_branch),
            base_branch: base.clone(),
            action: Some("branch-off".into()),
            ref_name: base,
            ..GitOptions::default()
        },
        WorktreeTarget::CheckoutBranch { branch } => GitOptions {
            create_worktree: Some(true),
            action: Some("checkout".into()),
            ref_name: Some(branch),
            ..GitOptions::default()
        },
        WorktreeTarget::CheckoutPr { pr_number } => GitOptions {
            create_worktree: Some(true),
            action: Some("checkout".into()),
            github_pr_number: Some(pr_number),
            ..GitOptions::default()
        },
    });
    let legacy = legacy_name
        .filter(|n| !n.is_empty())
        .map(|name| GitOptions {
            create_worktree: Some(true),
            create_new_branch: Some(true),
            new_branch_name: Some(name.clone()),
            worktree_slug: Some(name),
            ..GitOptions::default()
        });
    let Some(options) = from_target.or(git).or(legacy) else {
        return Ok(None);
    };
    let trimmed = |value: Option<String>| {
        value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let base_branch = trimmed(options.base_branch);
    let create_worktree = options.create_worktree == Some(true);
    let create_new_branch = options.create_new_branch == Some(true);
    let new_branch_name = options.new_branch_name.as_deref().map(slugify);
    let requested_slug = options.worktree_slug.as_deref().map(slugify);
    let worktree_slug = requested_slug.or(new_branch_name.clone());
    let ref_name = trimmed(options.ref_name);
    let change_request = options.checkout_source.is_some() || options.github_pr_number.is_some();
    if !create_worktree
        && !create_new_branch
        && base_branch.is_none()
        && ref_name.is_none()
        && options.action.is_none()
        && !change_request
    {
        return Ok(None);
    }
    if let Some(base) = &base_branch {
        assert_safe_ref(base, "base branch")?;
    }
    if create_new_branch {
        let name = new_branch_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .ok_or("New branch name is required")?;
        validate_slug(name).map_err(|error| format!("Invalid branch name: {error}"))?;
    }
    if let Some(slug) = &worktree_slug {
        validate_slug(slug).map_err(|error| format!("Invalid worktree name: {error}"))?;
    }
    Ok(Some(GitIntent {
        base_branch,
        create_new_branch,
        new_branch_name,
        create_worktree,
        worktree_slug,
        ref_name,
        action: options.action,
        change_request,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendRequest {
    request_id: String,
    agent_id: String,
    text: String,
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    images: Option<Vec<ImagePayload>>,
    #[serde(default)]
    active_turn_behavior: Option<String>,
    /// The agent sending it, when one is (Paseo 0.12's provenance).
    #[serde(default)]
    source_agent_id: Option<String>,
    /// Paseo 0.13: the mode to switch the agent to before the prompt.
    #[serde(default)]
    session_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaitRequest {
    request_id: String,
    agent_id: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentRequest {
    #[serde(default)]
    request_id: Option<String>,
    agent_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionResponse {
    agent_id: String,
    /// The permission id, not an RPC id.
    request_id: String,
    response: Value,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClearAttention {
    #[serde(default)]
    request_id: Option<String>,
    agent_id: OneOrMany,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAgent {
    request_id: String,
    agent_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMode {
    request_id: String,
    agent_id: String,
    mode_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetModel {
    request_id: String,
    agent_id: String,
    #[serde(default)]
    model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetThinking {
    request_id: String,
    agent_id: String,
    #[serde(default)]
    thinking_option_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshProviders {
    request_id: String,
    #[serde(default)]
    providers: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Plain {
    request_id: String,
    #[serde(default)]
    agent_id: Option<String>,
}

/// `None`: the message did not validate.
pub fn handle(
    daemon: &Arc<Daemon>,
    kind: &str,
    message: &Value,
    shape: ClientShape,
    late: &Late,
) -> Option<Vec<Value>> {
    fn parse<T: for<'de> Deserialize<'de>>(message: &Value) -> Option<T> {
        T::deserialize(message).ok()
    }
    let spawn =
        |future: std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Value>> + Send>>| {
            let late = late.clone();
            tokio::spawn(async move {
                for frame in future.await {
                    let _ = late.send(frame);
                }
            });
            Some(Vec::new())
        };
    let daemon = daemon.clone();
    match kind {
        "agent.create.request" | "create_agent_request" => {
            let request: Create = parse(message)?;
            let legacy = kind == "create_agent_request";
            spawn(Box::pin(
                async move { create(&daemon, request, legacy).await },
            ))
        }
        "send_agent_message_request" => {
            let request: SendRequest = parse(message)?;
            spawn(Box::pin(async move {
                // As Paseo: the mode changes first, and a mode the agent
                // cannot take fails the send.
                let switched = match request.session_mode.as_deref().filter(|m| !m.is_empty()) {
                    Some(mode) => {
                        agents::configure(&daemon, &request.agent_id, Setting::Mode(mode.into()))
                            .map(drop)
                    }
                    None => Ok(()),
                };
                if let Err(error) = switched {
                    return vec![session(
                        "send_agent_message_response",
                        json!({"requestId": request.request_id, "agentId": request.agent_id,
                               "accepted": false, "error": error}),
                    )];
                }
                let result = agents::send(
                    &daemon,
                    &request.agent_id,
                    &request.text,
                    request.message_id,
                    images(request.images),
                    request.active_turn_behavior.as_deref(),
                    request.source_agent_id.as_deref(),
                )
                .await;
                let (agent_id, error) = match result {
                    Ok(id) => (id, Value::Null),
                    Err(error) => (request.agent_id, error.into()),
                };
                vec![session(
                    "send_agent_message_response",
                    json!({"requestId": request.request_id, "agentId": agent_id,
                           "accepted": error.is_null(), "error": error}),
                )]
            }))
        }
        // `paseo send`, `paseo run` and `paseo wait` wait with it.
        "wait_for_finish_request" => {
            let request: WaitRequest = parse(message)?;
            spawn(Box::pin(async move {
                let mut payload =
                    agents::wait_for_finish(&daemon, &request.agent_id, request.timeout_ms).await;
                payload["requestId"] = request.request_id.into();
                vec![session("wait_for_finish_response", payload)]
            }))
        }
        "agent.detach.request" => {
            let request: AgentRequest = parse(message)?;
            let (agent_id, error) = match agents::detach(&daemon, &request.agent_id) {
                Ok(id) => (id, Value::Null),
                Err(error) => (request.agent_id, error.into()),
            };
            Some(vec![session(
                "agent.detach.response",
                json!({"requestId": request.request_id, "agentId": agent_id,
                       "accepted": error.is_null(), "error": error}),
            )])
        }
        "cancel_agent_request" => {
            let request: AgentRequest = parse(message)?;
            spawn(Box::pin(async move {
                let result = agents::cancel(&daemon, &request.agent_id).await;
                let Some(request_id) = request.request_id else {
                    return Vec::new();
                };
                let (agent, error) = match result {
                    Ok(agent) => (
                        serde_json::to_value(agent).unwrap_or(Value::Null),
                        Value::Null,
                    ),
                    Err(error) => (Value::Null, error.into()),
                };
                vec![session(
                    "cancel_agent_response",
                    json!({"requestId": request_id, "agentId": request.agent_id,
                           "agent": agent, "error": error}),
                )]
            }))
        }
        "archive_agent_request" => {
            let request: AgentRequest = parse(message)?;
            let request_id = request.request_id.clone()?;
            spawn(Box::pin(async move {
                match agents::archive(&daemon, &request.agent_id).await {
                    Ok(archived_at) => vec![session(
                        "agent_archived",
                        json!({"agentId": request.agent_id, "archivedAt": archived_at,
                               "requestId": request_id}),
                    )],
                    Err(error) => vec![rpc_error(
                        &request_id,
                        Some("archive_agent_request"),
                        &format!("Request failed: {error}"),
                        rpc::HANDLER_ERROR,
                    )],
                }
            }))
        }
        "delete_agent_request" => {
            let request: Plain = parse(message)?;
            let agent_id = request.agent_id?;
            spawn(Box::pin(async move {
                // The exact id only: nothing destructive by prefix or title.
                agents::delete(&daemon, &agent_id).await;
                vec![session(
                    "agent_deleted",
                    json!({"agentId": agent_id, "requestId": request.request_id}),
                )]
            }))
        }
        "refresh_agent_request" => {
            let request: AgentRequest = parse(message)?;
            spawn(Box::pin(async move {
                let result = agents::refresh(&daemon, &request.agent_id).await;
                let Some(request_id) = request.request_id else {
                    return Vec::new();
                };
                match result {
                    Ok(size) => vec![session(
                        "status",
                        json!({"status": "agent_refreshed", "agentId": request.agent_id,
                               "requestId": request_id, "timelineSize": size}),
                    )],
                    Err(error) => vec![rpc_error(
                        &request_id,
                        Some("refresh_agent_request"),
                        &error,
                        "agent_refresh_failed",
                    )],
                }
            }))
        }
        "agent_permission_response" => {
            let request: PermissionResponse = parse(message)?;
            // Through the late channel: the reply follows the resolution
            // events it caused, as in Paseo.
            let frames = match agents::respond_permission(
                &daemon,
                &request.agent_id,
                &request.request_id,
                request.response,
            ) {
                Ok(resolution) => vec![session(
                    "agent_permission_resolved",
                    json!({"agentId": request.agent_id, "requestId": request.request_id,
                               "resolution": resolution}),
                )],
                Err(error) => vec![rpc_error(
                    &request.request_id,
                    Some("agent_permission_response"),
                    &format!("Request failed: {error}"),
                    rpc::HANDLER_ERROR,
                )],
            };
            spawn(Box::pin(async move { frames }))
        }
        "clear_agent_attention" => {
            let request: ClearAttention = parse(message)?;
            let (ids, echo) = match request.agent_id {
                OneOrMany::One(id) => (vec![id.clone()], Value::String(id)),
                OneOrMany::Many(ids) => (ids.clone(), json!(ids)),
            };
            let agents = agents::clear_attention(&daemon, &ids);
            Some(match request.request_id {
                Some(request_id) => vec![session(
                    "clear_agent_attention_response",
                    json!({"requestId": request_id, "agentId": echo, "agents": agents}),
                )],
                None => Vec::new(),
            })
        }
        "update_agent_request" => {
            let request: UpdateAgent = parse(message)?;
            let result =
                agents::rename_and_label(&daemon, &request.agent_id, request.name, request.labels);
            Some(vec![accepted_reply(
                "update_agent_response",
                &request.request_id,
                &request.agent_id,
                result,
                false,
            )])
        }
        "set_agent_mode_request" => {
            let request: SetMode = parse(message)?;
            let result =
                agents::configure(&daemon, &request.agent_id, Setting::Mode(request.mode_id));
            Some(vec![accepted_reply(
                "set_agent_mode_response",
                &request.request_id,
                &request.agent_id,
                result,
                true,
            )])
        }
        "set_agent_model_request" => {
            let request: SetModel = parse(message)?;
            let result =
                agents::configure(&daemon, &request.agent_id, Setting::Model(request.model_id));
            Some(vec![accepted_reply(
                "set_agent_model_response",
                &request.request_id,
                &request.agent_id,
                result,
                false,
            )])
        }
        "set_agent_thinking_request" => {
            let request: SetThinking = parse(message)?;
            let result = agents::configure(
                &daemon,
                &request.agent_id,
                Setting::Thinking(request.thinking_option_id),
            );
            Some(vec![accepted_reply(
                "set_agent_thinking_response",
                &request.request_id,
                &request.agent_id,
                result,
                true,
            )])
        }
        "get_providers_snapshot_request" => {
            let request: Plain = parse(message)?;
            let mut payload = agents::providers_snapshot(&daemon, shape.compact_provider_snapshots);
            payload["requestId"] = request.request_id.into();
            Some(vec![session("get_providers_snapshot_response", payload)])
        }
        // Acknowledged at once; the snapshot follows as a
        // `providers_snapshot_update` once the probes are done, as in Paseo.
        "refresh_providers_snapshot_request" => {
            let request: RefreshProviders = parse(message)?;
            let daemon = daemon.clone();
            tokio::spawn(async move {
                crate::discovery::refresh(&daemon, request.providers.as_deref()).await;
            });
            Some(vec![session(
                "refresh_providers_snapshot_response",
                json!({"requestId": request.request_id, "acknowledged": true}),
            )])
        }
        "list_commands_request" => {
            let request: Plain = parse(message)?;
            Some(vec![session(
                "list_commands_response",
                json!({"requestId": request.request_id, "agentId": request.agent_id,
                       "commands": [], "error": null}),
            )])
        }
        _ => None,
    }
}

fn accepted_reply(
    kind: &str,
    request_id: &str,
    agent_id: &str,
    result: Result<String, String>,
    with_notice: bool,
) -> Value {
    let (agent_id, error) = match result {
        Ok(id) => (id, Value::Null),
        Err(error) => (agent_id.to_string(), Value::String(error)),
    };
    let mut payload = json!({"requestId": request_id, "agentId": agent_id,
                             "accepted": error.is_null(), "error": error});
    if with_notice {
        payload["notice"] = Value::Null;
    }
    session(kind, payload)
}

async fn create(daemon: &Arc<Daemon>, request: Create, legacy: bool) -> Vec<Value> {
    let request_id = request.request_id.clone();
    let idempotency_key = request.idempotency_key.clone();
    let result = create_with_git(daemon, request).await;
    if legacy {
        return vec![match result {
            Ok(agent) => session(
                "status",
                json!({"status": "agent_created", "agentId": agent.id,
                       "requestId": request_id, "agent": agent}),
            ),
            Err(error) => session(
                "status",
                json!({"status": "agent_create_failed", "requestId": request_id,
                       "error": error}),
            ),
        }];
    }
    vec![match result {
        Ok(agent) => {
            let key = idempotency_key.unwrap_or_else(|| request_id.clone());
            session(
                "agent.create.response",
                json!({
                    "requestId": request_id, "agent": agent, "error": null,
                    "creation": {
                        "kind": "agent", "idempotencyKey": key, "revision": 3,
                        "phase": "completed", "error": null,
                        "workspaceId": agent.workspace_id, "agentId": agent.id, "agent": agent,
                    },
                }),
            )
        }
        Err(error) => session(
            "agent.create.response",
            json!({"requestId": request_id, "agent": null, "error": error}),
        ),
    }]
}

/// `createSessionAgent`: the refusals, then the git options (a new worktree
/// and workspace, or a branch made or switched to in place), then the agent.
/// A worktree made for an agent that then could not start is archived again.
async fn create_with_git(daemon: &Arc<Daemon>, request: Create) -> Result<AgentSnapshot, String> {
    if request.internal == Some(true) {
        return Err(
            "Public internal agents were removed; create a background workspace instead.".into(),
        );
    }
    let has_git =
        request.git.is_some() || request.worktree.is_some() || request.worktree_name.is_some();
    let intent = git_intent(request.git, request.worktree_name, request.worktree.clone())?;
    let creates_worktree = intent.as_ref().is_some_and(|i| i.create_worktree);
    if request.background.is_some()
        && (request.workspace_id.is_some()
            || (request.caller_agent_id.is_some() && !creates_worktree))
    {
        return Err(
            "background is workspace creation intent; configure the selected workspace at creation."
                .into(),
        );
    }
    let mut cwd = git::resolve(Path::new(&git::expand_tilde(&request.config.cwd)))
        .to_string_lossy()
        .into_owned();
    let needs_directory =
        has_git || (request.workspace_id.is_none() && request.caller_agent_id.is_none());
    if needs_directory && !Path::new(&cwd).is_dir() {
        return Err(format!(
            "Working directory does not exist or is not a directory: {cwd}"
        ));
    }
    let mut workspace_id = request.workspace_id.clone();
    let mut worktree = None;
    if let Some(intent) = intent {
        if intent.create_worktree {
            let caller_workspace = match &request.caller_agent_id {
                Some(id) => daemon
                    .read(|store| store.agents.get(id).map(|a| a.workspace_id.clone()))
                    .ok_or_else(|| format!("Caller agent {id} not found"))?,
                None => None,
            };
            let spec = WorktreeSpec {
                cwd: cwd.clone(),
                worktree_slug: intent.worktree_slug,
                action: intent.action,
                ref_name: intent.ref_name,
                change_request: intent.change_request,
                base_branch: intent.base_branch,
                first_prompt: request.initial_prompt.clone(),
                background: request.background,
                caller_workspace_id: caller_workspace,
                ..WorktreeSpec::default()
            };
            let made = make_worktree_workspace(daemon, spec)
                .await
                .map_err(|error| error.message)?;
            cwd = made.workspace_cwd.clone();
            workspace_id = Some(made.workspace_id.clone());
            // A `worktree` target sets the worktree up as a workspace does;
            // the other forms report it on the agent's timeline.
            if request.worktree.is_some() {
                workspace_requests::start_workspace_setup(daemon, &made);
            }
            worktree = Some((made, request.worktree.is_none()));
        } else {
            let (dir, intent) = (PathBuf::from(&cwd), intent.clone());
            tokio::task::spawn_blocking(move || {
                if intent.create_new_branch {
                    let base = match intent.base_branch {
                        Some(base) => base,
                        None => worktrees::require_default_branch(&dir)?,
                    };
                    let name = intent.new_branch_name.unwrap_or_default();
                    worktrees::create_branch(&dir, &base, &name)
                } else if let Some(base) = intent.base_branch {
                    worktrees::checkout_existing(&dir, &base)
                } else {
                    Ok(())
                }
            })
            .await
            .map_err(|err| err.to_string())??;
        }
    }
    let created = agents::create(
        daemon,
        CreateAgent {
            provider: request.config.provider,
            cwd,
            model: request.config.model,
            mode_id: request.config.mode_id,
            thinking_option_id: request.config.thinking_option_id,
            title: request.config.title,
            workspace_id,
            labels: request.labels.unwrap_or_default(),
            initial_prompt: request.initial_prompt,
            client_message_id: request.client_message_id,
            images: images(request.images),
            system_prompt: request.config.system_prompt,
            env: request.config.env.unwrap_or_default(),
            background: request.background == Some(true),
            agent_id: request.agent_id,
            caller_agent_id: request.caller_agent_id,
        },
    )
    .await;
    match (created, worktree) {
        (Ok(agent), Some((made, on_timeline))) => {
            if on_timeline {
                workspace_requests::start_agent_setup(daemon, &made, &agent.id);
            }
            Ok(agent)
        }
        (Ok(agent), None) => Ok(agent),
        (Err(error), Some((made, _))) => {
            if let Err(cleanup) =
                workspace_requests::archive_workspace(daemon, &made.workspace_id).await
            {
                warn!(%cleanup, "the worktree made for a failed agent was not cleaned up");
            }
            Err(error)
        }
        (Err(error), None) => Err(error),
    }
}
