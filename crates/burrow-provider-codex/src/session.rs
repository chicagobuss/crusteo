//! One Codex session: `codex app-server` over JSON-RPC, as Paseo's
//! `CodexAppServerAgentSession` drives it, translated to provider events.
//!
//! The protocol (verified against codex-cli 0.162.1):
//! - we send requests `{id, method, params}` and get `{id, result|error}`;
//!   the first is `initialize`, then the `initialized` notification;
//! - a thread is started (`thread/start`) or resumed (`thread/resume`) once
//!   the model list and the user's config are known, and every prompt is a
//!   `turn/start` on it; `turn/steer` adds to the running turn and
//!   `turn/interrupt` stops it;
//! - app-server notifies `turn/started`, `item/started`, deltas,
//!   `item/completed`, `thread/tokenUsage/updated` and `turn/completed`;
//! - it asks us (requests with an `id` of its own) for command, file-change
//!   and MCP approvals and for answers to questions.
//!
//! The mapping follows Paseo: assistant text and reasoning stream as deltas
//! keyed by item id, with only a missing suffix sent on completion; tool
//! items open on `item/started` and close on `item/completed`. Items of
//! other threads (Codex's sub-agents) are not mapped yet.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use base64::Engine;
use burrow_provider::{Command, Image, ProviderEvent};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::catalog::{self, Catalog};
use crate::process::Output;
use crate::tools;

/// Paseo opens a new assistant message of a turn with this rule.
const MESSAGE_BOUNDARY: &str = "\n\n---\n\n";

/// The client Paseo names itself: Codex treats this name as
/// non-originating, so requests keep Codex's own CLI identity.
pub fn initialize_params() -> Value {
    json!({
        "clientInfo": {"name": "codex_app_server_daemon", "title": "Codex App Server Daemon",
                       "version": "0.0.0"},
        "capabilities": {"experimentalApi": true, "mcpServerOpenaiFormElicitation": true},
    })
}

/// The two ends of an app-server process (or a test double).
pub struct Transport {
    pub input: mpsc::UnboundedSender<Value>,
    pub output: mpsc::UnboundedReceiver<Output>,
}

/// Starts (or restarts) `codex app-server`.
pub trait Launcher: Send + 'static {
    fn launch(&mut self) -> std::io::Result<Transport>;
}

/// What a session runs with; mode, model and thinking can change.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub cwd: String,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub thinking: Option<String>,
    pub system_prompt: Option<String>,
}

/// What one of our requests was for.
#[derive(Debug)]
enum Request {
    Initialize,
    ModelList,
    ConfigRead,
    CollaborationModes,
    ThreadStart,
    ThreadResume,
    ThreadUnarchive,
    TurnStart,
    TurnSteer { input: Vec<Value> },
    TurnInterrupt,
}

#[derive(Debug, Clone, PartialEq)]
enum Turn {
    Foreground(String),
    Autonomous,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PermissionKind {
    Command,
    File,
    Question,
    Elicitation,
}

struct Permission {
    /// The app-server's id for its request, to answer it.
    rpc_id: Value,
    kind: PermissionKind,
    request: Value,
    /// The id Paseo gives the request (`permission-<itemId>`).
    paseo_id: String,
    item_id: Option<String>,
    questions: Vec<Value>,
}

/// What app-server knows before a thread starts.
#[derive(Default)]
struct Setup {
    models: Option<Value>,
    config: Option<Value>,
    collaboration_modes: Vec<Value>,
    awaiting: usize,
}

/// Paseo's bookkeeping for one turn's items.
#[derive(Default)]
struct Tracking {
    agent_text: HashMap<String, String>,
    reasoning_text: HashMap<String, String>,
    boundary: bool,
    started: HashSet<String>,
    completed: HashSet<String>,
    /// `contextCompaction` items started and not yet completed.
    compactions: Vec<Option<String>>,
    /// A completion seen on one channel (`thread/compacted` or the item)
    /// still expected on the other.
    unpaired_notifications: usize,
    unpaired_items: usize,
}

pub struct Session<L: Launcher> {
    launcher: L,
    settings: Settings,
    catalog: Arc<Mutex<Catalog>>,
    transport: Option<Transport>,
    events: mpsc::UnboundedSender<ProviderEvent>,
    next_id: u64,
    requests: HashMap<u64, Request>,
    setup: Setup,
    /// The thread: resumed when known at start, else started.
    thread: Option<String>,
    /// The thread the daemon was told about (`SessionStarted`).
    announced: Option<String>,
    ready: bool,
    restart_needed: bool,
    model: Option<String>,
    effort: Option<String>,
    collaboration_mode: Option<(String, String)>,
    sandbox: Option<Value>,
    turn: Option<Turn>,
    /// Codex's id for the running turn, from `turn/started`.
    native_turn: Option<String>,
    /// `turn/start` was sent and `turn/started` has not come yet.
    turn_requested: bool,
    interrupt_pending: bool,
    /// Turn input waiting for the thread to be ready.
    held: Option<Vec<Value>>,
    /// Steers waiting for the turn's id, or for the turn to end.
    held_steers: Vec<Vec<Value>>,
    follow_ups: Vec<Vec<Value>>,
    permissions: HashMap<String, Permission>,
    tracking: Tracking,
    latest_usage: Option<Value>,
}

impl<L: Launcher> Session<L> {
    /// `resume` is the thread to continue; `catalog` is shared with the
    /// provider, which shows what the latest session listed.
    pub fn new(
        launcher: L,
        settings: Settings,
        resume: Option<String>,
        catalog: Arc<Mutex<Catalog>>,
        events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Self {
        Self {
            launcher,
            settings,
            catalog,
            transport: None,
            events,
            next_id: 0,
            requests: HashMap::new(),
            setup: Setup::default(),
            announced: None,
            thread: resume,
            ready: false,
            restart_needed: false,
            model: None,
            effort: None,
            collaboration_mode: None,
            sandbox: None,
            turn: None,
            native_turn: None,
            turn_requested: false,
            interrupt_pending: false,
            held: None,
            held_steers: Vec::new(),
            follow_ups: Vec::new(),
            permissions: HashMap::new(),
            tracking: Tracking::default(),
            latest_usage: None,
        }
    }

    fn emit(&self, event: ProviderEvent) {
        let _ = self.events.send(event);
    }

    fn turn_id(&self) -> Option<String> {
        match &self.turn {
            Some(Turn::Foreground(id)) => Some(id.clone()),
            _ => None,
        }
    }

    fn item(&self, item: Value) {
        self.emit(ProviderEvent::Item {
            turn: self.turn_id(),
            item,
        });
    }

    fn cwd(&self) -> Option<&str> {
        Some(self.settings.cwd.as_str()).filter(|c| !c.is_empty())
    }

    fn send(&self, frame: Value) {
        if let Some(transport) = &self.transport {
            let _ = transport.input.send(frame);
        }
    }

    fn request(&mut self, method: &str, params: Value, purpose: Request) {
        self.next_id += 1;
        let id = self.next_id;
        self.requests.insert(id, purpose);
        self.send(json!({"id": id, "method": method, "params": params}));
    }

    fn start_process(&mut self) -> std::io::Result<()> {
        self.transport = Some(self.launcher.launch()?);
        self.restart_needed = false;
        self.ready = false;
        self.next_id = 0;
        self.requests.clear();
        self.setup = Setup::default();
        self.request("initialize", initialize_params(), Request::Initialize);
        Ok(())
    }

    /// Runs until shut down or the app-server can no longer be started.
    pub async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        if let Err(err) = self.start_process() {
            self.emit(ProviderEvent::Exited {
                error: Some(format!("could not start codex: {err}")),
            });
            return;
        }
        loop {
            let output = async {
                match self.transport.as_mut() {
                    Some(transport) => transport.output.recv().await,
                    None => std::future::pending().await,
                }
            };
            // Biased toward app-server: what it already said (an approval
            // request, a finished turn) is handled before a command about it.
            tokio::select! {
                biased;
                output = output => match output {
                    Some(Output::Message(message)) => self.on_message(message),
                    Some(Output::Garbage(_)) => {}
                    Some(Output::Exited { stderr, .. }) => {
                        self.transport = None;
                        self.on_exit(stderr);
                    }
                    None => {
                        self.transport = None;
                        self.on_exit(String::new());
                    }
                },
                command = commands.recv() => match command {
                    None | Some(Command::Shutdown) => break,
                    Some(command) => self.on_command(command),
                },
            }
        }
        self.close().await;
        self.emit(ProviderEvent::Exited { error: None });
    }

