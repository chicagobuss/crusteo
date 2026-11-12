//! Letting go of sessions: the idle release Paseo 0.14 makes, the explicit
//! close, and the restart a prompt makes when its provider's CLI was upgraded.

use std::sync::Arc;
use std::time::Duration;

use burrow_protocol::directory::AgentStatus;
use burrow_provider::Command;
use tokio::sync::watch;
use tracing::{info, warn};

use super::create::PARENT_AGENT_LABEL;
use super::live::{Live, interrupt};
use super::{resolve, update};
use crate::daemon::Daemon;

/// How long a session told to stop gets before its replacement starts
/// anyway. Codex's gives app-server 2 s to exit.
const SESSION_END_WAIT: Duration = Duration::from_secs(10);

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
        super::set_cli(daemon, agent_id, None);
        released.push(agent_id.clone());
    }
    released
}

/// `kill_agent` (Paseo's `closeAgent`): ends the agent's CLI, its turn
/// with it. The agent stays, `closed`, and a later prompt resumes it.
pub async fn close(daemon: &Arc<Daemon>, key: &str) -> Result<String, String> {
    let agent_id = resolve(daemon, key)?;
    if let Some(live) = daemon.agents.live(&agent_id) {
        let _gate = live.gate.lock().await;
        let _ = interrupt(&live).await;
        let ended = live.ended.subscribe();
        (daemon.agents.released.lock())
            .unwrap_or_else(|p| p.into_inner())
            .insert(agent_id.clone(), ended);
        daemon.agents.forget(&agent_id);
        let _ = live.commands.send(Command::Shutdown);
    }
    update(daemon, &agent_id, |agent| {
        agent.status = AgentStatus::Closed;
        agent.active_turn = None;
        agent.pending_permissions.clear();
        agent.cli = None;
    });
    Ok(agent_id)
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
pub(super) async fn restart_if_superseded(daemon: &Arc<Daemon>, agent_id: &str) {
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
