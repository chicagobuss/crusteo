//! The Antigravity provider: runs the user's own `agy` (found on PATH, as
//! Paseo does) in its stream-json driver mode, the way Paseo 0.12's bundled
//! `antigravity-provider` plugin drives it. Agents show as `antigravity`,
//! with the models `agy models` lists and one mode, `full-access`.
//!
//! Set `BURROW_AGY_TRACE=<dir>` to record every frame each process
//! exchanges with the CLI as JSONL (`{"dir":"in"|"out","frame":...}`).

#![forbid(unsafe_code)]

pub mod catalog;
pub mod process;
pub mod prompt;
pub mod session;
pub mod timeline;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use burrow_provider::{
    Capabilities, Model, Provider, ProviderError, ProviderInfo, Session as ProviderSession,
    SessionSpec,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::process::Transport;
use crate::session::{Launcher, Session, Settings};

/// How long `agy --version` and `agy models` may take (Paseo's limit).
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// `agy`, found and version-checked, with the models it offers.
#[derive(Debug, Clone)]
pub struct AntigravityProvider {
    binary: PathBuf,
    version: [u32; 3],
    models: Arc<RwLock<Vec<Model>>>,
    refreshing: Arc<AtomicBool>,
    trace_dir: Option<PathBuf>,
}

impl AntigravityProvider {
    /// Finds `agy` (or `binary`), checks its version and reads its models.
    /// The error says why it cannot be offered. Models that cannot be read
    /// (not signed in) are read again when a session starts.
    pub fn detect(binary: Option<&Path>) -> Result<Self, String> {
        let binary = binary.map_or_else(|| PathBuf::from("agy"), Path::to_path_buf);
        let text = probe(&binary, &["--version"]).map_err(|err| {
            format!(
                "agy not found on PATH ({err}). Install Antigravity, then run `agy` and sign in."
            )
        })?;
        let version = catalog::parse_version(&text)
            .ok_or("Could not read the Antigravity version. Update `agy` to 1.1.15 or later.")?;
        if !catalog::supported(version) {
            return Err(format!(
                "Antigravity {} is unsupported. Update `agy` to 1.1.15 or later.",
                dotted(version)
            ));
        }
        let provider = Self {
            binary,
            version,
            models: Arc::default(),
            refreshing: Arc::default(),
            trace_dir: burrow_provider::env::var_os("BURROW_AGY_TRACE").map(PathBuf::from),
        };
        if let Err(err) = provider.refresh_models() {
            warn!(%err, "could not read Antigravity's models");
        }
        Ok(provider)
    }

    pub fn version(&self) -> String {
        dotted(self.version)
    }

    fn refresh_models(&self) -> Result<(), String> {
        let output = probe(&self.binary, &["models"])?;
        let models = catalog::parse_models(&output)?;
        *self.models.write().unwrap_or_else(|p| p.into_inner()) = models;
        Ok(())
    }

    /// Reads the models again in the background while there are none.
    fn refresh_models_if_missing(&self) {
        let missing = self
            .models
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty();
        if !missing || self.refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let provider = self.clone();
        std::thread::spawn(move || {
            match provider.refresh_models() {
                Ok(()) => info!("read Antigravity's models"),
                Err(err) => warn!(%err, "could not read Antigravity's models"),
            }
            provider.refreshing.store(false, Ordering::SeqCst);
        });
    }
}

fn dotted(version: [u32; 3]) -> String {
    version.map(|n| n.to_string()).join(".")
}

/// What the provider offers with `models`, as Paseo's plugin describes it.
pub fn info(models: Vec<Model>) -> ProviderInfo {
    ProviderInfo {
        id: catalog::PROVIDER_ID.into(),
        label: "Antigravity".into(),
        description: Some("Antigravity CLI on your host".into()),
        modes: catalog::modes(),
        default_mode_id: Some(catalog::MODE.into()),
        models,
        capabilities: Capabilities {
            streaming: true,
            session_persistence: true,
            dynamic_modes: true,
            // A client's `mcpServers` are not passed to agy yet.
            mcp_servers: false,
            reasoning_stream: true,
            tool_invocations: true,
            steer: false,
        },
    }
}

impl Provider for AntigravityProvider {
    fn info(&self) -> ProviderInfo {
        info(
            self.models
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
        )
    }

