//! One ACP session: an agent process spoken to in JSON-RPC 2.0 over stdio,
//! translated to provider events the way Paseo's `ACPAgentSession` does.
//!
//! The lifecycle:
//! 1. `initialize`, then `session/new`, or, to resume, `session/load` (the
//!    agent replays the conversation as `session/update`s, which are not
//!    shown again: Burrow has its own timeline) or `session/resume`;
//! 2. the configured mode, model and thinking option are applied with
//!    `session/set_mode`, `session/set_model` and `session/set_config_option`
//!    where they differ from the session's; prompts wait for this;
//! 3. each prompt is a `session/prompt` request whose answer ends the turn
//!    (`stopReason: "cancelled"` after a `session/cancel`).
//!
//! In between, the agent streams `session/update` notifications (message
//! and thought chunks, tool calls, plans, mode and config changes, usage)
//! and asks the client to `session/request_permission`, read and write
//! files, and run terminals.

use std::collections::{HashMap, VecDeque};

use burrow_provider::{Command, Image, Mode, ProviderEvent};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::catalog;
use crate::process::{Output, Transport};
use crate::terminal::{self, Terminals};
use crate::tools::{self, Snapshot};

/// Starts the agent process (or a test double).
pub trait Launcher: Send + 'static {
    fn launch(&mut self) -> std::io::Result<Transport>;
}

/// What a session is started with.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// The provider id clients see (`cursor`).
    pub provider: String,
    /// The provider named inside permission requests. Paseo runs every
    /// configured ACP agent through its generic `acp` client and renames only
    /// the outer events, so its requests say `acp`.
    pub permission_provider: String,
    pub cwd: String,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub thinking: Option<String>,
    /// The agent's session id, to resume instead of starting fresh.
    pub resume: Option<String>,
    /// Vendor flags sent as `clientCapabilities._meta`.
    pub capability_meta: Option<Value>,
    /// Extra environment for terminals the agent runs.
    pub env: Vec<(String, String)>,
    /// MCP servers for the session (`mcpServers`), Burrow's own among them.
    pub mcp_servers: Vec<burrow_provider::McpServer>,
}

/// `mcpServers` for `session/new`, `session/load` and `session/resume`: each
/// server over HTTP when the agent says it takes HTTP servers
/// (`mcpCapabilities.http`), else as its stdio command, which every ACP
/// agent must support; one with neither is left out.
pub fn mcp_servers(servers: &[burrow_provider::McpServer], capabilities: &Value) -> Value {
    let pairs = |pairs: &[(String, String)]| -> Vec<Value> {
        (pairs.iter())
            .map(|(name, value)| json!({"name": name, "value": value}))
            .collect()
    };
    let http = capabilities["mcpCapabilities"]["http"] == true;
    let listed: Vec<Value> = (servers.iter())
        .filter_map(|server| match (&server.stdio, http) {
            (_, true) => Some(
                json!({"type": "http", "name": server.name, "url": server.url,
                                     "headers": pairs(&server.headers)}),
            ),
            (Some(stdio), false) => Some(json!({"name": server.name, "command": stdio.command,
                                                "args": stdio.args, "env": pairs(&stdio.env)})),
            (None, false) => None,
        })
        .collect();
    Value::Array(listed)
}

/// The `initialize` request Paseo sends.
pub fn initialize_params(meta: Option<&Value>) -> Value {
    let mut capabilities = json!({
        "fs": {"readTextFile": false, "writeTextFile": false},
        "terminal": true,
    });
    if let Some(meta) = meta.filter(|m| m.as_object().is_some_and(|o| !o.is_empty())) {
        capabilities["_meta"] = meta.clone();
    }
    // Agents see the client Paseo is, so they behave as they do under it.
    json!({
        "protocolVersion": 1,
        "clientCapabilities": capabilities,
        "clientInfo": {"name": "Paseo", "version": "dev"},
    })
}

/// `summarizeACPRequestError`: the message, the code, and a diagnostic.
pub fn summarize_error(error: &Value) -> (String, Option<String>, Option<String>) {
    let Some(message) = error["message"].as_str() else {
        return (error.to_string(), None, None);
    };
    let Some(code) = error["code"].as_i64() else {
        return (message.to_string(), None, None);
    };
    let detail = data_message(&error["data"]);
    let message = match detail {
        Some(detail) if detail != message => format!("{message}: {detail}"),
        _ => message.to_string(),
    };
    let data = match error.get("data") {
        Some(data) => format!(" | data={data}"),
        None => String::new(),
    };
    let diagnostic = format!("{message} | code={code}{data}");
    (message, Some(code.to_string()), Some(diagnostic))
}

fn data_message(data: &Value) -> Option<String> {
    let record = data.as_object()?;
    for key in ["details", "errorMessage", "message", "detail", "title"] {
        if let Some(text) = record.get(key).and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            return Some(text.trim().to_string());
        }
    }
    data_message(&data["error"])
}

/// `mapACPUsage`.
fn usage_of(usage: &Value) -> Option<Value> {
    let usage = usage.as_object()?;
    let mut out = serde_json::Map::new();
    for (from, to) in [
        ("inputTokens", "inputTokens"),
        ("outputTokens", "outputTokens"),
        ("cachedReadTokens", "cachedInputTokens"),
    ] {
        if let Some(n) = usage.get(from).filter(|v| v.is_number()) {
            out.insert(to.into(), n.clone());
        }
    }
    Some(Value::Object(out))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// `initialize` and opening the session.
    Opening,
    /// Applying the configured mode, model and thinking option.
    Configuring,
    Ready,
}

