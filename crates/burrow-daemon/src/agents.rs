//! The agent manager: provider sessions, turns, permissions and attention.
//!
//! Behaviour follows Paseo's `agent-manager.ts` (0.11), as documented in
//! `docs/burrow/design.md`:
//!
//! - **Turns.** A prompt opens `foreground-turn-N` (or the provider's own
//!   name for it: Codex's `codex-turn-N`, a UUID for ACP agents, as in
//!   Paseo); a turn the agent starts by
//!   itself is `autonomous-turn-N` (one counter, as in Paseo's Claude
//!   provider, continued across restarts from the stored timeline). The
//!   manager announces `turn_started` and records the `user_message` itself,
//!   so the provider's own turn start for that turn is ignored.
//! - **Sends while a turn runs** interrupt it (the default) or steer into it
//!   when the provider can. There is no daemon-side queue.
//! - **Coalescing:** text and tool updates go through [`Coalescer`].
//! - **Attention** latches: `finished` when a turn ends (cancels included),
//!   `error` when it fails; only clearing, or archiving, resets it. A
//!   permission request is announced but does not set it.
//! - **Permissions** never time out. An interrupt or a failed turn denies
//!   what is left; archive drops them silently.
//! - **Lazy load.** Agents start their provider on first use after a restart.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use burrow_protocol::directory::{
    ActiveTurn, AgentCapabilities, AgentMode, AgentSnapshot, AgentStatus, AttentionReason,
    ProjectKind, WorkspaceKind,
};
use burrow_provider::{Command, Image, Provider, ProviderEvent, ProviderInfo, SessionSpec};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::clock;
use crate::coalesce::{Coalescer, coalescable};
use crate::daemon::Daemon;
use crate::git::GitFacts;
use crate::ids;
use crate::store::{ProjectRecord, Store, StoreEvent, WorkspaceRecord};

/// How long an interrupt may take before the daemon gives up on it.
const INTERRUPT_GRACE: Duration = Duration::from_secs(15);
/// How long a session told to stop gets before its replacement starts
/// anyway. Codex's gives app-server 2 s to exit.
const SESSION_END_WAIT: Duration = Duration::from_secs(10);
/// Client message ids remembered per agent, to drop duplicate sends.
const REMEMBERED_SENDS: usize = 256;
const TITLE_LIMIT: usize = 60;
const PREVIEW_LIMIT: usize = 220;

/// The providers this daemon offers, and the agents running right now.
#[derive(Default)]
pub struct Agents {
    providers: RwLock<BTreeMap<String, Arc<dyn Provider>>>,
    live: Mutex<HashMap<String, Arc<Live>>>,
    /// Sessions let go of for being idle, until their CLI has exited: a
    /// send waits for that before it resumes the session in a new one.
    released: Mutex<HashMap<String, watch::Receiver<bool>>>,
    /// How each provider is found again ([`crate::discovery`]).
    pub(crate) watched: tokio::sync::Mutex<Vec<crate::discovery::Watched>>,
}

/// A running agent: the session's command channel and turn state.
struct Live {
    commands: mpsc::UnboundedSender<Command>,
    provider: Arc<dyn Provider>,
    info: ProviderInfo,
    /// Serialises sends, cancels and archives of one agent.
    gate: tokio::sync::Mutex<()>,
    state: Mutex<RunState>,
    /// Counts finished turns; waiters watch it to learn an interrupt landed.
    turns_done: watch::Sender<u64>,
    /// Set once the session has ended and its CLI is gone.
    ended: watch::Sender<bool>,
    /// When it last did anything: a provider event, or a send.
    active_at: Mutex<Instant>,
}

#[derive(Default)]
struct RunState {
    active_turn: Option<String>,
    /// Set while an interrupt makes way for a new prompt: the agent stays
    /// running and no attention is raised for the canceled turn.
    replacing: bool,
    turn_counter: u64,
    /// Pending permissions in order: the client-facing id, the provider's
    /// own id, and the request as clients saw it.
    permissions: Vec<(String, String, Value)>,
    recent_sends: VecDeque<String>,
    /// The session has work running outside a turn
    /// ([`ProviderEvent::BackgroundWork`]).
    background: bool,
}

impl Agents {
    pub fn register(&self, provider: Arc<dyn Provider>) {
        let id = provider.info().id;
        self.providers
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, provider);
    }

    pub fn provider(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
    }

    pub fn infos(&self) -> Vec<ProviderInfo> {
        self.all().iter().map(|p| p.info()).collect()
    }

    /// Whether a provider is still finding out what it offers.
    pub(crate) fn any_loading(&self) -> bool {
        self.all().iter().any(|p| p.loading())
    }

    fn all(&self) -> Vec<Arc<dyn Provider>> {
        self.providers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Whether the agent has a provider session running.
    pub fn is_live(&self, agent_id: &str) -> bool {
        self.live(agent_id).is_some()
    }

    fn live(&self, agent_id: &str) -> Option<Arc<Live>> {
        self.live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(agent_id)
            .cloned()
    }

    fn forget(&self, agent_id: &str) -> Option<Arc<Live>> {
        self.live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(agent_id)
    }

    /// Asks every running session to stop (daemon shutdown).
    pub fn shutdown(&self) {
        let live: Vec<_> = self
            .live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .collect();
        for (_, agent) in live {
            let _ = agent.commands.send(Command::Shutdown);
        }
    }

    /// [`shutdown`](Self::shutdown), then waits up to `within` for every
    /// session's CLI to be gone. Each CLI leads a process group of its own
    /// ([`burrow_provider::group`]), so a service manager stopping Burrow
    /// does not reach them; Burrow ends them before it exits.
    pub async fn stop_all(&self, within: Duration) {
        let ended: Vec<watch::Receiver<bool>> = (self.live.lock())
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(|agent| agent.ended.subscribe())
            .collect();
        self.shutdown();
        if ended.is_empty() {
            return;
        }
        let all = futures_util::future::join_all(ended.into_iter().map(|mut ended| async move {
            let _ = ended.wait_for(|ended| *ended).await;
        }));
        let _ = tokio::time::timeout(within, all).await;
        // What ignored the group's SIGTERM gets its SIGKILL before Burrow,
        // whose thread sends it, exits.
        tokio::time::sleep(burrow_provider::group::KILL_AFTER + Duration::from_millis(200)).await;
    }
}

// --- snapshot helpers ---------------------------------------------------

pub(crate) fn capabilities(info: &ProviderInfo) -> AgentCapabilities {
    let c = info.capabilities;
    AgentCapabilities {
        supports_streaming: c.streaming,
        supports_session_persistence: c.session_persistence,
        supports_dynamic_modes: c.dynamic_modes,
        supports_mcp_servers: c.mcp_servers,
        supports_reasoning_stream: c.reasoning_stream,
        supports_tool_invocations: c.tool_invocations,
    }
}

pub(crate) fn modes(info: &ProviderInfo) -> Vec<AgentMode> {
    info.modes
        .iter()
        .map(|m| AgentMode {
            id: m.id.clone(),
            label: m.label.clone(),
            description: m.description.clone(),
        })
        .collect()
}

/// Paseo's title rule: the first non-empty line, whitespace collapsed, at
/// most 60 characters.
pub fn derive_title(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = collapsed.chars().take(TITLE_LIMIT).collect();
    Some(cut.trim().to_string()).filter(|t| !t.is_empty())
}

/// Notification preview text: markdown markers stripped, whitespace
/// collapsed, at most 220 characters.
pub fn preview(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with("```") || line.chars().all(|c| c == '-' || c == '*' || c == '_') {
            plain.push(' ');
            continue;
        }
        let line = line.trim_start_matches(['#', '>', ' ']);
        let line = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .unwrap_or(line);
        plain.push_str(line);
        plain.push(' ');
    }
    let plain: String = plain.replace(['`', '*', '~'], "");
    let collapsed = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > PREVIEW_LIMIT {
        let cut: String = collapsed.chars().take(PREVIEW_LIMIT - 3).collect();
        format!("{}...", cut.trim_end())
    } else {
        collapsed
    }
}

/// Applies `change` to an active agent's snapshot, bumps `updatedAt`, saves
/// it and publishes the upsert. Returns the new snapshot.
fn update(
    daemon: &Daemon,
    agent_id: &str,
    change: impl FnOnce(&mut AgentSnapshot),
) -> Option<AgentSnapshot> {
    daemon.apply(|store| {
        let Some(mut agent) = store.agents.get(agent_id).cloned() else {
            return (None, Vec::new());
        };
        change(&mut agent);
        agent.updated_at = clock::after(&agent.updated_at);
        let events = store.upsert_agent(agent.clone());
        (Some(agent), events)
    })
}

fn publish(daemon: &Daemon, events: Vec<StoreEvent>) {
    daemon.apply(|_| ((), events));
}

