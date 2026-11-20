//! The Claude Code provider: runs the user's own `claude` (found on PATH, as
//! Paseo does) in the Agent SDK's stream-json control mode.
//!
//! Set `BURROW_CLAUDE_TRACE=<dir>` to record every frame each session
//! exchanges with the CLI as JSONL (`{"dir":"in"|"out","frame":...}`), the
//! format the replay tests use.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod history;
pub mod probe;
pub mod process;
pub mod session;
pub mod tools;

use std::path::{Path, PathBuf};

use burrow_provider::{
    Capabilities, Provider, ProviderError, ProviderInfo, Session as ProviderSession, SessionSpec,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::session::{Launcher, Session, Settings, Transport};

/// Claude Code, found and version-checked.
#[derive(Debug, Clone)]
pub struct ClaudeProvider {
    binary: PathBuf,
    version: Option<Vec<u32>>,
    /// What the CLI itself said it offers; `None` when it did not (an old
    /// CLI, or the probe failed), and the fixed manifest stands in.
    models: Option<Vec<burrow_provider::Model>>,
    cli: burrow_provider::CliInfo,
    trace_dir: Option<PathBuf>,
    /// What the transcripts last said, for the session listings.
    described: std::sync::Arc<history::Described>,
    /// How a failed sign-in is explained, and the token file agents get.
    sign_in: session::SignIn,
}

impl ClaudeProvider {
    /// Finds `claude` (or `binary`), reads its version and asks it for its
    /// models. `None` when it cannot be run.
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
        let models = match probe::models(&binary) {
            Ok(models) => Some(models),
            Err(err) => {
                tracing::warn!(%err, "could not read Claude's models; offering the built-in list");
                None
            }
        };
        let cli = burrow_provider::CliInfo::of(&binary, version_text(&version));
        Some(Self {
            binary,
            version,
            cli,
            models,
            trace_dir: burrow_provider::env::var_os("BURROW_CLAUDE_TRACE").map(PathBuf::from),
            described: std::sync::Arc::default(),
            sign_in: session::SignIn::default(),
        })
    }

    /// The host's name, for saying where to sign in when a turn fails for
    /// want of a sign-in.
    pub fn on_host(mut self, host: Option<String>) -> Self {
        self.sign_in.host = host;
        self
    }

    /// A file holding a long-lived token (from `claude setup-token`) that
    /// each agent's CLI is given as `CLAUDE_CODE_OAUTH_TOKEN`, ahead of the
    /// host's own sign-in. It is read at every launch, so a new token needs
    /// no restart; with no file there, the CLI signs in as it would by
    /// itself. An agent's own `env` can still set the variable.
    pub fn with_token_file(mut self, path: PathBuf) -> Self {
        match read_token(&path) {
            Ok(Some(_)) => {
                tracing::info!(file = %path.display(), "Claude agents sign in with the token there");
            }
            Ok(None) => {}
            Err(err) => tracing::warn!(%err, "not giving Claude agents the token file"),
        }
        self.sign_in.token_file = Some(path);
        self
    }

    pub fn version(&self) -> Option<String> {
        version_text(&self.version)
    }
}

fn version_text(version: &Option<Vec<u32>>) -> Option<String> {
    (version.as_ref()).map(|v| v.iter().map(u32::to_string).collect::<Vec<_>>().join("."))
}

/// What the provider offers for a CLI of `version` (`None`: unknown). Also
/// what an agent imported without a CLI at hand is described with.
pub fn info(version: Option<&[u32]>) -> ProviderInfo {
    info_with(version, None)
}

/// [`info`] with the models the CLI reported, when it did.
pub fn info_with(
    version: Option<&[u32]>,
    models: Option<&[burrow_provider::Model]>,
) -> ProviderInfo {
    ProviderInfo {
        id: "claude".into(),
        label: "Claude".into(),
        description: Some("Anthropic's Claude Code".into()),
        modes: catalog::modes(),
        default_mode_id: Some(catalog::DEFAULT_MODE.into()),
        models: models.map_or_else(|| catalog::models(version), <[_]>::to_vec),
        capabilities: Capabilities {
            streaming: true,
            session_persistence: true,
            dynamic_modes: true,
            // A client's `mcpServers` are not passed to the CLI yet.
            mcp_servers: false,
            reasoning_stream: true,
            tool_invocations: true,
            steer: true,
        },
    }
}

impl Provider for ClaudeProvider {
    fn info(&self) -> ProviderInfo {
        info_with(self.version.as_deref(), self.models.as_deref())
    }

