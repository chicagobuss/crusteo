//! Prompts to an agent: from its user, another agent (Paseo's
//! `<paseo-system>` envelopes), a notice about a child, or a schedule, and
//! the turns they start, join, wait behind or replace.
//!
//! A prompt for an agent mid-turn is steered into the turn, queued behind
//! it, or replaces it ([`Busy`]). Agents, notices and schedules default to
//! steering where the provider can and queueing where it cannot, so one
//! agent never cuts another off unless it asks to. The queue is held in
//! memory: Burrow restarts only when its agents are quiet, so there is
//! nothing to keep across one.

use std::sync::Arc;
use std::time::Duration;

use burrow_protocol::directory::{ActiveTurn, AgentStatus};
use burrow_provider::{Command, Image};
use serde_json::{Value, json};
use tokio::time::Instant;
use tracing::warn;

use super::idle::restart_if_superseded;
use super::live::{Live, Queued, active_turn, ensure_live, interrupt};
use super::{derive_title, record, resolve, stream, unarchive, update};
use crate::clock;
use crate::daemon::Daemon;
use crate::ids;

/// Client message ids remembered per agent, to drop duplicate sends.
const REMEMBERED_SENDS: usize = 256;
/// Prompts that may wait behind one agent's turn.
pub const QUEUE_LIMIT: usize = 20;

/// What a prompt does to an agent that is mid-turn (`activeTurnBehavior`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    /// Steer where the provider can, else wait behind the turn.
    Auto,
    /// Steer where the provider can, else replace the turn (Paseo's).
    Steer,
    /// Wait behind the turn, even where it could be steered.
    Queue,
    /// Replace the turn.
    Interrupt,
}

impl Busy {
    /// As sent: `auto`, `steer`, `queue` or `interrupt`. Unsent (or not one
    /// of those), an agent's prompt is `auto` and a user's `interrupt`, as
    /// Paseo's apps expect.
    pub fn parse(value: Option<&str>, from_agent: bool) -> Busy {
        match value.map(str::trim) {
            Some("auto") => Busy::Auto,
            Some("steer") => Busy::Steer,
            Some("queue") => Busy::Queue,
            Some("interrupt") => Busy::Interrupt,
            _ if from_agent => Busy::Auto,
            _ => Busy::Interrupt,
        }
    }
}

/// What became of a prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// It started a turn.
    Started,
    /// It joined the running turn.
    Steered,
    /// It waits behind the running turn, at this place (1 is next).
    Queued { position: usize },
    /// It replaced the running turn.
    Interrupted,
    /// A retried send that already went through.
    Duplicate,
}

impl Delivery {
    /// As receipts name it.
    pub fn name(&self) -> &'static str {
        match self {
            Delivery::Started => "started",
            Delivery::Steered => "steered",
            Delivery::Queued { .. } => "queued",
            Delivery::Interrupted => "interrupted",
            Delivery::Duplicate => "duplicate",
        }
    }
}

/// A delivered prompt: the agent it went to (resolved), its message id, and
/// what became of it.
#[derive(Clone, Debug)]
pub struct Delivered {
    pub agent_id: String,
    pub message_id: String,
    pub delivery: Delivery,
    /// The turn it started or joined; none while it waits.
    pub turn_id: Option<String>,
}

/// `send_agent_message_request`.
pub async fn send(
    daemon: &Arc<Daemon>,
    key: &str,
    text: &str,
    message_id: Option<String>,
    images: Vec<Image>,
    behavior: Option<&str>,
    source_agent: Option<&str>,
) -> Result<Delivered, String> {
    let from = match source_agent.filter(|s| !s.trim().is_empty()) {
        Some(sender) => From::Agent(sender),
        None => From::User,
    };
    let busy = Busy::parse(behavior, matches!(from, From::Agent(_)));
    deliver(daemon, key, text, message_id, images, busy, from).await
}

/// The id an outside sender's messages carry: no agent has it, so nothing
/// is ever told about the turn it starts.
pub const OUTSIDE_PREFIX: &str = "external:";