fn stream(daemon: &Daemon, agent_id: &str, event: Value) {
    publish(
        daemon,
        vec![StoreEvent::Stream {
            agent_id: agent_id.into(),
            event,
            timestamp: clock::now_iso(),
        }],
    );
}

fn record(daemon: &Daemon, agent_id: &str, turn: Option<String>, item: Value) {
    daemon.apply(|store| {
        let event = store.append_timeline(agent_id, item, turn, clock::now_iso());
        ((), event.into_iter().collect())
    });
}

/// The highest `*-turn-N` number in an agent's stored timeline.
fn last_turn_number(store: &mut Store, agent_id: &str) -> u64 {
    store
        .ensure_timeline(agent_id)
        .map(|timeline| {
            timeline
                .rows()
                .iter()
                .filter_map(|row| row.turn_id.as_deref())
                .filter_map(|turn| turn.rsplit_once("-turn-"))
                .filter_map(|(_, n)| n.parse::<u64>().ok())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

fn last_assistant_text(store: &Store, agent_id: &str) -> String {
    let Some(timeline) = store.timeline(agent_id) else {
        return String::new();
    };
    let mut chunks: Vec<&str> = timeline
        .rows()
        .iter()
        .rev()
        .take_while(|row| row.item["type"] == "assistant_message")
        .filter_map(|row| row.item["text"].as_str())
        .collect();
    chunks.reverse();
    chunks.concat()
}

/// `agent_attention_required`, for event subscribers.
fn attention(daemon: &Daemon, agent: &AgentSnapshot, reason: AttentionReason) {
    if agent.labels.contains_key("paseo.parent-agent-id") {
        return;
    }
    let (reason_name, title, fallback) = match reason {
        AttentionReason::Finished => ("finished", "Agent finished", "Finished working."),
        AttentionReason::Permission => (
            "permission",
            "Agent needs permission",
            "Permission requested.",
        ),
        AttentionReason::Error => ("error", "Agent needs attention", "Encountered an error."),
    };
    let body = match reason {
        AttentionReason::Finished => {
            preview(&daemon.read(|store| last_assistant_text(store, &agent.id)))
        }
        AttentionReason::Permission => agent
            .pending_permissions
            .last()
            .map(permission_summary)
            .map(|s| preview(&s))
            .unwrap_or_default(),
        AttentionReason::Error => String::new(),
    };
    let body = if body.is_empty() {
        fallback.to_string()
    } else {
        body
    };
    let timestamp = clock::now_iso();
    publish(
        daemon,
        vec![StoreEvent::Session {
            kind: "agent_attention_required",
            payload: json!({
                "agentId": agent.id,
                "reason": reason_name,
                "timestamp": timestamp,
                "shouldNotify": false,
                "notification": {
                    "title": title,
                    "body": body,
                    "data": {
                        "serverId": daemon.config.server_id,
                        "workspaceId": agent.workspace_id,
                        "agentId": agent.id,
                        "reason": reason_name,
                    },
                },
            }),
        }],
    );
}

fn permission_summary(request: &Value) -> String {
    let parts: Vec<&str> = ["title", "description"]
        .iter()
        .filter_map(|key| request[*key].as_str())
        .filter(|s| !s.is_empty())
        .collect();
    if !parts.is_empty() {
        return parts.join(" - ");
    }
    for key in ["input", "metadata"] {
        if !request[key].is_null() {
            return request[key].to_string();
        }
    }
    request["name"]
        .as_str()
        .or(request["kind"].as_str())
        .unwrap_or_default()
        .to_string()
}

// --- starting and loading ---------------------------------------------------

/// What `agent.create.request` asks for.
#[derive(Debug, Clone, Default)]
pub struct CreateAgent {
    pub provider: String,
    pub cwd: String,
    pub model: Option<String>,
    pub mode_id: Option<String>,
    pub thinking_option_id: Option<String>,
    pub title: Option<String>,
    pub workspace_id: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub initial_prompt: Option<String>,
    pub client_message_id: Option<String>,
    pub images: Vec<Image>,
    pub system_prompt: Option<String>,
    /// Set in its CLI's environment (`paseo run --env`), kept with it.
    pub env: BTreeMap<String, String>,
    /// With no `workspace_id`: make a new background workspace for it.
    pub background: bool,
    /// The id the client chose for it (`agentId`), else a new UUID.
    pub agent_id: Option<String>,
    /// The agent that asks for it (`callerAgentId`): without a workspace it
    /// joins the caller's, in the caller's directory, as its child.
    pub caller_agent_id: Option<String>,
}

/// The label Paseo puts on an agent another agent made.
pub const PARENT_AGENT_LABEL: &str = "paseo.parent-agent-id";

/// Finds or makes the workspace an agent in `cwd` belongs to. Finding
/// skips background workspaces, as Paseo's lookup by path does, and a
/// `background` create always makes a new one: background is how a
/// workspace is created, not which one is picked.
pub(crate) fn placement(
    store: &mut Store,
    cwd: &str,
    git: Option<GitFacts>,
    background: bool,
) -> (String, Vec<StoreEvent>) {
    if !background
        && let Some((workspace, _)) =
            (store.workspaces.iter()).find(|(w, _)| w.cwd == cwd && !w.background)
    {
        return (workspace.workspace_id.clone(), Vec::new());
    }
    // Without a project id this cannot fail.
    new_workspace(
        store,
        NewWorkspace {
            cwd: cwd.to_string(),
            git,
            background,
            ..NewWorkspace::default()
        },
    )
    .unwrap_or_default()
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// `findOrCreateProjectForDirectory`: the active project rooted at `root`,
/// its kind and key brought up to date, or a new one. Returns its id.
pub(crate) fn find_or_create_project(
    store: &mut Store,
    root: &str,
    git: Option<&GitFacts>,
) -> (String, Vec<StoreEvent>) {
    let kind = if git.is_some() {
        ProjectKind::Git
    } else {
        ProjectKind::NonGit
    };
    let key = crate::project_key::derive(root, git, &store.server_id);
    let existing = (store.projects.iter())
        .find(|(p, _)| p.root_path == root)
        .map(|(p, _)| p.clone());
    match existing {
        Some(project) if project.kind == kind && project.project_key == key => {
            (project.project_id, Vec::new())
        }
        Some(project) => {
            let id = project.project_id.clone();
            let events = store.upsert_project(ProjectRecord {
                kind,
                project_key: key,
                ..project
            });
            (id, events)
        }
        None => {
            let id = ids::project_id();
            let events = store.upsert_project(ProjectRecord {
                project_id: id.clone(),
                project_key: key,
                root_path: root.to_string(),
                kind,
                display_name: basename(root),
                custom_name: None,
            });
            (id, events)
        }
    }
}

/// What a new workspace record holds.
#[derive(Debug, Clone, Default)]
pub(crate) struct NewWorkspace {
    pub cwd: String,
    pub git: Option<GitFacts>,
    pub background: bool,
    pub title: Option<String>,
    /// Its project; else the project rooted at `cwd`, made if missing.
    pub project_id: Option<String>,
    /// The exact ref a worktree Burrow made was cut from.
    pub base_branch: Option<String>,
    /// The id the client chose for it, else a new one.
    pub workspace_id: Option<String>,
}

/// Makes a workspace, as Paseo's `initialWorkspacePlacement` describes it:
/// a worktree when the checkout is a linked one, named after its branch.
pub(crate) fn new_workspace(
    store: &mut Store,
    workspace: NewWorkspace,
) -> Result<(String, Vec<StoreEvent>), String> {
    let mut events = Vec::new();
    let project_id = match workspace.project_id {
        Some(id) if store.projects.get(&id).is_some() => id,
        Some(id) => return Err(format!("Unknown project: {id}")),
        None => {
            let (id, made) = find_or_create_project(store, &workspace.cwd, workspace.git.as_ref());
            events.extend(made);
            id
        }
    };
    let kind = match &workspace.git {
        Some(git) if git.main_repo_root.is_some() => WorkspaceKind::Worktree,
        Some(_) => WorkspaceKind::LocalCheckout,
        None => WorkspaceKind::Directory,
    };
    let display_name = (workspace.git.as_ref())
        .and_then(|git| git.branch.clone())
        .filter(|branch| !branch.trim().is_empty() && !branch.eq_ignore_ascii_case("HEAD"))
        .unwrap_or_else(|| basename(&workspace.cwd));
    let workspace_id = workspace.workspace_id.unwrap_or_else(ids::workspace_id);
    events.extend(store.upsert_workspace(WorkspaceRecord {
        workspace_id: workspace_id.clone(),
        project_id,
        cwd: workspace.cwd,
        kind,
        display_name,
        title: workspace.title,
        pinned_at: None,
        labels: Vec::new(),
        git: workspace.git,
        background: workspace.background,
        base_branch: workspace.base_branch,
        archived_at: None,
        created_at: Some(clock::now_iso()),
        setup_status: None,
    }));
    Ok((workspace_id, events))
}

/// Creates an agent, starts its session and, with an initial prompt, its
/// first turn. Returns the agent as it stands when the turn has started.
pub async fn create(daemon: &Arc<Daemon>, request: CreateAgent) -> Result<AgentSnapshot, String> {
    let provider = daemon
        .agents
        .provider(&request.provider)
        .ok_or_else(|| format!("Provider '{}' is not available", request.provider))?;
    let mut request = request;
    if let Some(caller_id) = request.caller_agent_id.clone() {
        let caller = daemon
            .read(|store| store.agents.get(&caller_id).cloned())
            .ok_or_else(|| format!("Caller agent {caller_id} not found"))?;
        if request.workspace_id.is_none() {
            let workspace = caller
                .workspace_id
                .clone()
                .ok_or_else(|| format!("Caller agent {caller_id} has no workspace"))?;
            request.workspace_id = Some(workspace);
            request.cwd = caller.cwd.clone();
        }
        request
            .labels
            .insert(PARENT_AGENT_LABEL.to_string(), caller_id);
    }
    if !std::path::Path::new(&request.cwd).is_dir() {
        return Err(format!(
            "Working directory does not exist or is not a directory: {}",
            request.cwd
        ));
    }
    let info = provider.info();
    let agent_id = request.agent_id.clone().unwrap_or_else(ids::agent_id);
    if daemon.read(|store| store.agent(&agent_id).is_some()) {
        return Err("agent_id_conflict".into());
    }
    let title = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| request.initial_prompt.as_deref().and_then(derive_title));
    let now = clock::now_iso();
    let snapshot = AgentSnapshot {
        id: agent_id.clone(),
        provider: info.id.clone(),
        cwd: request.cwd.clone(),
        workspace_id: None,
        model: request.model.clone(),
        features: Some(Vec::new()),
        thinking_option_id: request.thinking_option_id.clone(),
        effective_thinking_option_id: request.thinking_option_id.clone(),
        created_at: now.clone(),
        updated_at: now,
        last_user_message_at: None,
        status: AgentStatus::Initializing,
        active_turn: None,
        capabilities: capabilities(&info),
        current_mode_id: request
            .mode_id
            .clone()
            .or_else(|| info.default_mode_id.clone()),
        available_modes: modes(&info),
        pending_permissions: Vec::new(),
        persistence: None,
        runtime_info: Some(json!({
            "provider": info.id, "sessionId": null, "model": null,
            "modeId": request.mode_id.clone().or_else(|| info.default_mode_id.clone()),
        })),
        last_usage: None,
        last_error: None,
        title,
        labels: request.labels.clone(),
        requires_attention: false,
        attention_reason: None,
        attention_timestamp: None,
        archived_at: None,
    };
    let workspace_id = request.workspace_id.clone();
    let background = request.background;
    let cwd = request.cwd.clone();
    let system_prompt = request.system_prompt.clone();
    let env = request.env.clone();
    let probe_dir = std::path::PathBuf::from(&cwd);
    let git = tokio::task::spawn_blocking(move || crate::git::probe(&probe_dir))
        .await
        .ok()
        .flatten();
    daemon.apply(|store| {
        let (workspace_id, mut events) = match workspace_id {
            Some(id) if store.workspaces.get(&id).is_some() => (id, Vec::new()),
            Some(id) => return (Err(format!("Workspace {id} not found")), Vec::new()),
            None => placement(store, &cwd, git, background),
        };
        let mut snapshot = snapshot;
        snapshot.workspace_id = Some(workspace_id);
        // What its CLI is started with, every time: the prompt and env asked for.
        let mut config = serde_json::Map::new();
        if let Some(prompt) = &system_prompt {
            config.insert("systemPrompt".into(), prompt.clone().into());
        }
        if !env.is_empty() {
            config.insert("env".into(), json!(env));
        }
        if !config.is_empty() {
            store.set_provider_config(&snapshot.id, Value::Object(config));
        }
        events.extend(store.upsert_agent(snapshot));
        (Ok(()), events)
    })?;
    ensure_live(daemon, &agent_id)?;
    if let Some(prompt) = request
        .initial_prompt
        .as_deref()
        .filter(|p| !p.trim().is_empty() || !request.images.is_empty())
    {
        // Paseo 0.12: a first prompt that fails does not undo the creation;
        // the failure shows on the agent's timeline.
        if let Err(error) = send(
            daemon,
            &agent_id,
            prompt,
            request.client_message_id.clone(),
            request.images.clone(),
            None,
            // An agent another made hears its first prompt from it.
            request.caller_agent_id.as_deref(),
        )
        .await
        {
            warn!(agent_id, %error, "the first prompt did not start");
        }
    }
    daemon
        .read(|store| store.agent(&agent_id).cloned())
        .ok_or_else(|| "The agent disappeared while starting".to_string())
}

/// What an agent's CLI does not inherit from Burrow's environment, as Paseo
/// takes them out of its own:
/// - `PASEO_HOME`, which beside the `PASEO_HOST` agents get makes Paseo's CLI
///   refuse to pick a daemon;
/// - what a Paseo agent, terminal, or the desktop app sets, should Burrow be
///   started from one;
/// - a Claude Code session's markers, which Paseo clears for every provider.
pub const AGENT_UNSET: &[&str] = &[
    "PASEO_HOME",
    "PASEO_WORKSPACE_ID",
    "PASEO_TERMINAL_ID",
    "PASEO_NODE_ENV",
    "PASEO_DESKTOP_MANAGED",
    "PASEO_SUPERVISED",
    "ELECTRON_RUN_AS_NODE",
    "ELECTRON_NO_ATTACH_CONSOLE",
    "ESBUILD_BINARY_PATH",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_AGENT_SDK_VERSION",
];

/// Where Paseo's desktop app keeps its `paseo` CLI. Under Paseo's own
/// daemon an agent finds it there; under Burrow it is put on the agent's
/// `PATH` when no `paseo` is on it already.
const PASEO_APP_CLI_DIRS: &[&str] = &["/Applications/Paseo.app/Contents/Resources/bin"];

/// The environment an agent's CLI gets, as Paseo gives it: who it is
/// (`PASEO_AGENT_ID`, `PASEO_AGENT_CWD`), which Paseo's CLI uses to say a
/// message is from it, to make its children its own and to steer rather
/// than interrupt; where its daemon is (`PASEO_HOST`), so a `paseo` command
/// it runs reaches Burrow without `--host`; what the client that made it
/// asked for (`paseo run --env`); and the Paseo app's CLI on its `PATH`.
fn agent_env(
    daemon: &Daemon,
    agent_id: &str,
    cwd: &str,
    provider_config: Option<&Value>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = (provider_config.and_then(|c| c["env"].as_object()))
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
        .collect();
    if let Some(path) = path_with_paseo_cli(std::env::var_os("PATH")) {
        env.push(("PATH".into(), path));
    }
    if let Some(endpoint) = daemon.local_endpoint.get() {
        env.push(("PASEO_HOST".into(), endpoint.clone()));
    }
    env.push(("PASEO_AGENT_ID".into(), agent_id.to_string()));
    env.push(("PASEO_AGENT_CWD".into(), cwd.to_string()));
    env
}

/// `path` with the Paseo app's CLI folder at the end, when no `paseo` is on
/// it and the app is installed; `None` when nothing needs adding.
fn path_with_paseo_cli(path: Option<std::ffi::OsString>) -> Option<String> {
    let path = path?.into_string().ok()?;
    let has = |dir: &std::path::Path| dir.join("paseo").is_file();
    if std::env::split_paths(&path).any(|dir| has(&dir)) {
        return None;
    }
    let app = PASEO_APP_CLI_DIRS
        .iter()
        .map(std::path::Path::new)
        .find(|dir| has(dir))?;
    Some(format!("{path}:{}", app.display()))
}

/// Starts the agent's provider session if it is not running (lazy load).
fn ensure_live(daemon: &Arc<Daemon>, agent_id: &str) -> Result<Arc<Live>, String> {
    let mut live_map = daemon.agents.live.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(live) = live_map.get(agent_id) {
        return Ok(live.clone());
    }
    let (snapshot, provider_config, turn_counter) = daemon.apply(|store| {
        let snapshot = store.agents.get(agent_id).cloned();
        let config = store.provider_config(agent_id).cloned();
        let counter = last_turn_number(store, agent_id);
        ((snapshot, config, counter), Vec::new())
    });
    let snapshot = snapshot.ok_or_else(|| format!("Agent not found: {agent_id}"))?;
    let provider = daemon.agents.provider(&snapshot.provider).ok_or_else(|| {
        format!(
            "Agent {agent_id} references unavailable provider '{}'",
            snapshot.provider
        )
    })?;
    let info = provider.info();
    let resume = snapshot
        .persistence
        .as_ref()
        .and_then(|p| p["sessionId"].as_str())
        .map(str::to_string);
    let spec = SessionSpec {
        cwd: snapshot.cwd.clone(),
        model: snapshot.model.clone(),
        mode_id: snapshot.current_mode_id.clone(),
        thinking_option_id: snapshot.thinking_option_id.clone(),
        resume,
        system_prompt: provider_config
            .as_ref()
            .and_then(|c| c["systemPrompt"].as_str())
            .map(str::to_string),
        env: agent_env(daemon, agent_id, &snapshot.cwd, provider_config.as_ref()),
        unset: AGENT_UNSET.iter().map(|key| key.to_string()).collect(),
    };
    // A session Burrow never recorded (started elsewhere): take its
    // conversation from the provider's own records, once.
    if let Some(session_id) = &spec.resume {
        let empty = daemon.apply(|store| {
            let empty = store
                .ensure_timeline(agent_id)
                .is_some_and(|t| t.rows().is_empty());
            (empty, Vec::new())
        });
        if empty {
            let history = provider.history(&spec.cwd, session_id);
            daemon.apply(|store| {
                let events = history
                    .into_iter()
                    .filter_map(|entry| {
                        let at = entry.timestamp.unwrap_or_else(clock::now_iso);
                        store.append_timeline(agent_id, entry.item, None, at)
                    })
                    .collect();
                ((), events)
            });
        }
    }
    let opening = provider.opening_items(&spec);
    let session = provider.start(spec).map_err(|err| err.to_string())?;
    for item in opening {
        record(daemon, agent_id, None, item);
    }
    let (turns_done, _) = watch::channel(0);
    let live = Arc::new(Live {
        commands: session.commands,
        provider: provider.clone(),
        info: info.clone(),
        gate: tokio::sync::Mutex::new(()),
        state: Mutex::new(RunState {
            turn_counter,
            ..RunState::default()
        }),
        turns_done,
        ended: watch::channel(false).0,
        active_at: Mutex::new(Instant::now()),
    });
    live_map.insert(agent_id.to_string(), live.clone());
    drop(live_map);
    tokio::spawn(drive(
        daemon.clone(),
        agent_id.to_string(),
        live.clone(),
        session.events,
    ));
    update(daemon, agent_id, |agent| {
        if matches!(
            agent.status,
            AgentStatus::Initializing | AgentStatus::Closed | AgentStatus::Running
        ) {
            agent.status = AgentStatus::Idle;
        }
        agent.active_turn = None;
        agent.capabilities = capabilities(&info);
        // A provider that lists no modes up front (ACP) reports the session's.
        if !info.modes.is_empty() {
            agent.available_modes = modes(&info);
        }
        if agent.current_mode_id.is_none() {
            agent.current_mode_id = info.default_mode_id.clone();
        }
    });
    Ok(live)
}

// --- the event loop of one session ------------------------------------------

async fn drive(
    daemon: Arc<Daemon>,
    agent_id: String,
    live: Arc<Live>,
    mut events: mpsc::UnboundedReceiver<ProviderEvent>,
) {
    let mut coalescer = Coalescer::default();
    loop {
        let deadline = coalescer.deadline();
        let event = tokio::select! {
            event = events.recv() => event,
            _ = sleep_until_opt(deadline), if deadline.is_some() => {
                flush(&daemon, &agent_id, &mut coalescer);
                continue;
            }
        };
        let Some(event) = event else { break };
        debug!(agent_id, ?event, "provider event");
        live.touch();
        if let ProviderEvent::Item { turn, item } = event {
            let turn = turn.or_else(|| active_turn(&live));
            if coalescable(&item) {
                if coalescer.push(turn, item, Instant::now()) {
                    flush(&daemon, &agent_id, &mut coalescer);
                }
            } else {
                flush(&daemon, &agent_id, &mut coalescer);
                record(&daemon, &agent_id, turn, item);
            }
            continue;
        }
        flush(&daemon, &agent_id, &mut coalescer);
        if !on_event(&daemon, &agent_id, &live, event) {
            break;
        }
    }
    flush(&daemon, &agent_id, &mut coalescer);
    live.ended.send_replace(true);
    // The session ended: whatever was running is over.
    let still_registered = daemon
        .agents
        .live(&agent_id)
        .is_some_and(|current| Arc::ptr_eq(&current, &live));
    if still_registered {
        daemon.agents.forget(&agent_id);
        finish_turn(&daemon, &agent_id, &live, None, TurnEnd::Closed);
        update(&daemon, &agent_id, |agent| {
            agent.status = AgentStatus::Closed;
            agent.active_turn = None;
            agent.pending_permissions.clear();
        });
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn active_turn(live: &Live) -> Option<String> {
    live.state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .active_turn
        .clone()
}

fn flush(daemon: &Daemon, agent_id: &str, coalescer: &mut Coalescer) {
    if coalescer.is_empty() {
        return;
    }
    for entry in coalescer.take(Instant::now()) {
        record(daemon, agent_id, entry.turn, entry.item);
    }
}

enum TurnEnd {
    Completed(Option<Value>),
    Failed {
        error: String,
        code: Option<String>,
        diagnostic: Option<String>,
    },
    Canceled(String),
    Closed,
}

/// Handles one non-item provider event. Returns false when the session is over.
fn on_event(daemon: &Arc<Daemon>, agent_id: &str, live: &Arc<Live>, event: ProviderEvent) -> bool {
    let provider = live.info.id.clone();
    match event {
        ProviderEvent::SessionStarted { session_id, model } => {
            let changed = update(daemon, agent_id, |agent| {
                agent.persistence = Some(json!({
                    "provider": provider, "sessionId": session_id, "nativeHandle": session_id,
                    "metadata": {"provider": provider, "cwd": agent.cwd,
                                   "modeId": agent.current_mode_id, "model": agent.model},
                }));
                let mut runtime = json!({
                    "provider": provider, "sessionId": session_id,
                    "model": agent.model, "modeId": agent.current_mode_id,
                });
                if let Some(model) = &model {
                    runtime["extra"] = json!({"runtimeModel": model});
                }
                agent.runtime_info = Some(runtime);
            });
            if changed.is_some() {
                stream(
                    daemon,
                    agent_id,
                    json!({"type": "thread_started", "sessionId": session_id, "provider": provider}),
                );
            }
        }
        ProviderEvent::BackgroundWork { running } => {
            live.state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .background = running;
        }
        ProviderEvent::TurnStarted { turn } => {
            let started = {
                let mut state = live.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.active_turn.is_some() {
                    // The foreground turn the manager already announced, or
                    // an autonomous turn while one runs: nothing new.
                    None
                } else {
                    let turn = turn.unwrap_or_else(|| {
                        state.turn_counter += 1;
                        format!("autonomous-turn-{}", state.turn_counter)
                    });
                    state.active_turn = Some(turn.clone());
                    Some(turn)
                }
            };
            if let Some(turn) = started {
                let now = clock::now_iso();
                stream(
                    daemon,
                    agent_id,
                    json!({"type": "turn_started", "provider": provider, "turnId": turn}),
                );
                update(daemon, agent_id, |agent| {
                    agent.status = AgentStatus::Running;
                    agent.last_error = None;
                    agent.active_turn = Some(ActiveTurn {
                        turn_id: turn,
                        started_at: Some(now),
                    });
                });
            }
        }
        ProviderEvent::Item { .. } => {}
        ProviderEvent::PermissionRequested { id, request } => {
            // A request that names its own id keeps it (ACP's are UUIDs).
            let client_id = request["id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("permission-{}", uuid::Uuid::new_v4()));
            let mut request = match request {
                Value::Object(map) => map,
                _ => Map::new(),
            };
            request.insert("id".into(), client_id.clone().into());
            request
                .entry("provider")
                .or_insert_with(|| provider.clone().into());
            let request = Value::Object(request);
            let first = {
                let mut state = live.state.lock().unwrap_or_else(|p| p.into_inner());
                state
                    .permissions
                    .push((client_id.clone(), id, request.clone()));
                state.permissions.len() == 1
            };
            let agent = update(daemon, agent_id, |agent| {
                agent.pending_permissions.push(request.clone());
            });
            stream(
                daemon,
                agent_id,
                json!({"type": "permission_requested", "provider": provider, "request": request}),
            );
            publish(
                daemon,
                vec![StoreEvent::Session {
                    kind: "agent_permission_request",
                    payload: json!({"agentId": agent_id, "request": request}),
                }],
            );
            if first && let Some(agent) = agent {
                attention(daemon, &agent, AttentionReason::Permission);
            }
        }
        ProviderEvent::PermissionCanceled { id } => {
            let client_id = {
                let mut state = live.state.lock().unwrap_or_else(|p| p.into_inner());
                let found = state.permissions.iter().position(|(_, p, _)| *p == id);
                found.map(|at| state.permissions.remove(at).0)
            };
            if let Some(client_id) = client_id {
                resolved(
                    daemon,
                    agent_id,
                    &provider,
                    &client_id,
                    json!({"behavior": "deny", "message": "Permission request canceled"}),
                );
            }
        }
        ProviderEvent::ModeChanged { mode_id } => {
            update(daemon, agent_id, |agent| {
                agent.current_mode_id = Some(mode_id);
            });
        }
        ProviderEvent::ModesAvailable { modes: listed } => {
            let info = ProviderInfo {
                modes: listed,
                ..ProviderInfo::default()
            };
            update(daemon, agent_id, |agent| {
                agent.available_modes = modes(&info);
            });
        }
        ProviderEvent::RuntimeInfo { info } => {
            update(daemon, agent_id, |agent| {
                // Paseo's `resolveEffectiveThinkingOptionId`: the runtime's
                // value wins whenever it has one, even `null`.
                if let Some(thinking) = info.get("thinkingOptionId") {
                    agent.effective_thinking_option_id = thinking.as_str().map(str::to_string);
                }
                agent.runtime_info = Some(info);
            });
        }
        ProviderEvent::UsageUpdated { usage } => {
            update(daemon, agent_id, |agent| {
                agent.last_usage = Some(usage);
            });
        }
        ProviderEvent::TurnCompleted { turn, usage } => {
            finish_turn(daemon, agent_id, live, turn, TurnEnd::Completed(usage));
        }
        ProviderEvent::TurnFailed {
            turn,
            error,
            code,
            diagnostic,
        } => {
            finish_turn(
                daemon,
                agent_id,
                live,
                turn,
                TurnEnd::Failed {
                    error,
                    code,
                    diagnostic,
                },
            );
        }
        ProviderEvent::TurnCanceled { turn, reason } => {
            finish_turn(daemon, agent_id, live, turn, TurnEnd::Canceled(reason));
        }
        ProviderEvent::Exited { error } => {
            if let Some(error) = error {
                warn!(agent_id, %error, "provider session ended with an error");
            }
            return false;
        }
    }
    true
}

/// Publishes a resolution: the stream event and the session event.
fn resolved(daemon: &Daemon, agent_id: &str, provider: &str, request_id: &str, resolution: Value) {
    update(daemon, agent_id, |agent| {
        agent
            .pending_permissions
            .retain(|p| p["id"].as_str() != Some(request_id));
    });
    stream(
        daemon,
        agent_id,
        json!({"type": "permission_resolved", "provider": provider,
               "requestId": request_id, "resolution": resolution}),
    );
    publish(
        daemon,
        vec![StoreEvent::Session {
            kind: "agent_permission_resolved",
            payload: json!({"agentId": agent_id, "requestId": request_id, "resolution": resolution}),
        }],
    );
}

fn finish_turn(daemon: &Daemon, agent_id: &str, live: &Live, turn: Option<String>, end: TurnEnd) {
    let (turn_id, replacing, leftover) = {
        let mut state = live.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(active) = state.active_turn.clone() else {
            return; // already finished: a repeated terminal event
        };
        if turn.as_ref().is_some_and(|t| *t != active) {
            return; // a stale terminal event for an older turn
        }
        state.active_turn = None;
        let leftover = match end {
            TurnEnd::Completed(_) | TurnEnd::Closed => Vec::new(),
            _ => std::mem::take(&mut state.permissions),
        };
        (active, state.replacing, leftover)
    };
    let provider = live.info.id.clone();
    let deny = match &end {
        TurnEnd::Failed { .. } => "Turn failed",
        _ => "Interrupted",
    };
    for (client_id, _, _) in leftover {
        resolved(
            daemon,
            agent_id,
            &provider,
            &client_id,
            json!({"behavior": "deny", "message": deny}),
        );
    }
    let event = match &end {
        TurnEnd::Completed(usage) => {
            let mut event =
                json!({"type": "turn_completed", "provider": provider, "turnId": turn_id});
            if let Some(usage) = usage {
                event["usage"] = usage.clone();
            }
            Some(event)
        }
        TurnEnd::Failed {
            error,
            code,
            diagnostic,
        } => {
            let mut parts = vec![if error.trim().is_empty() {
                "Provider run failed".to_string()
            } else {
                error.trim().to_string()
            }];
            if let Some(code) = code.as_deref().filter(|c| !c.trim().is_empty()) {
                parts.push(format!("code: {code}"));
            }
            if let Some(diag) = diagnostic
                .as_deref()
                .filter(|d| !d.trim().is_empty() && *d != error)
            {
                parts.push(diag.to_string());
            }
            record(
                daemon,
                agent_id,
                None,
                json!({"type": "assistant_message", "text": format!("[System Error] {}", parts.join("\n\n"))}),
            );
            let mut event = json!({"type": "turn_failed", "provider": provider,
                                   "turnId": turn_id, "error": error});
            if let Some(code) = code {
                event["code"] = code.clone().into();
            }
            if let Some(diagnostic) = diagnostic {
                event["diagnostic"] = diagnostic.clone().into();
            }
            Some(event)
        }
        TurnEnd::Canceled(reason) => Some(
            json!({"type": "turn_canceled", "provider": provider, "turnId": turn_id, "reason": reason}),
        ),
        TurnEnd::Closed => None,
    };
    if let Some(event) = event {
        stream(daemon, agent_id, event);
    }
    let failed = matches!(end, TurnEnd::Failed { .. });
    let mut raised = None;
    let agent = update(daemon, agent_id, |agent| {
        let was_running = agent.status == AgentStatus::Running;
        agent.active_turn = None;
        if let TurnEnd::Completed(Some(usage)) = &end {
            let mut merged = agent
                .last_usage
                .as_ref()
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if let Some(usage) = usage.as_object() {
                merged.extend(usage.clone());
            }
            agent.last_usage = Some(Value::Object(merged));
        }
        if let TurnEnd::Failed { error, .. } = &end {
            agent.last_error = Some(error.clone());
        }
        if replacing && !failed {
            return; // a new turn takes over; stay running, no attention
        }
        agent.status = if failed {
            AgentStatus::Error
        } else if matches!(end, TurnEnd::Closed) {
            AgentStatus::Closed
        } else {
            AgentStatus::Idle
        };
        if !agent.requires_attention && !matches!(end, TurnEnd::Closed) {
            let reason = if failed {
                Some(AttentionReason::Error)
            } else if was_running {
                Some(AttentionReason::Finished)
            } else {
                None
            };
            if let Some(reason) = reason {
                agent.requires_attention = true;
                agent.attention_reason = Some(reason);
                agent.attention_timestamp = Some(clock::now_iso());
                raised = Some(reason);
            }
        }
    });
    if let (Some(agent), Some(reason)) = (agent, raised) {
        attention(daemon, &agent, reason);
    }
    live.turns_done.send_modify(|n| *n += 1);
}

// --- requests ---------------------------------------------------------------

fn resolve(daemon: &Daemon, key: &str) -> Result<String, String> {
    daemon
        .read(|store| store.find_agent(key).map(|a| a.id.clone()))
        .ok_or_else(|| format!("Agent not found: {key}"))
}

fn unarchive(daemon: &Daemon, agent_id: &str) {
    daemon.apply(|store| {
        let Some(mut agent) = store.archived.get(agent_id).cloned() else {
            return ((), Vec::new());
        };
        agent.archived_at = None;
        agent.status = AgentStatus::Closed;
        agent.updated_at = clock::after(&agent.updated_at);
        let events = store.upsert_agent(agent);
        ((), events)
    });
}

/// Waits until the turn count moves past `seen`.
async fn turn_finished(live: &Live, seen: u64) -> bool {
    let mut done = live.turns_done.subscribe();
    tokio::time::timeout(INTERRUPT_GRACE, done.wait_for(|n| *n > seen))
        .await
        .is_ok_and(|r| r.is_ok())
}

/// Interrupts the running turn, if any, and waits for it to end.
async fn interrupt(live: &Live) -> Result<(), ()> {
    if active_turn(live).is_none() {
        return Ok(());
    }
    let seen = *live.turns_done.borrow();
    let _ = live.commands.send(Command::Interrupt);
    if turn_finished(live, seen).await {
        Ok(())
    } else {
        Err(())
    }
}

impl Live {
    fn touch(&self) {
        *self.active_at.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }

    /// A turn, a question, a prompt making way for another, or work outside
    /// a turn: anything that stopping its CLI would lose.
    fn busy(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.active_turn.is_some()
            || !state.permissions.is_empty()
            || state.replacing
            || state.background
    }

    fn idle_for(&self) -> Duration {
        self.active_at
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .elapsed()
    }
}

/// Lets go of idle agents' CLIs every `idle_check`, as Paseo 0.14 releases
/// idle runtimes: the next prompt resumes the session in a new one.
pub async fn release_idle_loop(daemon: Arc<Daemon>, mut shutdown: watch::Receiver<bool>) {
    let Some(after) = daemon.config.timing.idle_release else {
        return;
    };
    loop {
        tokio::select! {
            _ = tokio::time::sleep(daemon.config.timing.idle_check) => {}
            _ = shutdown.changed() => return,
        }
        release_idle(&daemon, after).await;
    }
}

/// Stops the CLI of each agent that has done nothing for `after`, when it
/// has a provider session to resume, is idle, and none of its subagents is
/// busy. Returns the agents let go of.
pub async fn release_idle(daemon: &Arc<Daemon>, after: Duration) -> Vec<String> {
    let live: Vec<(String, Arc<Live>)> = (daemon.agents.live.lock())
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(|(id, live)| (id.clone(), live.clone()))
        .collect();
    let busy: Vec<&str> = (live.iter())
        .filter(|(_, live)| live.busy())
        .map(|(id, _)| id.as_str())
        .collect();
    let mut released = Vec::new();
    for (agent_id, agent) in &live {
        let resumable = daemon.read(|store| {
            let snapshot = store.agents.get(agent_id)?;
            let children_busy = (store.agents.iter())
                .map(|(child, _)| child)
                .filter(|child| child.labels.get(PARENT_AGENT_LABEL) == Some(agent_id))
                .any(|child| busy.contains(&child.id.as_str()));
            let session = (snapshot.persistence.as_ref())
                .and_then(|p| p["sessionId"].as_str())
                .is_some();
            Some(session && snapshot.status == AgentStatus::Idle && !children_busy)
        });
        if resumable != Some(true) || agent.busy() || agent.idle_for() < after {
            continue;
        }
        let _gate = agent.gate.lock().await;
        let still = (daemon.agents.live(agent_id)).is_some_and(|now| Arc::ptr_eq(&now, agent));
        if !still || agent.busy() || agent.idle_for() < after {
            continue;
        }
        info!(
            agent_id,
            provider = agent.info.id,
            "releasing the CLI of an agent idle for an hour; its next prompt resumes it"
        );
        let ended = agent.ended.subscribe();
        (daemon.agents.released.lock())
            .unwrap_or_else(|p| p.into_inner())
            .insert(agent_id.clone(), ended);
        daemon.agents.forget(agent_id);
        let _ = agent.commands.send(Command::Shutdown);
        released.push(agent_id.clone());
    }
    released
}

/// Waits for the CLI of a session let go of for being idle to exit, so the
/// one that resumes it does not find the session still held.
async fn wait_released(daemon: &Daemon, agent_id: &str) {
    let ended = (daemon.agents.released.lock())
        .unwrap_or_else(|p| p.into_inner())
        .remove(agent_id);
    if let Some(mut ended) = ended
        && tokio::time::timeout(SESSION_END_WAIT, ended.wait_for(|ended| *ended))
            .await
            .is_err()
    {
        warn!(
            agent_id,
            "the released session did not end in time; resuming it anyway"
        );
    }
}

/// Closes an idle agent's session when its provider was found again since
/// it started (its CLI was upgraded), so the prompt about to be sent starts
/// the new CLI on the same provider session. A running agent keeps its CLI.
async fn restart_if_superseded(daemon: &Arc<Daemon>, agent_id: &str) {
    wait_released(daemon, agent_id).await;
    let Some(live) = daemon.agents.live(agent_id) else {
        return;
    };
    let current = daemon.agents.provider(&live.info.id);
    if current.is_none_or(|current| Arc::ptr_eq(&current, &live.provider)) {
        return;
    }
    let _gate = live.gate.lock().await;
    let idle = {
        let state = live.state.lock().unwrap_or_else(|p| p.into_inner());
        state.active_turn.is_none() && state.permissions.is_empty()
    };
    let still = daemon
        .agents
        .live(agent_id)
        .is_some_and(|now| Arc::ptr_eq(&now, &live));
    if idle && still {
        info!(
            agent_id,
            provider = live.info.id,
            "restarting the agent's CLI, which was upgraded"
        );
        let mut ended = live.ended.subscribe();
        daemon.agents.forget(agent_id);
        let _ = live.commands.send(Command::Shutdown);
        // The old CLI lets go of the session before the new one resumes it:
        // Codex refuses a thread another app-server still has open.
        if tokio::time::timeout(SESSION_END_WAIT, ended.wait_for(|ended| *ended))
            .await
            .is_err()
        {
            warn!(
                agent_id,
                "the old session did not end in time; starting the new one anyway"
            );
        }
    }
}

/// `send_agent_message_request`. Returns the resolved agent id once the
/// turn has started (or the message joined the running one).
pub async fn send(
    daemon: &Arc<Daemon>,
    key: &str,
    text: &str,
    message_id: Option<String>,
    images: Vec<Image>,
    behavior: Option<&str>,
    source_agent: Option<&str>,
) -> Result<String, String> {
    let agent_id = resolve(daemon, key)?;
    unarchive(daemon, &agent_id);
    restart_if_superseded(daemon, &agent_id).await;
    let live = ensure_live(daemon, &agent_id)?;
    let _gate = live.gate.lock().await;
    live.touch();
    if let Some(message_id) = &message_id {
        let mut state = live.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.recent_sends.contains(message_id) {
            return Ok(agent_id); // a retried send that already went through
        }
        if state.recent_sends.len() >= REMEMBERED_SENDS {
            state.recent_sends.pop_front();
        }
        state.recent_sends.push_back(message_id.clone());
    }
    let client_message_id = message_id.unwrap_or_else(ids::agent_id);
    let text = text.trim().to_string();
    let (prompt, mut user_message) = match source_agent.filter(|s| !s.trim().is_empty()) {
        Some(sender) => from_agent(daemon, sender, &client_message_id, &text),
        None => (
            text.clone(),
            json!({"type": "user_message", "text": text,
                   "messageId": client_message_id, "clientMessageId": client_message_id}),
        ),
    };
    // Kept, unlike Paseo, so other clients can show them too.
    if !images.is_empty() && source_agent.is_none() {
        let (keeper, kept) = (daemon.clone(), images.clone());
        let kept = tokio::task::spawn_blocking(move || keeper.attachments.keep(&kept))
            .await
            .unwrap_or_default();
        if !kept.is_empty() {
            user_message["images"] = kept.into();
        }
    }

    if let Some(turn) = active_turn(&live) {
        if behavior == Some("steer") && live.info.capabilities.steer {
            let _ = live.commands.send(Command::Steer {
                message_id: client_message_id.clone(),
                text: prompt,
                images,
            });
            record(daemon, &agent_id, Some(turn), user_message);
            update(daemon, &agent_id, |agent| {
                agent.last_user_message_at = Some(clock::now_iso());
            });
            return Ok(agent_id);
        }
        live.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replacing = true;
        let interrupted = interrupt(&live).await;
        live.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replacing = false;
        if interrupted.is_err() {
            return Err(format!(
                "Cannot replace agent {agent_id} because its active run cancellation was not acknowledged"
            ));
        }
    }

    let turn_id = {
        let mut state = live.state.lock().unwrap_or_else(|p| p.into_inner());
        state.turn_counter += 1;
        let turn = live.provider.foreground_turn_id(state.turn_counter);
        state.active_turn = Some(turn.clone());
        turn
    };
    let started_at = clock::now_iso();
    stream(
        daemon,
        &agent_id,
        json!({"type": "turn_started", "provider": live.info.id, "turnId": turn_id}),
    );
    record(daemon, &agent_id, Some(turn_id.clone()), user_message);
    update(daemon, &agent_id, |agent| {
        if agent.title.is_none() && agent.last_user_message_at.is_none() {
            agent.title = derive_title(&text);
        }
        agent.status = AgentStatus::Running;
        agent.last_error = None;
        agent.last_user_message_at = Some(started_at.clone());
        agent.active_turn = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            started_at: Some(started_at),
        });
    });
    live.commands
        .send(Command::Prompt {
            turn_id,
            message_id: client_message_id,
            text: prompt,
            images,
        })
        .map_err(|_| format!("Agent {agent_id} is not running"))?;
    Ok(agent_id)
}

/// A prompt another agent sent, as Paseo 0.12 delivers one: the provider
/// gets it in a `<paseo-system>` envelope naming the sender, so the agent
/// knows it is not from its user, and the timeline has it as an
/// `agent_message` call from the sender, which clients show as "From ...".
/// Returns the provider's text and the timeline item.
fn from_agent(daemon: &Daemon, sender: &str, message_id: &str, text: &str) -> (String, Value) {
    let title = daemon
        .read(|store| store.agent(sender).and_then(|agent| agent.title.clone()))
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty());
    let named = match &title {
        Some(title) => format!(" source-agent-title=\"{}\"", escape_xml(title)),
        None => String::new(),
    };
    let envelope = format!(
        "<paseo-system version=\"1\" kind=\"agent-message\" source-agent-id=\"{}\" message-id=\"{}\"{named}>\n{}\n</paseo-system>",
        escape_xml(sender),
        escape_xml(message_id),
        escape_xml(text),
    );
    let mut from = json!({ "id": sender });
    if let Some(title) = title {
        from["title"] = title.into();
    }
    let item = json!({
        "type": "tool_call",
        "callId": format!("paseo-agent-message:{message_id}"),
        "agentMessage": { "event": "message", "sender": from, "text": text },
        "name": "agent_message",
        "status": "completed",
        "error": null,
        "detail": { "type": "plain_text", "text": text, "icon": "bot" },
    });
    (envelope, item)
}

/// As Paseo escapes an envelope's attributes and body.
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// `wait_for_finish_request`, answered as Paseo answers it: at once when the
/// agent waits for a permission or is not working, else as soon as it asks
/// for one or its turn ends; `timeout` after `timeout_ms`. The payload's
/// `status` is `idle`, `error`, `permission` or `timeout`, with the agent as
/// `final`, its last error, and its last reply as `lastMessage`.
pub async fn wait_for_finish(daemon: &Arc<Daemon>, key: &str, timeout_ms: Option<u64>) -> Value {
    let answer = |status: &str, agent: Option<&AgentSnapshot>, error: Option<String>| {
        let last = agent
            .map(|agent| daemon.read(|store| last_assistant_text(store, &agent.id)))
            .filter(|text| !text.is_empty());
        json!({"status": status, "final": agent, "error": error, "lastMessage": last})
    };
    // Watched before the first look, so a change between the two is seen.
    let mut changes = daemon.changes();
    let agent_id = match resolve(daemon, key) {
        Ok(id) => id,
        Err(error) => return answer("error", None, Some(error)),
    };
    let deadline = (timeout_ms.filter(|ms| *ms > 0))
        .map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
    loop {
        let Some(agent) = daemon.read(|store| store.agent(&agent_id).cloned()) else {
            return answer("error", None, Some(format!("Agent not found: {agent_id}")));
        };
        let busy = matches!(
            agent.status,
            AgentStatus::Running | AgentStatus::Initializing
        ) || agent.active_turn.is_some();
        if !agent.pending_permissions.is_empty() {
            return answer("permission", Some(&agent), None);
        }
        if !busy && agent.status == AgentStatus::Error {
            let error = (agent.last_error.clone())
                .filter(|error| !error.trim().is_empty())
                .unwrap_or_else(|| "Agent failed".into());
            return answer("error", Some(&agent), Some(error));
        }
        if !busy {
            return answer("idle", Some(&agent), None);
        }
        // Until the agents change (or the stream skipped some, so look again).
        let changed = async {
            loop {
                match changes.recv().await {
                    Ok(event) if matches!(*event, StoreEvent::Agent { .. }) => return true,
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
                }
            }
        };
        let changed = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, changed).await {
                Ok(changed) => changed,
                Err(_) => return answer("timeout", Some(&agent), None),
            },
            None => changed.await,
        };
        if !changed {
            return answer("error", Some(&agent), Some("Burrow is stopping".into()));
        }
    }
}

/// `agent.detach.request` (`paseo detach`): the agent no longer belongs to
/// the agent that made it, so it stands on its own in clients.
pub fn detach(daemon: &Arc<Daemon>, key: &str) -> Result<String, String> {
    let agent_id = resolve(daemon, key)?;
    update(daemon, &agent_id, |agent| {
        agent.labels.remove(PARENT_AGENT_LABEL);
        agent.labels.remove("paseo.parent-server-id");
    });
    Ok(agent_id)
}

/// `cancel_agent_request`. Returns the agent once the turn has ended.
pub async fn cancel(daemon: &Arc<Daemon>, key: &str) -> Result<Option<AgentSnapshot>, String> {
    let agent_id = resolve(daemon, key)?;
    if let Some(live) = daemon.agents.live(&agent_id) {
        let _gate = live.gate.lock().await;
        interrupt(&live).await.map_err(|_| {
            format!(
                "Cannot stop agent {agent_id} because its active run cancellation was not acknowledged"
            )
        })?;
    }
    Ok(daemon.read(|store| store.agent(&agent_id).cloned()))
}

/// `archive_agent_request`. Returns `archivedAt`.
pub async fn archive(daemon: &Arc<Daemon>, key: &str) -> Result<String, String> {
    let agent_id = resolve(daemon, key)?;
    if let Some(archived) = daemon.read(|store| {
        store
            .archived
            .get(&agent_id)
            .and_then(|a| a.archived_at.clone())
    }) {
        return Ok(archived);
    }
    if let Some(live) = daemon.agents.live(&agent_id) {
        let _gate = live.gate.lock().await;
        let _ = interrupt(&live).await;
        daemon.agents.forget(&agent_id);
        let _ = live.commands.send(Command::Shutdown);
    }
    let archived_at = clock::now_iso();
    daemon.apply(|store| {
        if let Some(mut agent) = store.agents.get(&agent_id).cloned() {
            agent.pending_permissions.clear();
            let mut events = store.upsert_agent(agent);
            events.extend(store.archive_agent(&agent_id, &archived_at));
            return ((), events);
        }
        ((), Vec::new())
    });
    Ok(archived_at)
}

/// `delete_agent_request`: stops the agent's session if it runs, then
/// forgets the agent, active or archived, with its record and timeline.
/// The provider's own session files are left, as in Paseo. Returns whether
/// there was such an agent; Paseo answers the same either way.
pub async fn delete(daemon: &Arc<Daemon>, agent_id: &str) -> bool {
    if let Some(live) = daemon.agents.live(agent_id) {
        // Closed, not interrupted, as Paseo's `closeAgent`: no turn end and
        // no attention for an agent about to go.
        let _gate = live.gate.lock().await;
        daemon.agents.forget(agent_id);
        let _ = live.commands.send(Command::Shutdown);
    }
    daemon.apply(|store| store.delete_agent(agent_id))
}

/// `refresh_agent_request`: unarchives and reloads the session. Returns the
/// timeline size.
pub async fn refresh(daemon: &Arc<Daemon>, key: &str) -> Result<usize, String> {
    let agent_id = resolve(daemon, key)?;
    unarchive(daemon, &agent_id);
    if let Some(live) = daemon.agents.live(&agent_id) {
        let _gate = live.gate.lock().await;
        let _ = interrupt(&live).await;
        daemon.agents.forget(&agent_id);
        let _ = live.commands.send(Command::Shutdown);
    }
    ensure_live(daemon, &agent_id)?;
    Ok(daemon.apply(|store| {
        let size = store
            .ensure_timeline(&agent_id)
            .map_or(0, |t| t.rows().len());
        (size, Vec::new())
    }))
}

/// `agent_permission_response`. Returns the resolution.
pub fn respond_permission(
    daemon: &Arc<Daemon>,
    key: &str,
    request_id: &str,
    response: Value,
) -> Result<Value, String> {
    let agent_id = resolve(daemon, key)?;
    let live = daemon
        .agents
        .live(&agent_id)
        .ok_or_else(|| format!("Agent {agent_id} has no pending permission {request_id}"))?;
    let provider_id = {
        let mut state = live.state.lock().unwrap_or_else(|p| p.into_inner());
        let at = state
            .permissions
            .iter()
            .position(|(client, _, _)| client == request_id)
            .ok_or_else(|| format!("Permission request {request_id} not found"))?;
        if !question_answer_deliverable(&state.permissions[at].2, &response) {
            // Paseo 0.12: the question stays pending and can be answered again.
            return Err("question_answer_required: answer at least one question".into());
        }
        state.permissions.remove(at).1
    };
    let _ = live.commands.send(Command::RespondPermission {
        id: provider_id,
        response: response.clone(),
    });
    resolved(
        daemon,
        &agent_id,
        &live.info.id,
        request_id,
        response.clone(),
    );
    Ok(response)
}

/// An `allow` answer to a question must carry at least one non-empty
/// answer, keyed by a question's header or its full text.
fn question_answer_deliverable(request: &Value, response: &Value) -> bool {
    if request["kind"] != "question" || response["behavior"] != "allow" {
        return true;
    }
    let answers = &response["updatedInput"]["answers"];
    request["input"]["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|q| {
            ["header", "question"].iter().any(|key| {
                q[*key]
                    .as_str()
                    .and_then(|k| answers[k].as_str())
                    .is_some_and(|a| !a.is_empty())
            })
        })
}

