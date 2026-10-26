//! The Codex provider: runs the user's own `codex` (found on PATH, as Paseo
//! does) as `codex app-server`, one process per session.
//!
//! The model catalog comes from app-server itself (`model/list`, with the
//! user's configured defaults), read once at startup and refreshed by every
//! session that starts.
//!
//! Set `BURROW_CODEX_TRACE=<dir>` to record every frame each session
//! exchanges with app-server as JSONL (`{"dir":"in"|"out","frame":...}`),
//! the format the replay tests use.

#![forbid(unsafe_code)]

pub mod blocking;
pub mod catalog;
pub mod history;
pub mod process;
pub mod session;
pub mod tools;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_provider::{
    Capabilities, HistoryItem, Provider, ProviderError, ProviderInfo, Session as ProviderSession,
    SessionSpec,
};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::catalog::Catalog;
use crate::process::{JsonProcess, Output};
use crate::session::{Launcher, Session, Settings, Transport};

/// How long the startup catalog probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Makes the transport of a session that does not run `codex` itself.
pub type Launch = Arc<dyn Fn() -> std::io::Result<Transport> + Send + Sync>;

/// Codex, found and version-checked.
#[derive(Clone)]
pub struct CodexProvider {
    binary: PathBuf,
    version: Option<Vec<u32>>,
    catalog: Arc<Mutex<Catalog>>,
    trace_dir: Option<PathBuf>,
    launch: Option<Launch>,
}

impl std::fmt::Debug for CodexProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexProvider")
            .field("binary", &self.binary)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl CodexProvider {
    /// Finds `codex` (or `binary`), reads its version and asks app-server
    /// for its models. `None` when it cannot be run.
    pub fn detect(binary: Option<&Path>) -> Option<Self> {
        let binary = binary.map_or_else(|| PathBuf::from("codex"), Path::to_path_buf);
        let output = std::process::Command::new(&binary)
            .arg("--version")
            .output()
            .ok()?;
        let version = catalog::parse_version(&String::from_utf8_lossy(&output.stdout));
        let catalog = probe(&binary).unwrap_or_else(|err| {
            tracing::warn!(%err, "could not read Codex's models; offering none until a session starts");
            Catalog::default()
        });
        Some(Self {
            binary,
            version,
            catalog: Arc::new(Mutex::new(catalog)),
            trace_dir: burrow_provider::env::var_os("BURROW_CODEX_TRACE").map(PathBuf::from),
            launch: None,
        })
    }

    /// A provider whose sessions talk to `launch`'s transports instead of a
    /// `codex` process: a scripted app-server in tests.
    pub fn with_launcher(version: Option<Vec<u32>>, catalog: Catalog, launch: Launch) -> Self {
        Self {
            binary: PathBuf::from("codex"),
            version,
            catalog: Arc::new(Mutex::new(catalog)),
            trace_dir: None,
            launch: Some(launch),
        }
    }

    pub fn version(&self) -> Option<String> {
        self.version
            .as_ref()
            .map(|v| v.iter().map(u32::to_string).collect::<Vec<_>>().join("."))
    }

    fn catalog(&self) -> std::sync::MutexGuard<'_, Catalog> {
        self.catalog.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// `model/list` and the user's config from a short-lived app-server.
fn probe(binary: &Path) -> Result<Catalog, String> {
    let mut client = blocking::Client::start(binary, None, PROBE_TIMEOUT)?;
    let models = client.call("model/list", json!({}), PROBE_TIMEOUT)?;
    let config = client
        .call("config/read", json!({}), PROBE_TIMEOUT)
        .unwrap_or(Value::Null);
    Ok(Catalog::from_responses(&models, &config))
}

/// What the provider offers for a Codex of `version` with `catalog`.
pub fn info(version: Option<&[u32]>, catalog: &Catalog) -> ProviderInfo {
    let auto_review = catalog::auto_review_available(version);
    ProviderInfo {
        id: "codex".into(),
        label: "Codex".into(),
        description: Some(
            "OpenAI's Codex workspace agent with sandbox controls and optional network access"
                .into(),
        ),
        modes: catalog::modes(auto_review),
        default_mode_id: Some(catalog::default_mode(auto_review).into()),
        models: catalog.models(),
        capabilities: Capabilities {
            streaming: true,
            session_persistence: true,
            dynamic_modes: false,
            mcp_servers: true,
            reasoning_stream: true,
            tool_invocations: true,
            steer: true,
        },
    }
}

impl Provider for CodexProvider {
    fn info(&self) -> ProviderInfo {
        info(self.version.as_deref(), &self.catalog())
    }

    fn start(&self, spec: SessionSpec) -> Result<ProviderSession, ProviderError> {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let settings = Settings {
            cwd: spec.cwd.clone(),
            model: spec.model.clone(),
            mode: spec.mode_id.clone(),
            thinking: spec.thinking_option_id.clone(),
            system_prompt: spec.system_prompt.clone(),
        };
        let catalog = self.catalog.clone();
        let resume = spec.resume.clone();
        match &self.launch {
            Some(launch) => {
                let launcher = FnLauncher(launch.clone());
                let session = Session::new(launcher, settings, resume, catalog, events);
                tokio::spawn(session.run(command_rx));
            }
            None => {
                let launcher = CliLauncher {
                    binary: self.binary.clone(),
                    cwd: PathBuf::from(&spec.cwd),
                    env: spec.env.clone(),
                    trace: self.trace_dir.as_ref().map(|dir| {
                        let name = format!("codex-{}.jsonl", uuid::Uuid::new_v4().simple());
                        dir.join(name)
                    }),
                };
                let session = Session::new(launcher, settings, resume, catalog, events);
                tokio::spawn(session.run(command_rx));
            }
        }
        Ok(ProviderSession {
            commands,
            events: event_rx,
        })
    }

    fn history(&self, cwd: &str, session_id: &str) -> Vec<HistoryItem> {
        let read = || -> Result<Value, String> {
            let mut client =
                blocking::Client::start(&self.binary, Some(Path::new(cwd)), PROBE_TIMEOUT)?;
            client.call(
                "thread/read",
                json!({"threadId": session_id, "includeTurns": true}),
                PROBE_TIMEOUT,
            )
        };
        match read() {
            Ok(thread) => history::from_thread(&thread, Some(cwd).filter(|c| !c.is_empty())),
            Err(err) => {
                tracing::warn!(%err, session_id, "could not read the Codex thread");
                Vec::new()
            }
        }
    }

    /// Paseo's Codex numbers its own turns.
    fn foreground_turn_id(&self, ordinal: u64) -> String {
        format!("codex-turn-{ordinal}")
    }

    fn model_fields(&self, model_id: &str) -> Map<String, Value> {
        self.catalog().fields(model_id)
    }
}

struct FnLauncher(Launch);

impl Launcher for FnLauncher {
    fn launch(&mut self) -> std::io::Result<Transport> {
        (self.0)()
    }
}

struct CliLauncher {
    binary: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    trace: Option<PathBuf>,
}

impl Launcher for CliLauncher {
    fn launch(&mut self) -> std::io::Result<Transport> {
        let mut command = tokio::process::Command::new(&self.binary);
        command.arg("app-server");
        if self.cwd.is_dir() {
            command.current_dir(&self.cwd);
        }
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
    let JsonProcess {
        input,
        mut output,
        child,
    } = process;
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
    let JsonProcess {
        input: process_input,
        mut output,
        child,
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
