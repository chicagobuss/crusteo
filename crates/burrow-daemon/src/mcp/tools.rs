//! The tools' handlers: each reads its arguments as Paseo's schema has
//! them and does what Paseo's `paseo-tools.ts` does, through the same
//! operations the WebSocket requests use. Errors are the text the model
//! sees, as a thrown error is in Paseo.
//!
//! An agent caller's tools are scoped to it, as Paseo's are: what it makes
//! is its child in its workspace, paths are relative to its directory,
//! heartbeats are its own, and what it sends is from it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use burrow_protocol::directory::{AgentSnapshot, AgentStatus, WorkspaceKind};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::finish;
use crate::agents::{self, CreateAgent, Setting};
use crate::clock;
use crate::daemon::Daemon;
use crate::schedules::{self, Cadence, Schedule, Target};
use crate::store::WorkspaceRecord;

/// The tools Burrow serves, of Paseo's 40. Not here: terminals, workspace
/// scripts and `speak`, which need what Burrow does not have.
pub const NAMES: &[&str] = &[
    "list_hosts",
    "create_workspace",
    "list_workspaces",
    "archive_workspace",
    "create_agent",
    "send_agent_prompt",
    "get_agent_status",
    "list_agents",
    "cancel_agent",
    "archive_agent",
    "kill_agent",
    "update_agent",
    "rename_workspace",
    "create_schedule",
    "create_heartbeat",
    "delete_heartbeat",
    "list_schedules",
    "inspect_schedule",
    "pause_schedule",
    "resume_schedule",
    "delete_schedule",
    "update_schedule",
    "schedule_logs",
    "run_schedule_once",
    "list_providers",
    "list_models",
    "list_profiles",
    "inspect_provider",
    "get_agent_activity",
    "set_agent_mode",
    "list_pending_permissions",
    "respond_to_permission",
];

/// The tools that take a `serverId` to act on another host (Paseo's host
/// tools). Burrow has no connected hosts: only its own id is accepted.
const HOST_TOOLS: &[&str] = &[
    "create_agent",
    "create_workspace",
    "list_workspaces",
    "send_agent_prompt",
    "get_agent_status",
    "get_agent_activity",
    "list_agents",
];

const NOTIFY_GUIDANCE: &str = "You will get notified when the prompted agent finishes, errors, or needs permission. Do not poll for status; continue with other work until the notification arrives.";

type Outcome = Result<Value, String>;

/// Runs tool `name` for `caller` (an agent's id, or none for a top-level
/// client).
pub async fn call(daemon: &Arc<Daemon>, caller: Option<&str>, name: &str, args: Value) -> Outcome {
    let cx = Cx { daemon, caller };
    if HOST_TOOLS.contains(&name) {
        cx.check_host(&args)?;
    }
    let mut result = match name {
        "list_hosts" => cx.list_hosts(),
        "create_workspace" => cx.create_workspace(parse(args)?).await,
        "list_workspaces" => cx.list_workspaces(parse(args)?),
        "archive_workspace" => cx.archive_workspace(parse(args)?).await,
        "create_agent" => cx.create_agent(parse(args)?).await,
        "send_agent_prompt" => cx.send_agent_prompt(parse(args)?).await,
        "get_agent_status" => cx.get_agent_status(parse(args)?),
        "list_agents" => cx.list_agents(parse(args)?),
        "cancel_agent" => cx.cancel_agent(parse(args)?).await,
        "archive_agent" => {
            let AgentId { agent_id } = parse(args)?;
            agents::archive(daemon, &agent_id).await?;
            Ok(json!({"success": true}))
        }
        "kill_agent" => {
            let AgentId { agent_id } = parse(args)?;
            agents::close(daemon, &agent_id).await?;
            Ok(json!({"success": true}))
        }
        "update_agent" => cx.update_agent(parse(args)?),
        "rename_workspace" => cx.rename_workspace(parse(args)?),
        "set_agent_mode" => {
            let SetMode { agent_id, mode_id } = parse(args)?;
            agents::configure(daemon, &agent_id, Setting::Mode(mode_id.clone()))?;
            Ok(json!({"success": true, "newMode": mode_id}))
        }
        "list_pending_permissions" => Ok(cx.pending_permissions()),
        "respond_to_permission" => {
            let Respond {
                agent_id,
                request_id,
                response,
            } = parse(args)?;
            agents::respond_permission(daemon, &agent_id, &request_id, response)?;
            Ok(json!({"success": true}))
        }
        "get_agent_activity" => cx.get_agent_activity(parse(args)?),
        "list_providers" => Ok(json!({"providers": providers(daemon)})),
        "list_models" => cx.list_models(parse(args)?),
        // Burrow has no agent profiles (Paseo's `agentProfiles` setting).
        "list_profiles" => Ok(json!({"profiles": []})),
        "inspect_provider" => cx.inspect_provider(parse(args)?),
        "create_schedule" => cx.create_schedule(parse(args)?),
        "create_heartbeat" => cx.create_heartbeat(parse(args)?),
        "delete_heartbeat" => {
            let ScheduleId { id } = parse(args)?;
            cx.own_heartbeat(&id)?;
            daemon.schedules.remove(&id);
            Ok(json!({"success": true}))
        }
        "list_schedules" => {
            let all: Vec<Value> = (daemon.schedules.list().iter())
                .filter(|s| matches!(s.target, Target::NewAgent { .. }))
                .map(Schedule::summary)
                .collect();
            Ok(json!({"schedules": all}))
        }
        "inspect_schedule" => {
            let ScheduleId { id } = parse(args)?;
            value(&cx.new_agent_schedule(&id)?)
        }
        "pause_schedule" | "resume_schedule" => {
            let ScheduleId { id } = parse(args)?;
            cx.new_agent_schedule(&id)?;
            if name == "pause_schedule" {
                schedules::pause(daemon, &id)?;
            } else {
                schedules::resume(daemon, &id)?;
            }
            Ok(json!({"success": true}))
        }
        "delete_schedule" => {
            let ScheduleId { id } = parse(args)?;
            cx.new_agent_schedule(&id)?;
            daemon.schedules.remove(&id);
            Ok(json!({"success": true}))
        }
        "update_schedule" => cx.update_schedule(parse(args)?),
        "schedule_logs" => {
            let ScheduleId { id } = parse(args)?;
            let mut runs = cx.new_agent_schedule(&id)?.runs;
            runs.sort_by(|a, b| a.started_at.cmp(&b.started_at));
            Ok(json!({"runs": runs}))
        }
        "run_schedule_once" => {
            let ScheduleId { id } = parse(args)?;
            cx.new_agent_schedule(&id)?;
            value(&schedules::run_once(daemon, &id).await?)
        }
        _ => Err(format!("Paseo tool not found: {name}")),
    };
    // A host tool's answer names the host it ran on.
    if HOST_TOOLS.contains(&name)
        && let Ok(Value::Object(fields)) = &mut result
    {
        fields.insert("serverId".into(), daemon.config.server_id.clone().into());
    }
    result
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("Invalid arguments: {e}"))
}