/// `clear_agent_attention`. Returns the snapshots of the agents it names.
pub fn clear_attention(daemon: &Arc<Daemon>, keys: &[String]) -> Vec<AgentSnapshot> {
    let mut agents = Vec::new();
    for key in keys {
        let Ok(agent_id) = resolve(daemon, key) else {
            continue;
        };
        let requires =
            daemon.read(|store| store.agents.get(&agent_id).map(|a| a.requires_attention));
        let snapshot = if requires == Some(true) {
            update(daemon, &agent_id, |agent| {
                agent.requires_attention = false;
                agent.attention_reason = None;
                agent.attention_timestamp = None;
            })
        } else {
            daemon.read(|store| store.agent(&agent_id).cloned())
        };
        agents.extend(snapshot);
    }
    agents
}

/// `update_agent_request`: rename and merge labels.
pub fn rename_and_label(
    daemon: &Arc<Daemon>,
    key: &str,
    name: Option<String>,
    labels: Option<BTreeMap<String, String>>,
) -> Result<String, String> {
    let agent_id = resolve(daemon, key)?;
    let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    let labels = labels.filter(|l| !l.is_empty());
    if name.is_none() && labels.is_none() {
        return Err("Nothing to update (provide name and/or labels)".into());
    }
    let updated = update(daemon, &agent_id, |agent| {
        if let Some(name) = name {
            agent.title = Some(name);
        }
        if let Some(labels) = labels {
            agent.labels.extend(labels);
        }
    });
    updated
        .map(|_| agent_id)
        .ok_or_else(|| "Archived agents cannot be updated".into())
}

