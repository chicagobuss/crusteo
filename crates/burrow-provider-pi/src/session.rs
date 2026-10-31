//! One pi session: `pi --mode rpc`, as Paseo's pi provider drives it,
//! translated to provider events.
//!
//! The protocol (pi 1.0's `docs/rpc*.md`):
//! - stdin takes commands (`prompt`, `steer`, `abort`, `clear_queue`,
//!   `get_state`, `set_model`, `set_thinking_level`, `get_session_stats`),
//!   each with an `id` that its `response` repeats, and
//!   `extension_ui_response`s;
//! - stdout gives those responses, the session events (`agent_start`,
//!   `message_update` deltas, `tool_execution_*`, `agent_end`,
//!   `agent_settled`, ...) and `extension_ui_request`s from extensions.
//!
//! The mapping follows Paseo's `pi/agent.ts`: text and reasoning stream as
//! deltas, a tool call opens on `tool_execution_start` and finishes on its
//! end, a run is over at `agent_settled` (or `agent_end` from a pi that does
//! not settle), and failed when its last assistant message has an error. An
//! extension's dialog becomes a question, answered back as its
//! `extension_ui_response`. pi asks no permission before running tools.

use std::collections::HashMap;

use base64::Engine;
use burrow_provider::{Command, Image, ProviderEvent};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::catalog;
use crate::history;
use crate::process::Output;
use crate::tools::{self, Call};

/// The two ends of a pi process (or a test double).
pub struct Transport {
    pub input: mpsc::UnboundedSender<Value>,
    pub output: mpsc::UnboundedReceiver<Output>,
}

/// Starts (or restarts) pi. `resume` is the session to continue.
pub trait Launcher: Send + 'static {
    fn launch(&mut self, resume: Option<&str>, settings: &Settings) -> std::io::Result<Transport>;
}

/// What a launch depends on that can change during a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// `<provider>/<id>`.
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub append_system_prompt: Option<String>,
    /// Extension files pi loads (`--extension`).
    pub extensions: Vec<String>,
}

/// Header of the single question an extension dialog becomes.
const ANSWER_HEADER: &str = "Response";

/// What a request we sent was for.
#[derive(Debug, Clone, PartialEq)]
enum Request {
    State,
    Prompt,
    Steer,
    ClearQueue,
    Abort,
    Stats,
    SetModel,
    SetThinking,
}

struct Turn {
    /// The prompt's turn id; `None` for a run pi started by itself.
    id: Option<String>,
    /// pi started a run for it (`agent_start`).
    ran: bool,
    /// The run ended well; waiting for the usage to report with it.
    completing: bool,
}

pub struct Session<L: Launcher> {
    launcher: L,
    settings: Settings,
    transport: Option<Transport>,
    events: mpsc::UnboundedSender<ProviderEvent>,
    session_id: Option<String>,
    /// The current process was started to resume `session_id`.
    resuming: bool,
    /// `get_state` has answered; until then, commands are held.
    ready: bool,
    held: Vec<Value>,
    restart_needed: bool,
    next_request: u64,
    requests: HashMap<String, Request>,
    turn: Option<Turn>,
    /// An interrupt is waiting for pi's `abort` answer; a failure the
    /// aborted run reported meanwhile.
    interrupting: Option<Option<String>>,
    /// `agent_end` messages, kept until `agent_settled`.
    settled: Option<Vec<Value>>,
    tools: HashMap<String, Call>,
    /// The id streamed text of the current assistant message carries.
    assistant_message: Option<String>,
    /// Open extension dialogs: id to method.
    dialogs: HashMap<String, String>,
    /// The model pi runs, as `get_state` described it.
    model: Option<Value>,
}