    fn start(&self, spec: SessionSpec) -> Result<ProviderSession, ProviderError> {
        self.refresh_models_if_missing();
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let launcher = CliLauncher {
            binary: self.binary.clone(),
            cwd: spec.cwd.clone(),
            env: spec.env.clone(),
            unset: spec.unset.clone(),
            trace_dir: self.trace_dir.clone(),
        };
        let settings = Settings {
            model: spec.model.clone(),
        };
        let resume = spec.resume.as_deref().and_then(session::conversation_id);
        let session = Session::new(
            launcher,
            settings,
            spec.system_prompt.clone(),
            resume,
            events,
        );
        tokio::spawn(session.run(command_rx));
        Ok(ProviderSession {
            commands,
            events: event_rx,
        })
    }

    fn opening_items(&self, _spec: &SessionSpec) -> Vec<Value> {
        vec![catalog::full_access_notice()]
    }
}

struct CliLauncher {
    binary: PathBuf,
    cwd: String,
    env: Vec<(String, String)>,
    unset: Vec<String>,
    trace_dir: Option<PathBuf>,
}

/// The launch flags, as Paseo's plugin builds them.
pub fn arguments(cwd: &str, conversation: Option<&str>, settings: &Settings) -> Vec<String> {
    let mut args: Vec<String> = [
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--add-dir",
        cwd,
        "--disable-slash-commands",
        "--dangerously-skip-permissions",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(model) = &settings.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(conversation) = conversation {
        args.extend(["--conversation".into(), conversation.into()]);
    }
    args
}

impl Launcher for CliLauncher {
    fn launch(
        &mut self,
        conversation: Option<&str>,
        settings: &Settings,
    ) -> std::io::Result<Transport> {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .args(arguments(&self.cwd, conversation, settings))
            .current_dir(&self.cwd);
        for key in &self.unset {
            command.env_remove(key);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let trace = self.trace_dir.as_ref().map(|dir| {
            let name = format!("agy-{}.jsonl", uuid::Uuid::new_v4().simple());
            dir.join(name)
        });
        process::spawn(command, trace.as_deref())
    }
}

/// Runs `binary args` to the end, within [`PROBE_TIMEOUT`]. Returns stdout,
/// or why it failed.
fn probe(binary: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = std::process::Command::new(binary)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = read(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = read(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Antigravity discovery timed out".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if status.success() {
        return Ok(stdout);
    }
    let code = status
        .code()
        .map_or_else(|| "a signal".to_string(), |c| c.to_string());
    Err(catalog::diagnostic(&match stderr.trim() {
        "" => format!("agy exited with code {code}"),
        text => text.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_follow_paseo() {
        let fresh = arguments("/home/user/repo", None, &Settings::default());
        assert_eq!(
            fresh.join(" "),
            "--input-format stream-json --output-format stream-json --add-dir /home/user/repo \
             --disable-slash-commands --dangerously-skip-permissions"
        );
        let resumed = arguments(
            "/home/user/repo",
            Some("conv-1"),
            &Settings {
                model: Some("gemini-3.8-flash-low".into()),
            },
        );
        assert!(
            resumed
                .join(" ")
                .ends_with("--model gemini-3.8-flash-low --conversation conv-1")
        );
        assert!(!resumed.iter().any(|a| a == "--mode" || a == "--effort"));
    }

    #[test]
    fn info_matches_the_plugin() {
        let info = info(Vec::new());
        assert_eq!(info.id, "antigravity");
        assert_eq!(info.label, "Antigravity");
        assert_eq!(info.default_mode_id.as_deref(), Some("full-access"));
        assert_eq!(info.modes.len(), 1);
        assert_eq!(info.modes[0].icon.as_deref(), Some("ShieldOff"));
        assert!(!info.capabilities.steer && info.capabilities.session_persistence);
    }
}