/// `send_agent_message_request` with `sourceName` (Burrow's own): a prompt
/// from a sender outside Burrow — a script, a CI job, an assistant on
/// another machine — delivered as another agent's message from `name`, so
/// clients show it as "From <name>" rather than as their user's prompt.
pub async fn send_from_outside(
    daemon: &Arc<Daemon>,
    key: &str,
    text: &str,
    message_id: Option<String>,
    images: Vec<Image>,
    behavior: Option<&str>,
    name: &str,
) -> Result<Delivered, String> {
    let name = outside_name(name)?;
    let busy = Busy::parse(behavior, true);
    let from = From::Outside(&name);
    deliver(daemon, key, text, message_id, images, busy, from).await
}

/// A sender's name as messages show it: one line, trimmed, up to 64
/// characters.
fn outside_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.contains(['\n', '\r']) {
        return Err("A sender's name is one line of 1 to 64 characters".into());
    }
    Ok(name.to_string())
}

/// How a wait for a queued prompt ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waited {
    /// Its turn has started (or it never waited), this one when known.
    Started {
        turn: Option<String>,
    },
    /// The agent was stopped, closed or restarted before it got to it.
    Dropped,
    TimedOut,
}

/// Waits until the prompt `message_id` no longer waits behind `agent_id`'s
/// turn: started, or dropped by a stop. One the session does not know of (it
/// ended or was restarted since) is looked for in the timeline: its turn if
/// it got there, else dropped, so no earlier turn is taken for it.
pub async fn wait_started(
    daemon: &Arc<Daemon>,
    agent_id: &str,
    message_id: &str,
    deadline: Option<Instant>,
) -> Waited {
    let Some(live) = daemon.agents.live(agent_id) else {
        return in_timeline(daemon, agent_id, message_id);
    };
    let mut started = live.started.subscribe();
    loop {
        match live.waiting(message_id) {
            Some(Waited::Started { turn: None }) => {
                return in_timeline(daemon, agent_id, message_id);
            }
            Some(waited) => return waited,
            None => {}
        }
        let changed = started.changed();
        let changed = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, changed).await {
                Ok(changed) => changed,
                Err(_) => return Waited::TimedOut,
            },
            None => changed.await,
        };
        if changed.is_err() {
            return in_timeline(daemon, agent_id, message_id);
        }
    }
}

/// Where the prompt `message_id` went, by the agent's timeline: the turn its
/// row was recorded in, or dropped when it never got there.
fn in_timeline(daemon: &Daemon, agent_id: &str, message_id: &str) -> Waited {
    let call = format!("paseo-agent-message:{message_id}");
    let found = daemon.apply(|store| {
        let found = store.ensure_timeline(agent_id).and_then(|timeline| {
            (timeline.rows().iter().rev())
                .find(|row| {
                    row.item["messageId"] == message_id || row.item["callId"] == call.as_str()
                })
                .map(|row| row.turn_id.clone())
        });
        (found, Vec::new())
    });
    match found {
        Some(turn) => Waited::Started { turn },
        None => Waited::Dropped,
    }
}

/// `wait_started` for up to `timeout_ms` (none or 0: no limit), and what is
/// left of it after.
pub async fn wait_started_within(
    daemon: &Arc<Daemon>,
    agent_id: &str,
    message_id: &str,
    timeout_ms: Option<u64>,
) -> (Waited, Option<u64>) {
    let limit = timeout_ms.filter(|ms| *ms > 0).map(Duration::from_millis);
    let begun = Instant::now();
    let waited = wait_started(daemon, agent_id, message_id, limit.map(|l| begun + l)).await;
    let left = limit.map(|l| l.saturating_sub(begun.elapsed()).as_millis().max(1) as u64);
    (waited, left)
}

/// A schedule's prompt, as Paseo's `formatSystemNotificationPrompt` sends
/// it: into a running turn as [`Busy::Auto`] has it, the provider gets it in a bare
/// `<paseo-system>` envelope, and the timeline has it as a `heartbeat`
/// call, as Paseo projects such a prompt.
pub(crate) async fn send_system(
    daemon: &Arc<Daemon>,
    key: &str,
    body: &str,
) -> Result<Delivered, String> {
    deliver(
        daemon,
        key,
        body,
        None,
        Vec::new(),
        Busy::Auto,
        From::System,
    )
    .await
}

