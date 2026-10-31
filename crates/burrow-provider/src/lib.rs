//! How Burrow drives a coding agent.
//!
//! A provider starts sessions. A session is an actor: the daemon sends it
//! [`Command`]s and reads [`ProviderEvent`]s, and the adapter task owns
//! whatever process or connection backs it (`claude` with stream-json on
//! stdio, `codex app-server` JSON-RPC, an ACP agent). The daemon's agent
//! manager turns events into timeline rows, turn liveness, permission
//! requests and attention; adapters never see the wire protocol.
//!
//! Turn ids are opaque strings. A prompt carries the id the daemon chose for
//! that turn ([`Provider::foreground_turn_id`]: `foreground-turn-N` unless the
//! provider says otherwise), and the adapter reports it back on the
//! turn's events. A turn the agent starts by itself carries the adapter's own
//! id (Claude: `autonomous-turn-N`), or `None`, and the daemon then names it
//! `autonomous-<uuid>`, as Paseo's agent manager does.

#![forbid(unsafe_code)]

pub mod echo;
pub mod env;
pub mod group;

use serde_json::Value;
use tokio::sync::mpsc;

/// What a provider offers before any session exists (the providers snapshot).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderInfo {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub modes: Vec<Mode>,
    pub default_mode_id: Option<String>,
    pub models: Vec<Model>,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mode {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    /// A Lucide icon name, as Paseo's apps expect (`ShieldCheck`, ...).
    pub icon: Option<String>,
    /// `safe`, `moderate`, `dangerous`, `planning`.
    pub color_tier: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub is_default: bool,
    pub context_window_max_tokens: Option<u64>,
    pub thinking: Vec<ThinkingOption>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThinkingOption {
    pub id: String,
    pub label: String,
    pub is_default: bool,
}

/// `AgentCapabilityFlags` for agents of this provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub streaming: bool,
    pub session_persistence: bool,
    pub dynamic_modes: bool,
    pub mcp_servers: bool,
    pub reasoning_stream: bool,
    pub tool_invocations: bool,
    /// A message sent during a turn can join it (`activeTurnBehavior: "steer"`).
    pub steer: bool,
}

/// How to start a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSpec {
    pub cwd: String,
    pub model: Option<String>,
    pub mode_id: Option<String>,
    pub thinking_option_id: Option<String>,
    /// The provider's own session id, to resume instead of starting fresh.
    pub resume: Option<String>,
    pub system_prompt: Option<String>,
    /// Set in the CLI's environment, after `unset` and before the
    /// provider's own variables.
    pub env: Vec<(String, String)>,
    /// Taken out of the environment the CLI would inherit.
    pub unset: Vec<String>,
    /// MCP servers the CLI connects to, beside its own.
    pub mcp_servers: Vec<McpServer>,
}

/// An MCP server an agent's CLI reaches over HTTP (Streamable HTTP), such
/// as Burrow's own, which gives agents Paseo's tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// What the agent knows it by; its tools are `mcp__<name>__<tool>`.
    pub name: String,
    pub url: String,
    /// Sent with every request.
    pub headers: Vec<(String, String)>,
    /// The same server as a command speaking MCP on stdio, for agents
    /// that take no HTTP servers (an ACP agent need only support stdio).
    pub stdio: Option<StdioCommand>,
}

/// A command an agent starts to reach an MCP server over its stdio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioCommand {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// An image attached to a prompt: base64 data and its MIME type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Starts a turn.
    Prompt {
        turn_id: String,
        message_id: String,
        text: String,
        images: Vec<Image>,
    },
    /// Adds a message to the running turn. Only sent when
    /// [`Capabilities::steer`] is set and a turn is running.
    Steer {
        message_id: String,
        text: String,
        images: Vec<Image>,
    },
    /// Cancels the running turn, if any.
    Interrupt,
    SetMode(String),
    SetModel(Option<String>),
    SetThinking(Option<String>),
    /// Answers a [`ProviderEvent::PermissionRequested`] with the client's
    /// `AgentPermissionResponse` (`{behavior: "allow"|"deny", ...}`).
    RespondPermission {
        id: String,
        response: Value,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    /// The provider bound its own session id (resumable later).
    /// The model it reports running, if it says.
    SessionStarted {
        session_id: String,
        model: Option<String>,
    },
    /// A turn began. `turn` is the prompt's turn id, an adapter-made
    /// autonomous id, or `None`.
    TurnStarted { turn: Option<String> },
    /// One canonical timeline row: an `AgentTimelineItem` as JSON
    /// (`assistant_message` delta, `reasoning`, `tool_call` update, `todo`, ...).
    Item { turn: Option<String>, item: Value },
    /// A tool, plan, mode or question prompt. `request` is an
    /// `AgentPermissionRequest`, usually without `id` (the daemon makes one;
    /// an `id` the adapter sets is kept); `id` is the adapter's own key for
    /// the answer. Requests never time out.
    PermissionRequested { id: String, request: Value },
    /// The provider withdrew a request it had made.
    PermissionCanceled { id: String },
    /// Work the session runs outside a turn (an ACP agent's terminals) has
    /// started, or all of it has stopped. While it runs, the daemon keeps
    /// the session's CLI even when the agent is idle.
    BackgroundWork { running: bool },
    /// The provider changed mode itself (for example after a plan is approved).
    ModeChanged { mode_id: String },
    /// The session listed its own modes, which replace the provider's
    /// (ACP agents report modes per session).
    ModesAvailable { modes: Vec<Mode> },
    TurnCompleted {
        turn: Option<String>,
        usage: Option<Value>,
    },
    TurnFailed {
        turn: Option<String>,
        error: String,
        code: Option<String>,
        diagnostic: Option<String>,
    },
    TurnCanceled {
        turn: Option<String>,
        reason: String,
    },
    /// The session ended; no more events follow.
    Exited { error: Option<String> },
    /// Usage reported during a turn (`AgentUsage` as JSON); it replaces the
    /// agent's `lastUsage`, which drives the context meter (Paseo's
    /// `usage_updated`). Codex sends it, and ACP agents for `usage_update`;
    /// other providers need not.
    UsageUpdated { usage: Value },
    /// The provider's own `AgentRuntimeInfo` (`provider`, `sessionId`,
    /// `model`, `thinkingOptionId`, `modeId`, `extra`), replacing what the
    /// daemon derived. A `thinkingOptionId` in it becomes the agent's
    /// `effectiveThinkingOptionId`. Codex sends it, as it resolves its default
    /// model and reasoning effort itself, and so do ACP agents, whose
    /// sessions report their model, mode and title.
    RuntimeInfo { info: Value },
}