    fn cli(&self) -> Option<burrow_provider::CliInfo> {
        Some(self.cli.clone())
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
                let name = format!("claude-{}.jsonl", uuid::Uuid::new_v4().simple());
                dir.join(name)
            }),
            token_file: self.sign_in.token_file.clone(),
            used_token: false,
        };
        let settings = Settings {
            model: spec.model.clone(),
            mode: spec.mode_id.clone(),
            thinking: spec.thinking_option_id.clone(),
            append_system_prompt: spec.system_prompt.clone(),
            mcp_servers: spec.mcp_servers.clone(),
        };
        let session = Session::new(launcher, settings, spec.resume.clone(), events)
            .signing_in(self.sign_in.clone());
        tokio::spawn(session.run(command_rx));
        Ok(ProviderSession {
            commands,
            events: event_rx,
        })
    }

    fn history(&self, cwd: &str, session_id: &str) -> Vec<burrow_provider::HistoryItem> {
        history::read(&history::config_dir(), cwd, session_id)
    }

    fn recent_sessions(&self, limit: usize) -> Result<Vec<burrow_provider::RecentSession>, String> {
        history::recent(&history::config_dir(), None, limit, &self.described)
    }

    fn imports_sessions(&self) -> bool {
        true
    }

    fn importable_sessions(
        &self,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<burrow_provider::RecentSession>, String> {
        history::recent(&history::config_dir(), cwd, limit, &self.described)
    }

    fn session_title(&self, cwd: &str, session_id: &str) -> Option<String> {
        history::renamed(&history::config_dir(), cwd, session_id)
    }
}

struct CliLauncher {
    binary: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    unset: Vec<String>,
    trace: Option<PathBuf>,
    /// See [`ClaudeProvider::with_token_file`].
    token_file: Option<PathBuf>,
    /// Whether the last launch gave the CLI the token file's token.
    used_token: bool,
}

/// What the CLI is told to sign in with, ahead of the host's own sign-in.
pub const TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// What the CLI signs in with ahead of [`TOKEN_ENV`] (the install guide's
/// order).
const KEYS_AHEAD_OF_TOKEN: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

/// The token in `path`, trimmed. `None` when there is no file or it is
/// empty. Refused when others than its owner can read it, as ssh refuses a
/// private key: `chmod 600` it.
pub fn read_token(path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    let failed = |err: std::io::Error| format!("{}: {err}", path.display());
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(failed(err)),
    };
    // The file opened, not whatever is at the path by now.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata().map_err(failed)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{} can be read by others than its owner; run chmod 600 on it",
                path.display()
            ));
        }
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(failed)?;
    let token = text.trim();
    Ok((!token.is_empty()).then(|| token.to_string()))
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
        // Echoes each prompt as it enters a query, with `command_lifecycle`
        // frames (queued, started, completed) under its uuid: how a turn
        // tells its own result from that of a query run ahead of it.
        "--replay-user-messages",
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
    if !settings.mcp_servers.is_empty() {
        args.extend(["--mcp-config".into(), mcp_config(&settings.mcp_servers)]);
    }
    if let Some(resume) = resume {
        args.push(format!("--resume={resume}"));
    }
    args
}

/// `--mcp-config`'s inline JSON, as the Agent SDK passes it: the servers
/// join those of the user's own settings.
fn mcp_config(servers: &[burrow_provider::McpServer]) -> String {
    let servers: serde_json::Map<String, Value> = servers
        .iter()
        .map(|server| {
            let headers: serde_json::Map<String, Value> = (server.headers.iter())
                .map(|(name, value)| (name.clone(), value.clone().into()))
                .collect();
            let config = json!({"type": "http", "url": server.url, "headers": headers});
            (server.name.clone(), config)
        })
        .collect();
    json!({"mcpServers": servers}).to_string()
}

/// What a Claude Code session sets for its children. A Burrow started from
/// inside one would pass them on, and the CLI will not start inside another
/// session; Paseo clears the same ones.
const PARENT_SESSION_ENV: [&str; 4] = [
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_AGENT_SDK_VERSION",
];

/// Who launched the CLI. Paseo 0.14 names itself rather than say "sdk-ts",
/// whose sessions used subscription limits up faster (getpaseo/paseo#7976);
/// Burrow does the same, after the provider's own variables so none of
/// them can undo it.
pub const ENTRYPOINT: &str = "Burrow";

impl CliLauncher {
    /// The file's token, unless the agent's own `env` names one.
    fn token(&self) -> Option<String> {
        if self.env.iter().any(|(key, _)| key == TOKEN_ENV) {
            return None;
        }
        match read_token(self.token_file.as_deref()?) {
            Ok(token) => token,
            Err(err) => {
                tracing::warn!(%err, "not giving the Claude agent the token file");
                None
            }
        }
    }

