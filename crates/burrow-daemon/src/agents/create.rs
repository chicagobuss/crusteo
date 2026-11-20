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
    /// With no `mode_id`, take the mode from the caller (Paseo does this
    /// for its MCP `create_agent`, not for a client's create request).
    pub mode_from_caller: bool,
    /// Made to run with nobody watching (a schedule's run): with no
    /// `mode_id`, it gets the provider's unattended mode.
    pub unattended: bool,
}

/// The caller a new agent may take its mode from.
#[derive(Debug, Clone, Copy)]
pub struct ModeParent<'a> {
    pub provider: &'a str,
    pub mode_id: Option<&'a str>,
    /// The caller's mode runs without asking.
    pub unattended: bool,
}

/// The mode a new agent starts in, or `None` for the provider's default
/// (Paseo's `resolveAndValidateCreateAgentMode`).
///
/// - A mode asked for must be one of the provider's.
/// - Without one, a caller on the same provider passes on its mode; an
///   unattended caller (or an unattended creation) on another provider gets
///   that provider's unattended mode; otherwise a caller on another provider
///   is refused, since its mode means nothing there.
///
/// A provider that lists no modes before a session (they come from its
/// sessions) takes any mode asked for, and its default otherwise.
pub fn resolve_mode(
    requested: Option<&str>,
    provider: &str,
    modes: &[burrow_provider::Mode],
    parent: Option<ModeParent<'_>>,
    unattended: bool,
) -> Result<Option<String>, String> {
    let list = || {
        modes
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let unattended_mode = modes.iter().find(|m| m.unattended).map(|m| m.id.clone());
    if let Some(mode) = requested {
        if !modes.is_empty() && !modes.iter().any(|m| m.id == mode) {
            return Err(format!(
                "Invalid mode '{mode}' for provider '{provider}'. Available modes: {}",
                list()
            ));
        }
        return Ok(Some(mode.to_string()));
    }
    let Some(parent) = parent else {
        return Ok(if unattended { unattended_mode } else { None });
    };
    if parent.provider == provider {
        // A mode an earlier Burrow took unchecked is not passed on: the
        // provider's default instead.
        let known = |mode: &&str| modes.is_empty() || modes.iter().any(|m| m.id == *mode);
        return Ok(parent.mode_id.filter(known).map(str::to_string));
    }
    if (unattended || parent.unattended)
        && let Some(mode) = unattended_mode
    {
        return Ok(Some(mode));
    }
    if modes.is_empty() {
        return Ok(None);
    }
    Err(format!(
        "cannot inherit mode '{}' from caller (provider '{}') for new agent (provider '{provider}'). \
         Pass an explicit mode. Available modes for '{provider}': {}",
        parent.mode_id.unwrap_or("<none>"),
        parent.provider,
        list()
    ))
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
    let info = provider.info();
    let caller = match request.caller_agent_id.clone() {
        Some(caller_id) => Some(
            daemon
                .read(|store| store.agents.get(&caller_id).cloned())
                .ok_or_else(|| format!("Caller agent {caller_id} not found"))?,
        ),
        None => None,
    };
    // The caller runs unattended when its mode is its provider's unattended one.
    let parent_unattended = caller.as_ref().is_some_and(|caller| {
        let mode = caller.current_mode_id.as_deref();
        daemon.agents.provider(&caller.provider).is_some_and(|p| {
            p.info()
                .modes
                .iter()
                .any(|m| m.unattended && Some(m.id.as_str()) == mode)
        })
    });
    request.mode_id = resolve_mode(
        request.mode_id.as_deref(),
        &info.id,
        &info.modes,
        caller
            .as_ref()
            .filter(|_| request.mode_from_caller)
            .map(|caller| ModeParent {
                provider: &caller.provider,
                mode_id: caller.current_mode_id.as_deref(),
                unattended: parent_unattended,
            }),
        request.unattended,
    )?;
    if let Some(caller) = caller {
        let caller_id = caller.id.clone();
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

#[cfg(test)]
mod tests {
    use super::{ModeParent, resolve_mode};
    use burrow_provider::Mode;

    fn modes(ids: &[&str], unattended: Option<&str>) -> Vec<Mode> {
        ids.iter()
            .map(|id| Mode {
                id: id.to_string(),
                unattended: Some(*id) == unattended,
                ..Mode::default()
            })
            .collect()
    }

    fn codex() -> Vec<Mode> {
        modes(&["auto", "full-access"], Some("full-access"))
    }

    fn parent<'a>(provider: &'a str, mode: Option<&'a str>, unattended: bool) -> ModeParent<'a> {
        ModeParent {
            provider,
            mode_id: mode,
            unattended,
        }
    }

    #[test]
    fn a_mode_asked_for_must_be_the_providers() {
        let opencode = modes(&["build", "plan"], None);
        assert_eq!(
            resolve_mode(Some("plan"), "opencode", &opencode, None, false),
            Ok(Some("plan".into()))
        );
        assert_eq!(
            resolve_mode(Some("bypassPermissions"), "codex", &codex(), None, false),
            Err("Invalid mode 'bypassPermissions' for provider 'codex'. Available modes: auto, full-access".into())
        );
        // An explicit mode wins over an unattended caller.
        let claude = parent("claude", Some("bypassPermissions"), true);
        assert_eq!(
            resolve_mode(Some("auto"), "codex", &codex(), Some(claude), false),
            Ok(Some("auto".into()))
        );
    }

    #[test]
    fn a_provider_without_listed_modes_takes_any_mode() {
        assert_eq!(
            resolve_mode(Some("agent"), "cursor", &[], None, false),
            Ok(Some("agent".into()))
        );
        let claude = parent("claude", Some("default"), false);
        assert_eq!(resolve_mode(None, "pi", &[], Some(claude), false), Ok(None));
    }

    #[test]
    fn without_a_caller_the_default_or_the_unattended_mode() {
        assert_eq!(resolve_mode(None, "codex", &codex(), None, false), Ok(None));
        assert_eq!(
            resolve_mode(None, "codex", &codex(), None, true),
            Ok(Some("full-access".into()))
        );
    }

    #[test]
    fn a_caller_on_the_same_provider_passes_on_its_mode() {
        let claude = modes(&["default", "bypassPermissions"], Some("bypassPermissions"));
        let caller = parent("claude", Some("bypassPermissions"), true);
        assert_eq!(
            resolve_mode(None, "claude", &claude, Some(caller), false),
            Ok(Some("bypassPermissions".into()))
        );
        let caller = parent("claude", None, false);
        assert_eq!(
            resolve_mode(None, "claude", &claude, Some(caller), false),
            Ok(None)
        );
        // One an earlier Burrow took unchecked: the provider's default.
        let caller = parent("codex", Some("bypassPermissions"), false);
        assert_eq!(
            resolve_mode(None, "codex", &codex(), Some(caller), false),
            Ok(None)
        );
    }

    #[test]
    fn an_unattended_caller_on_another_provider_gets_its_unattended_mode() {
        let caller = parent("claude", Some("bypassPermissions"), true);
        assert_eq!(
            resolve_mode(None, "codex", &codex(), Some(caller), false),
            Ok(Some("full-access".into()))
        );
    }

    #[test]
    fn another_providers_mode_is_refused() {
        let caller = parent("claude", Some("default"), false);
        assert_eq!(
            resolve_mode(None, "codex", &codex(), Some(caller), false),
            Err("cannot inherit mode 'default' from caller (provider 'claude') for new agent (provider 'codex'). Pass an explicit mode. Available modes for 'codex': auto, full-access".into())
        );
        // An unattended caller, but no unattended mode to give.
        let opencode = modes(&["build", "plan"], None);
        let caller = parent("claude", Some("bypassPermissions"), true);
        assert!(resolve_mode(None, "opencode", &opencode, Some(caller), false).is_err());
        let caller = parent("opencode", None, false);
        assert!(
            resolve_mode(None, "codex", &codex(), Some(caller), false)
                .unwrap_err()
                .contains("cannot inherit mode '<none>'")
        );
    }
}
