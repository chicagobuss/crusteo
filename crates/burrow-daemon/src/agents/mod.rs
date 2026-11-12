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
//! - **Coalescing:** text and tool updates go through
//!   [`crate::coalesce::Coalescer`].
//! - **Attention** latches: `finished` when a turn ends (cancels included),
//!   `error` when it fails; only clearing, or archiving, resets it. A
//!   permission request is announced but does not set it.
//! - **Permissions** never time out. An interrupt or a failed turn denies
//!   what is left; archive drops them silently.
//! - **Lazy load.** Agents start their provider on first use after a restart.
//!
//! The work is split by seam: this module keeps the registry ([`Agents`]) and
//! the store helpers every part shares; `create` makes an agent,
//! `placement` decides where it lives, `live` runs one session, `messages`
//! delivers prompts, `permissions` answers and announces them, `requests`
//! serves the other agent requests, `providers` builds the providers
//! snapshot, `idle` lets CLI sessions go, and `import` makes an agent of a
//! session the provider's CLI started elsewhere.

mod create;
mod idle;
mod import;
mod live;
mod messages;
mod permissions;
pub(crate) mod placement;
mod providers;
mod requests;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use burrow_protocol::directory::{AgentCapabilities, AgentMode, AgentSnapshot, AgentStatus};
use burrow_provider::{Command, Provider, ProviderInfo};
use serde_json::Value;
use tokio::sync::watch;

use crate::clock;
use crate::daemon::Daemon;
use crate::store::{Store, StoreEvent};

pub use create::{CreateAgent, PARENT_AGENT_LABEL, create};
pub use idle::{close, release_idle, release_idle_loop};
pub use import::{ImportSession, import_session};
pub use messages::{
    Busy, Delivered, Delivery, QUEUE_LIMIT, Waited, send, wait_started, wait_started_within,
};
pub(crate) use messages::{send_notice, send_notice_once, send_system};
pub use permissions::{clear_attention, respond_permission};
pub(crate) use placement::{NewWorkspace, find_or_create_project, new_workspace, placement};
pub use providers::{announce_when_loaded, providers_snapshot, publish_providers};
pub use requests::{
    Setting, archive, cancel, configure, delete, detach, refresh, rename_and_label,
    wait_for_finish, wait_for_turn,
};

use live::Live;

/// Paseo's title limit.
const TITLE_LIMIT: usize = 60;
/// Paseo's notification preview limit.
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
    /// True while the providers are first looked for at startup
    /// ([`crate::discovery::start`]), which runs while Burrow already
    /// listens, until their models are in and pushed to subscribers. Each
    /// registration also wakes its receivers, so
    /// [`until_found`](Self::until_found) can wait for one provider.
    discovering: watch::Sender<bool>,
}