/// A notice that an agent finished, failed or needs permission, to the
/// agent that asked to be told (Paseo's finish notifications): into a
/// running turn as [`Busy::Auto`] has it (Paseo steers, else interrupts),
/// from `child`, in an `agent-notification` envelope whose
/// `event` is `finished`, `errored`, `permission-required` or `closed`.
/// An archived recipient is left archived.
pub(crate) async fn send_notice(
    daemon: &Arc<Daemon>,
    recipient: &str,
    child: &str,
    event: &str,
    body: &str,
) -> Result<Delivered, String> {
    let from = From::Notice { child, event };
    deliver(daemon, recipient, body, None, Vec::new(), Busy::Auto, from).await
}

/// [`send_notice`] under a message id, so the same notice sent again (a
/// Reef retrying one about an agent on another host) is delivered once.
pub(crate) async fn send_notice_once(
    daemon: &Arc<Daemon>,
    recipient: &str,
    child: &str,
    event: &str,
    body: &str,
    message_id: String,
) -> Result<Delivered, String> {
    let from = From::Notice { child, event };
    let id = Some(message_id);
    deliver(daemon, recipient, body, id, Vec::new(), Busy::Auto, from).await
}

/// Who a prompt is from.
#[derive(Clone, Copy)]
enum From<'a> {
    User,
    /// Another agent, by id.
    Agent(&'a str),
    /// A sender outside Burrow, by the name it gave.
    Outside(&'a str),
    /// A notice about another agent (`event`).
    Notice {
        child: &'a str,
        event: &'a str,
    },
    /// The daemon itself: a schedule firing.
    System,
}

async fn deliver(
    daemon: &Arc<Daemon>,
    key: &str,
    text: &str,
    message_id: Option<String>,
    images: Vec<Image>,
    busy: Busy,
    from: From<'_>,
) -> Result<Delivered, String> {
    let agent_id = resolve(daemon, key)?;
    super::until_provider_of(daemon, &agent_id).await;
    if !matches!(from, From::Notice { .. }) {
        unarchive(daemon, &agent_id);
    }
    restart_if_superseded(daemon, &agent_id).await;
    // A session whose CLI exited as this waited for it is let go of under
    // the gate; the next one takes the prompt.
    let mut live = ensure_live(daemon, &agent_id)?;
    let mut gate = live.gate.lock().await;
    for _ in 0..2 {
        if !*live.ended.borrow() {
            break;
        }
        drop(gate);
        live = ensure_live(daemon, &agent_id)?;
        gate = live.gate.lock().await;
    }
    live.touch();
    let client_message_id = message_id.clone().unwrap_or_else(ids::agent_id);
    let delivered = |delivery, turn_id| Delivered {
        agent_id: agent_id.clone(),
        message_id: client_message_id.clone(),
        delivery,
        turn_id,
    };
    if let Some(message_id) = &message_id
        && (live.state.lock().unwrap_or_else(|p| p.into_inner()))
            .recent_sends
            .contains(message_id)
    {
        return Ok(delivered(Delivery::Duplicate, None));
    }
    // Remembered once taken (under the gate, so no copy gets in first): a
    // send refused can be sent again.
    let remember = || {
        if let Some(message_id) = &message_id {
            let mut state = live.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.recent_sends.len() >= REMEMBERED_SENDS {
                state.recent_sends.pop_front();
            }
            state.recent_sends.push_back(message_id.clone());
        }
    };
    let text = text.trim().to_string();
    let (prompt, mut user_message) = match from {
        From::Agent(sender) => from_agent(daemon, sender, &client_message_id, &text, None),
        From::Outside(name) => envelope(
            &format!("{OUTSIDE_PREFIX}{name}"),
            Some(name.to_string()),
            &client_message_id,
            &text,
            None,
        ),
        From::Notice { child, event } => {
            from_agent(daemon, child, &client_message_id, &text, Some(event))
        }
        From::System => from_system(&text),
        From::User => (
            text.clone(),
            json!({"type": "user_message", "text": text,
                   "messageId": client_message_id, "clientMessageId": client_message_id}),
        ),
    };
    // Kept, unlike Paseo, so other clients can show them too.
    if !images.is_empty() && matches!(from, From::User) {
        let (keeper, kept) = (daemon.clone(), images.clone());
        let kept = tokio::task::spawn_blocking(move || keeper.attachments.keep(&kept))
            .await
            .unwrap_or_default();
        if !kept.is_empty() {
            user_message["images"] = kept.into();
        }
    }

    let mut delivery = Delivery::Started;
    if let Some(turn) = active_turn(&live) {
        let can_steer = live.info.capabilities.steer;
        match busy {
            Busy::Auto | Busy::Steer if can_steer => {
                let sent = live.commands.send(Command::Steer {
                    message_id: client_message_id.clone(),
                    text: prompt,
                    images,
                });
                if sent.is_err() {
                    return Err(format!("Agent {agent_id} is not running"));
                }
                remember();
                record(daemon, &agent_id, Some(turn.clone()), user_message);
                update(daemon, &agent_id, |agent| {
                    agent.last_user_message_at = Some(clock::now_iso());
                });
                return Ok(delivered(Delivery::Steered, Some(turn)));
            }
            Busy::Auto | Busy::Queue => {
                let mut state = live.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.queued.len() >= QUEUE_LIMIT {
                    return Err(format!(
                        "Agent {agent_id} already has {QUEUE_LIMIT} prompts waiting behind its turn"
                    ));
                }
                state.queued.push_back(Queued {
                    message_id: client_message_id.clone(),
                    prompt,
                    user_message,
                    images,
                    text,
                    names: matches!(from, From::User | From::Agent(_) | From::Outside(_)),
                });
                let position = state.queued.len();
                drop(state);
                remember();
                return Ok(delivered(Delivery::Queued { position }, None));
            }
            Busy::Steer | Busy::Interrupt => {}
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
        delivery = Delivery::Interrupted;
    }

    let turn_id = live.begin_turn();
    let queued = Queued {
        message_id: client_message_id.clone(),
        prompt,
        user_message,
        images,
        text,
        names: matches!(from, From::User | From::Agent(_) | From::Outside(_)),
    };
    start(daemon, &agent_id, &live, queued, turn_id.clone())?;
    remember();
    Ok(delivered(delivery, Some(turn_id)))
}

/// Starts `turn_id` (already the live session's active turn) with a prompt.
/// When the session has ended, or ends as the prompt goes to it, the turn is
/// given up and the agent is left closed, not running.
fn start(
    daemon: &Daemon,
    agent_id: &str,
    live: &Live,
    prompt: Queued,
    turn_id: String,
) -> Result<(), String> {
    let not_running = || format!("Agent {agent_id} is not running");
    if *live.ended.borrow() || live.commands.is_closed() {
        live.give_up_turn(&turn_id);
        return Err(not_running());
    }
    let started_at = clock::now_iso();
    stream(
        daemon,
        agent_id,
        json!({"type": "turn_started", "provider": live.info.id, "turnId": turn_id}),
    );
    record(daemon, agent_id, Some(turn_id.clone()), prompt.user_message);
    update(daemon, agent_id, |agent| {
        // A schedule's prompt or a notice does not name the agent.
        if agent.title.is_none() && agent.last_user_message_at.is_none() && prompt.names {
            agent.title = derive_title(&prompt.text);
        }
        agent.status = AgentStatus::Running;
        agent.last_error = None;
        agent.last_user_message_at = Some(started_at.clone());
        agent.active_turn = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            started_at: Some(started_at),
        });
    });
    let sent = live.commands.send(Command::Prompt {
        turn_id: turn_id.clone(),
        message_id: prompt.message_id,
        text: prompt.prompt,
        images: prompt.images,
    });
    // The session's end is marked before it closes its turn, so an end not
    // seen here still finds this turn and closes it.
    if sent.is_err() || *live.ended.borrow() {
        super::live::abandon_turn(daemon, agent_id, live, &turn_id);
        return Err(not_running());
    }
    Ok(())
}

