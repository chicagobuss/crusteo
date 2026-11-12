//! One agent session: the provider's events turned into turns, permissions and
//! attention, and the runtime state the agent manager drives.
//!
//! A session starts lazily ([`ensure_live`]), runs in its own task
//! ([`drive`]), and ends when the provider says so: what it left running is
//! closed, and the agent is marked closed until a prompt resumes it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_protocol::directory::{ActiveTurn, AgentStatus, AttentionReason};
use burrow_provider::{Command, Image, Provider, ProviderEvent, ProviderInfo, SessionSpec};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, warn};

use super::permissions::{attention, resolved};
use super::placement::{AGENT_UNSET, agent_env};
use super::{capabilities, last_turn_number, modes, publish, record, stream, update};
use crate::clock;
use crate::coalesce::{Coalescer, coalescable};
use crate::daemon::Daemon;
use crate::store::StoreEvent;

/// How long an interrupt may take before the daemon gives up on it.
const INTERRUPT_GRACE: Duration = Duration::from_secs(15);

/// A running agent: the session's command channel and turn state.
pub(super) struct Live {
    pub(super) commands: mpsc::UnboundedSender<Command>,
    pub(super) provider: Arc<dyn Provider>,
    pub(super) info: ProviderInfo,
    /// Serialises sends, cancels and archives of one agent.
    pub(super) gate: tokio::sync::Mutex<()>,
    pub(super) state: Mutex<RunState>,
    /// Counts finished turns; waiters watch it to learn an interrupt landed.
    pub(super) turns_done: watch::Sender<u64>,
    /// Set once the session has ended and its CLI is gone.
    pub(super) ended: watch::Sender<bool>,
    /// When it last did anything: a provider event, or a send.
    pub(super) active_at: Mutex<Instant>,
    /// Moves when a queued prompt starts or is dropped.
    pub(super) started: watch::Sender<u64>,
}

/// A prompt waiting behind the running turn, ready to start one.
pub(super) struct Queued {
    pub(super) message_id: String,
    /// What the provider gets.
    pub(super) prompt: String,
    /// What the timeline gets.
    pub(super) user_message: Value,
    pub(super) images: Vec<Image>,
    /// As sent, to name an unnamed agent by.
    pub(super) text: String,
    /// Whether it may name the agent (a user's or agent's prompt may).
    pub(super) names: bool,
}

/// Queued prompts' ends (dropped, or the turn started) remembered, for
/// those waiting on them.
const REMEMBERED_ENDS: usize = 64;

#[derive(Default)]
pub(super) struct RunState {
    pub(super) active_turn: Option<String>,
    /// Set while an interrupt makes way for a new prompt: the agent stays
    /// running and no attention is raised for the canceled turn.
    pub(super) replacing: bool,
    pub(super) turn_counter: u64,
    /// Pending permissions in order: the client-facing id, the provider's
    /// own id, and the request as clients saw it.
    pub(super) permissions: Vec<(String, String, Value)>,
    pub(super) recent_sends: VecDeque<String>,
    /// The session has work running outside a turn
    /// ([`ProviderEvent::BackgroundWork`]).
    pub(super) background: bool,
    /// Prompts waiting behind the running turn, first first.
    pub(super) queued: VecDeque<Queued>,
    /// The queued prompt being started now.
    pub(super) starting: Option<String>,
    /// Queued prompts a stop dropped.
    pub(super) dropped: VecDeque<String>,
    /// Queued prompts started: message id and turn id.
    pub(super) started: VecDeque<(String, String)>,
}

/// Starts the agent's provider session if it is not running (lazy load).
pub(super) fn ensure_live(daemon: &Arc<Daemon>, agent_id: &str) -> Result<Arc<Live>, String> {
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
        // Paseo's tools, as `paseo`; providers that take MCP servers use it.
        mcp_servers: crate::mcp::servers_for(daemon, agent_id),
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
        started: watch::channel(0).0,
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
        agent.cli = super::live_cli(&live, None);
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
            agent.cli = None;
        });
    }
    live.drop_queued(&agent_id, "its session ended");
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