    /// Whether the CLI gets an API key, which it signs in with ahead of
    /// [`TOKEN_ENV`]: from the agent's environment, else Burrow's own.
    fn key_ahead_of_token(&self) -> bool {
        KEYS_AHEAD_OF_TOKEN.iter().any(|key| {
            let value = match self.env.iter().rev().find(|(k, _)| k == key) {
                Some((_, value)) => Some(value.clone()),
                None if self.unset.iter().any(|u| u == key) => None,
                None => std::env::var(key).ok(),
            };
            value.is_some_and(|value| !value.trim().is_empty())
        })
    }

    fn command(
        &self,
        resume: Option<&str>,
        settings: &Settings,
        token: Option<&str>,
    ) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .args(arguments(resume, settings))
            .current_dir(&self.cwd)
            .env_remove("NODE_OPTIONS");
        for key in PARENT_SESSION_ENV
            .iter()
            .copied()
            .chain(self.unset.iter().map(String::as_str))
        {
            command.env_remove(key);
        }
        if let Some(token) = token {
            command.env(TOKEN_ENV, token);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
            .env("CLAUDE_CODE_ENTRYPOINT", ENTRYPOINT)
            .env("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1");
        command
    }
}

impl Launcher for CliLauncher {
    fn launch(&mut self, resume: Option<&str>, settings: &Settings) -> std::io::Result<Transport> {
        let token = self.token();
        // A key the CLI takes ahead of the token is what a refused sign-in
        // was refused with, not the token file.
        self.used_token = token.is_some() && !self.key_ahead_of_token();
        let command = self.command(resume, settings, token.as_deref());
        let process = process::spawn(command, self.trace.as_deref())?;
        Ok(Transport {
            input: process.input,
            output: process.output,
        })
    }