    /// Closes stdin, which ends app-server, and gives it a moment to exit
    /// before it is killed.
    async fn close(&mut self) {
        let Some(Transport { input, mut output }) = self.transport.take() else {
            return;
        };
        drop(input);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(message) = output.recv().await {
                if matches!(message, Output::Exited { .. }) {
                    break;
                }
            }
        })
        .await;
    }

    fn on_exit(&mut self, stderr: String) {
        let detail = stderr.trim().lines().last().unwrap_or_default().to_string();
        self.ready = false;
        self.restart_needed = true;
        self.requests.clear();
        let ids: Vec<String> = self.permissions.drain().map(|(id, _)| id).collect();
        for id in ids {
            self.emit(ProviderEvent::PermissionCanceled { id });
        }
        if self.turn.is_some() {
            let turn = self.turn_id();
            self.end_turn();
            self.emit(ProviderEvent::TurnFailed {
                turn,
                error: if detail.is_empty() {
                    "Codex app-server exited unexpectedly".into()
                } else {
                    format!("Codex app-server exited: {detail}")
                },
                code: None,
                diagnostic: None,
            });
        }
        warn!(%detail, "codex app-server exited; it restarts with the next prompt");
    }

    /// Forgets the running turn.
    fn end_turn(&mut self) {
        self.turn = None;
        self.native_turn = None;
        self.turn_requested = false;
        self.interrupt_pending = false;
        self.held = None;
        self.held_steers.clear();
        self.tracking = Tracking::default();
    }

    // --- commands -----------------------------------------------------------

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Prompt {
                turn_id,
                text,
                images,
                ..
            } => {
                if (self.transport.is_none() || self.restart_needed)
                    && let Err(err) = self.start_process()
                {
                    self.emit(ProviderEvent::TurnStarted {
                        turn: Some(turn_id.clone()),
                    });
                    self.emit(ProviderEvent::TurnFailed {
                        turn: Some(turn_id),
                        error: format!("could not start codex: {err}"),
                        code: None,
                        diagnostic: None,
                    });
                    return;
                }
                self.turn = Some(Turn::Foreground(turn_id.clone()));
                self.emit(ProviderEvent::TurnStarted {
                    turn: Some(turn_id),
                });
                let input = user_input(&text, &images);
                self.begin_turn(input);
            }
            Command::Steer { text, images, .. } => {
                let input = user_input(&text, &images);
                match (&self.native_turn, &self.turn) {
                    (Some(_), Some(_)) => self.steer(input),
                    (None, Some(_)) => self.held_steers.push(input),
                    (_, None) => self.start_follow_up(input),
                }
            }
            Command::Interrupt => self.interrupt(),
            Command::SetMode(mode) => {
                if !catalog::known_mode(&mode) {
                    warn!(mode, "not a Codex mode; turns run as auto");
                }
                self.settings.mode = Some(mode);
                self.runtime_info();
            }
            Command::SetModel(model) => {
                let (model, _) = self
                    .catalog
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .resolve(model.as_deref(), self.effort.as_deref());
                self.settings.model = model.clone();
                self.model = model;
                self.runtime_info();
            }
            Command::SetThinking(thinking) => {
                let effort = catalog::normalize_effort(thinking.as_deref());
                self.settings.thinking = effort.clone();
                if effort.is_some() {
                    self.effort = effort;
                } else {
                    let catalog = self.catalog.lock().unwrap_or_else(|p| p.into_inner());
                    self.effort = catalog.resolve(self.model.as_deref(), None).1;
                }
                self.runtime_info();
            }
            Command::RespondPermission { id, response } => self.answer(&id, &response),
            Command::Shutdown => {}
        }
    }

    /// Sends the turn, or holds it until the thread is ready.
    fn begin_turn(&mut self, input: Vec<Value>) {
        if self.ready {
            self.send_turn_start(input);
        } else {
            self.held = Some(input);
        }
    }

    /// A message the turn it was meant for could not take: it starts a turn
    /// of its own.
    fn start_follow_up(&mut self, input: Vec<Value>) {
        if self.turn.is_some() {
            self.follow_ups.push(input);
            return;
        }
        if self.transport.is_none() || self.restart_needed {
            let _ = self.start_process();
        }
        self.turn = Some(Turn::Autonomous);
        self.emit(ProviderEvent::TurnStarted { turn: None });
        self.begin_turn(input);
    }

    fn mode(&self) -> &str {
        self.settings
            .mode
            .as_deref()
            .unwrap_or(catalog::FALLBACK_MODE)
    }

    /// `turn/start`, as Paseo's `buildTurnStartParams` builds it.
    fn send_turn_start(&mut self, input: Vec<Value>) {
        let preset = catalog::preset(self.mode());
        let native_type = catalog::sandbox_policy_type(preset.sandbox);
        let sandbox = match &self.sandbox {
            Some(resolved) if resolved["type"] == native_type => resolved.clone(),
            _ => catalog::sandbox_policy(preset.sandbox),
        };
        let mut params = json!({
            "threadId": self.thread,
            "input": input,
            "approvalPolicy": preset.approval_policy,
            "sandboxPolicy": sandbox,
            "approvalsReviewer": preset.reviewer,
        });
        if let Some(model) = &self.model {
            params["model"] = model.clone().into();
        }
        if let Some(effort) = &self.effort {
            params["effort"] = effort.clone().into();
        }
        params["serviceTier"] = "default".into();
        if let Some(mode) = self.collaboration_settings() {
            params["collaborationMode"] = mode;
        }
        if !self.settings.cwd.is_empty() {
            params["cwd"] = self.settings.cwd.clone().into();
        }
        if let Some(prompt) = &self.settings.system_prompt {
            params["developerInstructions"] = prompt.clone().into();
        }
        self.native_turn = None;
        self.turn_requested = true;
        self.request("turn/start", params, Request::TurnStart);
    }

    /// The collaboration mode a turn runs in (Paseo's
    /// `resolveCollaborationMode`); plan mode is not offered, so it is
    /// Codex's non-plan mode.
    fn collaboration_settings(&self) -> Option<Value> {
        let (_, mode) = self.collaboration_mode.as_ref()?;
        let model = self.model.as_ref()?;
        let mut settings = json!({"model": model});
        if let Some(effort) = &self.effort {
            settings["reasoning_effort"] = effort.clone().into();
        }
        if let Some(prompt) = &self.settings.system_prompt {
            settings["developer_instructions"] = prompt.clone().into();
        }
        Some(json!({"mode": mode, "settings": settings}))
    }

    fn steer(&mut self, input: Vec<Value>) {
        let params = json!({
            "threadId": self.thread,
            "expectedTurnId": self.native_turn,
            "input": input,
        });
        self.request("turn/steer", params, Request::TurnSteer { input });
    }

    fn interrupt(&mut self) {
        if self.turn.is_none() {
            return;
        }
        if let Some(native) = self.native_turn.clone() {
            let params = json!({"threadId": self.thread, "turnId": native});
            self.request("turn/interrupt", params, Request::TurnInterrupt);
        } else if self.turn_requested {
            // turn/start was accepted before Codex named the turn: stop it
            // as soon as `turn/started` does.
            self.interrupt_pending = true;
        } else {
            // Never sent: nothing to stop in Codex.
            let turn = self.turn_id();
            self.end_turn();
            self.emit(ProviderEvent::TurnCanceled {
                turn,
                reason: "interrupted".into(),
            });
        }
    }

    // --- permissions --------------------------------------------------------

    fn answer(&mut self, id: &str, response: &Value) {
        let Some(permission) = self.permissions.remove(id) else {
            warn!(id, "an answer to a permission request that is not pending");
            return;
        };
        let allow = response["behavior"] == "allow";
        let decision = if allow {
            "accept"
        } else if response["interrupt"] == true {
            "cancel"
        } else {
            "decline"
        };
        let result = match permission.kind {
            PermissionKind::Command | PermissionKind::File => {
                if !allow {
                    self.denied_call(&permission, response);
                }
                json!({"decision": decision})
            }
            PermissionKind::Elicitation => json!({
                "action": decision,
                "content": if allow { json!({}) } else { Value::Null },
                "_meta": null,
            }),
            PermissionKind::Question => self.answer_question(&permission, response),
        };
        self.send(json!({"id": permission.rpc_id, "result": result}));
    }

    /// Paseo's `emitDeniedToolCallTimelineEvent`.
    fn denied_call(&self, permission: &Permission, response: &Value) {
        let request = &permission.request;
        let name = match request["name"].as_str() {
            Some("CodexBash") => "shell",
            Some("CodexFileChange") => "apply_patch",
            Some(other) => other,
            None => "",
        };
        let detail = request.get("detail").cloned().unwrap_or_else(|| {
            json!({"type": "unknown", "input": request.get("input").cloned().unwrap_or(Value::Null),
                   "output": null})
        });
        let message = response["message"].as_str().unwrap_or("Permission denied");
        self.item(json!({
            "type": "tool_call", "callId": permission.paseo_id, "name": name,
            "status": "failed", "error": {"message": message}, "detail": detail,
            "metadata": {"permissionRequestId": permission.paseo_id, "denied": true},
        }));
    }

    /// Answers by question header (`mapCodexQuestionResponseByHeader`);
    /// returns the app-server's `{answers}`.
    fn answer_question(&self, permission: &Permission, response: &Value) -> Value {
        let call_id = permission
            .item_id
            .clone()
            .unwrap_or_else(|| permission.paseo_id.clone());
        let questions = &permission.questions;
        if response["behavior"] != "allow" {
            let status = if response["interrupt"] == true {
                "canceled"
            } else {
                "failed"
            };
            let message = response["message"].as_str().unwrap_or("Question dismissed");
            self.item(question_call(
                &call_id,
                questions,
                status,
                None,
                Some(json!({"message": message})),
            ));
            return json!({"answers": {}});
        }
        let given = &response["updatedInput"]["answers"];
        let mut answers = serde_json::Map::new();
        for question in questions {
            let Some(text) = given[question["header"].as_str().unwrap_or_default()]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            else {
                continue;
            };
            let values: Vec<String> = if question["multiSelect"] == true {
                text.split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .collect()
            } else {
                vec![text.to_string()]
            };
            if !values.is_empty() {
                answers.insert(
                    question["id"].as_str().unwrap_or_default().into(),
                    json!({"answers": values}),
                );
            }
        }
        if answers.is_empty() {
            // Paseo falls back to each question's first option.
            for question in questions {
                if let Some(label) = question["options"][0]["label"]
                    .as_str()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                {
                    answers.insert(
                        question["id"].as_str().unwrap_or_default().into(),
                        json!({"answers": [label]}),
                    );
                }
            }
        }
        let shown: serde_json::Map<String, Value> = answers
            .iter()
            .map(|(id, value)| (id.clone(), value["answers"].clone()))
            .collect();
        self.item(question_call(
            &call_id,
            questions,
            "completed",
            Some(&shown),
            None,
        ));
        json!({"answers": answers})
    }

    fn on_server_request(&mut self, id: Value, method: &str, params: &Value) {
        let key = match &id {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let permission = match method {
            "item/commandExecution/requestApproval" => self.command_approval(params),
            "item/fileChange/requestApproval" => file_approval(params),
            "item/tool/requestUserInput" | "tool/requestUserInput" => {
                self.question(params);
                question_request(params)
            }
            "mcpServer/elicitation/request" => match elicitation(params) {
                Ok(request) => request,
                Err(answer) => {
                    self.send(json!({"id": id, "result": answer}));
                    return;
                }
            },
            other => {
                // Paseo answers requests it has no handler for with `{}`.
                debug!(method = other, "unhandled request from codex");
                self.send(json!({"id": id, "result": {}}));
                return;
            }
        };
        let Some((kind, request)) = permission else {
            self.send(json!({"id": id, "error": {"message": format!("invalid {method} params")}}));
            return;
        };
        let item_id = params["itemId"].as_str().map(str::to_string);
        let paseo_id = format!(
            "permission-{}",
            params["approvalId"]
                .as_str()
                .or(item_id.as_deref())
                .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string)
        );
        let questions = request["input"]["questions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        self.permissions.insert(
            key.clone(),
            Permission {
                rpc_id: id,
                kind,
                request: request.clone(),
                paseo_id,
                item_id,
                questions,
            },
        );
        self.emit(ProviderEvent::PermissionRequested { id: key, request });
    }

    fn command_approval(&self, params: &Value) -> Option<(PermissionKind, Value)> {
        let item_id = params["itemId"].as_str()?;
        params["threadId"].as_str()?;
        params["turnId"].as_str()?;
        let command = params["command"].as_str();
        let cwd = params["cwd"].as_str();
        let preview = tools::exec_preview(item_id, &params["command"], cwd.or_else(|| self.cwd()));
        let mut input = serde_json::Map::new();
        if let Some(command) = command {
            input.insert("command".into(), command.into());
        }
        if let Some(cwd) = cwd {
            input.insert("cwd".into(), cwd.into());
        }
        let mut request = json!({
            "provider": "codex", "name": "CodexBash", "kind": "tool",
            "title": command.map_or_else(|| "Run command".to_string(), |c| format!("Run command: {c}")),
            "input": input,
            "detail": preview.map(|p| p["detail"].clone()).unwrap_or_else(|| json!({
                "type": "unknown", "input": {"command": command, "cwd": cwd}, "output": null,
            })),
            "metadata": {"itemId": item_id, "threadId": params["threadId"],
                         "turnId": params["turnId"]},
        });
        if let Some(reason) = params["reason"].as_str() {
            request["description"] = reason.into();
        }
        Some((PermissionKind::Command, request))
    }

    /// Shows a question on the timeline before it is asked.
    fn question(&self, params: &Value) {
        let Some(item_id) = params["itemId"].as_str() else {
            return;
        };
        let questions = normalize_questions(&params["questions"]);
        self.item(question_call(item_id, &questions, "running", None, None));
    }

    // --- app-server output --------------------------------------------------

    fn on_message(&mut self, message: Value) {
        let method = message["method"].as_str().map(str::to_string);
        match (message.get("id"), method) {
            (Some(id), Some(method)) => {
                self.on_server_request(id.clone(), &method, &message["params"])
            }
            (Some(id), None) => {
                let Some(request) = id.as_u64().and_then(|id| self.requests.remove(&id)) else {
                    return;
                };
                match message.get("error") {
                    Some(error) => self.on_error(request, error),
                    None => self.on_result(request, &message["result"]),
                }
            }
            (None, Some(method)) => self.on_notification(&method, &message["params"]),
            (None, None) => debug!(%message, "ignored codex message"),
        }
    }

    fn on_result(&mut self, request: Request, result: &Value) {
        match request {
            Request::Initialize => {
                self.send(json!({"method": "initialized", "params": {}}));
                self.setup.awaiting = 3;
                self.request("model/list", json!({}), Request::ModelList);
                let cwd = self.cwd().map_or(Value::Null, Value::from);
                self.request("config/read", json!({"cwd": cwd}), Request::ConfigRead);
                self.request(
                    "collaborationMode/list",
                    json!({}),
                    Request::CollaborationModes,
                );
            }
            Request::ModelList => {
                self.setup.models = Some(result.clone());
                self.setup_step();
            }
            Request::ConfigRead => {
                self.setup.config = Some(result.clone());
                self.setup_step();
            }
            Request::CollaborationModes => {
                self.setup.collaboration_modes =
                    result["data"].as_array().cloned().unwrap_or_default();
                self.setup_step();
            }
            Request::ThreadStart => {
                let Some(thread) = result["thread"]["id"].as_str().map(str::to_string) else {
                    self.setup_failed("Codex app-server did not return thread id".into());
                    return;
                };
                self.thread = Some(thread.clone());
                self.announce(&thread);
                self.thread_ready(result);
            }
            Request::ThreadResume => self.thread_ready(result),
            Request::ThreadUnarchive => self.resume_thread(),
            Request::TurnStart => {}
            Request::TurnSteer { .. } => {}
            Request::TurnInterrupt => {}
        }
    }

    fn on_error(&mut self, request: Request, error: &Value) {
        let message = error["message"]
            .as_str()
            .unwrap_or("unknown error")
            .to_string();
        match request {
            Request::Initialize => {
                self.setup_failed(format!("Codex app-server refused initialize: {message}"))
            }
            // Optional: Paseo carries on without them.
            Request::ModelList | Request::ConfigRead | Request::CollaborationModes => {
                debug!(%message, "codex setup request failed");
                self.setup_step();
            }
            Request::ThreadStart => self.setup_failed(message),
            Request::ThreadResume => {
                let thread = self.thread.clone().unwrap_or_default();
                if message.ends_with(&format!(
                    "Run `codex unarchive {thread}` to unarchive it first."
                )) {
                    // Paseo unarchives a thread an active agent still uses.
                    self.request(
                        "thread/unarchive",
                        json!({"threadId": thread}),
                        Request::ThreadUnarchive,
                    );
                    return;
                }
                self.setup_failed(format!("Failed to resume Codex thread {thread}: {message}"));
            }
            Request::ThreadUnarchive => {
                let thread = self.thread.clone().unwrap_or_default();
                if message.contains(&format!("no archived rollout found for thread id {thread}")) {
                    self.resume_thread();
                } else {
                    self.setup_failed(format!("Failed to resume Codex thread {thread}: {message}"));
                }
            }
            Request::TurnStart => {
                if self.turn.is_some() && self.native_turn.is_none() {
                    let turn = self.turn_id();
                    self.end_turn();
                    self.emit(ProviderEvent::TurnFailed {
                        turn,
                        error: message,
                        code: None,
                        diagnostic: None,
                    });
                    self.after_turn();
                }
            }
            Request::TurnSteer { input } => {
                // The turn ended under the steer: it becomes a turn of its own.
                debug!(%message, "codex refused a steer");
                self.start_follow_up(input);
            }
            Request::TurnInterrupt => {
                if message == "no active turn to interrupt" && self.turn.is_some() {
                    let turn = self.turn_id();
                    self.end_turn();
                    self.emit(ProviderEvent::TurnCanceled {
                        turn,
                        reason: "interrupted".into(),
                    });
                    self.after_turn();
                } else {
                    warn!(%message, "codex refused an interrupt");
                }
            }
        }
    }

    fn setup_step(&mut self) {
        self.setup.awaiting = self.setup.awaiting.saturating_sub(1);
        if self.setup.awaiting > 0 {
            return;
        }
        let fresh = Catalog::from_responses(
            self.setup.models.as_ref().unwrap_or(&Value::Null),
            self.setup.config.as_ref().unwrap_or(&Value::Null),
        );
        let (model, effort) = fresh.resolve(
            self.settings.model.as_deref(),
            self.settings.thinking.as_deref(),
        );
        if !fresh.models.is_empty() {
            *self.catalog.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
        }
        self.model = model;
        self.effort = effort;
        // Paseo's `findCollaborationMode("code")`.
        let modes = &self.setup.collaboration_modes;
        let name = |m: &Value| m["name"].as_str().unwrap_or_default().to_lowercase();
        let code = modes
            .iter()
            .find(|m| name(m).contains("auto") || name(m).contains("code"))
            .or_else(|| {
                modes
                    .iter()
                    .find(|m| !name(m).contains("plan") && !name(m).contains("read"))
            })
            .or(modes.first());
        self.collaboration_mode = code.map(|m| {
            (
                m["name"].as_str().unwrap_or_default().to_string(),
                m["mode"].as_str().unwrap_or("code").to_string(),
            )
        });
        if self.thread.is_some() {
            self.resume_thread();
        } else {
            self.start_thread();
        }
    }

    /// `thread/start` (Paseo's `buildThreadStartRequest`).
    fn start_thread(&mut self) {
        let preset = catalog::preset(self.mode());
        let mut params = json!({
            "model": self.model,
            "cwd": self.cwd(),
            "approvalPolicy": preset.approval_policy,
            "sandbox": preset.sandbox,
        });
        if let Some(prompt) = &self.settings.system_prompt {
            params["developerInstructions"] = prompt.clone().into();
        }
        params["approvalsReviewer"] = preset.reviewer.into();
        self.request("thread/start", params, Request::ThreadStart);
    }

    fn resume_thread(&mut self) {
        // Burrow keeps its own timeline, so the thread's turns are not
        // needed (and returning them all is deprecated for long threads).
        let mut params = json!({"threadId": self.thread, "excludeTurns": true});
        if let Some(prompt) = &self.settings.system_prompt {
            params["developerInstructions"] = prompt.clone().into();
        }
        self.request("thread/resume", params, Request::ThreadResume);
    }

    fn thread_ready(&mut self, result: &Value) {
        self.sandbox = result.get("sandbox").filter(|s| s.is_object()).cloned();
        self.ready = true;
        self.runtime_info();
        if let Some(input) = self.held.take() {
            self.send_turn_start(input);
        }
    }

    /// The thread could not start: the waiting turn fails, and the next
    /// prompt tries again.
    fn setup_failed(&mut self, error: String) {
        warn!(%error, "codex session setup failed");
        if self.turn.is_some() {
            let turn = self.turn_id();
            self.end_turn();
            self.emit(ProviderEvent::TurnFailed {
                turn,
                error,
                code: None,
                diagnostic: None,
            });
        }
        self.restart_needed = true;
    }

    /// Tells the daemon about a thread once (Paseo's `thread_started`).
    /// Returns whether it was news.
    fn announce(&mut self, thread: &str) -> bool {
        if self.announced.as_deref() == Some(thread) {
            return false;
        }
        self.announced = Some(thread.to_string());
        self.emit(ProviderEvent::SessionStarted {
            session_id: thread.to_string(),
            model: self.model.clone(),
        });
        true
    }

    /// `getRuntimeInfo`.
    fn runtime_info(&self) {
        let Some(thread) = &self.thread else { return };
        let mut info = json!({
            "provider": "codex", "sessionId": thread, "model": self.model,
            "thinkingOptionId": self.effort, "modeId": self.mode(),
        });
        if let Some((name, _)) = &self.collaboration_mode {
            info["extra"] = json!({"collaborationMode": name});
        }
        self.emit(ProviderEvent::RuntimeInfo { info });
    }

    fn on_notification(&mut self, method: &str, params: &Value) {
        let thread = if method == "thread/started" {
            params["thread"]["id"].as_str()
        } else {
            params["threadId"].as_str()
        };
        if let (Some(thread), Some(current)) = (thread, &self.thread)
            && thread != current
        {
            // A sub-agent's thread; not mapped yet.
            return;
        }
        match method {
            "thread/started" => {
                if let Some(thread) = thread {
                    let thread = thread.to_string();
                    self.thread.get_or_insert_with(|| thread.clone());
                    // The daemon's handler rewrites the runtime info.
                    if self.announce(&thread) && self.ready {
                        self.runtime_info();
                    }
                }
            }
            "turn/started" => self.on_turn_started(params),
            "turn/completed" => self.on_turn_completed(params),
            "item/started" => self.on_item_started(&params["item"]),
            "item/completed" => self.on_item_completed(&params["item"]),
            "item/agentMessage/delta" => self.on_agent_delta(params),
            "item/reasoning/summaryTextDelta" => {
                let (Some(item), Some(delta)) =
                    (params["itemId"].as_str(), params["delta"].as_str())
                else {
                    return;
                };
                self.tracking
                    .reasoning_text
                    .entry(item.to_string())
                    .or_default()
                    .push_str(delta);
                self.item(json!({"type": "reasoning", "text": delta}));
            }
            "turn/plan/updated" => self.item(tools::plan_todo(&params["plan"])),
            "thread/tokenUsage/updated" => {
                if let Some(usage) = usage(&params["tokenUsage"]) {
                    self.latest_usage = Some(usage.clone());
                    self.emit(ProviderEvent::UsageUpdated { usage });
                }
            }
            "thread/compacted" => self.on_compacted(),
            "serverRequest/resolved" => {
                let key = match &params["requestId"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if self.permissions.remove(&key).is_some() {
                    self.emit(ProviderEvent::PermissionCanceled { id: key });
                }
            }
            _ => {}
        }
    }

    fn on_turn_started(&mut self, params: &Value) {
        let Some(native) = params["turn"]["id"].as_str() else {
            return;
        };
        self.native_turn = Some(native.to_string());
        self.turn_requested = false;
        self.tracking = Tracking::default();
        if self.turn.is_none() {
            // Codex woke by itself.
            self.turn = Some(Turn::Autonomous);
            self.emit(ProviderEvent::TurnStarted { turn: None });
        }
        if self.interrupt_pending {
            self.interrupt_pending = false;
            self.interrupt();
            return;
        }
        for input in std::mem::take(&mut self.held_steers) {
            self.steer(input);
        }
    }

    fn on_turn_completed(&mut self, params: &Value) {
        if self.turn.is_none() {
            return;
        }
        // Some Codex builds end a turn without completing the compaction.
        for _ in std::mem::take(&mut self.tracking.compactions) {
            self.item(json!({"type": "compaction", "status": "completed"}));
        }
        let turn = self.turn_id();
        let status = params["turn"]["status"].as_str().unwrap_or_default();
        let error = params["turn"]["error"]["message"]
            .as_str()
            .unwrap_or("Codex turn failed")
            .to_string();
        self.end_turn();
        match status {
            "failed" => self.emit(ProviderEvent::TurnFailed {
                turn,
                error,
                code: None,
                diagnostic: None,
            }),
            "interrupted" => self.emit(ProviderEvent::TurnCanceled {
                turn,
                reason: "interrupted".into(),
            }),
            _ => self.emit(ProviderEvent::TurnCompleted {
                turn,
                usage: self.latest_usage.clone(),
            }),
        }
        self.after_turn();
    }

    /// Starts what had to wait for the turn to end.
    fn after_turn(&mut self) {
        if !self.follow_ups.is_empty() {
            let input: Vec<Value> = std::mem::take(&mut self.follow_ups)
                .into_iter()
                .flatten()
                .collect();
            self.start_follow_up(input);
        }
    }

    fn on_agent_delta(&mut self, params: &Value) {
        let (Some(item), Some(delta)) = (params["itemId"].as_str(), params["delta"].as_str())
        else {
            return;
        };
        let text = self
            .tracking
            .agent_text
            .entry(item.to_string())
            .or_default();
        let first = text.is_empty();
        text.push_str(delta);
        let shown = if first && self.tracking.boundary {
            format!("{MESSAGE_BOUNDARY}{delta}")
        } else {
            delta.to_string()
        };
        if first {
            self.tracking.boundary = false;
        }
        self.item(json!({"type": "assistant_message", "messageId": item, "text": shown}));
    }

    fn on_item_started(&mut self, raw: &Value) {
        let kind = tools::item_type(raw);
        match kind.as_deref() {
            Some("userMessage") => return, // the daemon records the prompt itself
            Some("contextCompaction") => {
                self.tracking
                    .compactions
                    .push(raw["id"].as_str().map(str::to_string));
                self.item(json!({"type": "compaction", "status": "loading"}));
                return;
            }
            _ => {}
        }
        let Some(item) = tools::thread_item(raw, false, self.cwd()) else {
            return;
        };
        if item["type"] != "tool_call" {
            return;
        }
        if let Some(id) = raw["id"].as_str()
            && !self.tracking.started.insert(id.to_string())
        {
            return;
        }
        self.item(item);
    }

    fn on_item_completed(&mut self, raw: &Value) {
        let kind = tools::item_type(raw);
        match kind.as_deref() {
            Some("userMessage") => return,
            Some("contextCompaction") => {
                self.on_compaction_item(raw["id"].as_str());
                return;
            }
            _ => {}
        }
        let Some(mut item) = tools::thread_item(raw, false, self.cwd()) else {
            return;
        };
        let id = raw["id"].as_str().map(str::to_string);
        if let Some(id) = &id
            && self.tracking.completed.contains(id)
        {
            return;
        }
        let streamed = id.as_ref().and_then(|id| match item["type"].as_str() {
            Some("assistant_message") => self.tracking.agent_text.remove(id),
            Some("reasoning") => self.tracking.reasoning_text.remove(id),
            _ => None,
        });
        if let Some(streamed) = streamed {
            // Only what the deltas did not already say.
            let full = item["text"].as_str().unwrap_or_default().to_string();
            match full.strip_prefix(streamed.as_str()) {
                Some("") => {}
                Some(suffix) => {
                    item["text"] = suffix.into();
                    self.item(item.clone());
                }
                None => self.item(item.clone()),
            }
        } else {
            self.item(item.clone());
        }
        if item["type"] == "assistant_message" {
            self.tracking.boundary = true;
        }
        if let Some(id) = id {
            self.tracking.started.remove(&id);
            self.tracking.completed.insert(id);
        }
    }

    /// A completed `contextCompaction` item. Codex can also report the same
    /// compaction as `thread/compacted`; one row shows for the pair.
    fn on_compaction_item(&mut self, id: Option<&str>) {
        let position = self
            .tracking
            .compactions
            .iter()
            .position(|c| c.as_deref() == id)
            .or_else(|| (!self.tracking.compactions.is_empty()).then_some(0));
        let consumed = position.map(|at| self.tracking.compactions.remove(at));
        if id.is_some() && consumed.is_none() && !self.tracking.compactions.is_empty() {
            return; // a late completion of an older item
        }
        if self.tracking.unpaired_notifications > 0 {
            self.tracking.unpaired_notifications -= 1;
            return;
        }
        self.item(json!({"type": "compaction", "status": "completed"}));
        self.tracking.unpaired_items += 1;
    }

    fn on_compacted(&mut self) {
        if self.tracking.unpaired_items > 0 {
            self.tracking.unpaired_items -= 1;
            return;
        }
        if !self.tracking.compactions.is_empty() {
            self.tracking.compactions.remove(0);
        }
        self.tracking.unpaired_notifications += 1;
        self.item(json!({"type": "compaction", "status": "completed"}));
    }
}

