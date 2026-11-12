//! `workspace.create.request`: a new workspace for a folder or a new
//! worktree, never reusing one, with an initial agent when asked, its
//! idempotency key honoured and its phases published as they happen.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use burrow_protocol::directory::{ProjectKind, WorkspaceDescriptor};
use burrow_protocol::envelope::session;
use serde::Deserialize;
use serde_json::{Value, json};

use super::setup::start_workspace_setup;
use super::worktree::{MadeWorktree, WorktreeSpec, make_worktree_workspace};
use super::{blocking, is_dir, text};
use crate::agent_requests::{AgentConfig, ImagePayload, images};
use crate::agents::{self, CreateAgent, NewWorkspace};
use crate::daemon::Daemon;
use crate::git;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateWorkspace {
    request_id: String,
    source: WorkspaceSource,
    /// Read by the dispatcher, which checks its shape (`wks_` and 16 hex).
    #[serde(default)]
    pub(super) workspace_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    subscribe: Option<bool>,
    #[serde(default)]
    internal: Option<bool>,
    #[serde(default)]
    background: Option<bool>,
    #[serde(default)]
    caller_agent_id: Option<String>,
    #[serde(default)]
    first_agent_context: Option<FirstAgentContext>,
    #[serde(default)]
    agent: Option<InitialAgent>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FirstAgentContext {
    #[serde(default)]
    prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum WorkspaceSource {
    Directory {
        path: String,
        #[serde(default, rename = "projectId")]
        project_id: Option<String>,
    },
    Worktree(Box<WorktreeSource>),
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeSource {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    ref_name: Option<String>,
    #[serde(default)]
    base_branch: Option<String>,
    #[serde(default)]
    branch_name: Option<String>,
    #[serde(default)]
    checkout_source: Option<Value>,
    #[serde(default)]
    github_pr_number: Option<u64>,
    #[serde(default)]
    worktree_slug: Option<String>,
}

/// `WorkspaceInitialAgentSchema`: `agent.create.request` without its
/// request, workspace and git fields.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitialAgent {
    config: AgentConfig,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    initial_prompt: Option<String>,
    #[serde(default)]
    client_message_id: Option<String>,
    #[serde(default)]
    images: Option<Vec<ImagePayload>>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    caller_agent_id: Option<String>,
    #[serde(default)]
    background: Option<Value>,
}

/// A failure with Paseo's wire code, if it has one.
struct Failure {
    message: String,
    code: Option<&'static str>,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message,
            code: None,
        }
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

/// The creation snapshot (`CreationSnapshotSchema`) as it moves through its
/// phases; each change bumps `revision`, as Paseo's creation service does.
struct Creation {
    snapshot: Value,
    subscribed: bool,
    updates: Box<dyn Fn(Vec<Value>) + Send>,
}

impl Creation {
    fn publish(&mut self, changes: Value) {
        let revision = self.snapshot["revision"].as_u64().unwrap_or(0) + 1;
        if let (Some(snapshot), Value::Object(changes)) = (self.snapshot.as_object_mut(), changes) {
            snapshot.extend(changes);
        }
        self.snapshot["revision"] = revision.into();
        self.notify();
    }

    fn notify(&self) {
        if self.subscribed {
            (self.updates)(vec![session(
                "workspace.create.update",
                self.snapshot.clone(),
            )]);
        }
    }
}

/// The request as its idempotency key fingerprints it.
fn fingerprint(message: &Value) -> Value {
    let mut intent = message.clone();
    if let Some(fields) = intent.as_object_mut() {
        for key in ["type", "requestId", "subscribe", "idempotencyKey"] {
            fields.remove(key);
        }
    }
    intent
}

/// `workspace.create.request`: a new workspace for a folder, or for a new
/// git worktree, never reusing one; with an initial agent when asked.
pub(super) async fn create_workspace(
    daemon: &Arc<Daemon>,
    request: CreateWorkspace,
    message: &Value,
    updates: impl Fn(Vec<Value>) + Send + 'static,
) -> Vec<Value> {
    let request_id = request.request_id.clone();
    let key = request
        .idempotency_key
        .clone()
        .unwrap_or_else(|| request_id.clone());
    let print = fingerprint(message);
    if request.idempotency_key.is_some() {
        let stored = daemon
            .creations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned();
        match stored {
            Some((stored_print, mut reply)) if stored_print == print => {
                reply["message"]["payload"]["requestId"] = request_id.into();
                return vec![reply];
            }
            Some(_) => {
                return vec![session(
                    "workspace.create.response",
                    json!({"requestId": request_id, "workspace": null,
                           "error": "workspace_request_key_conflict", "setupTerminalId": null}),
                )];
            }
            None => {}
        }
    }
    let reply = Reply {
        request_id: request_id.clone(),
        keyed: request.idempotency_key.is_some(),
        key: key.clone(),
        print,
    };
    let agent_id =
        (request.agent.as_ref()).map(|a| a.agent_id.clone().unwrap_or_else(crate::ids::agent_id));
    let workspace_id = request
        .workspace_id
        .clone()
        .unwrap_or_else(crate::ids::workspace_id);
    let mut creation = Creation {
        snapshot: json!({
            "kind": "workspace", "idempotencyKey": key, "revision": 0, "phase": "accepted",
            "error": null, "workspaceId": workspace_id, "agentId": agent_id,
        }),
        subscribed: request.subscribe == Some(true),
        updates: Box::new(updates),
    };
    creation.notify();

    let provisioned = provision(daemon, &request, &workspace_id).await;
    let made = match provisioned {
        Ok(made) => made,
        Err(failure) => {
            // Paseo: only a refusal before anything was made has a known outcome.
            let known = matches!(
                failure.code,
                Some("directory_not_found" | "source_required")
            );
            let mut changes = json!({"phase": "failed", "error": failure.message,
                                     "failedStage": "workspace", "outcomeUnknown": !known});
            if let Some(code) = failure.code {
                changes["errorCode"] = code.into();
            }
            creation.publish(changes);
            return finish(daemon, reply, creation, None);
        }
    };
    let descriptor = describe(daemon, &made.workspace_id);
    creation.publish(json!({"phase": "workspace_ready", "workspace": descriptor,
                            "workspaceId": made.workspace_id}));
    if let Some(worktree) = &made.worktree {
        start_workspace_setup(daemon, worktree);
    }

    let mut agent = None;
    if let (Some(input), Some(agent_id)) = (request.agent, agent_id) {
        let workspace_dir = descriptor
            .as_ref()
            .map(|w| w.workspace_directory.clone())
            .unwrap_or_else(|| made.cwd.clone());
        match create_initial_agent(daemon, input, &made, &workspace_dir, &agent_id).await {
            Ok((snapshot, prompted)) => {
                let value = json!(snapshot);
                creation.publish(json!({"phase": "agent_ready", "agent": value,
                                        "workspaceId": made.workspace_id}));
                if prompted {
                    creation.publish(json!({"phase": "prompt_started", "agent": value}));
                }
                agent = Some(value);
            }
            Err(error) => {
                let unknown = daemon.read(|store| store.agent(&agent_id).is_some());
                creation.publish(
                    json!({"phase": "failed", "error": error, "failedStage": "agent",
                                        "outcomeUnknown": unknown}),
                );
                return finish(daemon, reply, creation, None);
            }
        }
    }
    creation.publish(json!({"phase": "completed"}));
    finish(daemon, reply, creation, agent)
}

/// A workspace for one run of a schedule that starts a new agent (Paseo's
/// `createDirectoryWorkspace` and `createPaseoWorktreeWorkspace`): a new
/// one for `cwd`, or for a new worktree of it. Returns its id and the
/// folder the agent runs in.
pub(crate) async fn workspace_for_run(
    daemon: &Arc<Daemon>,
    cwd: &str,
    worktree: bool,
    prompt: &str,
) -> Result<(String, String), String> {
    let source = if worktree {
        json!({"kind": "worktree", "cwd": cwd})
    } else {
        json!({"kind": "directory", "path": cwd})
    };
    make_workspace(daemon, source, None, false, Some(prompt)).await
}

/// A new workspace from a `workspace.create` source (`{kind: "directory",
/// path}` or `{kind: "worktree", cwd, action, …}`), as the request makes
/// one, for the agent tools and schedules. Returns its id and the folder
/// an agent in it runs in.
pub(crate) async fn make_workspace(
    daemon: &Arc<Daemon>,
    source: Value,
    title: Option<&str>,
    background: bool,
    prompt: Option<&str>,
) -> Result<(String, String), String> {
    let request: CreateWorkspace = serde_json::from_value(json!({
        "requestId": "tool", "source": source, "title": title, "background": background,
        "firstAgentContext": {"prompt": prompt},
    }))
    .map_err(|err| err.to_string())?;
    let made = provision(daemon, &request, &crate::ids::workspace_id())
        .await
        .map_err(|failure| failure.message)?;
    if let Some(worktree) = &made.worktree {
        start_workspace_setup(daemon, worktree);
    }
    let directory = describe(daemon, &made.workspace_id)
        .map(|w| w.workspace_directory)
        .unwrap_or(made.cwd);
    Ok((made.workspace_id, directory))
}

/// What [`finish`] needs to answer and keep the reply.
struct Reply {
    request_id: String,
    /// The client gave an idempotency key: keep the reply for its retries.
    keyed: bool,
    key: String,
    print: Value,
}

/// The `workspace.create.response`, kept for retries once its outcome is
/// final (completed, or failed with an unknown outcome), as Paseo keeps it.
fn finish(daemon: &Daemon, reply: Reply, creation: Creation, agent: Option<Value>) -> Vec<Value> {
    let snapshot = creation.snapshot;
    let mut payload = json!({
        "requestId": reply.request_id,
        "workspace": snapshot.get("workspace").cloned().unwrap_or(Value::Null),
        "creation": snapshot,
        "error": snapshot["error"],
        "setupTerminalId": null,
    });
    if let Some(agent) = agent {
        payload["agent"] = agent;
    }
    if let Some(code) = snapshot.get("errorCode") {
        payload["errorCode"] = code.clone();
    }
    let frame = session("workspace.create.response", payload);
    let keep = snapshot["phase"] == "completed" || snapshot["outcomeUnknown"] == true;
    if reply.keyed && keep {
        daemon
            .creations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(reply.key, (reply.print, frame.clone()));
    }
    vec![frame]
}

/// A workspace made by [`provision`].
struct Provisioned {
    workspace_id: String,
    cwd: String,
    /// The folder an initial agent's `cwd` is relative to.
    source: String,
    worktree: Option<MadeWorktree>,
}

fn describe(daemon: &Daemon, workspace_id: &str) -> Option<WorkspaceDescriptor> {
    daemon.read(|store| {
        store
            .workspaces
            .get(workspace_id)
            .map(|w| store.workspace_descriptor(w))
    })
}

/// `createRequestedWorkspace`: the refusals, then a folder's workspace or a
/// worktree's.
async fn provision(
    daemon: &Arc<Daemon>,
    request: &CreateWorkspace,
    workspace_id: &str,
) -> Result<Provisioned, Failure> {
    if request.internal == Some(true) {
        return Err(
            "Public internal workspaces were removed; use background workspace creation.".into(),
        );
    }
    if request
        .agent
        .as_ref()
        .is_some_and(|a| a.background.is_some())
    {
        return Err("Configure background on the workspace, not its initial agent.".into());
    }
    let caller_workspace = match &request.caller_agent_id {
        Some(id) => Some(
            daemon
                .read(|store| store.agents.get(id).map(|a| a.workspace_id.clone()))
                .ok_or_else(|| format!("Caller agent {id} not found"))?,
        ),
        None => None,
    }
    .flatten();
    let first_prompt = match &request.agent {
        Some(agent) => agent.initial_prompt.clone(),
        None => request
            .first_agent_context
            .as_ref()
            .and_then(|c| c.prompt.clone()),
    };
    let taken = daemon.read(|store| {
        store.workspaces.get(workspace_id).is_some()
            || store.archived_workspaces.contains_key(workspace_id)
    });
    if taken {
        return Err("workspace_id_conflict".into());
    }
    match &request.source {
        WorkspaceSource::Directory { path, project_id } => {
            let cwd = git::expand_tilde(path);
            if !is_dir(&cwd) {
                return Err(Failure {
                    message: format!("Directory not found: {cwd}"),
                    code: Some("directory_not_found"),
                });
            }
            let cwd = text(&git::resolve(Path::new(&cwd)));
            let probe_dir = PathBuf::from(&cwd);
            let facts = blocking(move || git::probe(&probe_dir)).await;
            let title = (request.title.as_deref().map(str::trim))
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .or_else(|| first_prompt.as_deref().and_then(agents::derive_title));
            let background = match (request.background, &caller_workspace) {
                (Some(background), _) => background,
                (None, Some(caller)) => daemon
                    .read(|store| store.workspaces.get(caller).map(|w| w.background))
                    .ok_or_else(|| format!("Caller workspace {caller} not found"))?,
                (None, None) => false,
            };
            // A named project has its kind and key brought up to date.
            let named_root = (project_id.as_ref()).and_then(|id| {
                daemon.read(|store| store.projects.get(id).map(|p| p.root_path.clone()))
            });
            let named_facts = match &named_root {
                Some(root) => {
                    let root = PathBuf::from(root);
                    blocking(move || git::probe(&root)).await
                }
                None => None,
            };
            let made = daemon.apply(|store| {
                let mut events = Vec::new();
                if let Some(id) = project_id {
                    let Some(project) = store.projects.get(id).cloned() else {
                        return (Err(format!("Unknown project: {id}")), events);
                    };
                    let kind = if named_facts.is_some() {
                        ProjectKind::Git
                    } else {
                        ProjectKind::NonGit
                    };
                    let key = crate::project_key::derive(
                        &project.root_path,
                        named_facts.as_ref(),
                        &store.server_id,
                    );
                    if kind != project.kind || key != project.project_key {
                        events.extend(store.upsert_project(crate::store::ProjectRecord {
                            kind,
                            project_key: key,
                            ..project
                        }));
                    }
                }
                let new = NewWorkspace {
                    cwd: cwd.clone(),
                    git: facts,
                    background,
                    title,
                    project_id: project_id.clone(),
                    base_branch: None,
                    workspace_id: Some(workspace_id.to_string()),
                };
                match agents::new_workspace(store, new) {
                    Ok((id, made)) => {
                        events.extend(made);
                        (Ok(id), events)
                    }
                    Err(error) => (Err(error), events),
                }
            })?;
            Ok(Provisioned {
                workspace_id: made,
                cwd: cwd.clone(),
                source: cwd,
                worktree: None,
            })
        }
        WorkspaceSource::Worktree(source) => {
            let cwd = match (&source.cwd, &source.project_id) {
                (Some(cwd), _) => git::expand_tilde(cwd),
                (None, Some(project_id)) => daemon
                    .read(|store| store.projects.get(project_id).map(|p| p.root_path.clone()))
                    .ok_or_else(|| format!("Project not found: {project_id}"))?,
                (None, None) => {
                    return Err(Failure {
                        message: "cwd or projectId is required for a worktree-backed workspace"
                            .into(),
                        code: Some("source_required"),
                    });
                }
            };
            let spec = WorktreeSpec {
                cwd: cwd.clone(),
                workspace_id: Some(workspace_id.to_string()),
                project_id: source.project_id.clone(),
                worktree_slug: source.worktree_slug.clone(),
                action: source.action.clone(),
                ref_name: source.ref_name.clone(),
                branch_name: source.branch_name.clone(),
                change_request: source.checkout_source.is_some()
                    || source.github_pr_number.is_some(),
                base_branch: source.base_branch.clone(),
                title: request.title.clone(),
                first_prompt,
                background: request.background,
                caller_workspace_id: caller_workspace,
            };
            let made = make_worktree_workspace(daemon, spec)
                .await
                .map_err(|error| Failure::from(error.message))?;
            Ok(Provisioned {
                workspace_id: made.workspace_id.clone(),
                cwd: made.workspace_cwd.clone(),
                source: text(&git::resolve(Path::new(&cwd))),
                worktree: Some(made),
            })
        }
    }
}

/// The initial agent, in the new workspace. Its `cwd` must be inside the
/// source folder; it then works in the workspace's directory, since Paseo
/// places an agent made for a workspace in that workspace's `cwd`. Returns
/// it and whether its first prompt went out.
async fn create_initial_agent(
    daemon: &Arc<Daemon>,
    input: InitialAgent,
    made: &Provisioned,
    workspace_dir: &str,
    agent_id: &str,
) -> Result<(burrow_protocol::directory::AgentSnapshot, bool), String> {
    let wanted = git::resolve(Path::new(&input.config.cwd));
    if !wanted.starts_with(git::resolve(Path::new(&made.source))) {
        return Err("Agent directory must be inside the workspace source".into());
    }
    let cwd = workspace_dir.to_string();
    let prompted = input
        .initial_prompt
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty())
        || input.images.as_ref().is_some_and(|i| !i.is_empty());
    let agent = agents::create(
        daemon,
        CreateAgent {
            provider: input.config.provider,
            cwd,
            model: input.config.model,
            mode_id: input.config.mode_id,
            thinking_option_id: input.config.thinking_option_id,
            title: input.config.title,
            workspace_id: Some(made.workspace_id.clone()),
            labels: input.labels.unwrap_or_default(),
            initial_prompt: input.initial_prompt,
            client_message_id: input.client_message_id,
            images: images(input.images),
            system_prompt: input.config.system_prompt,
            env: input.config.env.unwrap_or_default(),
            background: false,
            agent_id: Some(input.agent_id.unwrap_or_else(|| agent_id.to_string())),
            caller_agent_id: input.caller_agent_id,
        },
    )
    .await?;
    Ok((agent, prompted))
}