/// What an answer from the agent is for.
#[derive(Debug)]
enum Pending {
    Initialize,
    NewSession,
    LoadSession,
    ResumeSession,
    Prompt {
        turn: String,
    },
    SetMode {
        mode: String,
        setup: bool,
    },
    SetModeOption {
        mode: String,
        config_id: String,
        setup: bool,
    },
    SetModel {
        model: String,
        setup: bool,
    },
    SetModelOption {
        model: String,
        config_id: String,
        setup: bool,
    },
    SetThinking {
        value: String,
        config_id: String,
        setup: bool,
    },
    Close,
}

#[derive(Debug)]
enum Setup {
    Mode(String),
    Model(String),
    Thinking(String),
}

struct Permission {
    /// The agent's JSON-RPC request id, to answer.
    rpc: Value,
    options: Vec<Value>,
}

pub struct Session<L: Launcher> {
    launcher: L,
    settings: Settings,
    transport: Option<Transport>,
    events: mpsc::UnboundedSender<ProviderEvent>,
    terminal_tx: mpsc::UnboundedSender<terminal::Event>,
    /// Taken by `run`.
    terminal_rx: Option<mpsc::UnboundedReceiver<terminal::Event>>,
    stage: Stage,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    /// Commands that arrived before the session was ready.
    held: VecDeque<Command>,
    setup: VecDeque<Setup>,
    agent_capabilities: Value,
    session_id: Option<String>,
    /// Updates that arrived before `session/new` answered.
    early: Vec<Value>,
    /// Set while `session/load` replays the conversation.
    replaying: bool,
    turn: Option<String>,
    /// The running turn's prompt had text, so the agent's echo of it is
    /// not shown again.
    submitted_text: bool,
    turn_usage: Option<Value>,
    fallback_message_id: Option<String>,
    /// A user message the agent is streaming (replays, or turns it began).
    pending_user: Option<(String, Option<String>)>,
    tools: HashMap<String, Snapshot>,
    tool_order: Vec<String>,
    permissions: HashMap<String, Permission>,
    terminals: Terminals,
    /// What [`Self::report_terminals`] last said.
    terminals_running: bool,
    modes: Vec<Mode>,
    current_mode: Option<String>,
    available_models: Option<Vec<String>>,
    current_model: Option<String>,
    thinking: Option<String>,
    config_options: Vec<Value>,
    title: Option<String>,
    updated_at: Option<String>,
}