/// `toAgentUsage`: the last request's tokens, and the context meter.
pub fn usage(token_usage: &Value) -> Option<Value> {
    if !token_usage.is_object() {
        return None;
    }
    let last = &token_usage["last"];
    let positive = |a: &Value, b: &Value| {
        [a, b].into_iter().find_map(|v| {
            v.as_f64()
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(|_| v.clone())
        })
    };
    let mut usage = serde_json::Map::new();
    for key in ["inputTokens", "cachedInputTokens", "outputTokens"] {
        if last[key].is_number() {
            usage.insert(key.into(), last[key].clone());
        }
    }
    if let Some(max) = positive(
        &token_usage["model_context_window"],
        &token_usage["modelContextWindow"],
    ) {
        usage.insert("contextWindowMaxTokens".into(), max);
    }
    if let Some(used) = positive(&last["total_tokens"], &last["totalTokens"]) {
        usage.insert("contextWindowUsedTokens".into(), used);
    }
    Some(Value::Object(usage))
}

/// A prompt as app-server input: images as files Codex reads
/// (`localImage`), then the text.
fn user_input(text: &str, images: &[Image]) -> Vec<Value> {
    let mut input = Vec::new();
    let text_block = |text: &str| json!({"type": "text", "text": text, "text_elements": []});
    if images.is_empty() {
        return vec![text_block(text)];
    }
    input.push(text_block(text));
    for image in images {
        match write_image(image) {
            Ok(path) => input.push(json!({"type": "localImage", "path": path})),
            Err(err) => {
                warn!(%err, "could not write an image for codex");
                input.push(text_block(&format!(
                    "User attached image (failed to write temp file): {err}"
                )));
            }
        }
    }
    input
}

