//! The agent requests that are not prompts, permissions or attention:
//! finishing, detaching, cancelling, archiving, deleting, refreshing, renaming
//! and configuring.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use burrow_protocol::directory::{AgentSnapshot, AgentStatus};
use burrow_provider::Command;
use serde_json::{Value, json};

use super::create::PARENT_AGENT_LABEL;
use super::live::{ensure_live, interrupt};
use super::{last_assistant_text, resolve, turn_error, turn_text, unarchive, update};
use crate::clock;
use crate::daemon::Daemon;
use crate::store::StoreEvent;

/// `wait_for_finish_request`, answered as Paseo answers it: at once when the
/// agent waits for a permission or is not working, else as soon as it asks
/// for one or its turn ends; `timeout` after `timeout_ms`. The payload's
/// `status` is `idle`, `error`, `permission` or `timeout`, with the agent as
/// `final`, its last error, and its last reply as `lastMessage`.
pub async fn wait_for_finish(daemon: &Arc<Daemon>, key: &str, timeout_ms: Option<u64>) -> Value {
    wait_for_turn(daemon, key, None, timeout_ms).await
}

/// [`wait_for_finish`], for one turn when `turn` names it: answered once it
/// is no longer the agent's running turn, from that turn's own rows, so a
/// turn that starts straight after it (a queued prompt's) is not taken for
/// it. A permission only counts while the turn runs.
pub async fn wait_for_turn(
    daemon: &Arc<Daemon>,
    key: &str,
    turn: Option<&str>,
    timeout_ms: Option<u64>,
) -> Value {
    let answer = |status: &str, agent: Option<&AgentSnapshot>, error: Option<String>| {
        let last = agent
            .map(|agent| {
                daemon.read(|store| match turn {
                    Some(turn) => turn_text(store, &agent.id, turn),
                    None => last_assistant_text(store, &agent.id),
                })
            })
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
        if let Some(turn) = turn {
            let running = (agent.active_turn.as_ref()).is_some_and(|t| t.turn_id == turn);
            if running && !agent.pending_permissions.is_empty() {
                return answer("permission", Some(&agent), None);
            }
            if !running {
                return match daemon.read(|store| turn_error(store, &agent_id, turn)) {
                    Some(error) => answer("error", Some(&agent), Some(error)),
                    None => answer("idle", Some(&agent), None),
                };
            }
        }
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
    // Its schedules end, as Paseo's `completeForAgent`.
    daemon.schedules.complete_for_agent(&agent_id);
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
    super::until_provider_of(daemon, &agent_id).await;
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

/// Its provider must be offered: while the providers are still being looked
/// for at startup, callers wait first with [`super::until_provider_of`].
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
