//! The Claude Code provider: runs the user's own `claude` (found on PATH, as
//! Paseo does) in the Agent SDK's stream-json control mode.
//!
//! Set `BURROW_CLAUDE_TRACE=<dir>` to record every frame each session
//! exchanges with the CLI as JSONL (`{"dir":"in"|"out","frame":...}`), the
//! format the replay tests use.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod history;
pub mod process;
pub mod session;
pub mod tools;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use burrow_provider::{
    Capabilities, Provider, ProviderError, ProviderInfo, Session as ProviderSession, SessionSpec,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::process::{JsonProcess, Output};
use crate::session::{Launcher, Session, Settings, Transport};

/// Claude Code, found and version-checked.
#[derive(Debug, Clone)]
pub struct ClaudeProvider {
    binary: PathBuf,
    version: Option<Vec<u32>>,
    trace_dir: Option<PathBuf>,
}

impl ClaudeProvider {
    /// Finds `claude` (or `binary`) and reads its version. `None` when it
    /// cannot be run.
    pub fn detect(binary: Option<&Path>) -> Option<Self> {
        let binary = binary.map_or_else(|| PathBuf::from("claude"), Path::to_path_buf);
        let output = std::process::Command::new(&binary)
            .arg("--version")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        // Only trust a real Claude Code version line (Paseo 0.12 does the same).
        let version = text
            .contains("(Claude Code)")
            .then(|| catalog::parse_version(text.trim()))
            .flatten();
        Some(Self {
            binary,
            version,
            trace_dir: burrow_provider::env::var_os("BURROW_CLAUDE_TRACE").map(PathBuf::from),
        })
    }

    pub fn version(&self) -> Option<String> {
        self.version
            .as_ref()
            .map(|v| v.iter().map(u32::to_string).collect::<Vec<_>>().join("."))
    }
}

/// What the provider offers for a CLI of `version` (`None`: unknown). Also
/// what an agent imported without a CLI at hand is described with.
pub fn info(version: Option<&[u32]>) -> ProviderInfo {
    ProviderInfo {
        id: "claude".into(),
        label: "Claude".into(),
        description: Some("Anthropic's Claude Code".into()),
        modes: catalog::modes(),
        default_mode_id: Some(catalog::DEFAULT_MODE.into()),
        models: catalog::models(version),
        capabilities: Capabilities {
            streaming: true,
            session_persistence: true,
            dynamic_modes: true,
            mcp_servers: true,
            reasoning_stream: true,
            tool_invocations: true,
            steer: true,
        },
    }
}

impl Provider for ClaudeProvider {
    fn info(&self) -> ProviderInfo {
        info(self.version.as_deref())
    }

    fn start(&self, spec: SessionSpec) -> Result<ProviderSession, ProviderError> {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let launcher = CliLauncher {
            binary: self.binary.clone(),
            cwd: PathBuf::from(&spec.cwd),
            env: spec.env.clone(),
            trace: self.trace_dir.as_ref().map(|dir| {
                let name = format!("claude-{}.jsonl", uuid::Uuid::new_v4().simple());
                dir.join(name)
            }),
        };
        let settings = Settings {
            model: spec.model.clone(),
            mode: spec.mode_id.clone(),
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
        history::read(&history::config_dir(), cwd, session_id)
    }
}

struct CliLauncher {
    binary: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    trace: Option<PathBuf>,
}

/// The launch flags, as the Agent SDK builds them for Paseo's options.
pub fn arguments(resume: Option<&str>, settings: &Settings) -> Vec<String> {
    let mut args: Vec<String> = [
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
        "--setting-sources=user,project,local",
        // Lets the agent switch to bypass later, as Paseo allows.
        "--allow-dangerously-skip-permissions",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    match settings.thinking.as_deref() {
        Some(catalog::THINKING_OFF) => args.extend(["--thinking".into(), "disabled".into()]),
        Some(level) if level != "default" => {
            let effort = if level == catalog::ULTRACODE {
                "xhigh"
            } else {
                level
            };
            args.extend([
                "--thinking".into(),
                "adaptive".into(),
                "--thinking-display".into(),
                "summarized".into(),
                "--effort".into(),
                effort.into(),
            ]);
        }
        _ => {}
    }
    if let Some(model) = &settings.model {
        args.extend(["--model".into(), model.clone()]);
    }
    // "default" is the CLI's own default; newer CLIs no longer accept the name.
    if let Some(mode) = settings.mode.as_deref().filter(|m| *m != "default") {
        args.extend(["--permission-mode".into(), mode.into()]);
    }
    if let Some(resume) = resume {
        args.push(format!("--resume={resume}"));
    }
    args
}

impl Launcher for CliLauncher {
    fn launch(&mut self, resume: Option<&str>, settings: &Settings) -> std::io::Result<Transport> {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .args(arguments(resume, settings))
            .current_dir(&self.cwd)
            .env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts")
            .env("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1")
            .env_remove("NODE_OPTIONS");
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let process = JsonProcess::spawn(command)?;
        Ok(match &self.trace {
            Some(path) => tee(process, path),
            None => keep_alive(process),
        })
    }
}

/// Owns the process for as long as its channels are in use.
fn keep_alive(process: JsonProcess) -> Transport {
    let process::Parts {
        input,
        mut output,
        child,
    } = process.into_parts();
    let (output_tx, output_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _child = child; // killed on drop
        while let Some(message) = output.recv().await {
            let done = matches!(message, Output::Exited { .. });
            if output_tx.send(message).is_err() || done {
                break;
            }
        }
    });
    Transport {
        input,
        output: output_rx,
    }
}

/// Like [`keep_alive`], also writing every frame to `path`.
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
    let process::Parts {
        input: process_input,
        mut output,
        child,
    } = process.into_parts();
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
        let _child = child;
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
    fn arguments_follow_the_sdk() {
        let settings = Settings {
            model: Some("claude-haiku-4-5".into()),
            mode: Some("default".into()),
            thinking: Some("high".into()),
            append_system_prompt: None,
        };
        let args = arguments(Some("abc"), &settings);
        let joined = args.join(" ");
        assert!(
            joined.starts_with("--output-format stream-json --verbose --input-format stream-json")
        );
        assert!(joined.contains("--permission-prompt-tool stdio"));
        assert!(joined.contains("--model claude-haiku-4-5"));
        assert!(joined.contains("--effort high"));
        assert!(
            !joined.contains("--permission-mode"),
            "default is the CLI default"
        );
        assert!(joined.ends_with("--resume=abc"));
        let plan = Settings {
            mode: Some("plan".into()),
            ..Settings::default()
        };
        assert!(
            arguments(None, &plan)
                .join(" ")
                .contains("--permission-mode plan")
        );
    }
}
