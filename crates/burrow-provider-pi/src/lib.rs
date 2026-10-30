//! The pi provider: runs the user's own `pi` coding agent (found on PATH, as
//! Paseo does) in its JSONL RPC mode (`pi --mode rpc`).
//!
//! Paseo also loads an extension of its own into pi, for its system prompt,
//! MCP servers and rewind. Burrow needs none of it: the system prompt
//! goes in with `--append-system-prompt`, and it offers neither MCP servers
//! nor rewind. Dialogs of the user's own extensions become questions.
//!
//! Set `BURROW_PI_TRACE=<dir>` to record every frame each session
//! exchanges with pi as JSONL (`{"dir":"in"|"out","frame":...}`), the format
//! the replay tests use.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod history;
pub mod process;
pub mod session;
pub mod tools;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use burrow_provider::{
    Capabilities, Model, Provider, ProviderError, ProviderInfo, Session as ProviderSession,
    SessionSpec,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::process::{JsonProcess, Output};
use crate::session::{Launcher, Session, Settings, Transport};

/// How long reading pi's model catalog may take.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(60);

/// pi, found, with the models it offers once read.
#[derive(Debug, Clone)]
pub struct PiProvider {
    binary: PathBuf,
    version: Option<String>,
    trace_dir: Option<PathBuf>,
    models: Arc<RwLock<Vec<Model>>>,
}

impl PiProvider {
    /// Finds `pi` (or `binary`) and reads its version. `None` when it
    /// cannot be run.
    pub fn detect(binary: Option<&Path>) -> Option<Self> {
        let binary = binary.map_or_else(|| PathBuf::from("pi"), Path::to_path_buf);
        let output = std::process::Command::new(&binary)
            .arg("--version")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let version = text
            .split_whitespace()
            .find(|word| word.split('.').count() == 3 && word.split('.').all(is_number))
            .map(str::to_string);
        Some(Self {
            binary,
            version,
            trace_dir: burrow_provider::env::var_os("BURROW_PI_TRACE").map(PathBuf::from),
            models: Arc::default(),
        })
    }

    pub fn version(&self) -> Option<String> {
        self.version.clone()
    }

    /// Reads the models pi is configured with from a throwaway pi process
    /// (`--no-session`, in the home folder, as Paseo's catalog does).
    /// Returns how many there are.
    pub async fn refresh_catalog(&self) -> Result<usize, String> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .args(["--mode", "rpc", "--no-session"])
            .current_dir(home);
        let mut process = JsonProcess::spawn(command).map_err(|e| e.to_string())?;
        for (id, kind) in [("models", "get_available_models"), ("state", "get_state")] {
            let _ = process.input.send(json!({"id": id, "type": kind}));
        }
        let read = async {
            let (mut available, mut current) = (None, None::<Option<String>>);
            while available.is_none() || current.is_none() {
                match process.output.recv().await {
                    Some(Output::Message(frame)) if frame["type"] == "response" => {
                        if frame["success"] != true {
                            return Err(format!(
                                "pi refused {}: {}",
                                frame["command"], frame["error"]
                            ));
                        }
                        match frame["id"].as_str() {
                            Some("models") => {
                                available = frame["data"]["models"].as_array().cloned();
                            }
                            Some("state") => {
                                current = Some(catalog::model_id(&frame["data"]["model"]));
                            }
                            _ => {}
                        }
                    }
                    Some(Output::Exited { stderr, .. }) => {
                        return Err(format!("pi exited: {}", stderr.trim()));
                    }
                    None => return Err("pi exited".into()),
                    _ => {}
                }
            }
            Ok((available.unwrap_or_default(), current.flatten()))
        };
        let (available, current) = tokio::time::timeout(CATALOG_TIMEOUT, read)
            .await
            .map_err(|_| "pi did not list its models in time".to_string())??;
        let models = catalog::models(&available, current.as_deref());
        let count = models.len();
        *self.models.write().unwrap_or_else(|p| p.into_inner()) = models;
        Ok(count)
    }
}

fn is_number(part: &str) -> bool {
    !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())
}

/// What the provider offers, with the models read from pi.
pub fn info(models: Vec<Model>) -> ProviderInfo {
    ProviderInfo {
        id: "pi".into(),
        label: catalog::LABEL.into(),
        description: Some(catalog::DESCRIPTION.into()),
        modes: Vec::new(),
        default_mode_id: None,
        models,
        capabilities: Capabilities {
            streaming: true,
            session_persistence: true,
            dynamic_modes: true,
            mcp_servers: false,
            reasoning_stream: true,
            tool_invocations: true,
            steer: true,
        },
    }
}

impl Provider for PiProvider {
    fn info(&self) -> ProviderInfo {
        info(
            self.models
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
        )
    }