    fn used_token(&self) -> bool {
        self.used_token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_starts_as_burrows_own_session() {
        let launcher = CliLauncher {
            binary: PathBuf::from("claude"),
            cwd: PathBuf::from("."),
            env: vec![
                ("CLAUDE_CODE_ENTRYPOINT".into(), "sdk-ts".into()),
                ("ANTHROPIC_MODEL".into(), "opus".into()),
            ],
            unset: vec!["PASEO_HOME".into()],
            trace: None,
            token_file: None,
            used_token: false,
        };
        let command = launcher.command(None, &Settings::default(), None);
        let env = envs(&command);
        let get = |key: &str| env.get(key).cloned();
        assert_eq!(get("CLAUDE_CODE_ENTRYPOINT"), Some(Some(ENTRYPOINT.into())));
        assert_eq!(
            get("CLAUDECODE"),
            Some(None),
            "a parent session's marker is cleared"
        );
        assert_eq!(get("CLAUDE_CODE_SSE_PORT"), Some(None));
        assert_eq!(get("ANTHROPIC_MODEL"), Some(Some("opus".into())));
        assert_eq!(get("PASEO_HOME"), Some(None), "what the daemon unsets goes");
        assert_eq!(get(TOKEN_ENV), None, "no token file, no token");
    }

    fn envs(
        command: &tokio::process::Command,
    ) -> std::collections::HashMap<String, Option<String>> {
        (command.as_std().get_envs())
            .map(|(key, value)| {
                let value = value.map(|v| v.to_string_lossy().into_owned());
                (key.to_string_lossy().into_owned(), value)
            })
            .collect()
    }

    #[test]
    fn the_token_file_signs_the_cli_in_unless_the_agent_names_its_own() {
        let dir = std::env::temp_dir().join(format!("burrow-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("claude-token");
        let mut launcher = CliLauncher {
            binary: PathBuf::from("claude"),
            cwd: PathBuf::from("."),
            env: Vec::new(),
            unset: Vec::new(),
            trace: None,
            token_file: Some(file.clone()),
            used_token: false,
        };
        assert_eq!(launcher.token(), None, "no file yet");
        std::fs::write(&file, "  \n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(launcher.token(), None, "an empty file is no token");
        std::fs::write(&file, "a-test-token\n").unwrap();
        assert_eq!(launcher.token().as_deref(), Some("a-test-token"));
        let command = launcher.command(None, &Settings::default(), launcher.token().as_deref());
        assert_eq!(
            envs(&command).get(TOKEN_ENV).cloned(),
            Some(Some("a-test-token".into()))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(launcher.token(), None, "a token others can read is refused");
            assert!(read_token(&file).unwrap_err().contains("chmod 600"));
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        launcher.env = vec![(TOKEN_ENV.into(), "the-agents-own".into())];
        assert_eq!(launcher.token(), None, "the agent's env wins");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A `claude` that answers `--version` and, in print mode, an
    /// `initialize` request with `reply`; it records its arguments. Once its
    /// input closes it takes a second to exit, as a CLI saving a renewed
    /// sign-in would, and leaves `exited` beside itself when it does.
    #[cfg(unix)]
    fn fake_claude(name: &str, version: &str, reply: Option<&str>) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("burrow-fake-claude-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fixture = dir.join("reply.json");
        let answer = match reply {
            Some(reply) => {
                std::fs::write(&fixture, reply).unwrap();
                format!(
                    "read line\ncat '{}'\ncat > /dev/null\nsleep 1\ntouch '{}'\n",
                    fixture.display(),
                    dir.join("exited").display()
                )
            }
            None => "exit 1\n".to_string(),
        };
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{version} (Claude Code)'; exit 0; fi\n\
             echo \"$@\" > '{}'\n{answer}",
            dir.join("args").display()
        );
        let cli = dir.join("claude");
        write_script(&cli, &script);
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        cli
    }

    /// Writes `text` to `path` for running: written here under another name
    /// and copied into place by `cp`. A test thread forking while this
    /// process held `path` open for writing would pass that open file to its
    /// child, and running it then fails with "Text file busy"; `cp`'s own
    /// open file goes to no one.
    #[cfg(unix)]
    fn write_script(path: &std::path::Path, text: &str) {
        let source = path.with_extension("src");
        std::fs::write(&source, text).unwrap();
        let copied = std::process::Command::new("cp")
            .arg(&source)
            .arg(path)
            .status()
            .unwrap();
        assert!(copied.success(), "cp to {}", path.display());
    }

    #[cfg(unix)]
    #[test]
    fn detect_takes_the_models_the_cli_reports() {
        let reply = include_str!("../tests/fixtures/initialize.json");
        let cli = fake_claude("ok", "2.1.317", Some(reply));
        let claude = ClaudeProvider::detect(Some(&cli)).expect("runs");
        assert_eq!(claude.version().as_deref(), Some("2.1.317"));
        let info = Provider::info(&claude);
        assert!(info.models.iter().any(|m| m.id == "claude-fable-5-2"));
        let default = info.models.iter().find(|m| m.is_default).unwrap();
        assert_eq!(default.id, "claude-opus-5-5");
        // The probe sends no prompt and keeps no transcript.
        let args = std::fs::read_to_string(cli.with_file_name("args")).unwrap();
        assert!(args.contains("--no-session-persistence"), "{args}");
        assert!(args.contains("-p"), "{args}");
        assert!(args.contains(r#"{"disableAllHooks":true}"#), "{args}");
        // It is left to exit by itself, not killed once it has answered.
        let exited = cli.with_file_name("exited");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !exited.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(exited.exists(), "the CLI was killed before it could exit");
        std::fs::remove_dir_all(cli.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn detect_falls_back_to_the_manifest_when_the_cli_does_not_answer() {
        let cli = fake_claude("silent", "2.1.303", None);
        let claude = ClaudeProvider::detect(Some(&cli)).expect("runs");
        let info = Provider::info(&claude);
        assert_eq!(
            info.models,
            catalog::models(catalog::parse_version("2.1.303").as_deref())
        );
        // An old CLI that answers without a model list does the same.
        let old = fake_claude(
            "old",
            "2.1.303",
            Some(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"probe","response":{}}}
"#,
            ),
        );
        let claude = ClaudeProvider::detect(Some(&old)).expect("runs");
        assert_eq!(Provider::info(&claude).models, info.models);
        std::fs::remove_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(old.parent().unwrap()).unwrap();
    }

    #[test]
    fn mcp_servers_go_in_as_the_sdks_inline_config() {
        let settings = Settings {
            mcp_servers: vec![burrow_provider::McpServer {
                name: "paseo".into(),
                url: "http://127.0.0.1:41234/mcp".into(),
                headers: vec![("Authorization".into(), "Bearer t0ken".into())],
                stdio: None,
            }],
            ..Settings::default()
        };
        let args = arguments(None, &settings);
        let at = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("the flag");
        let config: Value = serde_json::from_str(&args[at + 1]).unwrap();
        assert_eq!(
            config,
            json!({"mcpServers": {"paseo": {"type": "http", "url": "http://127.0.0.1:41234/mcp",
                "headers": {"Authorization": "Bearer t0ken"}}}})
        );
        assert!(!arguments(None, &Settings::default()).contains(&"--mcp-config".to_string()));
    }

    #[test]
    fn arguments_follow_the_sdk() {
        let settings = Settings {
            model: Some("claude-haiku-4-5".into()),
            mode: Some("default".into()),
            thinking: Some("high".into()),
            append_system_prompt: None,
            mcp_servers: Vec::new(),
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