pub(super) fn active_turn(live: &Live) -> Option<String> {
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

fn finish_turn(
    daemon: &Arc<Daemon>,
    agent_id: &str,
    live: &Arc<Live>,
    turn: Option<String>,
    end: TurnEnd,
) {
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
            // A message of its own, so it does not run on from the reply.
            record(
                daemon,
                agent_id,
                None,
                json!({"type": "assistant_message", "text": format!("[System Error] {}", parts.join("\n\n")),
                       "messageId": format!("{turn_id}:error")}),
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
    // The reply the turn ended with, for lists and searches: a failed turn
    // shows its error instead and keeps the last good reply.
    let reply = matches!(end, TurnEnd::Completed(_) | TurnEnd::Canceled(_))
        .then(|| daemon.read(|store| super::last_assistant_text(store, agent_id)))
        .filter(|text| !text.trim().is_empty())
        .map(|text| super::preview(&text));
    let mut raised = None;
    let agent = update(daemon, agent_id, |agent| {
        let was_running = agent.status == AgentStatus::Running;
        agent.active_turn = None;
        if let Some(reply) = reply {
            agent.last_reply_preview = Some(reply);
        }
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
    // What waited behind the turn goes next, unless a new prompt replaced
    // the turn (it waits behind that one) or the agent was stopped.
    match end {
        _ if replacing => {}
        TurnEnd::Completed(_) | TurnEnd::Failed { .. } => {
            if live.has_queued() {
                tokio::spawn(super::messages::drain(
                    daemon.clone(),
                    agent_id.to_string(),
                    live.clone(),
                ));
            }
        }
        TurnEnd::Canceled(_) => live.drop_queued(agent_id, "it was stopped"),
        TurnEnd::Closed => {}
    }
}

/// Waits until the turn count moves past `seen`.
async fn turn_finished(live: &Live, seen: u64) -> bool {
    let mut done = live.turns_done.subscribe();
    tokio::time::timeout(INTERRUPT_GRACE, done.wait_for(|n| *n > seen))
        .await
        .is_ok_and(|r| r.is_ok())
}

/// Interrupts the running turn, if any, and waits for it to end.
pub(super) async fn interrupt(live: &Live) -> Result<(), ()> {
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
    pub(super) fn touch(&self) {
        *self.active_at.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }

    /// A turn, a question, a prompt making way for another, or work outside
    /// a turn: anything that stopping its CLI would lose.
    pub(super) fn busy(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.active_turn.is_some()
            || !state.permissions.is_empty()
            || state.replacing
            || state.background
            || !state.queued.is_empty()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RunState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Makes a new foreground turn the active one; its id.
    pub(super) fn begin_turn(&self) -> String {
        let mut state = self.state();
        state.turn_counter += 1;
        let turn = self.provider.foreground_turn_id(state.turn_counter);
        state.active_turn = Some(turn.clone());
        turn
    }

    fn has_queued(&self) -> bool {
        !self.state().queued.is_empty()
    }

    /// With no turn running: the first queued prompt, taken off the queue,
    /// and the turn it is to start, made the active one.
    pub(super) fn next_queued(&self) -> Option<(Queued, String)> {
        let mut state = self.state();
        if state.active_turn.is_some() {
            return None;
        }
        let next = state.queued.pop_front()?;
        state.starting = Some(next.message_id.clone());
        state.turn_counter += 1;
        let turn = self.provider.foreground_turn_id(state.turn_counter);
        state.active_turn = Some(turn.clone());
        Some((next, turn))
    }

    /// The queued prompt `message_id` has started `turn`.
    pub(super) fn started_queued(&self, message_id: &str, turn: &str) {
        let mut state = self.state();
        if state.starting.as_deref() == Some(message_id) {
            state.starting = None;
        }
        if state.started.len() >= REMEMBERED_ENDS {
            state.started.pop_front();
        }
        state
            .started
            .push_back((message_id.to_string(), turn.to_string()));
        drop(state);
        self.started.send_modify(|n| *n += 1);
    }

    /// Drops what waits behind the turn, for `why` (logged).
    pub(super) fn drop_queued(&self, agent_id: &str, why: &str) {
        let count = {
            let mut state = self.state();
            let dropped: Vec<String> = state.queued.drain(..).map(|q| q.message_id).collect();
            for id in &dropped {
                if state.dropped.len() >= REMEMBERED_ENDS {
                    state.dropped.pop_front();
                }
                state.dropped.push_back(id.clone());
            }
            dropped.len()
        };
        if count > 0 {
            warn!(agent_id, count, why, "queued prompts dropped");
            self.started.send_modify(|n| *n += 1);
        }
    }

    /// Where the prompt `message_id` stands: still waiting (`None`), or how
    /// its wait ended.
    pub(super) fn waiting(&self, message_id: &str) -> Option<super::messages::Waited> {
        use super::messages::Waited;
        let state = self.state();
        if state.starting.as_deref() == Some(message_id)
            || state.queued.iter().any(|q| q.message_id == message_id)
        {
            None
        } else if state.dropped.iter().any(|id| id == message_id) {
            Some(Waited::Dropped)
        } else {
            let turn = (state.started.iter())
                .find(|(id, _)| id == message_id)
                .map(|(_, turn)| turn.clone());
            Some(Waited::Started { turn })
        }
    }

    pub(super) fn idle_for(&self) -> Duration {
        self.active_at
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .elapsed()
    }
}