/// Once a turn has ended: starts the prompt waiting behind it, if any and
/// nothing else got in first (the next turn's end then starts it).
pub(super) async fn drain(daemon: Arc<Daemon>, agent_id: String, live: Arc<Live>) {
    let _gate = live.gate.lock().await;
    if *live.ended.borrow() {
        return; // the session ended; its queue went with it
    }
    let Some((next, turn_id)) = live.next_queued() else {
        return;
    };
    let message_id = next.message_id.clone();
    match start(&daemon, &agent_id, &live, next, turn_id.clone()) {
        Ok(()) => live.started_queued(&message_id, &turn_id),
        Err(error) => {
            warn!(agent_id, %error, "a queued prompt did not start");
            live.drop_starting(&message_id);
        }
    }
}

/// A prompt another agent sent, as Paseo 0.12 delivers one: the provider
/// gets it in a `<paseo-system>` envelope naming the sender, so the agent
/// knows it is not from its user, and the timeline has it as an
/// `agent_message` call from the sender, which clients show as "From ...".
/// With an `event`, it is a notice about the sender (`agent-notification`),
/// shown as "worker finished". Returns the provider's text and the
/// timeline item.
fn from_agent(
    daemon: &Daemon,
    sender: &str,
    message_id: &str,
    text: &str,
    event: Option<&str>,
) -> (String, Value) {
    let title = daemon
        .read(|store| {
            store
                .find_agent(sender)
                .and_then(|agent| agent.title.clone())
        })
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty());
    envelope(sender, title, message_id, text, event)
}

