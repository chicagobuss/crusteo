//! Agents that speak the Agent Client Protocol (ACP): JSON-RPC 2.0 over the
//! agent's stdio, driven the way Paseo's `ACPAgentClient` drives them.
//!
//! The protocol client is generic. An [`AgentConfig`] says which agent it
//! runs: the provider id clients see, the command, and the agent's quirks.
//! [`cursor`], [`opencode`] and [`gemini`] are the ones Burrow offers;
//! another ACP agent (Copilot, ...) is one more such function.
//!
//! Set `BURROW_ACP_TRACE=<dir>` to record every frame each session
//! exchanges with its agent as JSONL (`{"dir":"in"|"out","frame":...}`), the
//! format the replay tests use. A recording holds account details; scrub it
//! before using it as a fixture.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod probe;
pub mod process;
pub mod session;
pub mod terminal;
pub mod tools;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use burrow_provider::{
    Capabilities, Provider, ProviderError, ProviderInfo, Session as ProviderSession, SessionSpec,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub use crate::probe::{Catalog, Models};
use crate::process::Transport;
use crate::session::{Launcher, Session, Settings};

/// How long a catalog probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// One ACP agent, as Paseo configures it.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    /// The provider id clients see.
    pub id: String,
    pub label: String,
    pub description: String,
    /// The program and its arguments; the program is looked up on PATH.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The provider named inside permission requests (see
    /// [`Settings::permission_provider`]).
    pub permission_provider: String,
    /// Sent as `clientCapabilities._meta`.
    pub capability_meta: Option<Value>,
    pub models: Models,
}

/// Cursor's agent, `cursor-agent acp`, as Paseo's catalog adds it: provider
/// `cursor`, models from Cursor's own extension, and the parameterized model
/// picker turned on.
pub fn cursor(binary: Option<&Path>) -> AgentConfig {
    let program = binary.map_or_else(|| "cursor-agent".into(), |p| p.display().to_string());
    AgentConfig {
        id: "cursor".into(),
        label: "Cursor".into(),
        description: "Cursor's coding agent".into(),
        command: vec![program, "acp".into()],
        env: Vec::new(),
        permission_provider: "acp".into(),
        capability_meta: Some(json!({"parameterizedModelPicker": true})),
        models: Models::CursorExtension,
    }
}

/// OpenCode's agent over ACP, `opencode acp`, under Paseo's provider id
/// (Paseo drives it through `opencode serve` instead); models and modes
/// come from its sessions.
pub fn opencode(binary: Option<&Path>) -> AgentConfig {
    let program = binary.map_or_else(|| "opencode".into(), |p| p.display().to_string());
    AgentConfig {
        id: "opencode".into(),
        label: "OpenCode".into(),
        description: "The open source coding agent".into(),
        command: vec![program, "acp".into()],
        env: Vec::new(),
        permission_provider: "acp".into(),
        capability_meta: None,
        models: Models::Session,
    }
}

/// Google's Gemini CLI over ACP, `gemini --acp`. Paseo has no Gemini
/// provider of its own; there it is a custom ACP agent.
pub fn gemini(binary: Option<&Path>) -> AgentConfig {
    let program = binary.map_or_else(|| "gemini".into(), |p| p.display().to_string());
    AgentConfig {
        id: "gemini".into(),
        label: "Gemini".into(),
        description: "Google's Gemini CLI".into(),
        command: vec![program, "--acp".into()],
        env: Vec::new(),
        permission_provider: "acp".into(),
        capability_meta: None,
        models: Models::Session,
    }
}

/// What Paseo's ACP agents can do (`DEFAULT_ACP_CAPABILITIES`). Nothing
/// steers a running ACP turn.
pub const CAPABILITIES: Capabilities = Capabilities {
    streaming: true,
    session_persistence: true,
    dynamic_modes: true,
    // A client's `mcpServers` are not passed on; the agent gets Burrow's own.
    mcp_servers: false,
    reasoning_stream: true,
    tool_invocations: true,
    steer: false,
};

/// The providers-snapshot entry of an agent with what its probe found.
/// There is no default mode: the session's own current mode stands.
pub fn info(config: &AgentConfig, catalog: &Catalog) -> ProviderInfo {
    ProviderInfo {
        id: config.id.clone(),
        label: config.label.clone(),
        description: Some(config.description.clone()),
        modes: catalog.modes.clone(),
        default_mode_id: None,
        models: catalog.models.clone(),
        capabilities: CAPABILITIES,
    }
}

/// An ACP agent, found on PATH.
#[derive(Debug, Clone)]
pub struct AcpProvider {
    config: Arc<AgentConfig>,
    version: Option<String>,
    catalog: Arc<RwLock<Catalog>>,
    /// Set until the first probe has finished, well or not.
    probing: Arc<AtomicBool>,
    trace_dir: Option<PathBuf>,
}

