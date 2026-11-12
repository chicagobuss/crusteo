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
pub mod process;
pub mod session_bus;

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

/// A session the provider's CLI keeps on this host, from its own records:
/// for Paseo's project suggestions, which group them by repository, and its
/// import sheet.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecentSession {
    /// The provider's own id for it.
    pub id: String,
    pub cwd: String,
    /// The name it has in the CLI (a rename, else a summary or the first
    /// prompt), when known.
    pub title: Option<String>,
    /// The first thing the user asked, when known.
    pub first_prompt: Option<String>,
    /// The latest thing the user asked, when known.
    pub last_prompt: Option<String>,
    /// Milliseconds since the epoch.
    pub last_activity_ms: i64,
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
    /// The CLI is run for that only when its file changed (see
    /// [`CachedVersion`]).
    pub fn cli(
        id: impl Into<String>,
        program: std::path::PathBuf,
        find: impl Fn() -> Option<std::sync::Arc<dyn Provider>> + Send + Sync + 'static,
    ) -> Self {
        let version = CachedVersion::default();
        Self {
            id: id.into(),
            program: Some(program.clone()),
            stamp: Box::new(move || version.of(&program)),
            find: Box::new(find),
        }
    }
}

/// How a provider's CLI was installed, as far as its path says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    /// The vendor's own installer, which updates the CLI itself (Claude
    /// Code's lives in a `claude/versions/` directory).
    Native,
    /// A package manager's (npm, Homebrew, apt, WinGet, Nix, ...).
    Package,
    Unknown,
}

impl InstallKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Package => "package",
            Self::Unknown => "unknown",
        }
    }

    /// Classifies the file a CLI resolves to (through its symlinks).
    pub fn of_path(real: &std::path::Path) -> Self {
        let path = real
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        let path = format!("{path}/");
        let has = |part: &str| path.contains(part);
        if has("/claude/versions/") {
            Self::Native
        } else if [
            "/node_modules/",
            "/cellar/",
            "/caskroom/",
            "/homebrew/",
            "/nix/store/",
        ]
        .iter()
        .any(|p| has(p))
            || has("/winget/")
            || has("/snap/")
            || ["/usr/bin/", "/usr/lib/", "/usr/local/lib/"]
                .iter()
                .any(|p| path.starts_with(p))
        {
            Self::Package
        } else {
            Self::Unknown
        }
    }
}

/// What a provider knows of the CLI it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliInfo {
    /// `2.1.317`, without the CLI's own wording around it.
    pub version: Option<String>,
    pub install_kind: InstallKind,
}

impl CliInfo {
    /// Of `binary` (a bare name is looked up on `PATH`), which reports
    /// `version`.
    pub fn of(binary: &std::path::Path, version: Option<String>) -> Self {
        let install_kind = find_program(binary)
            .and_then(|found| std::fs::canonicalize(found).ok())
            .map_or(InstallKind::Unknown, |real| InstallKind::of_path(&real));
        Self {
            version,
            install_kind,
        }
    }
}

/// A CLI's `--version`, read again only when the file the CLI resolves to
/// changed (its path, through symlinks, its size or its modified time).
/// The daemon looks every few minutes, and running a CLI is not free: one
/// built with Bun (OpenCode) unpacks a native library into the temporary
/// directory at every launch and never removes it.
#[derive(Default)]
pub struct CachedVersion {
    last: std::sync::Mutex<Option<(FileStamp, Option<String>)>>,
}

type FileStamp = (std::path::PathBuf, u64, Option<std::time::SystemTime>);

impl CachedVersion {
    /// What `program --version` prints, or `None` when it cannot be run.
    pub fn of(&self, program: &std::path::Path) -> Option<String> {
        // A CLI not found as a file (say, a Windows `.cmd` shim) is run
        // each time, as before.
        let Some(now) = file_stamp(program) else {
            return version_line(program);
        };
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((was, version)) = last.as_ref()
            && *was == now
        {
            return version.clone();
        }
        let version = version_line(program);
        // Only a CLI that ran is remembered: one that could not be run this
        // time is tried again at the next look, not taken as gone until its
        // file changes.
        *last = version.clone().map(|version| (now, Some(version)));
        version
    }
}

/// The file `program` runs, as a path through its symlinks, with its size
/// and modified time. A bare name is looked up on `PATH`.
fn file_stamp(program: &std::path::Path) -> Option<FileStamp> {
    let found = find_program(program)?;
    let real = std::fs::canonicalize(found).ok()?;
    let meta = std::fs::metadata(&real).ok()?;
    meta.is_file()
        .then(|| (real, meta.len(), meta.modified().ok()))
}

/// The file `program` names: itself when it has a directory, else the first
/// match on `PATH`.
pub fn find_program(program: &std::path::Path) -> Option<std::path::PathBuf> {
    if program.components().count() > 1 {
        Some(program.to_path_buf())
    } else {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    }
}