fn value<T: serde::Serialize>(value: &T) -> Outcome {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

/// `~` expanded, as Paseo's `expandUserPath`.
fn expand(path: &str) -> String {
    match (path.strip_prefix('~'), std::env::var("HOME")) {
        (Some(rest), Ok(home)) if rest.is_empty() || rest.starts_with('/') => {
            format!("{home}{rest}")
        }
        _ => path.to_string(),
    }
}

/// `path` under `base` unless it is absolute.
fn under(base: &str, path: &str) -> String {
    let path = expand(path);
    if Path::new(&path).is_absolute() {
        path
    } else {
        Path::new(base).join(path).to_string_lossy().into_owned()
    }
}

/// Paseo's `parseDurationString`: seconds, or `2h30m`-style parts.
fn duration_ms(text: &str) -> Result<i64, String> {
    let text = text.trim();
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        return text
            .parse::<i64>()
            .map(|s| s * 1000)
            .map_err(|e| e.to_string());
    }
    let (mut total, mut digits, mut matched) = (0i64, String::new(), false);
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            _ => {
                digits.clear();
                continue;
            }
        };
        if let Ok(n) = digits.parse::<i64>() {
            total += n * unit;
            matched = true;
        }
        digits.clear();
    }
    if !matched {
        return Err(format!(
            "Invalid duration format: {text}. Use formats like: 5m, 30s, 1h, 2h30m"
        ));
    }
    Ok(total)
}

/// `provider` or `provider/model`.
fn provider_model(text: &str) -> Result<(String, Option<String>), String> {
    let text = text.trim();
    match text.split_once('/') {
        None if !text.is_empty() => Ok((text.to_string(), None)),
        Some((provider, model)) if !provider.trim().is_empty() && !model.trim().is_empty() => {
            Ok((provider.trim().into(), Some(model.trim().into())))
        }
        _ => Err("provider must be <provider> or <provider>/<model>".into()),
    }
}

fn status_name(status: AgentStatus) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

/// Paseo's `toWorkspaceAutomationSummary`.
fn workspace_summary(workspace: &WorkspaceRecord) -> Value {
    let kind = serde_json::to_value(workspace.kind).unwrap_or(Value::Null);
    let isolation = if workspace.kind == WorkspaceKind::Worktree {
        "worktree"
    } else {
        "local"
    };
    json!({
        "workspaceId": workspace.workspace_id, "projectId": workspace.project_id,
        "cwd": workspace.cwd, "isolation": isolation, "kind": kind,
        "title": workspace.title, "background": workspace.background,
    })
}

/// The plain providers snapshot as Paseo's `toProviderSummary`s.
fn providers(daemon: &Daemon) -> Vec<Value> {
    let snapshot = agents::providers_snapshot(daemon, false);
    (snapshot["entries"].as_array().into_iter().flatten())
        .map(|entry| {
            let status = match entry["status"].as_str() {
                Some("ready") | None => "available",
                Some(other) => other,
            };
            let mut summary = json!({
                "id": entry["provider"], "label": entry.get("label").unwrap_or(&entry["provider"]),
                "description": entry["description"].as_str().unwrap_or_default(),
                "enabled": entry["enabled"].as_bool().unwrap_or(true),
                "modes": entry.get("modes").cloned().unwrap_or(json!([])),
                "status": status,
            });
            if let Some(error) = entry["error"].as_str() {
                summary["error"] = error.into();
            }
            summary
        })
        .collect()
}