impl<L: Launcher> Session<L> {
    pub fn new(
        launcher: L,
        settings: Settings,
        events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Self {
        let (terminal_tx, terminal_rx) = mpsc::unbounded_channel();
        Self {
            launcher,
            current_mode: settings.mode.clone(),
            current_model: settings.model.clone(),
            thinking: settings.thinking.clone(),
            settings,
            transport: None,
            events,
            terminal_tx,
            terminal_rx: Some(terminal_rx),
            stage: Stage::Opening,
            next_id: 0,
            pending: HashMap::new(),
            held: VecDeque::new(),
            setup: VecDeque::new(),
            agent_capabilities: Value::Null,
            session_id: None,
            early: Vec::new(),
            replaying: false,
            turn: None,
            submitted_text: false,
            turn_usage: None,
            fallback_message_id: None,
            pending_user: None,
            tools: HashMap::new(),
            tool_order: Vec::new(),
            permissions: HashMap::new(),
            terminals: Terminals::default(),
            terminals_running: false,
            modes: Vec::new(),
            available_models: None,
            config_options: Vec::new(),
            title: None,
            updated_at: None,
        }
    }

    // --- output -------------------------------------------------------------

    fn emit(&self, event: ProviderEvent) {
        let _ = self.events.send(event);
    }

    /// Says when the agent's terminals start or all stop running: Paseo's
    /// ACP agent will not let its runtime go while one runs, nor does Burrow.
    fn report_terminals(&mut self) {
        let running = self.terminals.running();
        if running != self.terminals_running {
            self.terminals_running = running;
            self.emit(ProviderEvent::BackgroundWork { running });
        }
    }

    /// Emits an event of the conversation, unless it is being replayed.
    fn emit_live(&self, event: ProviderEvent) {
        if !self.replaying {
            self.emit(event);
        }
    }

    fn item(&self, item: Value) {
        self.emit_live(ProviderEvent::Item {
            turn: self.turn.clone(),
            item,
        });
    }

    fn runtime_info(&self) -> Value {
        json!({
            "provider": self.settings.provider,
            "sessionId": self.session_id,
            "model": self.current_model,
            "thinkingOptionId": self.thinking,
            "modeId": self.current_mode,
            "extra": {"title": self.title, "updatedAt": self.updated_at},
        })
    }

    fn emit_runtime(&self) {
        self.emit_live(ProviderEvent::RuntimeInfo {
            info: self.runtime_info(),
        });
    }

    fn emit_mode(&self) {
        if !self.modes.is_empty() {
            self.emit_live(ProviderEvent::ModesAvailable {
                modes: self.modes.clone(),
            });
        }
        if let Some(mode) = &self.current_mode {
            self.emit_live(ProviderEvent::ModeChanged {
                mode_id: mode.clone(),
            });
        }
        self.emit_runtime();
    }

    fn write(&self, message: Value) {
        if let Some(transport) = &self.transport {
            let _ = transport.input.send(message);
        }
    }

    fn request(&mut self, method: &str, params: Value, pending: Pending) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, pending);
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    fn notify(&self, method: &str, params: Value) {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn reply(&self, id: &Value, result: Value) {
        self.write(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn reply_error(&self, id: &Value, code: i64, message: &str, data: Value) {
        self.write(json!({"jsonrpc": "2.0", "id": id,
                          "error": {"code": code, "message": message, "data": data}}));
    }

    fn session_params(&self) -> Value {
        json!({"sessionId": self.session_id})
    }

    // --- the loop -----------------------------------------------------------

    /// Runs until shut down or the agent process ends.
    pub async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        match self.launcher.launch() {
            Ok(transport) => self.transport = Some(transport),
            Err(err) => {
                let error = format!("could not start {}: {err}", self.settings.provider);
                self.emit(ProviderEvent::Exited { error: Some(error) });
                return;
            }
        }
        let params = initialize_params(self.settings.capability_meta.as_ref());
        self.request("initialize", params, Pending::Initialize);
        let mut terminal_rx = self.terminal_rx.take().expect("run once");
        let mut error = None;
        loop {
            let output = async {
                match self.transport.as_mut() {
                    Some(transport) => transport.output.recv().await,
                    None => std::future::pending().await,
                }
            };
            // Biased toward the agent: what it already said (a permission
            // request, a turn's end) is handled before a command answering it.
            tokio::select! {
                biased;
                output = output => match output {
                    Some(Output::Message(message)) => {
                        if let Err(fatal) = self.on_message(message) {
                            error = Some(fatal);
                            break;
                        }
                    }
                    Some(Output::Garbage(_)) => {}
                    Some(Output::Exited { code, signal, stderr }) => {
                        error = Some(self.on_exit(code, signal, &stderr));
                        break;
                    }
                    None => {
                        error = Some(self.on_exit(None, None, ""));
                        break;
                    }
                },
                Some(event) = terminal_rx.recv() => {
                    for (request, answer) in self.terminals.on_event(event) {
                        self.reply(&request, answer);
                    }
                    self.report_terminals();
                }
                command = commands.recv() => match command {
                    None | Some(Command::Shutdown) => {
                        self.shutdown();
                        break;
                    }
                    Some(command) => self.on_command(command),
                },
            }
        }
        self.terminals.kill_all();
        self.transport = None;
        self.emit(ProviderEvent::Exited { error });
    }

    /// The process ended. Returns the error the session ends with.
    fn on_exit(&mut self, code: Option<i32>, signal: Option<String>, stderr: &str) -> String {
        let status = match (&code, &signal) {
            (_, Some(signal)) => format!(
                "{}, {signal}",
                code.map_or("null".into(), |c| c.to_string())
            ),
            (Some(code), None) => code.to_string(),
            (None, None) => "null".into(),
        };
        let error = format!("ACP agent exited unexpectedly ({status})");
        let diagnostic = Some(stderr.trim().to_string()).filter(|s| !s.is_empty());
        if let Some(turn) = self.turn.clone() {
            self.synthesize_canceled_tools();
            self.finish_turn(ProviderEvent::TurnFailed {
                turn: Some(turn),
                error: error.clone(),
                code: None,
                diagnostic: diagnostic.clone(),
            });
        }
        self.fail_held(&error, diagnostic);
        warn!(%error, "the ACP agent exited");
        error
    }

    /// The session cannot open: prompts that waited for it fail.
    fn fail_held(&mut self, error: &str, diagnostic: Option<String>) {
        for command in std::mem::take(&mut self.held) {
            if let Command::Prompt { turn_id, .. } = command {
                self.emit(ProviderEvent::TurnFailed {
                    turn: Some(turn_id),
                    error: error.to_string(),
                    code: None,
                    diagnostic: diagnostic.clone(),
                });
            }
        }
    }

    fn shutdown(&mut self) {
        self.cancel_permissions();
        if self.session_id.is_some() {
            if self.turn.is_some() {
                self.notify("session/cancel", self.session_params());
            }
            if !self.agent_capabilities["sessionCapabilities"]["close"].is_null() {
                self.request("session/close", self.session_params(), Pending::Close);
            }
        }
    }

    // --- commands -----------------------------------------------------------

    fn on_command(&mut self, command: Command) {
        if self.stage != Stage::Ready {
            if !matches!(command, Command::Interrupt) {
                self.held.push_back(command);
                return;
            }
            // A prompt that was not sent yet is simply dropped.
            let mut canceled = Vec::new();
            self.held.retain(|held| match held {
                Command::Prompt { turn_id, .. } => {
                    canceled.push(turn_id.clone());
                    false
                }
                _ => true,
            });
            for turn in canceled {
                self.emit(ProviderEvent::TurnCanceled {
                    turn: Some(turn),
                    reason: "Interrupted".into(),
                });
            }
            return;
        }
        match command {
            Command::Prompt {
                turn_id,
                message_id,
                text,
                images,
            } => self.prompt(turn_id, message_id, &text, &images),
            // ACP has no way to add to a running turn; the daemon never
            // sends this to a provider without `steer`.
            Command::Steer { .. } => warn!("ACP agents cannot steer a running turn"),
            Command::Interrupt => {
                self.cancel_permissions();
                if self.turn.is_some() {
                    self.notify("session/cancel", self.session_params());
                }
            }
            Command::SetMode(mode) => self.set_mode(mode, false),
            Command::SetModel(Some(model)) => self.set_model(model, false),
            Command::SetModel(None) => self.current_model = None,
            Command::SetThinking(Some(value)) => self.set_thinking(value, false),
            Command::SetThinking(None) => self.thinking = None,
            Command::RespondPermission { id, response } => self.answer(&id, &response),
            Command::Shutdown => {}
        }
    }

    fn prompt(&mut self, turn: String, message_id: String, text: &str, images: &[Image]) {
        self.flush_user_message();
        self.turn = Some(turn.clone());
        self.fallback_message_id = None;
        self.submitted_text = !text.trim().is_empty();
        self.emit(ProviderEvent::TurnStarted {
            turn: Some(turn.clone()),
        });
        let mut prompt = Vec::new();
        if !text.is_empty() {
            prompt.push(json!({"type": "text", "text": text}));
        }
        for image in images {
            prompt.push(json!({"type": "image", "data": image.data, "mimeType": image.mime_type}));
        }
        let params =
            json!({"sessionId": self.session_id, "messageId": message_id, "prompt": prompt});
        self.request("session/prompt", params, Pending::Prompt { turn });
    }

    fn set_mode(&mut self, mode: String, setup: bool) {
        if !self.modes.is_empty() {
            if !self.modes.iter().any(|m| m.id == mode) {
                let ids: Vec<&str> = self.modes.iter().map(|m| m.id.as_str()).collect();
                warn!(
                    mode,
                    "is not valid {} mode. Available options: {}",
                    self.settings.provider,
                    ids.join(", ")
                );
                return self.next_setup(setup);
            }
            let mut params = self.session_params();
            params["modeId"] = mode.clone().into();
            self.request("session/set_mode", params, Pending::SetMode { mode, setup });
            return;
        }
        let Some(option) = catalog::select_option(&self.config_options, "mode", None) else {
            warn!(
                "{} does not expose ACP mode switching",
                self.settings.provider
            );
            return self.next_setup(setup);
        };
        if !catalog::has_choice(option, &mode) {
            warn!(
                mode,
                "is not valid {} mode config option", self.settings.provider
            );
            return self.next_setup(setup);
        }
        let config_id = option["id"].as_str().unwrap_or_default().to_string();
        let mut params = self.session_params();
        params["configId"] = config_id.clone().into();
        params["value"] = mode.clone().into();
        self.request(
            "session/set_config_option",
            params,
            Pending::SetModeOption {
                mode,
                config_id,
                setup,
            },
        );
    }

    fn set_model(&mut self, model: String, setup: bool) {
        match &self.available_models {
            Some(models) if !models.contains(&model) => {
                warn!(
                    model,
                    "is not a valid {} model. Available options: {}",
                    self.settings.provider,
                    models.join(", ")
                );
                self.next_setup(setup)
            }
            Some(_) => {
                let mut params = self.session_params();
                params["modelId"] = model.clone().into();
                self.request(
                    "session/set_model",
                    params,
                    Pending::SetModel { model, setup },
                );
            }
            None => self.set_model_option(model, setup),
        }
    }

    /// Selects a model through the `model` config option.
    fn set_model_option(&mut self, model: String, setup: bool) {
        let Some(option) = catalog::select_option(&self.config_options, "model", None) else {
            warn!(
                model,
                "{} does not expose ACP model selection; using provider default model",
                self.settings.provider
            );
            return self.next_setup(setup);
        };
        if !catalog::has_choice(option, &model) {
            warn!(
                model,
                "is not a valid {} model config option", self.settings.provider
            );
            return self.next_setup(setup);
        }
        let config_id = option["id"].as_str().unwrap_or_default().to_string();
        let mut params = self.session_params();
        params["configId"] = config_id.clone().into();
        params["value"] = model.clone().into();
        self.request(
            "session/set_config_option",
            params,
            Pending::SetModelOption {
                model,
                config_id,
                setup,
            },
        );
    }

    fn set_thinking(&mut self, value: String, setup: bool) {
        let Some(option) = catalog::select_option(&self.config_options, "thought_level", None)
        else {
            warn!(
                "{} does not expose ACP thought-level selection",
                self.settings.provider
            );
            return self.next_setup(setup);
        };
        let config_id = option["id"].as_str().unwrap_or_default().to_string();
        let mut params = self.session_params();
        params["configId"] = config_id.clone().into();
        params["value"] = value.clone().into();
        self.request(
            "session/set_config_option",
            params,
            Pending::SetThinking {
                value,
                config_id,
                setup,
            },
        );
    }

    /// Answers the agent's open permission requests with `cancelled`.
    fn cancel_permissions(&mut self) {
        for (_, permission) in self.permissions.drain().collect::<Vec<_>>() {
            self.reply(
                &permission.rpc,
                json!({"outcome": {"outcome": "cancelled"}}),
            );
        }
    }

    fn answer(&mut self, id: &str, response: &Value) {
        let Some(permission) = self.permissions.remove(id) else {
            warn!(id, "an answer to a permission request that is not pending");
            return;
        };
        let selected = tools::select_option(&permission.options, response);
        if selected.is_none() && response["selectedActionId"].is_string() {
            warn!(id, action = %response["selectedActionId"],
                  "the chosen action does not exist or does not match the answer");
        }
        let outcome = match selected {
            Some(option) => json!({"outcome": "selected", "optionId": option["optionId"]}),
            None => json!({"outcome": "cancelled"}),
        };
        self.reply(&permission.rpc, json!({"outcome": outcome}));
        if response["behavior"] == "deny" && response["interrupt"] == true && self.turn.is_some() {
            self.notify("session/cancel", self.session_params());
        }
    }

    // --- setup --------------------------------------------------------------

    fn begin_setup(&mut self) {
        self.stage = Stage::Configuring;
        if let Some(mode) = self.settings.mode.clone()
            && Some(&mode) != self.current_mode.as_ref()
        {
            self.setup.push_back(Setup::Mode(mode));
        }
        if let Some(model) = self.settings.model.clone()
            && Some(&model) != self.current_model.as_ref()
        {
            self.setup.push_back(Setup::Model(model));
        }
        if let Some(thinking) = self.settings.thinking.clone()
            && Some(&thinking) != self.thinking.as_ref()
        {
            self.setup.push_back(Setup::Thinking(thinking));
        }
        self.advance_setup();
    }

    fn next_setup(&mut self, setup: bool) {
        if setup {
            self.advance_setup();
        }
    }

    fn advance_setup(&mut self) {
        match self.setup.pop_front() {
            Some(Setup::Mode(mode)) => self.set_mode(mode, true),
            Some(Setup::Model(model)) => self.set_model(model, true),
            Some(Setup::Thinking(value)) => self.set_thinking(value, true),
            None => {
                self.stage = Stage::Ready;
                while let Some(command) = self.held.pop_front() {
                    self.on_command(command);
                }
            }
        }
    }

    // --- what the agent says ------------------------------------------------

    /// Handles one message. `Err` ends the session.
    fn on_message(&mut self, message: Value) -> Result<(), String> {
        let method = message["method"].as_str().map(str::to_string);
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                self.on_request(&method, id, &message["params"]);
                Ok(())
            }
            (Some(method), None) => {
                self.on_notification(&method, &message["params"]);
                Ok(())
            }
            (None, Some(id)) => {
                // Some agents echo numeric ids back as strings.
                let id = id
                    .as_u64()
                    .or_else(|| id.as_str().and_then(|s| s.parse().ok()));
                match id.and_then(|id| self.pending.remove(&id)) {
                    Some(pending) => self.on_response(pending, &message),
                    None => {
                        debug!(%message, "a response to no request");
                        Ok(())
                    }
                }
            }
            (None, None) => Ok(()),
        }
    }

