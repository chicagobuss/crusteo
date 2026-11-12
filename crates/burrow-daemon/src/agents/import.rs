//! Making an agent from a session a provider's CLI started outside Burrow
//! (Paseo's `import_agent_request`): a Claude Code transcript or a Codex
//! thread. The agent is `closed`, as every agent is after a restart, with the
//! session's conversation as its timeline and the session to resume at its
//! first prompt, so importing starts no CLI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use burrow_protocol::directory::{AgentSnapshot, AgentStatus};
use burrow_provider::{HistoryItem, ProviderInfo};
use serde_json::json;

use super::create::PARENT_AGENT_LABEL;
use super::placement::placement;
use super::{capabilities, derive_title, modes};
use crate::clock;
use crate::daemon::Daemon;
use crate::git::{self, GitFacts};
use crate::ids;
use crate::store::{Store, StoreEvent};

/// What `import_agent_request` asks for.
#[derive(Debug, Clone, Default)]
pub struct ImportSession {
    pub provider: String,
    /// The provider's own id for the session (`providerHandleId`).
    pub handle: String,
    /// Where the session ran.
    pub cwd: String,
    /// The workspace the agent joins; else the one for `cwd`, made when there
    /// is none.
    pub workspace_id: Option<String>,
    pub labels: BTreeMap<String, String>,
}

/// What the provider and the folder say of the session.
struct Read {
    facts: Option<GitFacts>,
    history: Vec<HistoryItem>,
    /// The name the user gave the session in its CLI.
    rename: Option<String>,
}

/// Makes the agent for a session, or brings back the archived one it already
/// has. Returns it with the size of its timeline. Fails when the provider
/// cannot import, the folder is gone, the workspace asked for does not hold
/// that folder, or an active agent has the session.
pub async fn import_session(
    daemon: &Arc<Daemon>,
    request: ImportSession,
) -> Result<(AgentSnapshot, usize), String> {
    // An id names a file in the provider's records: never a path.
    if request.handle.contains(['/', '\\']) || request.handle.contains("..") {
        return Err(format!("Invalid provider session id: {}", request.handle));
    }
    daemon.agents.until_found(&request.provider).await;
    let provider = (daemon.agents.provider(&request.provider))
        .ok_or_else(|| format!("Provider '{}' is not available", request.provider))?;
    if let Some(reason) = provider.unavailable() {
        return Err(format!(
            "{} is not available here: {reason}",
            provider.info().label
        ));
    }
    if !provider.imports_sessions() {
        return Err(format!(
            "Provider '{}' does not support importing sessions",
            request.provider
        ));
    }
    let cwd = git::resolve(Path::new(&git::expand_tilde(&request.cwd)))
        .to_string_lossy()
        .into_owned();
    if !Path::new(&cwd).is_dir() {
        return Err(format!(
            "Working directory does not exist or is not a directory: {cwd}"
        ));
    }
    // Refused before the conversation is read, which can take a while; an
    // archived agent has its timeline already.
    let archived = daemon.read(|store| check(store, &request, &cwd))?;
    let probe_dir = PathBuf::from(&cwd);
    let facts = tokio::task::spawn_blocking(move || git::probe(&probe_dir))
        .await
        .ok()
        .flatten();
    let (history, rename) = match archived {
        Some(_) => (Vec::new(), None),
        None => {
            let (provider, cwd, handle) = (provider.clone(), cwd.clone(), request.handle.clone());
            tokio::task::spawn_blocking(move || {
                (
                    provider.history(&cwd, &handle),
                    provider.session_title(&cwd, &handle),
                )
            })
            .await
            .map_err(|err| err.to_string())?
        }
    };
    let info = provider.info();
    let read = Read {
        facts,
        history,
        rename,
    };
    // One change to the store, so a second import of the same session waits
    // for this one and is told it is imported, and nothing that can fail
    // comes after the workspace is made.
    daemon.apply(|store| import_now(store, &info, &request, &cwd, read))
}

/// The archived agent of the session, to bring back, when there is one; or
/// why the session cannot be imported as asked. Changes nothing.
fn check(
    store: &Store,
    request: &ImportSession,
    cwd: &str,
) -> Result<Option<AgentSnapshot>, String> {
    if let Some(id) = &request.workspace_id {
        let workspace =
            (store.workspaces.get(id)).ok_or_else(|| format!("Workspace not found: {id}"))?;
        if store.projects.get(&workspace.project_id).is_none() {
            return Err(format!("Project not found: {}", workspace.project_id));
        }
        if !git::same_path(&workspace.cwd, cwd) {
            return Err(format!("Import cwd does not match workspace: {id}"));
        }
    }
    let of_session = |agent: &&AgentSnapshot| {
        let session = |key: &str| agent.persistence.as_ref().map(|p| &p[key]);
        agent.provider == request.provider
            && (session("sessionId").is_some_and(|id| *id == request.handle.as_str())
                || session("nativeHandle").is_some_and(|id| *id == request.handle.as_str()))
    };
    if (store.agents.iter().map(|(agent, _)| agent)).any(|agent| of_session(&agent)) {
        return Err(format!(
            "Provider session is already imported: {}",
            request.handle
        ));
    }
    match store.archived.values().find(of_session) {
        Some(agent) if !git::same_path(&agent.cwd, cwd) => Err(format!(
            "Provider session cwd does not match import cwd: {}",
            request.handle
        )),
        archived => Ok(archived.cloned()),
    }
}