impl<L: Launcher> Session<L> {
    pub fn new(
        launcher: L,
        settings: Settings,
        resume: Option<String>,
        events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Self {
        Self {
            launcher,
            settings,
            transport: None,
            events,
            session_id: resume,
            resuming: false,
            ready: false,
            held: Vec::new(),
            restart_needed: false,
            next_request: 0,
            requests: HashMap::new(),
            turn: None,
            interrupting: None,
            settled: None,
            tools: HashMap::new(),
            assistant_message: None,
            dialogs: HashMap::new(),
            model: None,
        }
    }

    fn emit(&self, event: ProviderEvent) {
        let _ = self.events.send(event);
    }

    fn turn_id(&self) -> Option<String> {
        self.turn.as_ref().and_then(|t| t.id.clone())
    }

    fn item(&self, item: Value) {
        self.emit(ProviderEvent::Item {
            turn: self.turn_id(),
            item,
        });
    }

    fn send(&mut self, frame: Value) {
        if !self.ready {
            self.held.push(frame);
            return;
        }
        if let Some(transport) = &self.transport {
            let _ = transport.input.send(frame);
        }
    }

    fn request(&mut self, kind: Request, mut command: Value) {
        self.next_request += 1;
        let id = format!("burrow-{}", self.next_request);
        command["id"] = id.clone().into();
        self.requests.insert(id, kind.clone());
        if kind == Request::State {
            // Sent at once: it is what makes the process ready.
            if let Some(transport) = &self.transport {
                let _ = transport.input.send(command);
            }
        } else {
            self.send(command);
        }
    }

    fn start_process(&mut self) -> std::io::Result<()> {
        let resume = self.session_id.clone();
        let transport = self.launcher.launch(resume.as_deref(), &self.settings)?;
        self.transport = Some(transport);
        self.resuming = resume.is_some();
        self.ready = false;
        self.restart_needed = false;
        self.requests.clear();
        self.request(Request::State, json!({"type": "get_state"}));
        Ok(())
    }

    /// Runs until shut down or pi can no longer be started.
    pub async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        if let Err(err) = self.start_process() {
            self.emit(ProviderEvent::Exited {
                error: Some(format!("could not start pi: {err}")),
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
            // Biased toward pi: what it already said is handled before a
            // command that may answer it.
            tokio::select! {
                biased;
                output = output => match output {
                    Some(Output::Message(message)) => self.on_message(message),
                    Some(Output::Garbage(_)) => {}
                    Some(Output::Exited { status, stderr }) => self.on_exit(status, stderr),
                    None => self.on_exit(None, String::new()),
                },
                command = commands.recv() => match command {
                    None | Some(Command::Shutdown) => break,
                    Some(command) => self.on_command(command),
                },
            }
        }
        // Dropping the transport closes pi's stdin: its orderly shutdown.
        self.transport = None;
        self.emit(ProviderEvent::Exited { error: None });
    }

    fn on_exit(&mut self, status: Option<i32>, stderr: String) {
        self.transport = None;
        self.requests.clear();
        let stderr = stderr.trim().to_string();
        // A session pi no longer has: start a new one, and keep what waits.
        if self.resuming && !self.ready && stderr.contains("No session found") {
            warn!(session = ?self.session_id, "pi no longer has this session; starting a new one");
            self.session_id = None;
            if self.start_process().is_ok() {
                return;
            }
        }
        self.held.clear();
        self.ready = false;
        self.restart_needed = true;
        for (id, _) in self.dialogs.drain().collect::<Vec<_>>() {
            self.emit(ProviderEvent::PermissionCanceled { id });
        }
        self.interrupting = None;
        self.settled = None;
        let code = status.map_or("null".to_string(), |c| c.to_string());
        let error = format!("Pi RPC process exited with code {code}\n{stderr}")
            .trim()
            .to_string();
        warn!(%error, "pi exited; it restarts with the next prompt");
        if self.turn.is_some() {
            self.cancel_tools("failed");
        }
        if let Some(turn) = self.turn.take() {
            let event = if turn.completing {
                ProviderEvent::TurnCompleted {
                    turn: turn.id,
                    usage: None,
                }
            } else {
                ProviderEvent::TurnFailed {
                    turn: turn.id,
                    error,
                    code: None,
                    diagnostic: None,
                }
            };
            self.emit(event);
        }
    }

    fn ensure_process(&mut self) -> bool {
        if self.transport.is_some() && !self.restart_needed {
            return true;
        }
        self.transport = None;
        match self.start_process() {
            Ok(()) => true,
            Err(err) => {
                self.emit(ProviderEvent::TurnFailed {
                    turn: self.turn_id(),
                    error: format!("could not start pi: {err}"),
                    code: None,
                    diagnostic: None,
                });
                self.turn = None;
                false
            }
        }
    }

    /// The prompt text and images pi gets. A model without image input gets
    /// each image as a file path instead, as in Paseo.
    fn prompt_payload(&self, kind: &str, text: &str, images: &[Image]) -> Value {
        let takes_images = self.model.as_ref().is_some_and(|m| {
            m["input"]
                .as_array()
                .is_some_and(|i| i.iter().any(|v| v == "image"))
        });
        let mut parts = vec![text.to_string()];
        let mut forwarded = Vec::new();
        for image in images {
            if takes_images {
                forwarded.push(json!({"type": "image", "data": image.data,
                                      "mimeType": image.mime_type}));
            } else {
                parts.push(image_hint(image));
            }
        }
        let text = parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut command = json!({"type": kind, "message": text});
        if !forwarded.is_empty() {
            command["images"] = forwarded.into();
        }
        command
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Prompt {
                turn_id,
                text,
                images,
                ..
            } => {
                if !self.ensure_process() {
                    return;
                }
                self.turn = Some(Turn {
                    id: Some(turn_id.clone()),
                    ran: false,
                    completing: false,
                });
                self.settled = None;
                self.assistant_message = None;
                self.emit(ProviderEvent::TurnStarted {
                    turn: Some(turn_id),
                });
                let prompt = self.prompt_payload("prompt", &text, &images);
                self.request(Request::Prompt, prompt);
            }
            Command::Steer { text, images, .. } => {
                // pi refuses extension commands as steers; a prompt marked
                // to steer runs them.
                let command = if text.trim_start().starts_with('/') {
                    let mut prompt = self.prompt_payload("prompt", &text, &images);
                    prompt["streamingBehavior"] = "steer".into();
                    prompt
                } else {
                    self.prompt_payload("steer", &text, &images)
                };
                self.request(Request::Steer, command);
            }
            Command::Interrupt => self.interrupt(),
            // pi has no modes; the daemon refuses unknown ones.
            Command::SetMode(_) => {}
            Command::SetModel(model) => {
                let Some(model) = model else { return };
                self.settings.model = Some(model.clone());
                match catalog::split_model(&model) {
                    Some((provider, id)) => self.request(
                        Request::SetModel,
                        json!({"type": "set_model", "provider": provider, "modelId": id}),
                    ),
                    None => warn!(model, "a pi model id names its provider: <provider>/<id>"),
                }
            }
            Command::SetThinking(option) => {
                let level =
                    catalog::thinking_level(option.as_deref()).unwrap_or(catalog::DEFAULT_THINKING);
                self.settings.thinking = Some(level.to_string());
                self.request(
                    Request::SetThinking,
                    json!({"type": "set_thinking_level", "level": level}),
                );
            }
            Command::RespondPermission { id, response } => self.answer(&id, &response),
            Command::Shutdown => {}
        }
    }

    fn interrupt(&mut self) {
        let Some(turn) = &self.turn else { return };
        if turn.completing {
            // The run is over; only its usage was still to come.
            let turn = self.turn.take().and_then(|t| t.id);
            self.emit(ProviderEvent::TurnCompleted { turn, usage: None });
            return;
        }
        if self.interrupting.is_some() {
            return;
        }
        self.interrupting = Some(None);
        // Close the dialogs first, so the run can stop.
        for (id, _) in self.dialogs.drain().collect::<Vec<_>>() {
            self.send(json!({"type": "extension_ui_response", "id": id, "cancelled": true}));
            self.emit(ProviderEvent::PermissionCanceled { id });
        }
        // Stop means stop: queued steers go too, then the run.
        self.request(Request::ClearQueue, json!({"type": "clear_queue"}));
        self.request(Request::Abort, json!({"type": "abort"}));
    }

    fn cancel_tools(&mut self, status: &str) {
        let open: Vec<(String, Call)> = self.tools.drain().collect();
        for (call_id, call) in open {
            self.item(tools::item(
                &call_id,
                &call,
                status,
                &Value::Null,
                Value::Null,
            ));
        }
    }

    fn answer(&mut self, id: &str, response: &Value) {
        let Some(method) = self.dialogs.remove(id) else {
            warn!(id, "an answer to a dialog that is not open");
            return;
        };
        let answer = response["updatedInput"]["answers"]
            .as_object()
            .and_then(|answers| answers.values().find_map(Value::as_str))
            .map(str::to_string);
        let reply = match answer {
            _ if response["behavior"] != "allow" => json!({"cancelled": true}),
            None => json!({"cancelled": true}),
            Some(answer) if method == "confirm" => {
                json!({"confirmed": answer.trim().eq_ignore_ascii_case("yes")})
            }
            Some(answer) => json!({"value": answer}),
        };
        let mut frame = json!({"type": "extension_ui_response", "id": id});
        if let (Some(frame), Some(reply)) = (frame.as_object_mut(), reply.as_object()) {
            frame.extend(reply.clone());
        }
        self.send(frame);
    }

    fn on_message(&mut self, message: Value) {
        match message["type"].as_str().unwrap_or_default() {
            "response" => self.on_response(&message),
            "extension_ui_request" => self.on_dialog(&message),
            "agent_start" => {
                match &mut self.turn {
                    Some(turn) => turn.ran = true,
                    None => {
                        // pi started a run by itself (an extension's message).
                        self.turn = Some(Turn {
                            id: None,
                            ran: true,
                            completing: false,
                        });
                        self.emit(ProviderEvent::TurnStarted { turn: None });
                    }
                }
                // Paseo reports the thread at the start of every run.
                if let Some(session_id) = self.session_id.clone() {
                    self.emit(ProviderEvent::SessionStarted {
                        session_id,
                        model: self.model.as_ref().and_then(catalog::model_id),
                    });
                }
            }
            "turn_start" => {
                if let Some(turn) = &mut self.turn {
                    turn.ran = true;
                }
            }
            "message_start" => {
                if message["message"]["role"] == "assistant" {
                    self.assistant_message = message["message"]["responseId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(str::to_string);
                }
            }
            "message_update" => self.on_update(&message),
            "message_end" => self.on_message_end(&message["message"]),
            "tool_execution_start" => {
                let call_id = message["toolCallId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let call = Call {
                    name: message["toolName"].as_str().unwrap_or_default().to_string(),
                    args: message["args"].clone(),
                };
                self.item(tools::item(
                    &call_id,
                    &call,
                    "running",
                    &Value::Null,
                    Value::Null,
                ));
                self.tools.insert(call_id, call);
            }
            "tool_execution_update" => {
                let call_id = message["toolCallId"].as_str().unwrap_or_default();
                if let Some(call) = self.tools.get(call_id) {
                    let partial = tools::result(&message["partialResult"]);
                    self.item(tools::item(call_id, call, "running", &partial, Value::Null));
                }
            }
            "tool_execution_end" => {
                let call_id = message["toolCallId"].as_str().unwrap_or_default();
                let call = self.tools.remove(call_id).unwrap_or_else(|| Call {
                    name: message["toolName"].as_str().unwrap_or_default().to_string(),
                    args: Value::Null,
                });
                let result = tools::result(&message["result"]);
                let item = if message["isError"] == true {
                    tools::item(call_id, &call, "failed", &result, message["result"].clone())
                } else {
                    tools::item(call_id, &call, "completed", &result, Value::Null)
                };
                self.item(item);
            }
            kind @ ("compaction_start" | "compaction_end") => {
                let trigger = if message["reason"] == "manual" {
                    "manual"
                } else {
                    "auto"
                };
                let status = if kind == "compaction_start" {
                    "loading"
                } else {
                    "completed"
                };
                self.item(json!({"type": "compaction", "status": status, "trigger": trigger}));
            }
            "auto_retry_start" => {
                let text = format!(
                    "Provider retry (attempt {}): {}",
                    message["attempt"],
                    message["errorMessage"].as_str().unwrap_or_default()
                );
                self.item(json!({"type": "error", "message": text}));
            }
            "thinking_level_changed" => {
                if let Some(level) = catalog::thinking_level(message["level"].as_str()) {
                    self.settings.thinking = Some(level.to_string());
                }
            }
            "command_output" => {
                let text = strip_ansi(message["text"].as_str().unwrap_or_default());
                if self.turn.is_some() && !text.trim().is_empty() {
                    self.item(json!({"type": "assistant_message", "text": text.trim()}));
                }
            }
            "agent_end" => {
                let messages = message["messages"].as_array().cloned().unwrap_or_default();
                if message.get("willRetry").is_none() {
                    // An older pi, which does not settle.
                    self.complete(messages);
                } else {
                    self.settled = Some(messages);
                }
            }
            "agent_settled" => {
                let messages = self.settled.take().unwrap_or_default();
                self.complete(messages);
            }
            kind => debug!(kind, "ignored pi event"),
        }
    }

    fn on_update(&mut self, message: &Value) {
        if message["message"].is_object() && message["message"]["role"] != "assistant" {
            return;
        }
        let event = &message["assistantMessageEvent"];
        match event["type"].as_str() {
            Some("text_delta") => {
                let id = self
                    .assistant_message
                    .get_or_insert_with(|| {
                        message["message"]["responseId"]
                            .as_str()
                            .filter(|id| !id.is_empty())
                            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string)
                    })
                    .clone();
                let text = event["delta"].as_str().unwrap_or_default();
                self.item(json!({"type": "assistant_message", "text": text, "messageId": id}));
            }
            Some("thinking_delta") => {
                let text = event["delta"].as_str().unwrap_or_default();
                self.item(json!({"type": "reasoning", "text": text}));
            }
            _ => {}
        }
    }

    fn on_message_end(&mut self, message: &Value) {
        match message["role"].as_str() {
            Some("assistant") => self.assistant_message = None,
            Some("custom") => {
                let text = history::user_text(&message["content"]);
                if message["display"] != false && !text.is_empty() {
                    let call_id = format!("pi-custom-{}", uuid::Uuid::new_v4());
                    self.item(history::custom_message(message, &text, &call_id));
                }
                // A message that started no run ends the turn.
                if self.turn.as_ref().is_some_and(|t| !t.ran) {
                    self.complete(Vec::new());
                }
            }
            _ => {}
        }
    }

    fn on_dialog(&mut self, message: &Value) {
        let Some(id) = message["id"].as_str().map(str::to_string) else {
            return;
        };
        let method = message["method"].as_str().unwrap_or_default().to_string();
        let text = |key: &str| message[key].as_str().map(str::to_string);
        let (question, options, extra) = match method.as_str() {
            "notify" => {
                if let Some(text) = text("message") {
                    let level = message["notifyType"]
                        .as_str()
                        .filter(|level| matches!(*level, "warning" | "error"))
                        .unwrap_or("info");
                    self.item(json!({"type": "notification", "level": level, "message": text}));
                }
                return;
            }
            "select" => {
                let options: Vec<String> = message["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o.as_str().map(str::to_string))
                    .collect();
                (
                    text("title").unwrap_or_else(|| "Select an option".into()),
                    options,
                    json!({}),
                )
            }
            "input" => {
                let placeholder = text("placeholder");
                let optional = optional_input(placeholder.as_deref());
                let title = text("title");
                let question = if !optional {
                    title.clone().unwrap_or_else(|| "Enter a value".into())
                } else if format!(
                    "{}\n{}",
                    title.as_deref().unwrap_or_default(),
                    placeholder.as_deref().unwrap_or_default()
                )
                .to_lowercase()
                .contains("comment")
                {
                    "Optional comment".into()
                } else {
                    "Optional response".into()
                };
                let mut extra = json!({});
                if let Some(placeholder) = placeholder {
                    extra["placeholder"] = placeholder.into();
                }
                if optional {
                    extra["allowEmpty"] = true.into();
                    extra["dismissLabel"] = "Skip".into();
                }
                (question, Vec::new(), extra)
            }
            "editor" => (
                text("title").unwrap_or_else(|| "Edit text".into()),
                Vec::new(),
                json!({}),
            ),
            "confirm" => {
                let question = [text("title"), text("message")]
                    .into_iter()
                    .flatten()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                (question, vec!["Yes".into(), "No".into()], json!({}))
            }
            // setStatus, setWidget, setTitle, set_editor_text: nothing to show.
            _ => return,
        };
        let mut entry = json!({
            "question": question, "header": ANSWER_HEADER,
            "options": options.iter().map(|label| json!({"label": label})).collect::<Vec<_>>(),
            "multiSelect": false,
        });
        if let (Some(entry), Some(extra)) = (entry.as_object_mut(), extra.as_object()) {
            entry.extend(extra.clone());
        }
        let request = json!({
            "provider": "pi", "name": format!("Pi {method}"), "kind": "question",
            "title": question, "input": {"questions": [entry]},
            "metadata": {"extensionUiMethod": method, "answerHeader": ANSWER_HEADER},
        });
        self.dialogs.insert(id.clone(), method);
        self.emit(ProviderEvent::PermissionRequested { id, request });
    }

    fn on_response(&mut self, response: &Value) {
        let Some(kind) = response["id"]
            .as_str()
            .and_then(|id| self.requests.remove(id))
        else {
            if response["success"] == false {
                warn!(error = %response["error"], command = %response["command"], "pi refused a command");
            }
            return;
        };
        let ok = response["success"] == true;
        let error = response["error"]
            .as_str()
            .unwrap_or("pi refused the command")
            .to_string();
        let data = &response["data"];
        match kind {
            Request::State => self.on_state(ok.then_some(data)),
            Request::Prompt if !ok => {
                if let Some(turn) = self.turn.take() {
                    self.interrupting = None;
                    self.emit(ProviderEvent::TurnFailed {
                        turn: turn.id,
                        error,
                        code: None,
                        diagnostic: None,
                    });
                }
            }
            Request::Prompt => {
                // A prompt an extension command handled starts no run.
                let ran = self.turn.as_ref().is_none_or(|t| t.ran);
                if data["disposition"] == "handled" && !ran {
                    self.complete(Vec::new());
                }
            }
            Request::Abort => {
                let Some(failure) = self.interrupting.take() else {
                    return;
                };
                if self.turn.is_none() {
                    return;
                }
                self.cancel_tools("canceled");
                self.settled = None;
                let Some(turn) = self.turn.take() else { return };
                let event = match failure {
                    Some(failure) if !ok => ProviderEvent::TurnFailed {
                        turn: turn.id,
                        error: failure,
                        code: None,
                        diagnostic: None,
                    },
                    _ => ProviderEvent::TurnCanceled {
                        turn: turn.id,
                        reason: "interrupted".into(),
                    },
                };
                self.emit(event);
            }
            Request::Stats => {
                if self.turn.as_ref().is_some_and(|t| t.completing) {
                    let turn = self.turn.take().and_then(|t| t.id);
                    let usage = if ok { usage(data) } else { None };
                    self.emit(ProviderEvent::TurnCompleted { turn, usage });
                }
            }
            Request::SetModel if ok => {
                if catalog::model_id(data).is_some() {
                    self.model = Some(data.clone());
                }
            }
            // Older pis have no clear_queue; the abort still stops the run.
            Request::ClearQueue if !ok => debug!(error, "pi did not clear its queue"),
            _ if !ok => warn!(?kind, error, "pi refused a command"),
            _ => {}
        }
    }

    fn on_state(&mut self, state: Option<&Value>) {
        if let Some(state) = state {
            if state["model"].is_object() {
                self.model = Some(state["model"].clone());
            }
            let current = self.model.as_ref().and_then(catalog::model_id);
            // Reported at the start of each run (`agent_start`), as in Paseo.
            if let Some(session_id) = state["sessionId"].as_str() {
                self.session_id = Some(session_id.to_string());
            }
            // A resumed pi runs the session's own model; switch to the one
            // asked for (pi refuses one it no longer has, and keeps its own).
            if self.resuming
                && let Some(wanted) = self.settings.model.clone()
                && current.as_deref() != Some(wanted.as_str())
                && let Some((provider, id)) = catalog::split_model(&wanted)
            {
                let command = json!({"type": "set_model", "provider": provider, "modelId": id});
                self.ready = true;
                self.request(Request::SetModel, command);
            }
        } else {
            warn!("pi did not report its state");
        }
        self.ready = true;
        for frame in std::mem::take(&mut self.held) {
            self.send(frame);
        }
    }

    /// The run is over: failed when its last assistant message has an
    /// error, else completed once the session's usage is read.
    fn complete(&mut self, messages: Vec<Value>) {
        if self.turn.as_ref().is_none_or(|t| t.completing) {
            return;
        }
        let last = messages.iter().rev().find(|m| m["role"] == "assistant");
        let error = last.and_then(error_message);
        let aborted = last.is_some_and(|m| {
            m["stopReason"]
                .as_str()
                .is_some_and(|r| r.eq_ignore_ascii_case("aborted"))
        });
        if let Some(failure) = &mut self.interrupting
            && (error.is_some() || aborted)
        {
            // The abort's answer ends the turn.
            *failure = Some(error.unwrap_or_else(|| "Pi turn failed".into()));
            return;
        }
        self.interrupting = None;
        self.cancel_tools("canceled");
        if let Some(error) = error {
            let turn = self.turn.take().and_then(|t| t.id);
            self.emit(ProviderEvent::TurnFailed {
                turn,
                error,
                code: None,
                diagnostic: None,
            });
            return;
        }
        if let Some(turn) = &mut self.turn {
            turn.completing = true;
        }
        self.request(Request::Stats, json!({"type": "get_session_stats"}));
    }
}

/// Paseo's failure text for an assistant message with an error.
fn error_message(message: &Value) -> Option<String> {
    let headline = message["errorMessage"]
        .as_str()
        .map(str::trim)
        .filter(|e| !e.is_empty())?;
    let mut details = Vec::new();
    if let Some(reason) = message["stopReason"].as_str() {
        details.push(format!("stopReason={reason}"));
    }
    if let (Some(provider), Some(model)) = (message["provider"].as_str(), message["model"].as_str())
    {
        details.push(format!("model={provider}/{model}"));
    }
    if let Some(model) = message["responseModel"].as_str() {
        details.push(format!("responseModel={model}"));
    }
    if let Some(id) = message["responseId"].as_str() {
        details.push(format!("responseId={id}"));
    }
    let partial = message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| match part["type"].as_str() {
            Some("text") => part["text"].as_str(),
            Some("thinking") => part["thinking"].as_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let partial = partial.trim();
    if !partial.is_empty() {
        let cut: String = partial.chars().take(500).collect();
        details.push(format!("partial={}", Value::from(cut)));
    }
    Some(if details.is_empty() {
        headline.to_string()
    } else {
        format!("{headline} ({})", details.join(", "))
    })
}

/// `AgentUsage` from `get_session_stats`; `None` when it has nothing.
pub fn usage(stats: &Value) -> Option<Value> {
    let number = |v: &Value| v.as_f64().unwrap_or(0.0);
    let input = number(&stats["tokens"]["input"]);
    let cached = number(&stats["tokens"]["cacheRead"]);
    let output = number(&stats["tokens"]["output"]);
    let cost = number(&stats["cost"]);
    let max = stats["contextUsage"]["contextWindow"].as_f64();
    let used = stats["contextUsage"]["tokens"].as_f64();
    if input == 0.0
        && cached == 0.0
        && output == 0.0
        && cost == 0.0
        && max.is_none()
        && used.is_none()
    {
        return None;
    }
    let mut usage = json!({
        "inputTokens": stats["tokens"]["input"].as_u64().unwrap_or(0),
        "cachedInputTokens": stats["tokens"]["cacheRead"].as_u64().unwrap_or(0),
        "outputTokens": stats["tokens"]["output"].as_u64().unwrap_or(0),
        "totalCostUsd": cost,
    });
    if max.is_some() {
        usage["contextWindowMaxTokens"] = stats["contextUsage"]["contextWindow"].clone();
    }
    if used.is_some() {
        usage["contextWindowUsedTokens"] = stats["contextUsage"]["tokens"].clone();
    }
    Some(usage)
}

/// An input dialog that may be left empty.
fn optional_input(placeholder: Option<&str>) -> bool {
    let placeholder = placeholder.unwrap_or_default().to_lowercase();
    placeholder
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == "optional" || word == "skip")
}

/// Writes an image a model cannot take to a file, and says where it is.
fn image_hint(image: &Image) -> String {
    let extension = match image.mime_type.as_str() {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    let written = base64::engine::general_purpose::STANDARD
        .decode(image.data.as_bytes())
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            let dir = std::env::temp_dir().join("burrow-pi-images");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let path = dir.join(format!("{}.{extension}", uuid::Uuid::new_v4()));
            std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
            Ok(path)
        });
    match written {
        Ok(path) => format!("[Image available at: {}]", path.display()),
        Err(err) => {
            format!("[Image attachment omitted: failed to write local file ({err})]")
        }
    }
}

/// `text` without ANSI escape sequences.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // Parameters and intermediates, then one final byte.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // An OSC string ends with BEL or ESC \.
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_follows_paseo() {
        let stats = json!({"tokens": {"input": 6523, "output": 26, "cacheRead": 0},
            "cost": 0.0006627, "contextUsage": {"tokens": 3284, "contextWindow": 1048576}});
        assert_eq!(
            usage(&stats),
            Some(
                json!({"inputTokens": 6523, "cachedInputTokens": 0, "outputTokens": 26,
                "totalCostUsd": 0.0006627, "contextWindowMaxTokens": 1048576,
                "contextWindowUsedTokens": 3284})
            )
        );
        assert_eq!(
            usage(&json!({"tokens": {"input": 0, "output": 0}, "cost": 0})),
            None
        );
    }

    #[test]
    fn failures_read_like_paseo() {
        let message = json!({"role": "assistant", "errorMessage": "rate limited",
            "stopReason": "error", "provider": "google", "model": "gemini",
            "content": [{"type": "text", "text": "partial"}]});
        assert_eq!(
            error_message(&message).unwrap(),
            r#"rate limited (stopReason=error, model=google/gemini, partial="partial")"#
        );
        assert_eq!(error_message(&json!({"errorMessage": " "})), None);
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(strip_ansi("\u{1b}[1;32mok\u{1b}[0m done"), "ok done");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}x"), "x");
        assert!(optional_input(Some(
            "Optional comment (press Enter to skip)..."
        )));
        assert!(!optional_input(Some("type something...")));
    }
}