fn write_image(image: &Image) -> std::io::Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image.data.trim())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let extension = match image.mime_type.as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    let dir = std::env::temp_dir().join("burrow-codex-images");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.{extension}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&path, bytes)?;
    Ok(path.to_string_lossy().into_owned())
}

/// `normalizeCodexQuestionPrompts`.
pub fn normalize_questions(raw: &Value) -> Vec<Value> {
    raw.as_array()
        .into_iter()
        .flatten()
        .filter_map(|q| {
            let non_empty = |key: &str| q[key].as_str().filter(|s| !s.is_empty());
            let (id, header, question) = (
                non_empty("id")?,
                non_empty("header")?,
                non_empty("question")?,
            );
            let options: Vec<Value> = q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| {
                    let label = o["label"].as_str().filter(|l| !l.is_empty())?;
                    let mut option = json!({"label": label});
                    if let Some(d) = o["description"].as_str().filter(|d| !d.trim().is_empty()) {
                        option["description"] = d.into();
                    }
                    Some(option)
                })
                .collect();
            let mut out =
                json!({"id": id, "header": header, "question": question, "options": options});
            for flag in ["multiSelect", "isOther", "isSecret"] {
                if q[flag] == true {
                    out[flag] = true.into();
                }
            }
            Some(out)
        })
        .collect()
}