/// [`import_session`]'s change to the store.
fn import_now(
    store: &mut Store,
    info: &ProviderInfo,
    request: &ImportSession,
    cwd: &str,
    read: Read,
) -> (Result<(AgentSnapshot, usize), String>, Vec<StoreEvent>) {
    let archived = match check(store, request, cwd) {
        Ok(archived) => archived,
        Err(error) => return (Err(error), Vec::new()),
    };
    let (workspace_id, workspace_cwd, mut events) = workspace_for(store, request, cwd, read.facts);
    let fresh = archived.is_none();
    let agent = match archived {
        Some(archived) => restored(archived, request, workspace_id),
        None => {
            // A rename, else what the first prompt makes of it, as for any
            // agent.
            let first_prompt = (read.history.iter())
                .filter(|entry| entry.item["type"] == "user_message")
                .find_map(|entry| entry.item["text"].as_str().and_then(derive_title));
            let title = read.rename.or(first_prompt);
            imported(info, request, workspace_id, &workspace_cwd, title)
        }
    };
    events.extend(store.upsert_agent(agent.clone()));
    if fresh {
        for entry in read.history {
            let at = entry.timestamp.unwrap_or_else(clock::now_iso);
            events.extend(store.append_timeline(&agent.id, entry.item, None, at));
        }
    }
    let size = (store.ensure_timeline(&agent.id)).map_or(0, |timeline| timeline.rows().len());
    (Ok((agent, size)), events)
}

/// The workspace the agent joins and the folder it runs in: the workspace
/// asked for (which [`check`] found to hold `cwd`); else the first that is
/// not a background one and holds the same folder, symlinks resolved; else
/// a new one for `cwd`. The agent runs in its workspace's own spelling of the
/// folder, which is what a client looks for when it opens the agent's
/// project.
fn workspace_for(
    store: &mut Store,
    request: &ImportSession,
    cwd: &str,
    facts: Option<GitFacts>,
) -> (String, String, Vec<StoreEvent>) {
    let existing = match &request.workspace_id {
        Some(id) => store.workspaces.get(id),
        None => (store.workspaces.iter().map(|(workspace, _)| workspace))
            .find(|w| !w.background && (w.cwd == cwd || git::same_path(&w.cwd, cwd))),
    };
    match existing.map(|w| (w.workspace_id.clone(), w.cwd.clone())) {
        Some((id, folder)) => (id, folder, Vec::new()),
        None => {
            let (id, events) = placement(store, cwd, facts, false);
            (id, cwd.to_string(), events)
        }
    }
}

/// An agent that was archived, with its session again in the workspace. The
/// agent that made it no longer has it, unless the request says so.
fn restored(
    mut agent: AgentSnapshot,
    request: &ImportSession,
    workspace_id: String,
) -> AgentSnapshot {
    agent.workspace_id = Some(workspace_id);
    agent.archived_at = None;
    agent.status = AgentStatus::Closed;
    agent.labels.extend(request.labels.clone());
    if !request.labels.contains_key(PARENT_AGENT_LABEL) {
        agent.labels.remove(PARENT_AGENT_LABEL);
    }
    agent.updated_at = clock::after(&agent.updated_at);
    agent
}

/// An agent for the session, that resumes it at its first prompt.
fn imported(
    info: &ProviderInfo,
    request: &ImportSession,
    workspace_id: String,
    cwd: &str,
    title: Option<String>,
) -> AgentSnapshot {
    let now = clock::now_iso();
    let mode = info.default_mode_id.clone();
    AgentSnapshot {
        id: ids::agent_id(),
        provider: info.id.clone(),
        cwd: cwd.to_string(),
        workspace_id: Some(workspace_id),
        model: None,
        features: Some(Vec::new()),
        thinking_option_id: None,
        effective_thinking_option_id: None,
        created_at: now.clone(),
        updated_at: now,
        last_user_message_at: None,
        status: AgentStatus::Closed,
        active_turn: None,
        capabilities: capabilities(info),
        current_mode_id: mode.clone(),
        available_modes: modes(info),
        pending_permissions: Vec::new(),
        persistence: Some(json!({
            "provider": info.id, "sessionId": request.handle, "nativeHandle": request.handle,
        })),
        runtime_info: Some(json!({
            "provider": info.id, "sessionId": request.handle, "model": null, "modeId": mode,
        })),
        last_usage: None,
        last_error: None,
        title,
        labels: request.labels.clone(),
        requires_attention: false,
        attention_reason: None,
        attention_timestamp: None,
        archived_at: None,
        // Filled from the timeline when Burrow next starts.
        last_reply_preview: None,
        cli: None,
    }
}