    fn start(&self, spec: SessionSpec) -> Result<ProviderSession, ProviderError> {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let launcher = CliLauncher {
            binary: self.binary.clone(),
            cwd: PathBuf::from(&spec.cwd),
            env: spec.env.clone(),
            unset: spec.unset.clone(),
            trace: self.trace_dir.as_ref().map(|dir| {
                let name = format!("pi-{}.jsonl", uuid::Uuid::new_v4().simple());
                dir.join(name)
            }),
        };
        let settings = Settings {
            model: spec.model.clone(),
            thinking: spec.thinking_option_id.clone(),
            append_system_prompt: spec.system_prompt.clone(),
        };
        let session = Session::new(launcher, settings, spec.resume.clone(), events);
        tokio::spawn(session.run(command_rx));
        Ok(ProviderSession {
            commands,
            events: event_rx,
        })
    }

    fn history(&self, cwd: &str, session_id: &str) -> Vec<burrow_provider::HistoryItem> {
        history::read(&history::sessions_dir(&[]), cwd, session_id)
    }
}

struct CliLauncher {
    binary: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    unset: Vec<String>,
    trace: Option<PathBuf>,
}

/// The launch flags, as Paseo's `buildPiLaunch` makes them. A resumed
/// session starts on its own model; the session switches it afterwards.
pub fn arguments(resume: Option<&str>, settings: &Settings) -> Vec<String> {
    let mut args = vec!["--mode".to_string(), "rpc".to_string()];
    if resume.is_none()
        && let Some(model) = &settings.model
    {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(level) = catalog::thinking_level(settings.thinking.as_deref()) {
        args.extend(["--thinking".into(), level.into()]);
    }
    if let Some(resume) = resume {
        args.extend(["--session".into(), resume.into()]);
    }
    if let Some(prompt) = settings
        .append_system_prompt
        .as_ref()
        .filter(|p| !p.trim().is_empty())
    {
        args.extend(["--append-system-prompt".into(), prompt.clone()]);
    }
    args
}

impl Launcher for CliLauncher {
    fn launch(&mut self, resume: Option<&str>, settings: &Settings) -> std::io::Result<Transport> {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .args(arguments(resume, settings))
            .current_dir(&self.cwd)
            .env_remove("NODE_OPTIONS");
        for key in &self.unset {
            command.env_remove(key);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let process = JsonProcess::spawn(command)?;
        Ok(match &self.trace {
            Some(path) => tee(process, path),
            None => Transport {
                input: process.input,
                output: process.output,
            },
        })
    }
}

/// The process's channels, also writing every frame to `path`.
fn tee(process: JsonProcess, path: &Path) -> Transport {
    let file = std::fs::File::create(path)
        .ok()
        .map(|f| Arc::new(Mutex::new(f)));
    let record = move |file: &Option<Arc<Mutex<std::fs::File>>>, dir: &str, frame: &Value| {
        if let Some(file) = file
            && let Ok(mut file) = file.lock()
        {
            let _ = writeln!(file, "{}", json!({"dir": dir, "frame": frame}));
        }
    };
    let JsonProcess {
        input: process_input,
        mut output,
    } = process;
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Value>();
    let (output_tx, output_rx) = mpsc::unbounded_channel();
    let in_file = file.clone();
    tokio::spawn(async move {
        while let Some(frame) = input_rx.recv().await {
            record(&in_file, "in", &frame);
            if process_input.send(frame).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(message) = output.recv().await {
            if let Output::Message(frame) = &message {
                record(&file, "out", frame);
            }
            let done = matches!(message, Output::Exited { .. });
            if output_tx.send(message).is_err() || done {
                break;
            }
        }
    });
    Transport {
        input: input_tx,
        output: output_rx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_follow_paseo() {
        let settings = Settings {
            model: Some("google/gemini-2.5-flash-lite".into()),
            thinking: Some("low".into()),
            append_system_prompt: Some("Be brief.".into()),
        };
        assert_eq!(
            arguments(None, &settings).join(" "),
            "--mode rpc --model google/gemini-2.5-flash-lite --thinking low \
             --append-system-prompt Be brief."
        );
        let resumed = arguments(Some("abc"), &settings).join(" ");
        assert!(resumed.contains("--session abc"));
        assert!(
            !resumed.contains("--model"),
            "a resumed session starts on its own model"
        );
        let unknown = Settings {
            thinking: Some("ultracode".into()),
            ..Settings::default()
        };
        assert_eq!(arguments(None, &unknown), ["--mode", "rpc"]);
    }

    #[test]
    fn the_provider_entry_matches_paseo() {
        let info = info(Vec::new());
        assert_eq!(info.id, "pi");
        assert_eq!(info.label, "Pi");
        assert!(info.modes.is_empty() && info.default_mode_id.is_none());
        assert!(!info.capabilities.mcp_servers && info.capabilities.steer);
    }
}