/// [`from_agent`]'s envelope and timeline item, for sender id `sender`
/// going by `title`.
fn envelope(
    sender: &str,
    title: Option<String>,
    message_id: &str,
    text: &str,
    event: Option<&str>,
) -> (String, Value) {
    let named = match &title {
        Some(title) => format!(" source-agent-title=\"{}\"", escape_xml(title)),
        None => String::new(),
    };
    let (kind, attribute) = match event {
        Some(event) => ("agent-notification", format!(" event=\"{event}\"")),
        None => ("agent-message", String::new()),
    };
    let envelope = format!(
        "<paseo-system version=\"1\" kind=\"{kind}\" source-agent-id=\"{}\" message-id=\"{}\"{named}{attribute}>\n{}\n</paseo-system>",
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
        "agentMessage": { "event": event.unwrap_or("message"), "sender": from, "text": text },
        "name": "agent_message",
        "status": "completed",
        "error": null,
        "detail": { "type": "plain_text", "text": text, "icon": "bot" },
    });
    (envelope, item)
}

/// A daemon's own prompt in Paseo's bare `<paseo-system>` envelope, and its
/// timeline item, a `heartbeat` call named by the envelope's hash, as
/// Paseo's `projectAgentMessage` makes it.
fn from_system(body: &str) -> (String, Value) {
    use sha2::Digest;
    let envelope = format!("<paseo-system>\n{body}\n</paseo-system>");
    let hash = sha2::Sha256::digest(envelope.as_bytes());
    let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
    let item = json!({
        "type": "tool_call",
        "callId": format!("paseo-system:{hex}"),
        "name": "heartbeat",
        "status": "completed",
        "error": null,
        "detail": { "type": "plain_text", "text": body, "icon": "sparkles" },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::Config;
    use burrow_provider::echo::EchoProvider;

    /// Review #8 (0.2.1): a send that got the agent's session just before it
    /// ended starts no turn on it, and leaves the agent as it was, not
    /// running with a turn nothing will finish.
    #[tokio::test]
    async fn a_prompt_for_a_session_that_has_ended_starts_no_turn() {
        let daemon = Daemon::new(Config::ephemeral());
        daemon.agents.register(Arc::new(EchoProvider {
            chunk_delay: Duration::ZERO,
            slow_delay: Duration::ZERO,
        }));
        let cwd = std::env::temp_dir().join(format!("burrow-start-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let request = super::super::CreateAgent {
            provider: "echo".into(),
            cwd: cwd.to_string_lossy().into(),
            ..Default::default()
        };
        let agent = super::super::create(&daemon, request)
            .await
            .expect("create");
        let live = ensure_live(&daemon, &agent.id).expect("live");

        // Its CLI exits between the send taking the session and starting.
        live.ended.send_replace(true);
        let turn = live.begin_turn();
        let prompt = Queued {
            message_id: "m-1".into(),
            prompt: "hello".into(),
            user_message: json!({"type": "user_message", "text": "hello", "messageId": "m-1"}),
            images: Vec::new(),
            text: "hello".into(),
            names: true,
        };
        let started = start(&daemon, &agent.id, &live, prompt, turn);
        assert_eq!(started, Err(format!("Agent {} is not running", agent.id)));
        assert_eq!(active_turn(&live), None);
        let after = daemon
            .read(|store| store.agent(&agent.id).cloned())
            .unwrap();
        assert_ne!(after.status, AgentStatus::Running);
        assert!(after.active_turn.is_none(), "{:?}", after.active_turn);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    async fn echo_agent() -> (Arc<Daemon>, String, std::path::PathBuf) {
        let daemon = Daemon::new(Config::ephemeral());
        daemon.agents.register(Arc::new(EchoProvider {
            chunk_delay: Duration::ZERO,
            slow_delay: Duration::ZERO,
        }));
        let cwd = std::env::temp_dir().join(format!("burrow-start-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let request = super::super::CreateAgent {
            provider: "echo".into(),
            cwd: cwd.to_string_lossy().into(),
            ..Default::default()
        };
        let agent = super::super::create(&daemon, request)
            .await
            .expect("create");
        (daemon, agent.id, cwd)
    }

    /// 0.2.2 review: a send refused (the queue was full) is not remembered,
    /// so sending it again once there is room queues it, not "duplicate".
    #[tokio::test]
    async fn a_refused_send_can_be_sent_again() {
        let (daemon, agent_id, cwd) = echo_agent().await;
        let live = ensure_live(&daemon, &agent_id).expect("live");
        live.begin_turn(); // a turn runs, so sends queue behind it
        let send = |id: String| {
            let daemon = daemon.clone();
            let agent_id = agent_id.clone();
            async move {
                deliver(
                    &daemon,
                    &agent_id,
                    "hello",
                    Some(id),
                    Vec::new(),
                    Busy::Queue,
                    From::User,
                )
                .await
                .map(|d| d.delivery)
            }
        };
        for n in 0..QUEUE_LIMIT {
            let queued = send(format!("m-{n}")).await;
            assert!(matches!(queued, Ok(Delivery::Queued { .. })), "{queued:?}");
        }
        assert!(send("again".into()).await.is_err());
        live.state.lock().unwrap().queued.pop_front();
        let retried = send("again".into()).await;
        assert!(
            matches!(retried, Ok(Delivery::Queued { .. })),
            "{retried:?}"
        );
        assert!(matches!(
            send("again".into()).await,
            Ok(Delivery::Duplicate)
        ));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// 0.2.2 review: a session's end waits for a send holding the gate, and
    /// it is let go of only once the agent is marked closed, so a session
    /// started after it is not the one marked. An interrupt waiting under
    /// the gate ends with the session, not after its grace.
    #[tokio::test]
    async fn a_session_ends_after_the_send_holding_its_gate() {
        let (daemon, agent_id, cwd) = echo_agent().await;
        let live = ensure_live(&daemon, &agent_id).expect("live");
        let gate = live.gate.lock().await;
        live.begin_turn();
        live.commands.send(Command::Shutdown).unwrap();
        let interrupted =
            tokio::time::timeout(Duration::from_secs(5), super::super::live::interrupt(&live))
                .await;
        assert_eq!(interrupted, Ok(Ok(())));
        assert!(*live.ended.borrow());
        tokio::time::sleep(Duration::from_millis(50)).await;
        let current = daemon.agents.live(&agent_id);
        assert!(
            current.is_some_and(|current| Arc::ptr_eq(&current, &live)),
            "let go of while a send held its gate"
        );
        drop(gate);
        let gone = tokio::time::timeout(Duration::from_secs(5), async {
            while daemon.agents.live(&agent_id).is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "the session was never let go of");
        let after = daemon
            .read(|store| store.agent(&agent_id).cloned())
            .unwrap();
        assert_eq!(after.status, AgentStatus::Closed);
        assert!(after.active_turn.is_none());
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
