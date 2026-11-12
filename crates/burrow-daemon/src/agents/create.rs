//! Making an agent: the request, its snapshot, its workspace and its first
//! turn.

use std::collections::BTreeMap;
use std::sync::Arc;

use burrow_protocol::directory::{AgentSnapshot, AgentStatus};
use burrow_provider::Image;
use serde_json::{Value, json};
use tracing::warn;

use super::live::ensure_live;
use super::messages::send;
use super::placement::placement;
use super::{capabilities, derive_title, modes};
use crate::clock;
use crate::daemon::Daemon;
use crate::ids;

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

/// Creates an agent, starts its session and, with an initial prompt, its
/// first turn. Returns the agent as it stands when the turn has started.
pub async fn create(daemon: &Arc<Daemon>, request: CreateAgent) -> Result<AgentSnapshot, String> {
    daemon.agents.until_found(&request.provider).await;
    let provider = daemon
        .agents
        .provider(&request.provider)
        .ok_or_else(|| format!("Provider '{}' is not available", request.provider))?;
    if let Some(reason) = provider.unavailable() {
        return Err(format!(
            "{} is not available here: {reason}",
            provider.info().label
        ));
    }
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
        last_reply_preview: None,
        cli: None,
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