/// `formatCodexQuestionPrompts`.
fn format_questions(questions: &[Value]) -> String {
    questions
        .iter()
        .map(|q| {
            let mut lines = vec![format!(
                "{}: {}",
                q["header"].as_str().unwrap_or_default(),
                q["question"].as_str().unwrap_or_default()
            )];
            let labels: Vec<&str> = q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| o["label"].as_str())
                .collect();
            if !labels.is_empty() {
                lines.push(format!("Options: {}", labels.join(", ")));
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_string()
}

/// `mapCodexQuestionRequestToToolCall`.
fn question_call(
    call_id: &str,
    questions: &[Value],
    status: &str,
    answers: Option<&serde_json::Map<String, Value>>,
    error: Option<Value>,
) -> Value {
    let formatted = format_questions(questions);
    let answered = answers.filter(|a| !a.is_empty()).map(|answers| {
        answers
            .iter()
            .map(|(id, values)| {
                let values: Vec<&str> = values
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                format!("{id}: {}", values.join(", "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    let text = match answered {
        Some(answered) if status == "completed" => [formatted.as_str(), "Answers:", &answered]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => formatted,
    };
    let mut metadata = json!({"questions": questions});
    if let Some(answers) = answers {
        metadata["answers"] = Value::Object(answers.clone());
    }
    let error = match status {
        "failed" => error.unwrap_or_else(|| json!({"message": "Question dismissed"})),
        _ => Value::Null,
    };
    json!({"type": "tool_call", "callId": call_id, "name": "request_user_input",
           "detail": {"type": "plain_text", "text": text, "icon": "brain"},
           "metadata": metadata, "status": status, "error": error})
}

fn file_approval(params: &Value) -> Option<(PermissionKind, Value)> {
    let item_id = params["itemId"].as_str()?;
    params["threadId"].as_str()?;
    params["turnId"].as_str()?;
    let reason = params["reason"].as_str();
    let mut request = json!({
        "provider": "codex", "name": "CodexFileChange", "kind": "tool",
        "title": "Apply file changes",
        "detail": {"type": "unknown", "input": {"reason": reason}, "output": null},
        "metadata": {"itemId": item_id, "threadId": params["threadId"], "turnId": params["turnId"]},
    });
    if let Some(reason) = reason {
        request["description"] = reason.into();
    }
    Some((PermissionKind::File, request))
}

fn question_request(params: &Value) -> Option<(PermissionKind, Value)> {
    let item_id = params["itemId"].as_str()?;
    params["threadId"].as_str()?;
    params["turnId"].as_str()?;
    params["questions"].as_array()?;
    let questions = normalize_questions(&params["questions"]);
    Some((
        PermissionKind::Question,
        json!({
            "provider": "codex", "name": "request_user_input", "kind": "question",
            "title": "Question",
            "detail": {"type": "plain_text", "text": format_questions(&questions), "icon": "brain"},
            "input": {"questions": questions},
            "metadata": {"itemId": item_id, "threadId": params["threadId"],
                         "turnId": params["turnId"], "questions": questions},
        }),
    ))
}

/// An MCP elicitation as a permission, or the answer Paseo gives without
/// asking: URL forms and forms with required fields are declined.
fn elicitation(params: &Value) -> Result<Option<(PermissionKind, Value)>, Value> {
    let declined = json!({"action": "decline", "content": null, "_meta": null});
    let (Some(_), Some(server), Some(mode), Some(message)) = (
        params["threadId"].as_str(),
        params["serverName"].as_str(),
        params["mode"].as_str(),
        params["message"].as_str(),
    ) else {
        return Ok(None);
    };
    if mode == "url" {
        return Err(declined);
    }
    if params["requestedSchema"]["required"]
        .as_array()
        .is_some_and(|r| !r.is_empty())
    {
        return Err(declined);
    }
    Ok(Some((
        PermissionKind::Elicitation,
        json!({
            "provider": "codex", "name": "CodexMcpElicitation", "kind": "tool",
            "title": format!("MCP approval: {server}"), "description": message,
            "input": {"mode": mode, "requestedSchema": params.get("requestedSchema").cloned().unwrap_or(Value::Null),
                      "url": params.get("url").cloned().unwrap_or(Value::Null)},
            "metadata": {"threadId": params["threadId"],
                         "turnId": params.get("turnId").cloned().unwrap_or(Value::Null),
                         "serverName": server,
                         "elicitationId": params.get("elicitationId").cloned().unwrap_or(Value::Null)},
        }),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_reads_the_last_request() {
        let usage = usage(&json!({
            "total": {"totalTokens": 34313, "inputTokens": 34243},
            "last": {"totalTokens": 17207, "inputTokens": 17142, "cachedInputTokens": 16896,
                     "outputTokens": 65, "reasoningOutputTokens": 0},
            "modelContextWindow": 258400,
        }))
        .unwrap();
        assert_eq!(
            usage,
            json!({"inputTokens": 17142, "cachedInputTokens": 16896, "outputTokens": 65,
                   "contextWindowMaxTokens": 258400, "contextWindowUsedTokens": 17207})
        );
    }

    #[test]
    fn questions_keep_paseos_shape() {
        let questions = normalize_questions(&json!([
            {"id": "q1", "header": "Color", "question": "Which?", "isOther": false,
             "options": [{"label": "red", "description": ""}, {"label": "blue", "description": "Cool"}]},
            {"id": "", "header": "x", "question": "dropped"},
        ]));
        assert_eq!(
            questions,
            vec![json!({"id": "q1", "header": "Color", "question": "Which?",
                        "options": [{"label": "red"}, {"label": "blue", "description": "Cool"}]})]
        );
        assert_eq!(
            format_questions(&questions),
            "Color: Which?\nOptions: red, blue"
        );
    }
}