impl AcpProvider {
    /// Runs `<program> --version`. `None` when the program cannot be run.
    pub fn detect(config: AgentConfig) -> Option<Self> {
        let program = config.command.first()?;
        let output = std::process::Command::new(program)
            .arg("--version")
            .output()
            .ok()?;
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(Self {
            config: Arc::new(config),
            version: Some(version).filter(|v| output.status.success() && !v.is_empty()),
            catalog: Arc::default(),
            probing: Arc::new(AtomicBool::new(true)),
            trace_dir: burrow_provider::env::var_os("BURROW_ACP_TRACE").map(PathBuf::from),
        })
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Asks the agent for its modes and models (one throwaway session in
    /// the home directory) and keeps them for [`Provider::info`].
    pub async fn refresh(&self) -> Result<(), String> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let cwd = home.display().to_string();
        let mut launcher = AgentLauncher {
            config: self.config.clone(),
            cwd: home,
            env: Vec::new(),
            unset: Vec::new(),
            trace: None,
        };
        let transport = launcher.launch().map_err(|err| err.to_string())?;
        let config = &self.config;
        let probe = probe::run(
            transport,
            &cwd,
            config.capability_meta.as_ref(),
            config.models,
        );
        let catalog = tokio::time::timeout(PROBE_TIMEOUT, probe)
            .await
            .map_err(|_| format!("probing {} timed out", config.id))
            .and_then(|probed| probed);
        self.probing.store(false, Ordering::Release);
        *self.catalog.write().unwrap_or_else(|p| p.into_inner()) = catalog?;
        Ok(())
    }

    /// [`refresh`](Self::refresh) on its own task, logging the outcome.
    pub fn refresh_in_background(&self) {
        let provider = self.clone();
        tokio::spawn(async move {
            match provider.refresh().await {
                Ok(()) => {
                    let catalog = provider.catalog.read().unwrap_or_else(|p| p.into_inner());
                    tracing::info!(
                        provider = provider.config.id,
                        models = catalog.models.len(),
                        modes = catalog.modes.len(),
                        "probed the ACP agent"
                    );
                }
                Err(error) => {
                    tracing::warn!(provider = provider.config.id, %error, "could not probe the ACP agent");
                }
            }
        });
    }
}

impl Provider for AcpProvider {
    fn info(&self) -> ProviderInfo {
        info(
            &self.config,
            &self.catalog.read().unwrap_or_else(|p| p.into_inner()),
        )
    }

    fn start(&self, spec: SessionSpec) -> Result<ProviderSession, ProviderError> {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let launcher = AgentLauncher {
            config: self.config.clone(),
            cwd: PathBuf::from(&spec.cwd),
            env: spec.env.clone(),
            unset: spec.unset.clone(),
            trace: self.trace_dir.as_ref().map(|dir| {
                let name = format!("{}-{}.jsonl", self.config.id, uuid::Uuid::new_v4().simple());
                dir.join(name)
            }),
        };
        let settings = Settings {
            provider: self.config.id.clone(),
            permission_provider: self.config.permission_provider.clone(),
            cwd: spec.cwd,
            model: spec.model,
            mode: spec.mode_id,
            thinking: spec.thinking_option_id,
            resume: spec.resume,
            capability_meta: self.config.capability_meta.clone(),
            env: spec.env,
            mcp_servers: spec.mcp_servers,
        };
        tokio::spawn(Session::new(launcher, settings, events).run(command_rx));
        Ok(ProviderSession {
            commands,
            events: event_rx,
        })
    }

    fn loading(&self) -> bool {
        self.probing.load(Ordering::Acquire)
    }

    /// Paseo's ACP turns are named by UUIDs.
    fn foreground_turn_id(&self, _number: u64) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

/// Starts the configured command in the session's directory.
struct AgentLauncher {
    config: Arc<AgentConfig>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    unset: Vec<String>,
    trace: Option<PathBuf>,
}

impl Launcher for AgentLauncher {
    fn launch(&mut self) -> std::io::Result<Transport> {
        let (program, args) = self
            .config
            .command
            .split_first()
            .ok_or_else(|| std::io::Error::other("no command configured"))?;
        let mut command = tokio::process::Command::new(program);
        command.args(args).current_dir(&self.cwd);
        for key in &self.unset {
            command.env_remove(key);
        }
        for (key, value) in self.config.env.iter().chain(&self.env) {
            command.env(key, value);
        }
        process::spawn(command, self.trace.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_is_configured_as_paseo_adds_it() {
        let config = cursor(None);
        assert_eq!(config.command, ["cursor-agent", "acp"]);
        let info = info(&config, &Catalog::default());
        assert_eq!(
            (info.id.as_str(), info.label.as_str()),
            ("cursor", "Cursor")
        );
        assert_eq!(info.description.as_deref(), Some("Cursor's coding agent"));
        assert_eq!(info.default_mode_id, None);
        assert!(!info.capabilities.steer && info.capabilities.session_persistence);
        let custom = cursor(Some(Path::new("/opt/cursor/bin/cursor-agent")));
        assert_eq!(custom.command[0], "/opt/cursor/bin/cursor-agent");
    }

    #[test]
    fn opencode_and_gemini_run_their_acp_modes_with_models_from_the_session() {
        let opencode = opencode(None);
        assert_eq!(opencode.command, ["opencode", "acp"]);
        assert_eq!(
            (opencode.id.as_str(), opencode.label.as_str()),
            ("opencode", "OpenCode")
        );
        let gemini = gemini(Some(Path::new("/usr/local/bin/gemini")));
        assert_eq!(gemini.command, ["/usr/local/bin/gemini", "--acp"]);
        assert_eq!(
            (gemini.id.as_str(), gemini.label.as_str()),
            ("gemini", "Gemini")
        );
        for config in [opencode, gemini] {
            assert_eq!(config.models, Models::Session);
            assert_eq!(config.capability_meta, None);
        }
    }
}