/// Mode, model and thinking changes.
pub enum Setting {
    Mode(String),
    Model(Option<String>),
    Thinking(Option<String>),
}

pub fn configure(daemon: &Arc<Daemon>, key: &str, setting: Setting) -> Result<String, String> {
    let agent_id = resolve(daemon, key)?;
    let live = ensure_live(daemon, &agent_id)?;
    let command = match &setting {
        Setting::Mode(mode) => {
            // The provider's modes, or the ones the session listed itself.
            let listed = daemon.read(|store| {
                store
                    .agent(&agent_id)
                    .is_some_and(|a| a.available_modes.iter().any(|m| m.id == *mode))
            });
            if !listed && !live.info.modes.iter().any(|m| m.id == *mode) {
                return Err(format!("Unknown mode '{mode}'"));
            }
            Command::SetMode(mode.clone())
        }
        Setting::Model(model) => Command::SetModel(model.clone()),
        Setting::Thinking(option) => Command::SetThinking(option.clone()),
    };
    let _ = live.commands.send(command);
    update(daemon, &agent_id, |agent| match setting {
        Setting::Mode(mode) => agent.current_mode_id = Some(mode),
        Setting::Model(model) => agent.model = model,
        Setting::Thinking(option) => {
            agent.effective_thinking_option_id = option.clone();
            agent.thinking_option_id = option;
        }
    });
    Ok(agent_id)
}