/// What `program --version` prints, or `None` when it cannot be run.
pub fn version_line(program: &std::path::Path) -> Option<String> {
    // A CLI being written as it is launched (an upgrade landing, or another
    // thread's child still holding the file open for a moment) fails with
    // ETXTBSY: try again shortly rather than report it missing.
    const BUSY: i32 = 26; // ETXTBSY on Linux and macOS
    let mut tries = 0;
    let output = loop {
        match std::process::Command::new(program)
            .arg("--version")
            .output()
        {
            Ok(output) => break output,
            Err(err) if cfg!(unix) && err.raw_os_error() == Some(BUSY) && tries < 5 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(20 * tries));
            }
            Err(_) => return None,
        }
    };
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
    /// Up to `limit` of the CLI's own sessions on this host, the most
    /// recently used first (Paseo's `listImportableSessions`), or why they
    /// could not be read. Blocking: call off the async runtime. None for a
    /// provider that keeps no sessions Burrow can read.
    fn recent_sessions(&self, _limit: usize) -> Result<Vec<RecentSession>, String> {
        Ok(Vec::new())
    }
    /// Whether the CLI's own sessions can be imported as agents (Paseo's
    /// import sheet): listed by [`importable_sessions`](Self::importable_sessions),
    /// read by [`history`](Self::history), and continued by a start that
    /// resumes them.
    fn imports_sessions(&self) -> bool {
        false
    }
    /// Up to `limit` of the CLI's own sessions that ran in `cwd` (anywhere
    /// when none), the most recently used first. A provider that cannot tell
    /// the directory of a session before reading it returns the sessions of
    /// other directories too, and the caller filters. Blocking: call off the
    /// async runtime.
    fn importable_sessions(
        &self,
        _cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RecentSession>, String> {
        self.recent_sessions(limit)
    }
    /// The name the user gave a session in the CLI itself (Claude's
    /// `/rename`), which an agent imported from it takes as its title.
    /// Blocking.
    fn session_title(&self, _cwd: &str, _session_id: &str) -> Option<String> {
        None
    }
    /// Whether [`info`](Self::info) is still being found out (an ACP agent
    /// probes for its models); the providers snapshot then says `loading`,
    /// and subscribers get a `providers_snapshot_update` once it is done.
    fn loading(&self) -> bool {
        false
    }
    /// The CLI this provider runs: its version and how it was installed.
    /// Clients show it, and tell when a CLI was upgraded under them.
    fn cli(&self) -> Option<CliInfo> {
        None
    }
    /// Why the provider cannot be used, when it found that out (an ACP
    /// agent whose probe has never succeeded, such as a CLI its vendor no
    /// longer serves). The providers snapshot then says `error` with this
    /// reason, clients offer it no more, and creating an agent on it is
    /// refused with it.
    fn unavailable(&self) -> Option<String> {
        None
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A CLI that counts its launches: the cached version runs it once, and
    /// again only after the file is replaced.
    #[test]
    fn a_cli_is_run_for_its_version_only_when_its_file_changes() {
        let dir = std::env::temp_dir().join(format!(
            "burrow-cached-version-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cli = dir.join("cli");
        let runs = dir.join("runs");
        let write = |version: &str| {
            let script = format!(
                "#!/bin/sh\necho run >> '{}'\necho {version}\n",
                runs.display()
            );
            std::fs::write(&cli, script).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let launches = || {
            std::fs::read_to_string(&runs)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };
        write("1.0.0");
        let cached = CachedVersion::default();
        assert_eq!(cached.of(&cli).as_deref(), Some("1.0.0"));
        assert_eq!(cached.of(&cli).as_deref(), Some("1.0.0"));
        assert_eq!(launches(), 1, "the second look does not run it");

        write("1.10.0");
        assert_eq!(cached.of(&cli).as_deref(), Some("1.10.0"));
        assert_eq!(launches(), 2, "an upgraded CLI is run again");

        let missing = dir.join("gone");
        assert_eq!(cached.of(&missing), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A CLI that could not be run is looked at again next time, though its
    /// file (path, size, modified time) did not change.
    #[test]
    fn a_cli_that_could_not_run_is_tried_again() {
        let dir = std::env::temp_dir().join(format!(
            "burrow-cached-failure-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cli = dir.join("cli");
        std::fs::write(&cli, "#!/bin/sh\necho 2.0.0\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o644)).unwrap();
        let cached = CachedVersion::default();
        assert_eq!(cached.of(&cli), None, "not executable yet");
        // chmod leaves the size and the modified time alone.
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(cached.of(&cli).as_deref(), Some("2.0.0"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod install_kind_tests {
    use super::InstallKind::{self, *};
    use std::path::Path;

    #[test]
    fn a_path_tells_how_the_cli_was_installed() {
        let kind = |p: &str| InstallKind::of_path(Path::new(p));
        assert_eq!(
            kind("/home/box/.local/share/claude/versions/2.1.317"),
            Native
        );
        assert_eq!(
            kind(r"C:\Users\box\.local\share\claude\versions\2.1.317"),
            Native
        );
        assert_eq!(
            kind("/usr/lib/node_modules/@anthropic-ai/claude-code/bin/claude"),
            Package
        );
        assert_eq!(
            kind("/opt/homebrew/Caskroom/claude-code/2.1.3/claude"),
            Package
        );
        assert_eq!(kind("/usr/bin/claude"), Package);
        assert_eq!(kind("/home/box/bin/claude"), Unknown);
        assert_eq!(Native.as_str(), "native");
    }
}