// --- arguments ---------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentId {
    agent_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMode {
    agent_id: String,
    mode_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Respond {
    agent_id: String,
    request_id: String,
    response: Value,
}

#[derive(Deserialize)]
struct ScheduleId {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkspaceArgs {
    isolation: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    background: Option<bool>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    worktree_slug: Option<String>,
    #[serde(default)]
    branch_name: Option<String>,
    #[serde(default)]
    base_branch: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    pr_number: Option<u64>,
    #[serde(default)]
    forge: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListWorkspacesArgs {
    #[serde(default)]
    include_background: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceId {
    workspace_id: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CreateSettings {
    #[serde(default)]
    mode_id: Option<String>,
    #[serde(default)]
    thinking_option_id: Option<String>,
    #[serde(default)]
    features: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateAgentArgs {
    title: String,
    provider: String,
    initial_prompt: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    settings: Option<CreateSettings>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    notify_on_finish: Option<bool>,
    /// Legacy top-level placement (Paseo's `COMPAT(nestedCreateAgentPlacement)`).
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendArgs {
    agent_id: String,
    prompt: String,
    #[serde(default)]
    session_mode: Option<String>,
    #[serde(default)]
    active_turn_behavior: Option<String>,
    #[serde(default)]
    notify_on_finish: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAgentsArgs {
    #[serde(default)]
    include_background: bool,
    #[serde(default)]
    include_archived: bool,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    since_hours: Option<i64>,
    #[serde(default)]
    statuses: Option<Vec<AgentStatus>>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateSettings {
    #[serde(default)]
    mode_id: Option<String>,
    #[serde(default, deserialize_with = "double")]
    model: Option<Option<String>>,
    #[serde(default, deserialize_with = "double")]
    thinking_option_id: Option<Option<String>>,
    #[serde(default)]
    features: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAgentArgs {
    agent_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    settings: Option<UpdateSettings>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    title: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityArgs {
    agent_id: String,
    #[serde(default)]
    limit: Option<f64>,
}

#[derive(Deserialize)]
struct ListModelsArgs {
    provider: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InspectProviderArgs {
    provider: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    settings: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateScheduleArgs {
    prompt: String,
    cron: String,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    isolation: Option<String>,
    #[serde(default)]
    max_runs: Option<u64>,
    #[serde(default)]
    expires_in: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeartbeatArgs {
    prompt: String,
    cron: String,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    max_runs: Option<u64>,
    #[serde(default)]
    expires_in: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateScheduleArgs {
    id: String,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default, deserialize_with = "double")]
    name: Option<Option<String>>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default, deserialize_with = "double")]
    max_runs: Option<Option<u64>>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default, deserialize_with = "double")]
    model: Option<Option<String>>,
    #[serde(default, deserialize_with = "double")]
    mode: Option<Option<String>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    expires_in: Option<String>,
    #[serde(default)]
    clear_expires: Option<bool>,
}

/// `null` or a value, told apart from a field left out.
fn double<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

// --- the handlers --------------------------------------------------------------------

struct Cx<'a> {
    daemon: &'a Arc<Daemon>,
    caller: Option<&'a str>,
}

impl Cx<'_> {
    /// The calling agent, when there is one; an error if it is gone.
    fn caller_agent(&self) -> Result<Option<AgentSnapshot>, String> {
        let Some(id) = self.caller else {
            return Ok(None);
        };
        self.daemon
            .read(|store| store.agents.get(id).cloned())
            .map(Some)
            .ok_or_else(|| format!("Parent agent {id} not found"))
    }

    /// A directory the call names: relative to the caller's own, which it
    /// defaults to; required without a caller.
    fn scoped_cwd(&self, requested: Option<&str>) -> Result<String, String> {
        let requested = requested.map(str::trim).filter(|p| !p.is_empty());
        match (self.caller_agent()?, requested) {
            (Some(parent), Some(path)) => Ok(under(&parent.cwd, path)),
            (Some(parent), None) => Ok(parent.cwd),
            (None, Some(path)) => Ok(expand(path)),
            (None, None) => Err("cwd is required outside an agent-scoped session".into()),
        }
    }

    fn check_host(&self, args: &Value) -> Result<(), String> {
        match args.get("serverId").and_then(Value::as_str) {
            Some(id) if id != self.daemon.config.server_id => Err(format!(
                "Host {id} is not registered; this host is {} (list_hosts)",
                self.daemon.config.server_id
            )),
            _ => Ok(()),
        }
    }

    fn list_hosts(&self) -> Outcome {
        let label = (self.daemon.config.hostname.clone())
            .unwrap_or_else(|| self.daemon.config.server_id.clone());
        Ok(json!({
            "self": {"serverId": self.daemon.config.server_id, "label": label},
            "hosts": [],
        }))
    }

    async fn create_workspace(&self, args: CreateWorkspaceArgs) -> Outcome {
        let background = match args.background {
            Some(background) => background,
            None => self.caller_agent()?.is_some_and(|caller| {
                let workspace = caller.workspace_id.clone().unwrap_or_default();
                self.daemon.read(|store| {
                    store
                        .workspaces
                        .get(&workspace)
                        .is_some_and(|w| w.background)
                })
            }),
        };
        let source = match args.isolation.as_str() {
            "local" => {
                let options = [
                    args.mode.is_some(),
                    args.worktree_slug.is_some(),
                    args.branch_name.is_some(),
                    args.base_branch.is_some(),
                    args.branch.is_some(),
                    args.pr_number.is_some(),
                    args.forge.is_some(),
                ];
                if options.contains(&true) {
                    return Err("Worktree options require isolation worktree".into());
                }
                let cwd = self.scoped_cwd(args.path.as_deref())?;
                if !Path::new(&cwd).is_dir() {
                    return Err(format!("Directory not found: {cwd}"));
                }
                json!({"kind": "directory", "path": cwd, "projectId": args.project_id})
            }
            "worktree" => {
                let cwd = match (&args.path, &args.project_id) {
                    (None, Some(project)) => self
                        .daemon
                        .read(|store| store.projects.get(project).map(|p| p.root_path.clone()))
                        .ok_or_else(|| format!("Project {project} not found"))?,
                    _ => self.scoped_cwd(args.path.as_deref())?,
                };
                let mut source = json!({"kind": "worktree", "cwd": cwd,
                    "projectId": args.project_id, "worktreeSlug": args.worktree_slug});
                match args.mode.as_deref().unwrap_or("branch-off") {
                    "branch-off" => {
                        if args.branch.is_some() || args.pr_number.is_some() || args.forge.is_some()
                        {
                            return Err(
                                "branch, prNumber, and forge require a checkout mode".into()
                            );
                        }
                        source["action"] = "branch-off".into();
                        source["branchName"] = args.branch_name.into();
                        source["refName"] = args.base_branch.into();
                    }
                    "checkout-branch" => {
                        let branch = args
                            .branch
                            .ok_or("branch is required for checkout-branch mode")?;
                        source["action"] = "checkout".into();
                        source["refName"] = branch.into();
                    }
                    "checkout-pr" => {
                        let number = args
                            .pr_number
                            .ok_or("prNumber is required for checkout-pr mode")?;
                        source["action"] = "checkout".into();
                        source["checkoutSource"] = json!({"kind": "change_request",
                            "forge": args.forge, "number": number});
                    }
                    other => return Err(format!("Unknown worktree mode: {other}")),
                }
                source
            }
            other => return Err(format!("Unknown isolation: {other}")),
        };
        let title = args
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let (workspace_id, _) =
            crate::workspace_requests::make_workspace(self.daemon, source, title, background, None)
                .await?;
        let workspace = self
            .daemon
            .read(|store| store.workspaces.get(&workspace_id).cloned())
            .ok_or("The workspace was not made")?;
        Ok(workspace_summary(&workspace))
    }

    fn list_workspaces(&self, args: ListWorkspacesArgs) -> Outcome {
        let workspaces: Vec<Value> = self.daemon.read(|store| {
            (store.workspaces.iter())
                .map(|(w, _)| w)
                .filter(|w| args.include_background || !w.background)
                .map(workspace_summary)
                .collect()
        });
        Ok(json!({"workspaces": workspaces}))
    }

    async fn archive_workspace(&self, args: WorkspaceId) -> Outcome {
        let id = args.workspace_id;
        let (workspace, agents) = self.daemon.read(|store| {
            let agents: Vec<String> = (store.agents.iter())
                .map(|(a, _)| a)
                .filter(|a| a.workspace_id.as_deref() == Some(&id))
                .map(|a| a.id.clone())
                .collect();
            (store.workspaces.get(&id).cloned(), agents)
        });
        let workspace = workspace.ok_or_else(|| format!("Workspace {id} not found"))?;
        crate::workspace_requests::archive_workspace(self.daemon, &id).await?;
        let removed = (workspace.git.as_ref())
            .filter(|git| git.paseo_owned)
            .is_some_and(|git| !Path::new(&git.worktree_root).exists());
        Ok(json!({"workspaceId": id, "archivedAgentIds": agents, "removedDirectory": removed}))
    }

    async fn create_agent(&self, args: CreateAgentArgs) -> Outcome {
        let title = args.title.trim().to_string();
        if title.is_empty() {
            return Err("Title is required".into());
        }
        if title.chars().count() > 60 {
            return Err("Title must be 60 characters or fewer".into());
        }
        let prompt = args.initial_prompt.trim().to_string();
        if prompt.is_empty() {
            return Err("initialPrompt is required".into());
        }
        let (provider, model) = provider_model(&args.provider)?;
        let model = model.ok_or("provider must be provider/model, for example codex/gpt-5.4")?;
        let settings = args.settings.unwrap_or_default();
        if settings.features.as_ref().is_some_and(|f| !f.is_empty()) {
            return Err("Provider features are not supported by this host".into());
        }
        let caller = self.caller_agent()?;
        let notify = args.notify_on_finish.unwrap_or(caller.is_some()) && caller.is_some();
        let (workspace_id, cwd) = match (&args.workspace_id, &caller, &args.cwd) {
            (Some(id), _, _) => {
                let workspace = (self.daemon)
                    .read(|store| store.workspaces.get(id).cloned())
                    .ok_or_else(|| format!("Workspace {id} not found"))?;
                (Some(id.clone()), workspace.cwd)
            }
            (None, Some(parent), _) => {
                let workspace = (parent.workspace_id.clone()).ok_or_else(|| {
                    format!("Caller agent {} has no current workspace", parent.id)
                })?;
                (Some(workspace), parent.cwd.clone())
            }
            // A top-level call without a workspace gets a new local one, for
            // its (legacy) `cwd` or the daemon's own directory.
            (None, None, cwd) => {
                let cwd = match cwd {
                    Some(cwd) => expand(cwd.trim()),
                    None => std::env::current_dir()
                        .map(|d| d.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "/".into()),
                };
                let source = json!({"kind": "directory", "path": cwd});
                let (id, dir) = crate::workspace_requests::make_workspace(
                    self.daemon,
                    source,
                    None,
                    false,
                    Some(&prompt),
                )
                .await?;
                (Some(id), dir)
            }
        };
        let mut labels = args.labels;
        if caller.is_some() {
            labels.insert(
                "paseo.parent-server-id".into(),
                self.daemon.config.server_id.clone(),
            );
        }
        let request = CreateAgent {
            provider,
            cwd,
            model: Some(model),
            mode_id: settings.mode_id,
            thinking_option_id: settings.thinking_option_id,
            title: Some(title),
            workspace_id,
            labels,
            initial_prompt: Some(prompt),
            client_message_id: None,
            images: Vec::new(),
            system_prompt: None,
            env: BTreeMap::new(),
            background: false,
            agent_id: None,
            caller_agent_id: self.caller.map(str::to_string),
        };
        let agent = agents::create(self.daemon, request).await?;
        if let (true, Some(caller)) = (notify, self.caller) {
            finish::register(self.daemon, caller, &agent.id);
        }
        let guidance = if notify {
            "You will get notified when the created agent finishes, errors, or needs permission. Do not poll for status; continue with other work until the notification arrives."
        } else {
            "Creation acknowledged. Observe completion using get_agent_status/get_agent_activity or SDK/RPC subscriptions and wait APIs."
        };
        let mut result = json!({
            "agentId": agent.id, "type": agent.provider, "status": status_name(agent.status),
            "cwd": agent.cwd, "currentModeId": agent.current_mode_id,
            "availableModes": agent.available_modes, "guidance": guidance,
        });
        if let Some(workspace) = &agent.workspace_id {
            result["workspaceId"] = workspace.clone().into();
        }
        Ok(result)
    }

    async fn send_agent_prompt(&self, args: SendArgs) -> Outcome {
        let exists = self
            .daemon
            .read(|store| store.find_agent(&args.agent_id).is_some());
        if !exists {
            return Err(format!("Agent {} not found", args.agent_id));
        }
        if let Some(mode) = &args.session_mode {
            agents::configure(self.daemon, &args.agent_id, Setting::Mode(mode.clone()))?;
        }
        let behavior = (args.active_turn_behavior.clone()).unwrap_or_else(|| {
            if self.caller.is_some() {
                "steer"
            } else {
                "interrupt"
            }
            .into()
        });
        let steer = (behavior == "steer").then_some("steer");
        let notify = self.caller.is_some() && args.notify_on_finish.unwrap_or(true);
        let status = |cx: &Self| {
            cx.daemon
                .read(|store| {
                    store
                        .find_agent(&args.agent_id)
                        .map(|a| status_name(a.status))
                })
                .unwrap_or_else(|| "idle".into())
        };
        let Some(caller) = self.caller else {
            let busy = self.daemon.read(|store| {
                store
                    .agents
                    .get(&args.agent_id)
                    .is_some_and(|a| a.active_turn.is_some())
            });
            agents::send(
                self.daemon,
                &args.agent_id,
                &args.prompt,
                None,
                Vec::new(),
                steer,
                None,
            )
            .await?;
            let disposition = if busy && steer.is_some() {
                "steered"
            } else {
                "turn_started"
            };
            return Ok(json!({
                "success": true, "disposition": disposition, "status": status(self),
                "guidance": "Dispatch acknowledged. Observe completion using get_agent_status/get_agent_activity or SDK/RPC subscriptions and wait APIs.",
            }));
        };
        // From an agent, it is a message from that agent, delivered in the
        // background; the reply is the receipt.
        let message_id = uuid::Uuid::new_v4().to_string();
        let (daemon, caller, target, prompt, id) = (
            self.daemon.clone(),
            caller.to_string(),
            args.agent_id.clone(),
            args.prompt.clone(),
            message_id.clone(),
        );
        tokio::spawn(async move {
            let sent = agents::send(
                &daemon,
                &target,
                &prompt,
                Some(id),
                Vec::new(),
                steer,
                Some(&caller),
            )
            .await;
            match sent {
                Ok(_) if notify => finish::register(&daemon, &caller, &target),
                Ok(_) => {}
                Err(error) => tracing::warn!(target, %error, "a tool's message was not delivered"),
            }
        });
        let guidance = if notify {
            format!("Queued for delivery. {NOTIFY_GUIDANCE}")
        } else {
            "Queued for delivery; provider dispatch has not occurred.".into()
        };
        Ok(json!({
            "success": true, "deliveryStatus": "queued", "messageId": message_id,
            "status": status(self), "guidance": guidance,
        }))
    }

    fn get_agent_status(&self, args: AgentId) -> Outcome {
        let agent = self
            .daemon
            .read(|store| store.find_agent(&args.agent_id).cloned())
            .ok_or_else(|| format!("Agent {} not found", args.agent_id))?;
        Ok(json!({"status": status_name(agent.status), "snapshot": agent}))
    }

    /// Paseo's `selectAgentListItems`: attention first, then running, then
    /// by latest activity; archived ones only when recent. An agent
    /// caller's list is of its own directory unless it names another.
    fn list_agents(&self, args: ListAgentsArgs) -> Outcome {
        let cwd = match args.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(cwd) => Some(expand(cwd)),
            None => self.caller_agent()?.map(|caller| caller.cwd),
        };
        let since = clock::now_ms() - args.since_hours.unwrap_or(48) * 3_600_000;
        let limit = args.limit.unwrap_or(50);
        let mut agents: Vec<AgentSnapshot> = self.daemon.read(|store| {
            let active = (store.agents.iter()).map(|(a, _)| a);
            let archived = (store.archived.values()).filter(|_| args.include_archived);
            active
                .chain(archived)
                .filter(|a| args.include_background || !store.in_background(a))
                .cloned()
                .collect()
        });
        let activity = |a: &AgentSnapshot| {
            [
                Some(a.updated_at.as_str()),
                a.last_user_message_at.as_deref(),
                a.attention_timestamp.as_deref(),
                a.archived_at.as_deref(),
                Some(a.created_at.as_str()),
            ]
            .into_iter()
            .flatten()
            .filter_map(clock::parse_iso_ms)
            .max()
            .unwrap_or(0)
        };
        let rank = |status: AgentStatus| match status {
            AgentStatus::Running => 0,
            AgentStatus::Initializing => 1,
            AgentStatus::Idle => 2,
            AgentStatus::Error => 3,
            AgentStatus::Closed => 4,
        };
        agents.retain(|a| {
            cwd.as_ref()
                .is_none_or(|cwd| a.cwd == *cwd || Path::new(&a.cwd).starts_with(cwd))
                && (args.statuses.as_ref()).is_none_or(|s| s.is_empty() || s.contains(&a.status))
                && (a.archived_at.is_none() || activity(a) >= since)
        });
        agents.sort_by(|a, b| {
            (b.requires_attention.cmp(&a.requires_attention))
                .then(rank(a.status).cmp(&rank(b.status)))
                .then(activity(b).cmp(&activity(a)))
        });
        let items: Vec<Value> = (agents.iter().take(limit))
            .map(|a| {
                let model = (a.runtime_info.as_ref())
                    .and_then(|r| r["model"].as_str().map(str::to_string))
                    .or_else(|| a.model.clone());
                json!({
                    "id": a.id, "shortId": a.id.chars().take(7).collect::<String>(),
                    "title": a.title, "provider": a.provider, "model": model,
                    "thinkingOptionId": a.thinking_option_id,
                    "effectiveThinkingOptionId": a.effective_thinking_option_id,
                    "status": status_name(a.status), "cwd": a.cwd,
                    "createdAt": a.created_at, "updatedAt": a.updated_at,
                    "lastUserMessageAt": a.last_user_message_at, "archivedAt": a.archived_at,
                    "requiresAttention": a.requires_attention,
                    "attentionReason": a.attention_reason,
                    "attentionTimestamp": a.attention_timestamp, "labels": a.labels,
                })
            })
            .collect();
        Ok(json!({"agents": items}))
    }

    async fn cancel_agent(&self, args: AgentId) -> Outcome {
        let running = self.daemon.read(|store| {
            store
                .agents
                .get(&args.agent_id)
                .is_some_and(|a| a.active_turn.is_some())
        });
        agents::cancel(self.daemon, &args.agent_id).await?;
        Ok(json!({"success": running}))
    }

    fn update_agent(&self, args: UpdateAgentArgs) -> Outcome {
        let settings = args.settings.unwrap_or_default();
        if settings.features.as_ref().is_some_and(|f| !f.is_empty()) {
            return Err("Provider features are not supported by this host".into());
        }
        if let Some(mode) = settings.mode_id {
            agents::configure(self.daemon, &args.agent_id, Setting::Mode(mode))?;
        }
        if let Some(model) = settings.model {
            agents::configure(self.daemon, &args.agent_id, Setting::Model(model))?;
        }
        if let Some(thinking) = settings.thinking_option_id {
            agents::configure(self.daemon, &args.agent_id, Setting::Thinking(thinking))?;
        }
        let named = args.name.as_deref().is_some_and(|n| !n.trim().is_empty());
        let labelled = args.labels.as_ref().is_some_and(|l| !l.is_empty());
        if named || labelled {
            agents::rename_and_label(self.daemon, &args.agent_id, args.name, args.labels)?;
        }
        Ok(json!({"success": true}))
    }

    fn rename_workspace(&self, args: RenameArgs) -> Outcome {
        let title = args.title.trim().to_string();
        if title.is_empty() {
            return Err("title is required".into());
        }
        let id = match args
            .workspace_id
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty())
        {
            Some(id) => id,
            None => match self.caller_agent()? {
                Some(caller) => caller.workspace_id.ok_or_else(|| {
                    format!("Caller agent {} has no current workspace", caller.id)
                })?,
                None => {
                    return Err("workspaceId is required outside an agent-scoped session".into());
                }
            },
        };
        let renamed = self
            .daemon
            .apply(|store| match store.workspaces.get(&id).cloned() {
                Some(mut workspace) => {
                    workspace.title = Some(title.clone());
                    (true, store.upsert_workspace(workspace))
                }
                None => (false, Vec::new()),
            });
        if !renamed {
            let archived = self
                .daemon
                .read(|store| store.archived_workspaces.contains_key(&id));
            return Err(if archived {
                format!("Workspace {id} is archived")
            } else {
                format!("Workspace {id} not found")
            });
        }
        Ok(json!({"success": true, "workspaceId": id, "title": title}))
    }

    fn pending_permissions(&self) -> Value {
        let permissions: Vec<Value> = self.daemon.read(|store| {
            (store.agents.iter())
                .map(|(a, _)| a)
                .flat_map(|a| {
                    (a.pending_permissions.iter()).map(move |request| {
                        json!({"agentId": a.id, "status": status_name(a.status), "request": request})
                    })
                })
                .collect()
        });
        json!({"permissions": permissions})
    }

    /// Paseo's `summarizeAgentActivity`, through crusteo's own timeline
    /// model: its rows folded as clients show them, one line or more each.
    fn get_agent_activity(&self, args: ActivityArgs) -> Outcome {
        let id = args.agent_id;
        let (agent, page) = self.daemon.apply(|store| {
            let agent = store.find_agent(&id).cloned();
            let page = agent
                .as_ref()
                .and_then(|_| store.ensure_timeline(&id))
                .map(|t| {
                    let tail = burrow_protocol::timeline::Direction::Tail;
                    let page = t.fetch(tail, None, usize::MAX, false, true);
                    let payload = json!({
                        "agentId": id, "direction": "tail", "reset": true, "epoch": page.epoch,
                        "window": page.window, "startCursor": page.start_cursor,
                        "endCursor": page.end_cursor, "hasOlder": false, "hasNewer": false,
                        "entries": page.entries,
                    });
                    (t.rows().len(), payload)
                });
            ((agent, page), Vec::new())
        });
        let agent = agent.ok_or_else(|| format!("Agent {id} not found"))?;
        let (updates, payload) = page.unwrap_or((0, json!({"entries": []})));
        let mut model = fleet::timeline::Timeline::default();
        model.apply_page(&payload);
        let items: Vec<&fleet::timeline::Item> = model.items().collect();
        let total = items.len();
        let limit = args.limit.filter(|l| *l >= 1.0).map(|l| l as usize);
        let shown = &items[total - limit.unwrap_or(total).min(total)..];
        let noun = if total == 1 { "activity" } else { "activities" };
        let header = match limit {
            Some(limit) if shown.len() < total => {
                format!(
                    "Showing {} of {total} {noun} (limited to {limit})",
                    shown.len()
                )
            }
            _ => format!("Showing all {total} {noun}"),
        };
        let lines = curate(shown, &agent.cwd);
        let text = if lines.is_empty() {
            "No activity to display.".to_string()
        } else {
            lines.join("\n")
        };
        Ok(json!({
            "agentId": id, "updateCount": updates, "currentModeId": agent.current_mode_id,
            "content": format!("{header}\n\n{text}"),
        }))
    }

    fn list_models(&self, args: ListModelsArgs) -> Outcome {
        let snapshot = agents::providers_snapshot(self.daemon, false);
        let models = (snapshot["entries"].as_array().into_iter().flatten())
            .find(|entry| entry["provider"] == args.provider.as_str())
            .map(|entry| entry.get("models").cloned().unwrap_or(json!([])))
            .ok_or_else(|| format!("Provider '{}' is not available", args.provider))?;
        Ok(json!({"provider": args.provider, "models": models}))
    }

    fn inspect_provider(&self, args: InspectProviderArgs) -> Outcome {
        let (provider, model) = provider_model(&args.provider)?;
        self.scoped_cwd(args.cwd.as_deref())?;
        let summary = providers(self.daemon)
            .into_iter()
            .find(|p| p["id"] == provider.as_str())
            .ok_or_else(|| format!("Provider '{provider}' is unavailable"))?;
        if summary["enabled"] == false {
            return Err(format!("Provider '{provider}' is disabled"));
        }
        if summary["status"] != "available" {
            return Err((summary["error"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Provider '{provider}' is unavailable")));
        }
        let selected = (args.settings.as_ref())
            .and_then(|s| s.get("model").and_then(Value::as_str).map(str::to_string))
            .or(model);
        Ok(json!({
            "provider": provider, "label": summary["label"], "description": summary["description"],
            "enabled": summary["enabled"], "status": summary["status"], "modes": summary["modes"],
            "selectedModel": selected, "features": [],
        }))
    }

    fn cadence(cron: &str, timezone: Option<&str>) -> Result<Cadence, String> {
        let expression = cron.trim();
        if expression.is_empty() {
            return Err("cron is required".into());
        }
        let cadence = Cadence::Cron {
            expression: expression.into(),
            timezone: timezone
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string),
        };
        schedules::validate(&cadence)?;
        Ok(cadence)
    }

    fn expiry(expires_in: Option<&str>) -> Result<Option<String>, String> {
        expires_in
            .map(|text| duration_ms(text).map(|ms| clock::iso_from_ms(clock::now_ms() + ms)))
            .transpose()
    }

    /// A new-agent schedule's target, from the caller's own setup when
    /// there is a caller (Paseo's `resolveNewAgentScheduleTarget`).
    fn new_agent_target(
        &self,
        provider: Option<&str>,
        cwd: Option<&str>,
        isolation: Option<&str>,
    ) -> Result<Target, String> {
        let mut config = Map::new();
        match self.caller_agent()? {
            Some(parent) => {
                let (provider, model) = match provider {
                    Some(text) => provider_model(text)?,
                    None => (parent.provider.clone(), parent.model.clone()),
                };
                if provider == parent.provider
                    && let Some(mode) = &parent.current_mode_id
                {
                    config.insert("modeId".into(), mode.clone().into());
                }
                if let Some(model) = model {
                    config.insert("model".into(), model.into());
                }
                if let Some(thinking) = &parent.thinking_option_id {
                    config.insert("thinkingOptionId".into(), thinking.clone().into());
                }
                let cwd = cwd.map(str::trim).filter(|c| !c.is_empty());
                config.insert("provider".into(), provider.into());
                config.insert(
                    "cwd".into(),
                    cwd.map(expand).unwrap_or_else(|| parent.cwd.clone()).into(),
                );
            }
            None => {
                let text = provider
                    .filter(|p| !p.trim().is_empty())
                    .ok_or("provider is required when target is new-agent")?;
                let (provider, model) = provider_model(text)?;
                let cwd = match cwd.map(str::trim).filter(|c| !c.is_empty()) {
                    Some(cwd) => expand(cwd),
                    None => std::env::current_dir()
                        .map(|d| d.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "/".into()),
                };
                config.insert("provider".into(), provider.into());
                config.insert("cwd".into(), cwd.into());
                if let Some(model) = model {
                    config.insert("model".into(), model.into());
                }
            }
        }
        if let Some(isolation) = isolation {
            config.insert("isolation".into(), isolation.into());
        }
        Ok(Target::NewAgent { config })
    }

    fn create_schedule(&self, args: CreateScheduleArgs) -> Outcome {
        let cadence = Self::cadence(&args.cron, args.timezone.as_deref())?;
        let target = self.new_agent_target(
            args.provider.as_deref(),
            args.cwd.as_deref(),
            args.isolation.as_deref(),
        )?;
        let expires = Self::expiry(args.expires_in.as_deref())?;
        let schedule = schedules::create_or_replace(
            self.daemon,
            args.name,
            &args.prompt,
            cadence,
            target,
            args.max_runs,
            expires,
        )?;
        Ok(schedule.summary())
    }

    fn create_heartbeat(&self, args: HeartbeatArgs) -> Outcome {
        let caller = self
            .caller
            .ok_or("create_heartbeat requires an agent-scoped session")?;
        self.caller_agent()?;
        let cadence = Self::cadence(&args.cron, args.timezone.as_deref())?;
        let expires = Self::expiry(args.expires_in.as_deref())?;
        let target = Target::Agent {
            agent_id: caller.to_string(),
        };
        let schedule = schedules::create_or_replace(
            self.daemon,
            args.name,
            &args.prompt,
            cadence,
            target,
            args.max_runs,
            expires,
        )?;
        Ok(schedule.summary())
    }

    /// A heartbeat of the caller's own.
    fn own_heartbeat(&self, id: &str) -> Result<Schedule, String> {
        let caller = self
            .caller
            .ok_or("Heartbeat operations require an agent-scoped session")?;
        let schedule = self.daemon.schedules.get_or_not_found(id)?;
        match &schedule.target {
            Target::Agent { agent_id } if agent_id == caller => Ok(schedule),
            Target::Agent { .. } => {
                Err(format!("Heartbeat {id} does not belong to caller {caller}"))
            }
            Target::NewAgent { .. } => Err(format!("Heartbeat not found: {id}")),
        }
    }

    /// A schedule that starts new agents (heartbeats are not "schedules"
    /// to these tools).
    fn new_agent_schedule(&self, id: &str) -> Result<Schedule, String> {
        let schedule = self.daemon.schedules.get_or_not_found(id)?;
        match schedule.target {
            Target::NewAgent { .. } => Ok(schedule),
            Target::Agent { .. } => Err(format!("Schedule not found: {id}")),
        }
    }

    fn update_schedule(&self, args: UpdateScheduleArgs) -> Outcome {
        self.new_agent_schedule(&args.id)?;
        let cron = args
            .cron
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty());
        let timezone = args
            .timezone
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        if timezone.is_some() && cron.is_none() {
            return Err("timezone can only be used with cron".into());
        }
        if args.expires_in.is_some() && args.clear_expires == Some(true) {
            return Err("Specify at most one of expiresIn or clearExpires".into());
        }
        let mut fields = json!({"scheduleId": args.id});
        if let Some(cron) = cron {
            fields["cadence"] =
                serde_json::to_value(Self::cadence(cron, timezone)?).map_err(|e| e.to_string())?;
        }
        if let Some(name) = args.name {
            fields["name"] = name.into();
        }
        if let Some(prompt) = args.prompt {
            fields["prompt"] = prompt.into();
        }
        if let Some(max_runs) = args.max_runs {
            fields["maxRuns"] = max_runs.into();
        }
        if let Some(at) = Self::expiry(args.expires_in.as_deref())? {
            fields["expiresAt"] = at.into();
        } else if args.clear_expires == Some(true) {
            fields["expiresAt"] = Value::Null;
        }
        let mut patch = Map::new();
        if let Some(text) = &args.provider {
            let (provider, model) = provider_model(text)?;
            patch.insert("provider".into(), provider.into());
            if let Some(model) = model {
                patch.insert("model".into(), model.into());
            }
        }
        if let Some(model) = args.model {
            patch.insert("model".into(), model.into());
        }
        if let Some(mode) = args.mode {
            patch.insert("modeId".into(), mode.into());
        }
        if let Some(cwd) = args.cwd {
            patch.insert("cwd".into(), cwd.into());
        }
        if !patch.is_empty() {
            fields["newAgentConfig"] = Value::Object(patch);
        }
        value(&schedules::update_from(self.daemon, fields)?)
    }
}

/// Paseo's `curateProjectedActivityEntries`, over crusteo's timeline
/// items and its tool labels.
fn curate(items: &[&fleet::timeline::Item], cwd: &str) -> Vec<String> {
    use fleet::timeline::ItemKind;
    let mut lines = Vec::new();
    for item in items {
        match &item.kind {
            ItemKind::User { text, .. } => lines.push(format!("[User] {}", text.trim())),
            ItemKind::Assistant { text, .. } if !text.trim().is_empty() => {
                lines.push(text.trim().to_string())
            }
            ItemKind::Thought { text, .. } if !text.trim().is_empty() => {
                lines.push(format!("[Thought] {}", text.trim()))
            }
            ItemKind::Tool(tool) => {
                let display = fleet::display::tool_display(tool, cwd);
                let summary = (display.summary.as_deref())
                    .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        if s.chars().count() > 200 {
                            format!("{}...", s.chars().take(197).collect::<String>())
                        } else {
                            s
                        }
                    });
                lines.push(match summary {
                    Some(summary) => format!("[{}] {summary}", display.name),
                    None => format!("[{}]", display.name),
                });
                if let Some(message) = &tool.agent_message
                    && !message.text.trim().is_empty()
                {
                    lines.push(message.text.trim().to_string());
                }
            }
            ItemKind::Todo { items, .. } => {
                lines.push("[Tasks]".into());
                for task in items {
                    let mark = if task.status == "completed" {
                        "[x]"
                    } else {
                        "[ ]"
                    };
                    lines.push(format!("- {mark} {}", task.text));
                }
            }
            ItemKind::Notification { level, message } if level == "error" => {
                lines.push(format!("[Error] {message}"))
            }
            ItemKind::Compaction { .. } => lines.push("[Compacted]".into()),
            _ => {}
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_paseos() {
        assert_eq!(duration_ms("90"), Ok(90_000));
        assert_eq!(duration_ms("2h30m"), Ok(9_000_000));
        assert_eq!(duration_ms("45s"), Ok(45_000));
        assert!(duration_ms("soon").is_err());
    }

    #[test]
    fn provider_and_model_split_on_the_slash() {
        assert_eq!(provider_model("codex"), Ok(("codex".into(), None)));
        assert_eq!(
            provider_model(" codex/gpt-6.1-sol "),
            Ok(("codex".into(), Some("gpt-6.1-sol".into())))
        );
        assert!(provider_model("codex/").is_err());
    }

    #[test]
    fn paths_resolve_against_the_caller() {
        let base = std::env::temp_dir().join("repo");
        let base = base.to_string_lossy();
        assert_eq!(
            Path::new(&under(&base, "sub")),
            Path::new(&*base).join("sub")
        );
        let elsewhere = std::env::temp_dir().join("elsewhere");
        let elsewhere = elsewhere.to_string_lossy();
        assert_eq!(under(&base, &elsewhere), elsewhere);
    }
}