/// `get_providers_snapshot_response` entries for every registered provider.
/// With `compact`, the shape clients get when they advertise
/// `compact_provider_snapshots`: thinking options shared through `thinkingSets`.
pub fn providers_snapshot(daemon: &Daemon, compact: bool) -> Value {
    let providers = daemon.agents.all();
    let infos: Vec<ProviderInfo> = providers.iter().map(|p| p.info()).collect();
    let mode_json = |m: &burrow_provider::Mode| {
        let mut mode = json!({"id": m.id, "label": m.label});
        if let Some(d) = &m.description {
            mode["description"] = d.clone().into();
        }
        if let Some(i) = &m.icon {
            mode["icon"] = i.clone().into();
        }
        if let Some(c) = &m.color_tier {
            mode["colorTier"] = c.clone().into();
        }
        mode
    };
    let thinking_json = |options: &[burrow_provider::ThinkingOption]| -> Value {
        options
            .iter()
            .map(|o| json!({"id": o.id, "label": o.label, "isDefault": o.is_default}))
            .collect()
    };
    let mut thinking_sets: Vec<Value> = Vec::new();
    let mut entries = Vec::new();
    for (provider, info) in providers.iter().zip(&infos) {
        if provider.loading() {
            // Paseo's entry while it probes: no models or modes yet.
            let mut entry = json!({"provider": info.id, "label": info.label, "status": "loading",
                                   "enabled": true, "defaultModeId": info.default_mode_id});
            if let Some(d) = &info.description {
                entry["description"] = d.clone().into();
            }
            entries.push(entry);
            continue;
        }
        let models: Vec<Value> =
            info.models
                .iter()
                .map(|m| {
                    let mut model = json!({"id": m.id, "label": m.label, "isDefault": m.is_default,
                                       "isSelectable": true});
                    // `AgentModelDefinitionSchema` requires it; the compact
                    // form, whose models sit under their entry, has none.
                    if !compact {
                        model["provider"] = info.id.clone().into();
                    }
                    if let Some(d) = &m.description {
                        model["description"] = d.clone().into();
                    }
                    if let Some(max) = m.context_window_max_tokens {
                        model["contextWindowMaxTokens"] = max.into();
                    }
                    if !m.thinking.is_empty() {
                        let default = m
                            .thinking
                            .iter()
                            .find(|o| o.is_default)
                            .or(m.thinking.first())
                            .map(|o| o.id.clone());
                        model["thinkingOptions"] = thinking_json(&m.thinking);
                        model["defaultThinkingOptionId"] = default.into();
                    }
                    if let Some(fields) = model.as_object_mut() {
                        for (key, value) in provider.model_fields(&m.id) {
                            if value.is_null() {
                                fields.remove(&key);
                            } else {
                                fields.insert(key, value);
                            }
                        }
                    }
                    if compact && let Some(fields) = model.as_object_mut() {
                        // Paseo's `compactModel`: the options move to a shared set.
                        let default = fields.remove("defaultThinkingOptionId");
                        if let Some(options) = fields.remove("thinkingOptions") {
                            let mut set = json!({"options": options});
                            if let Some(default) = default.filter(|d| !d.is_null()) {
                                set["defaultOptionId"] = default;
                            }
                            let index = thinking_sets
                                .iter()
                                .position(|s| *s == set)
                                .unwrap_or_else(|| {
                                    thinking_sets.push(set);
                                    thinking_sets.len() - 1
                                });
                            fields.insert("thinkingSet".into(), index.into());
                        } else if let Some(default) = default {
                            fields.insert("defaultThinkingOptionId".into(), default);
                        }
                    }
                    model
                })
                .collect();
        let mut entry = json!({
            "provider": info.id, "label": info.label, "status": "ready", "enabled": true,
            "defaultModeId": info.default_mode_id,
            "modes": info.modes.iter().map(mode_json).collect::<Vec<_>>(),
            "models": models,
        });
        if let Some(d) = &info.description {
            entry["description"] = d.clone().into();
        }
        entries.push(entry);
    }
    let now = clock::now_iso();
    // Paseo reports when each provider was last probed.
    let fetched: serde_json::Map<String, Value> = infos
        .iter()
        .map(|info| (info.id.clone(), Value::String(now.clone())))
        .collect();
    let hash = format!(
        "{:x}",
        fxhash(&serde_json::to_string(&entries).unwrap_or_default())
    );
    if compact {
        json!({"entries": [], "compactSnapshot": {"entries": entries, "thinkingSets": thinking_sets},
               "snapshotHash": hash, "generatedAt": now, "fetchedAt": fetched})
    } else {
        json!({"entries": entries, "snapshotHash": hash, "generatedAt": now, "fetchedAt": fetched})
    }
}