impl Agents {
    pub fn register(&self, provider: Arc<dyn Provider>) {
        let id = provider.info().id;
        self.providers
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, provider);
        self.discovering.send_modify(|_| {});
    }

    /// Whether the providers are still being looked for at startup.
    pub fn discovering(&self) -> bool {
        *self.discovering.borrow()
    }

    pub(crate) fn set_discovering(&self, discovering: bool) {
        self.discovering.send_replace(discovering);
    }

    /// Waits until the providers have been looked for at startup.
    pub async fn until_discovered(&self) {
        let mut discovering = self.discovering.subscribe();
        let _ = discovering.wait_for(|d| !*d).await;
    }

    /// Waits until the provider `id` is offered, or no longer can turn up
    /// at startup: what needs a provider waits for it then, rather than
    /// failing because Burrow answered before the provider was found.
    pub async fn until_found(&self, id: &str) {
        let mut discovering = self.discovering.subscribe();
        let _ = discovering
            .wait_for(|d| !*d || self.provider(id).is_some())
            .await;
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

    /// Each provider's CLI (version, install kind), in the order of [`infos`](Self::infos).
    pub fn clis(&self) -> Vec<Option<burrow_provider::CliInfo>> {
        self.all().iter().map(|p| p.cli()).collect()
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

    /// Every provider found, by id.
    pub fn by_id(&self) -> Vec<(String, Arc<dyn Provider>)> {
        (self
            .providers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter())
        .map(|(id, provider)| (id.clone(), provider.clone()))
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
/// The CLI the agent's process runs; current is what the host's provider is
/// now, when it differs from the one the agent started with.
fn live_cli(
    live: &Live,
    current: Option<&Arc<dyn Provider>>,
) -> Option<burrow_protocol::directory::AgentCli> {
    let version = live.provider.cli()?.version;
    let newer_installed = current
        .filter(|current| !Arc::ptr_eq(current, &live.provider))
        .and_then(|current| current.cli())
        .is_some_and(|now| now.version != version);
    Some(burrow_protocol::directory::AgentCli {
        version,
        newer_installed,
    })
}

/// Sets what an agent's `cli` says, telling clients only when it changed.
fn set_cli(daemon: &Daemon, agent_id: &str, cli: Option<burrow_protocol::directory::AgentCli>) {
    let same = daemon
        .read(|store| store.agents.get(agent_id).map(|a| a.cli == cli))
        .unwrap_or(true);
    if !same {
        update(daemon, agent_id, |agent| agent.cli = cli);
    }
}

/// After providers were found again: each running agent says which CLI its
/// process runs and whether the host has a newer one, which it switches to
/// at its next prompt.
pub fn refresh_cli_versions(daemon: &Arc<Daemon>) {
    let live: Vec<(String, Arc<Live>)> = (daemon.agents.live.lock())
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(|(id, live)| (id.clone(), live.clone()))
        .collect();
    for (agent_id, live) in live {
        let current = daemon.agents.provider(&live.info.id);
        set_cli(daemon, &agent_id, live_cli(&live, current.as_ref()));
    }
}

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
    store
        .timeline(agent_id)
        .map(|timeline| reply_text(timeline.rows()))
        .unwrap_or_default()
}

/// The text of the reply that ends `rows`: its last run of reply rows.
pub(crate) fn reply_text(rows: &[crate::timeline::Row]) -> String {
    let mut chunks: Vec<&str> = rows
        .iter()
        .rev()
        .take_while(|row| row.item["type"] == "assistant_message")
        .filter_map(|row| row.item["text"].as_str())
        .collect();
    chunks.reverse();
    chunks.concat()
}

/// [`last_assistant_text`] of one turn: its last run of reply rows.
fn turn_text(store: &Store, agent_id: &str, turn: &str) -> String {
    let Some(timeline) = store.timeline(agent_id) else {
        return String::new();
    };
    let mut chunks: Vec<&str> = (timeline.rows().iter().rev())
        .filter(|row| row.turn_id.as_deref() == Some(turn))
        .take_while(|row| row.item["type"] == "assistant_message")
        .filter_map(|row| row.item["text"].as_str())
        .collect();
    chunks.reverse();
    chunks.concat()
}

/// The error a turn failed with, from the row its failure left
/// (`<turn>:error`).
fn turn_error(store: &Store, agent_id: &str, turn: &str) -> Option<String> {
    let id = format!("{turn}:error");
    let timeline = store.timeline(agent_id)?;
    let row = (timeline.rows().iter().rev()).find(|row| row.item["messageId"] == id.as_str())?;
    let text = row.item["text"].as_str().unwrap_or_default();
    let text = text.strip_prefix("[System Error] ").unwrap_or(text);
    Some(if text.trim().is_empty() {
        "Agent failed".into()
    } else {
        text.to_string()
    })
}

/// While the providers are still being looked for at startup, waits for the
/// provider of the agent `key` names ([`Agents::until_found`]).
pub async fn until_provider_of(daemon: &Daemon, key: &str) {
    if !daemon.agents.discovering() {
        return;
    }
    let provider = daemon.read(|store| store.find_agent(key).map(|a| a.provider.clone()));
    if let Some(provider) = provider {
        daemon.agents.until_found(&provider).await;
    }
}

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
