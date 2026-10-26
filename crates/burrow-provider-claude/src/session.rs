//! One Claude Code session: the CLI in stream-json control mode, as the
//! Agent SDK drives it, translated to provider events.
//!
//! The protocol (verified against Agent SDK 0.3.246, which Paseo pins):
//! - stdin takes `{"type":"user",...}` messages and our `control_request`s
//!   (`initialize`, `interrupt`, `set_permission_mode`, `set_model`);
//! - stdout gives `system`, `stream_event` (partial deltas), `assistant`,
//!   `user` (tool results) and `result` messages, `control_response`s to our
//!   requests, and the CLI's own `control_request`s, of which `can_use_tool`
//!   is a permission prompt answered with a `control_response`.
//!
//! The mapping to timeline items follows Paseo's Claude provider: text and
//! reasoning stream as deltas keyed by the API message id, a tool call opens
//! as `unknown` on its block start and gains a typed detail from the full
//! assistant frame, and finishes on its `tool_result`.

use std::collections::HashMap;

use burrow_provider::{Command, Image, ProviderEvent};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::process::Output;
use crate::tools;

/// The two ends of a CLI process (or a test double).
pub struct Transport {
    pub input: mpsc::UnboundedSender<Value>,
    pub output: mpsc::UnboundedReceiver<Output>,
}

/// Starts (or restarts) the CLI. `resume` is the session to continue.
pub trait Launcher: Send + 'static {
    fn launch(&mut self, resume: Option<&str>, settings: &Settings) -> std::io::Result<Transport>;
}

/// What a launch depends on that can change during a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub model: Option<String>,
    pub mode: Option<String>,
    pub thinking: Option<String>,
    pub append_system_prompt: Option<String>,
}

enum Turn {
    Foreground(String),
    Autonomous,
}

struct Tool {
    name: String,
    input: Value,
}

struct Permission {
    tool_name: String,
    input: Value,
    tool_use_id: Option<String>,
}

#[derive(Default)]
struct Usage {
    context_used: Option<u64>,
}