/// The two ends of a running session.
#[derive(Debug)]
pub struct Session {
    pub commands: mpsc::UnboundedSender<Command>,
    pub events: mpsc::UnboundedReceiver<ProviderEvent>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{0} is not installed or not on PATH")]
    Unavailable(String),
    #[error("failed to start {provider}: {message}")]
    Start { provider: String, message: String },
}

/// One item of a conversation read back from the provider's own records.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryItem {
    pub timestamp: Option<String>,
    pub item: Value,
}

/// How to find one provider's CLI, at startup and again while the daemon
/// runs: CLIs are upgraded, installed and removed under a running daemon.
pub struct Discovery {
    /// The provider's id, as its [`ProviderInfo`] has it.
    pub id: String,
    /// The CLI it runs, as given (a bare name is looked up on `PATH`), for
    /// `provider_diagnostic_request`.
    pub program: Option<std::path::PathBuf>,
    /// Tells one install from another cheaply, `None` when the CLI cannot
    /// be run. Blocking.
    pub stamp: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// Finds the provider, models included; `None` when it isn't there.
    /// Blocking; run on a thread where `tokio::spawn` works.
    #[allow(clippy::type_complexity)]
    pub find: Box<dyn Fn() -> Option<std::sync::Arc<dyn Provider>> + Send + Sync>,
}

impl Discovery {
    /// A provider whose CLI is `program`, told apart by its `--version`.
    pub fn cli(
        id: impl Into<String>,
        program: std::path::PathBuf,
        find: impl Fn() -> Option<std::sync::Arc<dyn Provider>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            program: Some(program.clone()),
            stamp: Box::new(move || version_line(&program)),
            find: Box::new(find),
        }
    }
}

/// What `program --version` prints, or `None` when it cannot be run.
pub fn version_line(program: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new(program)
        .arg("--version")
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub trait Provider: Send + Sync {
    fn info(&self) -> ProviderInfo;
    /// Starts a session. Must be called inside a tokio runtime.
    fn start(&self, spec: SessionSpec) -> Result<Session, ProviderError>;
    /// The conversation of a provider session, from the provider's own
    /// records (Claude's transcript files). Used when Burrow has no
    /// timeline of its own for a session it resumes.
    fn history(&self, _cwd: &str, _session_id: &str) -> Vec<HistoryItem> {
        Vec::new()
    }
    /// Whether [`info`](Self::info) is still being found out (an ACP agent
    /// probes for its models); the providers snapshot then says `loading`,
    /// and subscribers get a `providers_snapshot_update` once it is done.
    fn loading(&self) -> bool {
        false
    }
    /// The id of the `ordinal`th foreground turn. Paseo's providers name
    /// their own turns: Claude's are `foreground-turn-N`, Codex's
    /// `codex-turn-N`, ACP agents' UUIDs.
    fn foreground_turn_id(&self, ordinal: u64) -> String {
        format!("foreground-turn-{ordinal}")
    }
    /// Fields merged over a model's providers-snapshot entry, for what
    /// [`Model`] does not carry (Codex's `metadata`, and `thinkingOptions`
    /// with descriptions). A `null` removes the field. Applied before the
    /// snapshot is compacted.
    fn model_fields(&self, _model_id: &str) -> serde_json::Map<String, Value> {
        serde_json::Map::new()
    }
    /// Timeline items to record each time a session for `spec` opens,
    /// before any turn (Paseo's opening notices, such as Antigravity's
    /// full-access warning). The daemon records them as it starts the
    /// session, so they come before the first prompt.
    fn opening_items(&self, _spec: &SessionSpec) -> Vec<Value> {
        Vec::new()
    }
}