/// How often providers that are still loading are looked at.
const LOADING_POLL: Duration = Duration::from_millis(250);

/// Pushes `providers_snapshot_update` to its subscribers once every provider
/// that is loading now has finished, as Paseo does when a probe lands.
/// Clients that fetched the snapshot while it said `loading` (Crusteo and
/// Reef fetch it once per host) otherwise never see what it found.
pub fn announce_when_loaded(daemon: &Arc<Daemon>) {
    if !daemon.agents.all().iter().any(|p| p.loading()) {
        return;
    }
    let daemon = daemon.clone();
    tokio::spawn(async move {
        while daemon.agents.all().iter().any(|p| p.loading()) {
            tokio::time::sleep(LOADING_POLL).await;
        }
        publish_providers(&daemon);
    });
}

/// Publishes the providers snapshot as a `providers_snapshot_update`.
pub fn publish_providers(daemon: &Daemon) {
    let push = |compact: bool| {
        let mut payload = providers_snapshot(daemon, compact);
        if let Some(fields) = payload.as_object_mut() {
            fields.remove("requestId");
        }
        payload
    };
    let (plain, compact) = (push(false), push(true));
    publish(daemon, vec![StoreEvent::Providers { plain, compact }]);
}

/// A stable, cheap content hash for `snapshotHash`.
fn fxhash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_follow_paseo() {
        assert_eq!(
            derive_title("\n\n  fix   the\tflaky test \nmore").as_deref(),
            Some("fix the flaky test")
        );
        let long = "x".repeat(80);
        assert_eq!(derive_title(&long).unwrap().len(), 60);
        assert_eq!(derive_title("   \n "), None);
    }

    #[test]
    fn previews_strip_markdown_and_cut_at_220() {
        assert_eq!(
            preview("# Done\n\n- **all** `tests` pass"),
            "Done all tests pass"
        );
        let long = "word ".repeat(100);
        let cut = preview(&long);
        assert!(cut.ends_with("...") && cut.chars().count() <= 220);
    }
}