    fn on_response(&mut self, pending: Pending, message: &Value) -> Result<(), String> {
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        let error = message.get("error").filter(|e| !e.is_null()).cloned();
        match pending {
            Pending::Initialize => {
                if let Some(error) = error {
                    return Err(self.fail_open("initialize", &error));
                }
                self.agent_capabilities = result["agentCapabilities"].clone();
                self.open();
            }
            Pending::NewSession => {
                if let Some(error) = error {
                    return Err(self.fail_open("session/new", &error));
                }
                self.session_id = result["sessionId"].as_str().map(str::to_string);
                self.opened(&result);
                for params in std::mem::take(&mut self.early) {
                    self.on_update(&params);
                }
            }
            Pending::LoadSession | Pending::ResumeSession => {
                self.flush_user_message();
                self.replaying = false;
                // Replayed calls are history, not running work.
                self.tools.clear();
                self.tool_order.clear();
                if let Some(error) = error {
                    // The agent lost the session: carry on in a new one.
                    let (message, _, _) = summarize_error(&error);
                    warn!(%message, "the ACP agent could not resume; starting a new session");
                    self.session_id = None;
                    self.new_session();
                    return Ok(());
                }
                self.opened(&result);
            }
            Pending::Prompt { turn } => {
                if self.turn.as_ref() != Some(&turn) {
                    return Ok(()); // a turn already ended (the process exited)
                }
                if let Some(error) = error {
                    let (message, code, diagnostic) = summarize_error(&error);
                    self.finish_turn(ProviderEvent::TurnFailed {
                        turn: Some(turn),
                        error: message,
                        code,
                        diagnostic,
                    });
                    return Ok(());
                }
                if let Some(usage) = usage_of(&result["usage"]) {
                    self.turn_usage = Some(usage);
                }
                if result["stopReason"] == "cancelled" {
                    self.synthesize_canceled_tools();
                    self.finish_turn(ProviderEvent::TurnCanceled {
                        turn: Some(turn),
                        reason: "Interrupted".into(),
                    });
                } else {
                    self.finish_turn(ProviderEvent::TurnCompleted {
                        turn: Some(turn),
                        usage: self.turn_usage.clone(),
                    });
                }
            }
            Pending::SetMode { mode, setup } => {
                match error {
                    Some(error) => warn!(mode, error = %error, "the agent refused the mode"),
                    None => {
                        self.current_mode = Some(mode);
                        self.emit_mode();
                    }
                }
                self.next_setup(setup);
            }
            Pending::SetModeOption {
                mode,
                config_id,
                setup,
            } => {
                match error {
                    Some(error) => warn!(mode, error = %error, "the agent refused the mode"),
                    None => {
                        self.current_mode =
                            Some(self.apply_option_response(&result, &config_id, "mode", &mode));
                        self.modes = catalog::modes(&Value::Null, &self.config_options).0;
                        self.emit_mode();
                    }
                }
                self.next_setup(setup);
            }
            Pending::SetModel { model, setup } => match error {
                // Paseo falls back to the model config option.
                Some(_) => self.set_model_option(model, setup),
                None => {
                    self.current_model = Some(model);
                    self.emit_runtime();
                    self.next_setup(setup);
                }
            },
            Pending::SetModelOption {
                model,
                config_id,
                setup,
            } => {
                match error {
                    Some(error) => warn!(model, error = %error, "the agent refused the model"),
                    None => {
                        self.current_model =
                            Some(self.apply_option_response(&result, &config_id, "model", &model));
                        self.emit_runtime();
                    }
                }
                self.next_setup(setup);
            }
            Pending::SetThinking {
                value,
                config_id,
                setup,
            } => {
                match error {
                    Some(error) => {
                        warn!(value, error = %error, "the agent refused the thought level");
                    }
                    None => {
                        self.thinking = Some(self.apply_option_response(
                            &result,
                            &config_id,
                            "thought_level",
                            &value,
                        ));
                        self.emit_runtime();
                    }
                }
                self.next_setup(setup);
            }
            Pending::Close => {}
        }
        Ok(())
    }