pub struct Session<L: Launcher> {
    launcher: L,
    settings: Settings,
    transport: Option<Transport>,
    events: mpsc::UnboundedSender<ProviderEvent>,
    session_id: Option<String>,
    turn: Option<Turn>,
    restart_needed: bool,
    /// Messages to send once `initialize` is answered.
    held: Vec<Value>,
    initialized: bool,
    next_request: u64,
    /// Our control requests waiting for an answer: id to what they were.
    controls: HashMap<String, Value>,
    current_message: Option<String>,
    /// API message ids whose text (or thinking) already streamed as deltas.
    streamed: HashMap<String, (bool, bool)>,
    /// Content block index to tool_use id, for the current message.
    blocks: HashMap<u64, String>,
    tools: HashMap<String, Tool>,
    permissions: HashMap<String, Permission>,
    usage: Usage,
    mode: Option<String>,
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
            mode: settings.mode.clone(),
            settings,
            transport: None,
            events,
            session_id: resume,
            turn: None,
            restart_needed: false,
            held: Vec::new(),
            initialized: false,
            next_request: 0,
            controls: HashMap::new(),
            current_message: None,
            streamed: HashMap::new(),
            blocks: HashMap::new(),
            tools: HashMap::new(),
            permissions: HashMap::new(),
            usage: Usage::default(),
        }
    }

    fn emit(&self, event: ProviderEvent) {
        let _ = self.events.send(event);
    }

    fn item(&self, item: Value) {
        self.emit(ProviderEvent::Item {
            turn: self.turn_id(),
            item,
        });
    }

    fn turn_id(&self) -> Option<String> {
        match &self.turn {
            Some(Turn::Foreground(id)) => Some(id.clone()),
            _ => None,
        }
    }

    fn write(&mut self, message: Value) {
        if !self.initialized && message["type"] == "user" {
            self.held.push(message);
            return;
        }
        if let Some(transport) = &self.transport {
            let _ = transport.input.send(message);
        }
    }

    fn control(&mut self, request: Value) {
        self.next_request += 1;
        let id = format!("burrow-{}", self.next_request);
        self.controls.insert(id.clone(), request.clone());
        if let Some(transport) = &self.transport {
            let _ = transport.input.send(json!({
                "type": "control_request", "request_id": id, "request": request,
            }));
        }
    }

    fn start_process(&mut self) -> std::io::Result<()> {
        let transport = self
            .launcher
            .launch(self.session_id.as_deref(), &self.settings)?;
        self.transport = Some(transport);
        self.initialized = false;
        self.restart_needed = false;
        let mut initialize = json!({"subtype": "initialize"});
        if let Some(prompt) = &self.settings.append_system_prompt {
            initialize["appendSystemPrompt"] = prompt.clone().into();
        }
        self.control(initialize);
        Ok(())
    }

    /// Runs until shut down or the CLI can no longer be started.
    pub async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        if let Err(err) = self.start_process() {
            self.emit(ProviderEvent::Exited {
                error: Some(format!("could not start claude: {err}")),
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
            // Biased toward the CLI: what it already said (a permission
            // request, a result) is handled before a command that may answer it.
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
        self.transport = None;
        self.emit(ProviderEvent::Exited { error: None });
    }

    fn on_exit(&mut self, stderr: String) {
        let detail = stderr.trim().lines().last().unwrap_or_default().to_string();
        if self.turn.is_some() {
            self.cancel_tools("failed");
            let turn = self.turn_id();
            self.turn = None;
            self.emit(ProviderEvent::TurnFailed {
                turn,
                error: if detail.is_empty() {
                    "Claude Code exited unexpectedly".into()
                } else {
                    format!("Claude Code exited: {detail}")
                },
                code: None,
                diagnostic: None,
            });
        }
        warn!(%detail, "claude exited; it restarts with the next prompt");
        self.restart_needed = true;
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
                    error: format!("could not start claude: {err}"),
                    code: None,
                    diagnostic: None,
                });
                self.turn = None;
                false
            }
        }
    }

    fn user_message(&self, text: &str, images: &[Image], uuid: &str, steer: bool) -> Value {
        let mut content = Vec::new();
        for image in images {
            if matches!(
                image.mime_type.as_str(),
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            ) {
                content.push(json!({"type": "image", "source": {
                    "type": "base64", "media_type": image.mime_type, "data": image.data}}));
            }
        }
        if !text.is_empty() {
            // The CLI expands a slash command only in the last block.
            content.push(json!({"type": "text", "text": text}));
        }
        let mut message = json!({
            "type": "user",
            "message": {"role": "user", "content": content},
            "parent_tool_use_id": null,
            "uuid": uuid,
            "session_id": self.session_id.clone().unwrap_or_default(),
        });
        if steer {
            message["priority"] = "next".into();
        }
        message
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Prompt {
                turn_id,
                message_id,
                text,
                images,
            } => {
                if !self.ensure_process() {
                    return;
                }
                self.turn = Some(Turn::Foreground(turn_id.clone()));
                self.emit(ProviderEvent::TurnStarted {
                    turn: Some(turn_id),
                });
                let message = self.user_message(&text, &images, &uuid_for(&message_id), false);
                self.write(message);
            }
            Command::Steer {
                message_id,
                text,
                images,
            } => {
                let message = self.user_message(&text, &images, &uuid_for(&message_id), true);
                self.write(message);
            }
            Command::Interrupt => {
                if self.turn.is_none() {
                    return;
                }
                self.cancel_tools("canceled");
                let turn = self.turn_id();
                self.turn = None;
                self.emit(ProviderEvent::TurnCanceled {
                    turn,
                    reason: "Interrupted".into(),
                });
                for (id, _) in self.permissions.drain().collect::<Vec<_>>() {
                    self.emit(ProviderEvent::PermissionCanceled { id });
                }
                // Stop means stop: also withdraw messages still queued in the
                // CLI (older CLIs ignore the field).
                self.control(json!({"subtype": "interrupt", "cancel_queued": true}));
            }
            Command::SetMode(mode) => {
                self.mode = Some(mode.clone());
                self.settings.mode = Some(mode.clone());
                self.control(json!({"subtype": "set_permission_mode", "mode": mode}));
            }
            Command::SetModel(model) => {
                self.settings.model = model.clone();
                self.control(json!({"subtype": "set_model", "model": model}));
            }
            Command::SetThinking(thinking) => {
                // Thinking is a launch flag: it applies from the next turn.
                self.settings.thinking = thinking;
                self.restart_needed = true;
            }
            Command::RespondPermission { id, response } => self.answer(&id, &response),
            Command::Shutdown => {}
        }
    }

    fn cancel_tools(&mut self, status: &str) {
        let tools: Vec<(String, Tool)> = self.tools.drain().collect();
        for (call_id, tool) in tools {
            self.item(
                json!({"type": "tool_call", "callId": call_id, "name": tool.name,
                "detail": tools::detail(&tool.name, &tool.input, None),
                "status": status, "error": null}),
            );
        }
    }

    fn answer(&mut self, id: &str, response: &Value) {
        let Some(permission) = self.permissions.remove(id) else {
            warn!(id, "an answer to a permission request that is not pending");
            return;
        };
        let body = if response["behavior"] == "allow" {
            let input = match response.get("updatedInput").filter(|v| v.is_object()) {
                Some(updated) if permission.tool_name == "AskUserQuestion" => {
                    question_answers(&permission.input, updated)
                }
                Some(updated) => updated.clone(),
                None => permission.input.clone(),
            };
            let mut body = json!({"behavior": "allow", "updatedInput": input});
            if let Some(rules) = response["updatedPermissions"].as_array() {
                body["updatedPermissions"] = rules.clone().into();
            }
            if let Some(tool_use_id) = &permission.tool_use_id {
                body["toolUseID"] = tool_use_id.clone().into();
            }
            body
        } else {
            json!({
                "behavior": "deny",
                "message": response["message"].as_str().unwrap_or("Permission request denied"),
                "interrupt": response["interrupt"].as_bool().unwrap_or(false),
            })
        };
        if let Some(transport) = &self.transport {
            let _ = transport
                .input
                .send(json!({"type": "control_response", "response": {
                "subtype": "success", "request_id": id, "response": body}}));
        }
        // An approved plan leaves plan mode, as in Paseo.
        if permission.tool_name == "ExitPlanMode" && response["behavior"] == "allow" {
            let mode = if response["selectedActionId"] == "implement_resume" {
                "bypassPermissions"
            } else {
                "acceptEdits"
            };
            self.on_command(Command::SetMode(mode.into()));
            self.emit(ProviderEvent::ModeChanged {
                mode_id: mode.into(),
            });
        }
    }

    fn on_message(&mut self, message: Value) {
        let kind = message["type"].as_str().unwrap_or_default();
        // Subagent traffic is not mapped yet (provider subagents are later work).
        let subagent = !message["parent_tool_use_id"].is_null()
            && matches!(kind, "assistant" | "user" | "stream_event");
        if subagent {
            return;
        }
        match kind {
            "system" => self.on_system(&message),
            "stream_event" => self.on_stream(&message["event"]),
            "assistant" => self.on_assistant(&message),
            "user" => self.on_user(&message),
            "result" => self.on_result(&message),
            "control_request" => self.on_cli_request(&message),
            "control_response" => self.on_control_response(&message["response"]),
            "control_cancel_request" => {
                let id = message["request_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                if self.permissions.remove(&id).is_some() {
                    self.emit(ProviderEvent::PermissionCanceled { id });
                }
            }
            _ => debug!(kind, "ignored claude message"),
        }
    }

    fn on_system(&mut self, message: &Value) {
        match message["subtype"].as_str() {
            Some("init") => {
                if let Some(session_id) = message["session_id"].as_str()
                    && self.session_id.as_deref() != Some(session_id)
                {
                    self.session_id = Some(session_id.to_string());
                    self.emit(ProviderEvent::SessionStarted {
                        session_id: session_id.to_string(),
                        model: message["model"].as_str().map(str::to_string),
                    });
                }
                if self.turn.is_none() {
                    // The CLI woke itself (a background task finished).
                    self.turn = Some(Turn::Autonomous);
                    self.emit(ProviderEvent::TurnStarted { turn: None });
                }
                self.note_mode(message["permissionMode"].as_str());
            }
            Some("status") => self.note_mode(message["permissionMode"].as_str()),
            Some("session_state_changed") => {
                if message["state"] == "idle" && matches!(self.turn, Some(Turn::Autonomous)) {
                    self.turn = None;
                    self.emit(ProviderEvent::TurnCompleted {
                        turn: None,
                        usage: None,
                    });
                }
            }
            Some("compact_boundary") => {
                let meta = &message["compact_metadata"];
                let mut item = json!({"type": "compaction", "status": "completed"});
                if let Some(trigger) = meta["trigger"].as_str() {
                    item["trigger"] = trigger.into();
                }
                if let Some(tokens) = meta["pre_tokens"].as_u64() {
                    item["preTokens"] = tokens.into();
                }
                self.item(item);
            }
            _ => {}
        }
    }

    fn note_mode(&mut self, mode: Option<&str>) {
        let Some(mode) = mode else { return };
        let mode = if mode == "manual" { "default" } else { mode };
        if self.mode.as_deref() != Some(mode) {
            self.mode = Some(mode.to_string());
            self.emit(ProviderEvent::ModeChanged {
                mode_id: mode.to_string(),
            });
        }
    }

    fn on_stream(&mut self, event: &Value) {
        match event["type"].as_str() {
            Some("message_start") => {
                self.current_message = event["message"]["id"].as_str().map(str::to_string);
                self.blocks.clear();
                let usage = &event["message"]["usage"];
                let used = [
                    "input_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ]
                .iter()
                .filter_map(|k| usage[*k].as_u64())
                .sum::<u64>();
                self.usage.context_used = Some(used);
            }
            Some("content_block_start") => {
                let block = &event["content_block"];
                if block["type"] == "tool_use"
                    && let Some(id) = block["id"].as_str()
                {
                    let name = block["name"].as_str().unwrap_or_default().to_string();
                    if let Some(index) = event["index"].as_u64() {
                        self.blocks.insert(index, id.to_string());
                    }
                    self.item(json!({"type": "tool_call", "callId": id, "name": name,
                        "detail": {"type": "unknown", "input": {}, "output": null},
                        "status": "running", "error": null}));
                    self.tools.insert(
                        id.to_string(),
                        Tool {
                            name,
                            input: json!({}),
                        },
                    );
                }
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                let message_id = self.current_message.clone();
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or_default();
                        if let Some(id) = &message_id {
                            self.streamed.entry(id.clone()).or_default().0 = true;
                        }
                        let mut item = json!({"type": "assistant_message", "text": text});
                        if let Some(id) = message_id {
                            item["messageId"] = id.into();
                        }
                        self.item(item);
                    }
                    Some("thinking_delta") => {
                        if let Some(id) = &message_id {
                            self.streamed.entry(id.clone()).or_default().1 = true;
                        }
                        let text = delta["thinking"].as_str().unwrap_or_default();
                        self.item(json!({"type": "reasoning", "text": text}));
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let (Some(used), Some(output)) = (
                    self.usage.context_used,
                    event["usage"]["output_tokens"].as_u64(),
                ) {
                    self.usage.context_used = Some(used + output);
                }
            }
            _ => {}
        }
    }

    fn on_assistant(&mut self, message: &Value) {
        let id = message["message"]["id"].as_str().map(str::to_string);
        let (text_streamed, thinking_streamed) = id
            .as_ref()
            .and_then(|id| self.streamed.get(id).copied())
            .unwrap_or_default();
        for block in message["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
        {
            match block["type"].as_str() {
                Some("text") if !text_streamed => {
                    let mut item = json!({"type": "assistant_message", "text": block["text"]});
                    if let Some(id) = &id {
                        item["messageId"] = id.clone().into();
                    }
                    self.item(item);
                }
                Some("thinking") if !thinking_streamed => {
                    self.item(json!({"type": "reasoning", "text": block["thinking"]}));
                }
                Some("tool_use") => {
                    let call_id = block["id"].as_str().unwrap_or_default().to_string();
                    let name = block["name"].as_str().unwrap_or_default().to_string();
                    let input = block["input"].clone();
                    self.item(json!({"type": "tool_call", "callId": call_id, "name": name,
                        "detail": tools::detail(&name, &input, None),
                        "status": "running", "error": null}));
                    if name == "TodoWrite"
                        && let Some(todo) = tools::todo(&input)
                    {
                        self.item(todo);
                    }
                    self.tools.insert(call_id, Tool { name, input });
                }
                _ => {}
            }
        }
    }

    fn on_user(&mut self, message: &Value) {
        let content = &message["message"]["content"];
        for block in content.as_array().into_iter().flatten() {
            if block["type"] != "tool_result" {
                continue;
            }
            let call_id = block["tool_use_id"].as_str().unwrap_or_default();
            let Some(tool) = self.tools.remove(call_id) else {
                continue;
            };
            let text = tools::result_text(&block["content"]);
            let failed = block["is_error"].as_bool().unwrap_or(false);
            let (name, status, detail, error) = if tool.name == "ExitPlanMode" {
                // Paseo shows a decided plan as plan_approval, completed either way.
                (
                    "plan_approval".to_string(),
                    "completed",
                    tools::detail(&tool.name, &tool.input, None),
                    Value::Null,
                )
            } else if failed {
                (
                    tool.name.clone(),
                    "failed",
                    tools::detail(&tool.name, &tool.input, None),
                    json!({"type": "tool_result", "content": text, "is_error": true,
                           "tool_use_id": call_id}),
                )
            } else {
                (
                    tool.name.clone(),
                    "completed",
                    tools::detail(&tool.name, &tool.input, Some(&text)),
                    Value::Null,
                )
            };
            let mut item = json!({"type": "tool_call", "callId": call_id, "name": name,
                "detail": detail, "status": status, "error": error});
            if tool.name == "ExitPlanMode" {
                item["metadata"] = json!({"approved": !failed});
            }
            self.item(item);
        }
    }

    fn on_result(&mut self, message: &Value) {
        let Some(_) = self.turn else {
            // A result after an interrupt or for a turn already closed.
            return;
        };
        let turn = self.turn_id();
        let session_lost = message["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|e| e.contains("No conversation found with session ID"));
        if session_lost {
            // Start the next turn as a new conversation.
            self.session_id = None;
            self.restart_needed = true;
        }
        self.cancel_tools("canceled");
        self.turn = None;
        self.streamed.clear();
        let ok = message["subtype"] == "success" && !message["is_error"].as_bool().unwrap_or(false);
        if ok {
            self.emit(ProviderEvent::TurnCompleted {
                turn,
                usage: Some(self.usage_of(message)),
            });
        } else {
            let errors: Vec<&str> = message["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let error = if errors.is_empty() {
                message["result"]
                    .as_str()
                    .unwrap_or("Claude Code reported an error")
                    .to_string()
            } else {
                errors.join("\n")
            };
            self.emit(ProviderEvent::TurnFailed {
                turn,
                error,
                code: message["subtype"].as_str().map(str::to_string),
                diagnostic: None,
            });
        }
    }

    fn usage_of(&self, message: &Value) -> Value {
        let usage = &message["usage"];
        let mut out = serde_json::Map::new();
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                out.insert(key.into(), value);
            }
        };
        put(
            "inputTokens",
            usage["input_tokens"].as_u64().map(Value::from),
        );
        put(
            "cachedInputTokens",
            usage["cache_read_input_tokens"].as_u64().map(Value::from),
        );
        put(
            "outputTokens",
            usage["output_tokens"].as_u64().map(Value::from),
        );
        put(
            "totalCostUsd",
            message["total_cost_usd"].as_f64().map(Value::from),
        );
        let max = message["modelUsage"]
            .as_object()
            .into_iter()
            .flat_map(|models| models.values())
            .filter_map(|m| m["contextWindow"].as_u64())
            .max()
            .or_else(|| crate::catalog::context_window(self.settings.model.as_deref()));
        put("contextWindowMaxTokens", max.map(Value::from));
        put(
            "contextWindowUsedTokens",
            self.usage.context_used.map(Value::from),
        );
        Value::Object(out)
    }

    fn on_cli_request(&mut self, message: &Value) {
        let request_id = message["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let request = &message["request"];
        match request["subtype"].as_str() {
            Some("can_use_tool") => {
                let tool_name = request["tool_name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let input = request["input"].clone();
                let tool_use_id = request["tool_use_id"].as_str().map(str::to_string);
                let permission =
                    permission_request(&tool_name, &input, request, self.mode.as_deref());
                self.permissions.insert(
                    request_id.clone(),
                    Permission {
                        tool_name,
                        input,
                        tool_use_id,
                    },
                );
                self.emit(ProviderEvent::PermissionRequested {
                    id: request_id,
                    request: permission,
                });
            }
            Some("hook_callback") => self.reply(&request_id, json!({})),
            other => {
                debug!(?other, "unsupported request from claude");
                if let Some(transport) = &self.transport {
                    let _ = transport.input.send(json!({"type": "control_response", "response": {
                        "subtype": "error", "request_id": request_id,
                        "error": format!("not supported by this host: {}", other.unwrap_or("?"))}}));
                }
            }
        }
    }

    fn reply(&self, request_id: &str, body: Value) {
        if let Some(transport) = &self.transport {
            let _ = transport
                .input
                .send(json!({"type": "control_response", "response": {
                "subtype": "success", "request_id": request_id, "response": body}}));
        }
    }

    fn on_control_response(&mut self, response: &Value) {
        let id = response["request_id"].as_str().unwrap_or_default();
        let Some(request) = self.controls.remove(id) else {
            return;
        };
        let failed = response["subtype"] == "error";
        match request["subtype"].as_str() {
            Some("initialize") => {
                if failed {
                    warn!(error = %response["error"], "claude refused initialize");
                }
                self.initialized = true;
                for message in std::mem::take(&mut self.held) {
                    self.write(message);
                }
            }
            // Newer CLIs call Paseo's "default" mode "manual".
            Some("set_permission_mode") if failed && request["mode"] == "default" => {
                self.control(json!({"subtype": "set_permission_mode", "mode": "manual"}));
            }
            Some(subtype) if failed => {
                warn!(subtype, error = %response["error"], "claude refused a request");
            }
            _ => {}
        }
    }
}

/// A stable UUID for a client message id (the CLI wants a UUID).
fn uuid_for(message_id: &str) -> String {
    match uuid::Uuid::parse_str(message_id) {
        Ok(uuid) => uuid.to_string(),
        Err(_) => uuid::Uuid::new_v4().to_string(),
    }
}

/// An `AgentPermissionRequest` (without `id`) for a `can_use_tool` request.
pub fn permission_request(
    tool_name: &str,
    input: &Value,
    request: &Value,
    mode: Option<&str>,
) -> Value {
    let questions = input["questions"].as_array().filter(|q| !q.is_empty());
    let kind = if tool_name == "ExitPlanMode" {
        "plan"
    } else if tool_name == "AskUserQuestion" && questions.is_some() {
        "question"
    } else {
        "tool"
    };
    let mut permission = json!({"provider": "claude", "name": tool_name, "kind": kind});
    let mut metadata = serde_json::Map::new();
    if let Some(id) = request["tool_use_id"].as_str() {
        metadata.insert("toolUseId".into(), id.into());
    }
    match kind {
        "question" => {
            let questions = questions.cloned().unwrap_or_default();
            if let Some(first) = questions.first() {
                if let Some(text) = first["question"].as_str() {
                    permission["title"] = text.into();
                }
                let labels: Vec<&str> = first["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o["label"].as_str())
                    .collect();
                if !labels.is_empty() {
                    permission["description"] = labels.join(" / ").into();
                }
            }
            let mut input = input.clone();
            if let Some(list) = input["questions"].as_array_mut() {
                for question in list {
                    question["allowOther"] = true.into();
                }
            }
            permission["input"] = input;
        }
        "plan" => {
            permission["input"] = input.clone();
            if let Some(plan) = input["plan"].as_str() {
                metadata.insert("planText".into(), plan.into());
            }
            let mut actions = vec![
                json!({"id": "reject", "label": "Reject", "behavior": "deny",
                       "variant": "danger", "intent": "dismiss"}),
                json!({"id": "implement", "label": "Implement", "behavior": "allow",
                       "variant": "primary", "intent": "implement"}),
            ];
            if mode == Some("bypassPermissions") {
                actions.push(
                    json!({"id": "implement_resume", "label": "Implement with Bypass",
                    "behavior": "allow", "variant": "secondary", "intent": "implement_resume"}),
                );
            }
            permission["actions"] = actions.into();
        }
        _ => {
            permission["input"] = input.clone();
            permission["detail"] = tools::detail(tool_name, input, None);
            for (from, to) in [("title", "title"), ("description", "description")] {
                if let Some(text) = request[from].as_str() {
                    permission[to] = text.into();
                }
            }
            if let Some(suggestions) = request["permission_suggestions"].as_array() {
                permission["suggestions"] = suggestions.clone().into();
            }
        }
    }
    if !metadata.is_empty() {
        permission["metadata"] = Value::Object(metadata);
    }
    permission
}

/// Paseo's AskUserQuestion translation: clients key answers by header (or
/// by the question text); Claude wants them keyed by the full question text,
/// with the original questions and without the client-only `allowOther`.
pub fn question_answers(original: &Value, updated: &Value) -> Value {
    let mut merged = original.clone();
    if let (Some(target), Some(source)) = (merged.as_object_mut(), updated.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    if let Some(questions) = merged["questions"].as_array_mut() {
        for question in questions.iter_mut() {
            if let Some(q) = question.as_object_mut() {
                q.remove("allowOther");
            }
        }
    }
    let answers = updated["answers"].clone();
    let mut normalized = serde_json::Map::new();
    for question in merged["questions"].as_array().into_iter().flatten() {
        let text = question["question"].as_str().unwrap_or_default();
        let header = question["header"].as_str().unwrap_or_default();
        if let Some(answer) = answers.get(text).or_else(|| answers.get(header))
            && !answer.is_null()
        {
            normalized.insert(text.to_string(), answer.clone());
        }
    }
    if !normalized.is_empty() {
        merged["answers"] = Value::Object(normalized);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_answers_are_keyed_by_question_text() {
        let original = json!({"questions": [
            {"question": "Which file?", "header": "File", "options": [{"label": "a"}], "multiSelect": false}
        ]});
        let updated = json!({"questions": [
            {"question": "Which file?", "header": "File", "options": [{"label": "a"}],
             "multiSelect": false, "allowOther": true}
        ], "answers": {"File": "a"}});
        let merged = question_answers(&original, &updated);
        assert_eq!(merged["answers"], json!({"Which file?": "a"}));
        assert!(merged["questions"][0].get("allowOther").is_none());
    }

    #[test]
    fn plan_requests_offer_resume_only_from_bypass() {
        let input = json!({"plan": "# Plan"});
        let request = json!({"tool_use_id": "toolu_1"});
        let plain = permission_request("ExitPlanMode", &input, &request, Some("plan"));
        assert_eq!(plain["kind"], "plan");
        assert_eq!(plain["actions"].as_array().unwrap().len(), 2);
        assert_eq!(plain["metadata"]["planText"], "# Plan");
        let bypass =
            permission_request("ExitPlanMode", &input, &request, Some("bypassPermissions"));
        assert_eq!(bypass["actions"].as_array().unwrap().len(), 3);
    }
}