    /// The session could not open. Returns the error it ends with.
    fn fail_open(&mut self, step: &str, error: &Value) -> String {
        let (message, _, diagnostic) = summarize_error(error);
        let message = format!("{} {step} failed: {message}", self.settings.provider);
        self.fail_held(&message, diagnostic);
        message
    }

    /// After `initialize`: opens a new session, or resumes the configured one.
    fn open(&mut self) {
        let Some(session_id) = self.settings.resume.clone() else {
            return self.new_session();
        };
        // Some agents insist on all three parameters, even an empty list.
        let servers = mcp_servers(&self.settings.mcp_servers, &self.agent_capabilities);
        let params =
            json!({"sessionId": session_id, "cwd": self.settings.cwd, "mcpServers": servers});
        if self.agent_capabilities["loadSession"] == true {
            self.session_id = Some(session_id);
            self.replaying = true;
            self.request("session/load", params, Pending::LoadSession);
        } else if !self.agent_capabilities["sessionCapabilities"]["resume"].is_null() {
            self.session_id = Some(session_id);
            self.request("session/resume", params, Pending::ResumeSession);
        } else {
            warn!(
                "{} does not support ACP session resume; starting a new session",
                self.settings.provider
            );
            self.new_session();
        }
    }

    fn new_session(&mut self) {
        let servers = mcp_servers(&self.settings.mcp_servers, &self.agent_capabilities);
        let params = json!({"cwd": self.settings.cwd, "mcpServers": servers});
        self.request("session/new", params, Pending::NewSession);
    }

    /// A session answered `session/new`, `session/load` or `session/resume`.
    fn opened(&mut self, state: &Value) {
        self.config_options = state["configOptions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let (modes, current) = catalog::modes(&state["modes"], &self.config_options);
        self.modes = modes;
        if current.is_some() {
            self.current_mode = current;
        }
        self.available_models = state["models"]["availableModels"].as_array().map(|models| {
            models
                .iter()
                .filter_map(|m| m["modelId"].as_str().map(str::to_string))
                .collect()
        });
        self.current_model = state["models"]["currentModelId"]
            .as_str()
            .map(str::to_string)
            .or_else(|| catalog::current_value(&self.config_options, "model"));
        if let Some(thinking) = catalog::current_value(&self.config_options, "thought_level") {
            self.thinking = Some(thinking);
        }
        if let Some(session_id) = self.session_id.clone() {
            self.emit(ProviderEvent::SessionStarted {
                session_id,
                model: self.current_model.clone(),
            });
        }
        self.emit_mode();
        self.begin_setup();
    }

    /// Takes a `session/set_config_option` answer's options; returns the
    /// option's new value (or the requested one, if it does not say).
    fn apply_option_response(
        &mut self,
        result: &Value,
        config_id: &str,
        category: &str,
        requested: &str,
    ) -> String {
        if let Some(options) = result["configOptions"].as_array() {
            self.config_options = options.clone();
        }
        let current = catalog::select_option(&self.config_options, category, Some(config_id))
            .or_else(|| catalog::select_option_by_id(&self.config_options, config_id))
            .and_then(|option| option["currentValue"].as_str());
        match current {
            Some(value) => value.to_string(),
            None => {
                warn!(
                    config_id,
                    requested, "the config option answer has no currentValue; using the request"
                );
                requested.to_string()
            }
        }
    }

    fn on_notification(&mut self, method: &str, params: &Value) {
        if method != "session/update" {
            debug!(method, "ignored ACP notification");
            return;
        }
        if self.session_id.is_none() {
            self.early.push(params.clone());
            return;
        }
        self.on_update(params);
    }

    fn on_update(&mut self, params: &Value) {
        if params["sessionId"].as_str() != self.session_id.as_deref() {
            return;
        }
        let update = &params["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or_default();
        if kind == "user_message_chunk" {
            return self.on_user_chunk(update);
        }
        self.flush_user_message();
        match kind {
            "agent_message_chunk" => {
                let text = tools::block_text(&update["content"]);
                if !text.is_empty() {
                    let message_id = match update["messageId"].as_str() {
                        Some(id) => {
                            self.fallback_message_id = None;
                            id.to_string()
                        }
                        None => self
                            .fallback_message_id
                            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
                            .clone(),
                    };
                    self.item(
                        json!({"type": "assistant_message", "text": text, "messageId": message_id}),
                    );
                }
            }
            "agent_thought_chunk" => {
                self.fallback_message_id = None;
                let text = tools::block_text(&update["content"]);
                if !text.is_empty() {
                    self.item(json!({"type": "reasoning", "text": text}));
                }
            }
            "tool_call" | "tool_call_update" => {
                if kind == "tool_call" {
                    self.fallback_message_id = None;
                }
                let id = update["toolCallId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                // A `tool_call` starts over; an update merges.
                let previous = (kind == "tool_call_update")
                    .then(|| self.tools.get(&id))
                    .flatten();
                let snapshot = tools::merge(&id, update, previous);
                let item = tools::item(&snapshot, &self.mcp_names(), &|t| self.terminals.view(t));
                if !self.tools.contains_key(&id) {
                    self.tool_order.push(id.clone());
                }
                self.tools.insert(id, snapshot);
                self.item(item);
            }
            "plan" => {
                self.fallback_message_id = None;
                self.item(tools::plan(update));
            }
            "current_mode_update" => {
                if let Some(mode) = update["currentModeId"].as_str() {
                    self.current_mode = Some(mode.to_string());
                }
                self.emit_mode();
            }
            "config_option_update" => {
                self.config_options = update["configOptions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let (modes, mode) = catalog::modes(&Value::Null, &self.config_options);
                let model = catalog::current_value(&self.config_options, "model");
                let thinking = catalog::current_value(&self.config_options, "thought_level");
                self.modes = modes;
                let mode_changed = mode.is_some();
                if mode.is_some() {
                    self.current_mode = mode;
                }
                let runtime_changed = model.is_some() || thinking.is_some();
                if model.is_some() {
                    self.current_model = model;
                }
                if thinking.is_some() {
                    self.thinking = thinking;
                }
                if mode_changed {
                    self.emit_mode();
                } else if runtime_changed {
                    self.emit_runtime();
                }
            }
            "session_info_update" => {
                let before = (self.title.clone(), self.updated_at.clone());
                if let Some(title) = update.get("title") {
                    self.title = title.as_str().map(str::to_string);
                }
                if let Some(at) = update.get("updatedAt") {
                    self.updated_at = at.as_str().map(str::to_string);
                }
                // Paseo reads these into runtimeInfo when the turn ends.
                if before != (self.title.clone(), self.updated_at.clone()) {
                    self.emit_runtime();
                }
            }
            "usage_update" => {
                let size = update["size"]
                    .as_f64()
                    .filter(|n| n.is_finite() && *n > 0.0);
                let used = update["used"]
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0);
                if let (Some(_), Some(_)) = (size, used) {
                    let mut usage = self.turn_usage.clone().unwrap_or_else(|| json!({}));
                    usage["contextWindowMaxTokens"] = update["size"].clone();
                    usage["contextWindowUsedTokens"] = update["used"].clone();
                    // Paseo forwards these even while replaying: the meter
                    // then shows the resumed session's context.
                    self.emit(ProviderEvent::UsageUpdated { usage });
                }
            }
            // Slash commands are not offered yet (`list_commands`).
            "available_commands_update" => {}
            other => debug!(other, "ignored ACP session update"),
        }
    }

    fn on_user_chunk(&mut self, update: &Value) {
        self.fallback_message_id = None;
        if self.turn.is_some() && self.submitted_text {
            return; // the prompt the daemon already shows
        }
        let text = tools::block_text(&update["content"]);
        if text.is_empty() {
            return;
        }
        let message_id = update["messageId"].as_str().map(str::to_string);
        let starts_new = matches!(
            (&self.pending_user, &message_id),
            (Some((_, Some(pending))), Some(id)) if pending != id
        );
        if starts_new {
            self.flush_user_message();
        }
        let pending = self
            .pending_user
            .get_or_insert_with(|| (String::new(), None));
        if pending.1.is_none() {
            pending.1 = message_id;
        }
        pending.0.push_str(&text);
    }

    fn flush_user_message(&mut self) {
        if let Some((text, message_id)) = self.pending_user.take() {
            let mut item = json!({"type": "user_message", "text": text});
            if let Some(id) = message_id {
                item["messageId"] = id.into();
            }
            self.item(item);
        }
    }

    fn finish_turn(&mut self, event: ProviderEvent) {
        self.flush_user_message();
        // A turn that did not complete leaves no way to answer its
        // permission requests: tell the agent they were cancelled.
        if !matches!(event, ProviderEvent::TurnCompleted { .. }) {
            self.cancel_permissions();
        }
        self.turn = None;
        self.fallback_message_id = None;
        self.submitted_text = false;
        self.emit(event);
    }

    /// Calls still running when a turn ends are shown as canceled.
    /// The names of the MCP servers the session gave the agent.
    fn mcp_names(&self) -> Vec<String> {
        (self.settings.mcp_servers.iter())
            .map(|server| server.name.clone())
            .collect()
    }

    fn synthesize_canceled_tools(&mut self) {
        let servers = self.mcp_names();
        for id in &self.tool_order {
            let Some(snapshot) = self.tools.get(id) else {
                continue;
            };
            if tools::status(snapshot) != "running" {
                continue;
            }
            let mut item = tools::item(snapshot, &servers, &|t| self.terminals.view(t));
            item["status"] = "canceled".into();
            item["error"] = Value::Null;
            self.item(item);
        }
    }

    // --- requests from the agent --------------------------------------------

    fn on_request(&mut self, method: &str, id: Value, params: &Value) {
        match method {
            "session/request_permission" => self.on_permission(id, params),
            "fs/read_text_file" | "fs/write_text_file" => self.on_file(method, id, params),
            "terminal/create" => {
                match self.terminals.create(
                    params,
                    &self.settings.cwd,
                    &self.settings.env,
                    self.terminal_tx.clone(),
                ) {
                    Ok(terminal) => self.reply(&id, json!({"terminalId": terminal})),
                    Err(err) => self.internal_error(&id, &err.to_string()),
                }
                self.report_terminals();
            }
            "terminal/output" => match self.terminals.output(terminal_id(params)) {
                Some(output) => self.reply(&id, output),
                None => self.unknown_terminal(&id, params),
            },
            "terminal/wait_for_exit" => {
                match self.terminals.wait(terminal_id(params), id.clone()) {
                    terminal::Wait::Exited(exit) => self.reply(&id, exit),
                    terminal::Wait::Later => {}
                    terminal::Wait::Unknown => self.unknown_terminal(&id, params),
                }
            }
            "terminal/kill" | "terminal/release" => {
                let known = if method == "terminal/kill" {
                    self.terminals.kill(terminal_id(params))
                } else {
                    self.terminals.release(terminal_id(params))
                };
                if known {
                    self.reply(&id, json!({}));
                } else {
                    self.unknown_terminal(&id, params);
                }
            }
            other => {
                debug!(other, "unsupported request from the ACP agent");
                self.reply_error(
                    &id,
                    -32601,
                    &format!("\"Method not found\": {other}"),
                    json!({"method": other}),
                );
            }
        }
    }

    /// The SDK's answer to a handler that threw.
    fn internal_error(&self, id: &Value, details: &str) {
        self.reply_error(id, -32603, "Internal error", json!({"details": details}));
    }

    fn unknown_terminal(&self, id: &Value, params: &Value) {
        self.internal_error(id, &format!("Unknown terminal '{}'", terminal_id(params)));
    }

    fn on_permission(&mut self, id: Value, params: &Value) {
        let call = &params["toolCall"];
        let call_id = call["toolCallId"].as_str().unwrap_or_default();
        let snapshot = self
            .tools
            .get(call_id)
            .cloned()
            .unwrap_or_else(|| tools::merge(call_id, call, None));
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = tools::permission_request(
            &self.settings.permission_provider,
            &request_id,
            params,
            &snapshot,
            &self.mcp_names(),
        );
        self.permissions.insert(
            request_id.clone(),
            Permission {
                rpc: id,
                options: params["options"].as_array().cloned().unwrap_or_default(),
            },
        );
        self.emit(ProviderEvent::PermissionRequested {
            id: request_id,
            request,
        });
    }

    /// Reads and writes run off the session's task and answer directly.
    fn on_file(&self, method: &str, id: Value, params: &Value) {
        let Some(transport) = &self.transport else {
            return;
        };
        let input = transport.input.clone();
        let params = params.clone();
        let write = method == "fs/write_text_file";
        tokio::task::spawn_blocking(move || {
            let path = std::path::PathBuf::from(params["path"].as_str().unwrap_or_default());
            let result = if write {
                path.parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| {
                        std::fs::write(&path, params["content"].as_str().unwrap_or_default())
                    })
                    .map(|()| json!({}))
            } else {
                std::fs::read_to_string(&path).map(|raw| {
                    json!({"content": text_slice(&raw, params["line"].as_u64(), params["limit"].as_u64())})
                })
            };
            let answer = match result {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(err) => json!({"jsonrpc": "2.0", "id": id, "error": {
                    "code": -32603, "message": "Internal error", "data": {"details": err.to_string()}}}),
            };
            let _ = input.send(answer);
        });
    }
}

fn terminal_id(params: &Value) -> &str {
    params["terminalId"].as_str().unwrap_or_default()
}

/// `readTextFile`'s window: from 1-based `line`, at most `limit` lines.
fn text_slice(raw: &str, line: Option<u64>, limit: Option<u64>) -> String {
    let line = line.filter(|l| *l > 0);
    let limit = limit.filter(|l| *l > 0);
    if line.is_none() && limit.is_none() {
        return raw.to_string();
    }
    let start = line.unwrap_or(1).saturating_sub(1) as usize;
    let lines = raw.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let window: Vec<&str> = match limit {
        Some(limit) => lines.skip(start).take(limit as usize).collect(),
        None => lines.skip(start).collect(),
    };
    window.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_summarized_as_paseo_does() {
        let error = json!({"code": -32602, "message": "Invalid params",
                           "data": {"message": "Unknown model config option: fast"}});
        let (message, code, diagnostic) = summarize_error(&error);
        assert_eq!(message, "Invalid params: Unknown model config option: fast");
        assert_eq!(code.as_deref(), Some("-32602"));
        assert!(
            diagnostic.unwrap().ends_with(
                r#"| code=-32602 | data={"message":"Unknown model config option: fast"}"#
            )
        );
        let (plain, code, _) = summarize_error(&json!({"code": 1, "message": "x"}));
        assert_eq!((plain.as_str(), code.as_deref()), ("x", Some("1")));
    }

    #[test]
    fn files_are_read_in_windows_of_lines() {
        let raw = "a\r\nb\nc\nd";
        assert_eq!(text_slice(raw, None, None), raw);
        assert_eq!(text_slice(raw, Some(2), Some(2)), "b\nc");
        assert_eq!(text_slice(raw, Some(3), None), "c\nd");
    }

    #[test]
    fn servers_go_over_http_when_the_agent_takes_it_else_as_their_stdio_command() {
        let server = burrow_provider::McpServer {
            name: "paseo".into(),
            url: "http://127.0.0.1:4100/mcp".into(),
            headers: vec![("Authorization".into(), "Bearer t".into())],
            stdio: Some(burrow_provider::StdioCommand {
                command: "/usr/local/bin/burrow".into(),
                args: vec!["mcp-bridge".into()],
                env: vec![("BURROW_MCP_TOKEN".into(), "t".into())],
            }),
        };
        let http = json!({"mcpCapabilities": {"http": true, "sse": true}});
        assert_eq!(
            mcp_servers(std::slice::from_ref(&server), &http),
            json!([{"type": "http", "name": "paseo", "url": "http://127.0.0.1:4100/mcp",
                    "headers": [{"name": "Authorization", "value": "Bearer t"}]}])
        );
        assert_eq!(
            mcp_servers(std::slice::from_ref(&server), &json!({})),
            json!([{"name": "paseo", "command": "/usr/local/bin/burrow", "args": ["mcp-bridge"],
                    "env": [{"name": "BURROW_MCP_TOKEN", "value": "t"}]}])
        );
        let http_only = burrow_provider::McpServer {
            stdio: None,
            ..server
        };
        assert_eq!(mcp_servers(&[http_only], &json!({})), json!([]));
    }

    #[test]
    fn initialize_carries_the_vendor_meta() {
        let plain = initialize_params(None);
        assert!(plain["clientCapabilities"].get("_meta").is_none());
        let cursor = initialize_params(Some(&json!({"parameterizedModelPicker": true})));
        assert_eq!(
            cursor["clientCapabilities"]["_meta"]["parameterizedModelPicker"],
            true
        );
        assert_eq!(cursor["clientCapabilities"]["terminal"], true);
    }
}
