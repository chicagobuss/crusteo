//! One Antigravity session: `agy` in its stream-json driver mode, translated
//! to provider events the way Paseo 0.12's plugin does (`session.ts`).
//!
//! The protocol (agy 1.1.15 and later):
//! - stdin takes `{"event":"user","message":{"content":"<text>"}}`, one turn
//!   per line;
//! - stdout gives `init` once at start (the conversation id), then for each
//!   turn `step_update` frames (`user_input`, `agent_response` text deltas,
//!   `tool` and `subagent` steps, each `ACTIVE`, `DONE` or `ERROR`) and a
//!   `result` (`SUCCESS` or `ERROR`, `"interrupted"` after `SIGINT`).
//!
//! There is no control channel. The CLI cannot ask for permission when
//! driven this way, so it always runs with `--dangerously-skip-permissions`;
//! an interrupt is `SIGINT`, after which the CLI exits; and the model is a
//! launch flag. Each of these ends the process, and the next prompt starts a
//! new one on the same conversation (`--conversation <id>`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use burrow_provider::{Command, ProviderEvent};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::catalog::{diagnostic, notification};
use crate::process::{Output, Stop, Transport};
use crate::prompt::PromptFiles;
use crate::timeline::tool_item;

/// How long the CLI may take to say `init` (Paseo's limit).
const STARTUP: Duration = Duration::from_secs(30);

/// Starts the CLI. `conversation` is the one to continue.
pub trait Launcher: Send + 'static {
    fn launch(
        &mut self,
        conversation: Option<&str>,
        settings: &Settings,
    ) -> std::io::Result<Transport>;
}

/// What a launch depends on that can change during a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Launched, waiting for `init`.
    Starting {
        deadline: Instant,
    },
    Ready,
    /// Asked to stop; its frames are ignored until it exits.
    Stopping,
}

struct Driver {
    transport: Transport,
    /// The model it was launched with.
    model: Option<String>,
    phase: Phase,
}

struct Turn {
    /// The daemon's turn id.
    id: String,
    /// Prefixes message and call ids (`<key>:<step index>`), as Paseo's
    /// own turn id does.
    key: String,
    /// The text to write, until the CLI is ready for it.
    unsent: Option<String>,
    /// Tool items by step, the latest of each.
    tools: BTreeMap<u64, Value>,
    usage_steps: HashSet<u64>,
    usage: Option<[u64; 3]>,
}

pub struct Session<L: Launcher> {
    launcher: L,
    settings: Settings,
    system_prompt: Option<String>,
    events: mpsc::UnboundedSender<ProviderEvent>,
    conversation: Option<String>,
    /// The system prompt goes in front of the first message of a new
    /// conversation only.
    first_message: bool,
    driver: Option<Driver>,
    turn: Option<Turn>,
    /// Denials already reported, counted per action: a result lists every
    /// denial since its process started.
    denials: HashMap<(String, String), usize>,
    files: PromptFiles,
}

/// The persistence handle Paseo's plugin bridge reports as the session id:
/// `plugin:{"version":1,"data":{"conversationId":"<id>"}}`.
pub fn persistence_id(conversation: &str) -> String {
    format!(
        "plugin:{{\"version\":1,\"data\":{{\"conversationId\":{}}}}}",
        Value::String(conversation.into())
    )
}

/// The conversation in a persistence handle, or a bare conversation id.
pub fn conversation_id(handle: &str) -> Option<String> {
    let id = match handle.strip_prefix("plugin:") {
        Some(encoded) => serde_json::from_str::<Value>(encoded).ok()?["data"]["conversationId"]
            .as_str()?
            .to_string(),
        None => handle.to_string(),
    };
    (!id.is_empty()).then_some(id)
}

impl<L: Launcher> Session<L> {
    pub fn new(
        launcher: L,
        settings: Settings,
        system_prompt: Option<String>,
        resume: Option<String>,
        events: mpsc::UnboundedSender<ProviderEvent>,
    ) -> Self {
        Self {
            launcher,
            settings,
            system_prompt: system_prompt.filter(|p| !p.trim().is_empty()),
            events,
            first_message: resume.is_none(),
            conversation: resume,
            driver: None,
            turn: None,
            denials: HashMap::new(),
            files: PromptFiles::default(),
        }
    }

    fn emit(&self, event: ProviderEvent) {
        let _ = self.events.send(event);
    }

    fn item(&self, item: Value) {
        self.emit(ProviderEvent::Item {
            turn: self.turn.as_ref().map(|t| t.id.clone()),
            item,
        });
    }

    /// Runs until shut down. The CLI starts at once, as Paseo opens it, so
    /// the conversation id is known before the first prompt.
    pub async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        self.launch();
        loop {
            let deadline = match &self.driver {
                Some(Driver {
                    phase: Phase::Starting { deadline },
                    ..
                }) => Some(*deadline),
                _ => None,
            };
            let output = async {
                match self.driver.as_mut() {
                    Some(driver) => driver.transport.output.recv().await,
                    None => std::future::pending().await,
                }
            };
            // Biased toward the CLI: a result it already sent is handled
            // before an interrupt that would otherwise cancel the turn.
            tokio::select! {
                biased;
                output = output => match output {
                    Some(Output::Message(frame)) => self.on_frame(frame),
                    Some(Output::Garbage(_)) => {}
                    Some(Output::Exited { status, stderr }) => self.on_exit(status, &stderr),
                    None => self.on_exit(None, ""),
                },
                _ = sleep_until(deadline), if deadline.is_some() => {
                    self.startup_failed("Antigravity did not initialize within 30 seconds");
                }
                command = commands.recv() => match command {
                    None | Some(Command::Shutdown) => break,
                    Some(command) => self.on_command(command),
                },
            }
        }
        if let Some(driver) = self.driver.take() {
            let _ = driver.transport.stop.send(Stop::Close);
        }
        self.emit(ProviderEvent::Exited { error: None });
    }

    fn launch(&mut self) {
        match self
            .launcher
            .launch(self.conversation.as_deref(), &self.settings)
        {
            Ok(transport) => {
                self.driver = Some(Driver {
                    transport,
                    model: self.settings.model.clone(),
                    phase: Phase::Starting {
                        deadline: Instant::now() + STARTUP,
                    },
                });
            }
            Err(err) => self.startup_failed(&format!("could not start agy: {err}")),
        }
    }

    fn stop(&mut self, how: Stop) {
        if let Some(driver) = self.driver.as_mut()
            && driver.phase != Phase::Stopping
        {
            let _ = driver.transport.stop.send(how);
            driver.phase = Phase::Stopping;
        }
    }

    /// Moves a prompt that waits toward the CLI: starts or restarts the CLI
    /// as needed, and writes the prompt once it is ready.
    fn advance(&mut self) {
        if self.turn.as_ref().is_none_or(|t| t.unsent.is_none()) {
            return;
        }
        let Some(driver) = &self.driver else {
            self.launch();
            return;
        };
        match driver.phase {
            Phase::Starting { .. } | Phase::Stopping => {}
            // A new model applies from this prompt: restart on the same
            // conversation, as Paseo defers it.
            Phase::Ready if driver.model != self.settings.model => self.stop(Stop::Close),
            Phase::Ready => {
                let input = driver.transport.input.clone();
                if let Some(text) = self.turn.as_mut().and_then(|t| t.unsent.take()) {
                    let _ = input.send(json!({"event": "user", "message": {"content": text}}));
                    self.first_message = false;
                }
            }
        }
    }

    fn phase(&self) -> Option<Phase> {
        self.driver.as_ref().map(|d| d.phase)
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Prompt {
                turn_id,
                text,
                images,
                ..
            } => {
                if self.turn.is_some() {
                    self.emit(ProviderEvent::TurnFailed {
                        turn: Some(turn_id),
                        error: "Antigravity already has an active turn; wait for it to finish"
                            .into(),
                        code: Some("TURN_ACTIVE".into()),
                        diagnostic: None,
                    });
                    return;
                }
                self.emit(ProviderEvent::TurnStarted {
                    turn: Some(turn_id.clone()),
                });
                let native = match self.files.encode(&text, &images) {
                    Ok(native) => native,
                    Err(error) => {
                        self.emit(ProviderEvent::TurnFailed {
                            turn: Some(turn_id),
                            error,
                            code: None,
                            diagnostic: None,
                        });
                        return;
                    }
                };
                let native = match &self.system_prompt {
                    Some(prompt) if self.first_message => format!("{prompt}\n\n{native}"),
                    _ => native,
                };
                self.turn = Some(Turn {
                    id: turn_id,
                    key: uuid::Uuid::new_v4().to_string(),
                    unsent: Some(native),
                    tools: BTreeMap::new(),
                    usage_steps: HashSet::new(),
                    usage: None,
                });
                self.advance();
            }
            Command::Interrupt => {
                let Some(turn) = &self.turn else { return };
                if turn.unsent.is_none() {
                    // The CLI has the prompt: SIGINT, and it exits.
                    self.stop(Stop::Interrupt);
                }
                self.finish_tools("canceled");
                let turn = self.turn.take().map(|t| t.id);
                self.emit(ProviderEvent::TurnCanceled {
                    turn,
                    reason: "Canceled by provider".into(),
                });
            }
            Command::SetModel(model) => self.settings.model = model,
            // Full access is the only mode, and effort is part of the model id.
            Command::SetMode(_) | Command::SetThinking(_) => {}
            Command::Steer { .. } => warn!("Antigravity cannot steer a running turn"),
            Command::RespondPermission { id, .. } => {
                warn!(id, "Antigravity asks for no permissions");
            }
            Command::Shutdown => {}
        }
    }

    fn on_frame(&mut self, frame: Value) {
        let Some(phase) = self.phase() else { return };
        let event = frame["event"].as_str().unwrap_or_default();
        match (phase, event) {
            (Phase::Stopping, _) => {}
            (Phase::Starting { .. }, "init") => {
                let id = frame["conversation_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                if id.is_empty() {
                    self.startup_failed("Antigravity started without a conversation");
                    return;
                }
                if self.conversation.as_ref().is_some_and(|c| *c != id) {
                    self.startup_failed("Antigravity restored a different conversation");
                    return;
                }
                if let Some(driver) = self.driver.as_mut() {
                    driver.phase = Phase::Ready;
                }
                self.denials.clear();
                if self.conversation.as_deref() != Some(&id) {
                    self.conversation = Some(id.clone());
                    self.emit(ProviderEvent::SessionStarted {
                        session_id: persistence_id(&id),
                        model: None,
                    });
                }
                self.advance();
            }
            (Phase::Starting { .. }, "result") => {
                let error = frame["result"]["error"]
                    .as_str()
                    .unwrap_or("Antigravity turn failed");
                self.startup_failed(&diagnostic(error));
            }
            (Phase::Ready, "step_update") => self.on_step(&frame["step_update"]),
            (Phase::Ready, "result") => self.on_result(&frame["result"]),
            _ => debug!(event, "ignored agy frame"),
        }
    }

    fn on_step(&mut self, step: &Value) {
        let Some(turn) = self.turn.as_mut().filter(|t| t.unsent.is_none()) else {
            return;
        };
        let Some(index) = step["step_index"].as_u64() else {
            return;
        };
        let id = format!("{}:{index}", turn.key);
        match step["step_type"].as_str() {
            Some("agent_response") => {
                let done = step["state"] == "DONE";
                let usage = &step["usage"];
                if done && usage.is_object() && turn.usage_steps.insert(index) {
                    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
                    let total = turn.usage.get_or_insert([0; 3]);
                    total[0] += count("input_tokens");
                    total[1] += count("output_tokens");
                    total[2] += count("cache_read_tokens");
                }
                if let Some(delta) = step["text_delta"].as_str().filter(|d| !d.is_empty()) {
                    let item = json!({"type": "assistant_message", "text": delta, "messageId": id});
                    self.item(item);
                }
            }
            Some("tool" | "subagent") => {
                let item = tool_item(step, &id);
                turn.tools.insert(index, item.clone());
                self.item(item);
            }
            _ => {}
        }
    }

    fn on_result(&mut self, result: &Value) {
        if self.turn.as_ref().is_none_or(|t| t.unsent.is_some()) {
            return;
        }
        let denied = self.new_denials(result["denied_actions"].as_array());
        if !denied.is_empty() {
            let title = if denied.len() > 1 {
                format!("{} actions denied", denied.len())
            } else if denied[0] == "command" {
                "Shell command denied".to_string()
            } else {
                "Action denied".to_string()
            };
            self.item(notification(&title, "Antigravity's own policy denied it."));
        }
        if result["status"] == "ERROR" {
            let error = result["error"]
                .as_str()
                .unwrap_or("Antigravity turn failed");
            let canceled = error == "interrupted";
            self.finish_tools(if canceled { "canceled" } else { "failed" });
            // The CLI exits after an error; retire it before it does.
            self.stop(Stop::Interrupt);
            let turn = self.turn.take().map(|t| t.id);
            self.emit(if canceled {
                ProviderEvent::TurnCanceled {
                    turn,
                    reason: diagnostic(error),
                }
            } else {
                ProviderEvent::TurnFailed {
                    turn,
                    error: diagnostic(error),
                    code: None,
                    diagnostic: None,
                }
            });
        } else {
            self.finish_tools("completed");
            let Some(turn) = self.turn.take() else { return };
            let usage = turn.usage.map(|[input, output, cached]| {
                json!({"inputTokens": input, "outputTokens": output, "cachedInputTokens": cached})
            });
            self.emit(ProviderEvent::TurnCompleted {
                turn: Some(turn.id),
                usage,
            });
        }
    }

    /// The denials in `actions` not reported yet, by their action name.
    fn new_denials(&mut self, actions: Option<&Vec<Value>>) -> Vec<String> {
        let mut counts: HashMap<(String, String), usize> = HashMap::new();
        let mut fresh = Vec::new();
        for action in actions.into_iter().flatten() {
            let key = (
                action["action"].as_str().unwrap_or_default().to_string(),
                action["display_name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
            let count = counts.entry(key.clone()).or_default();
            *count += 1;
            if *count > self.denials.get(&key).copied().unwrap_or(0) {
                fresh.push(key.0.clone());
            }
        }
        for (key, count) in counts {
            let reported = self.denials.entry(key).or_default();
            *reported = (*reported).max(count);
        }
        fresh
    }

    /// Re-sends the turn's still-running tools with a final status.
    fn finish_tools(&mut self, status: &str) {
        let Some(turn) = &self.turn else { return };
        let running: Vec<Value> = turn
            .tools
            .values()
            .filter(|item| item["status"] == "running")
            .cloned()
            .collect();
        for mut item in running {
            item["status"] = status.into();
            item["error"] = if status == "failed" {
                "Antigravity turn failed".into()
            } else {
                Value::Null
            };
            self.item(item);
        }
    }

    /// The CLI could not start: fails a prompt that waited for it.
    fn startup_failed(&mut self, error: &str) {
        warn!(error, "agy did not start");
        self.stop(Stop::Interrupt);
        if let Some(turn) = self.turn.take() {
            self.emit(ProviderEvent::TurnFailed {
                turn: Some(turn.id),
                error: error.to_string(),
                code: None,
                diagnostic: None,
            });
        }
    }

    fn on_exit(&mut self, status: Option<i32>, stderr: &str) {
        let Some(driver) = self.driver.take() else {
            return;
        };
        let reason = status.map_or_else(|| "signal".to_string(), |code| code.to_string());
        let message = diagnostic(&match stderr.trim() {
            "" => format!("Antigravity exited ({reason})"),
            text => text.to_string(),
        });
        match driver.phase {
            Phase::Stopping => {}
            Phase::Starting { .. } => self.startup_failed(&message),
            Phase::Ready if self.turn.as_ref().is_some_and(|t| t.unsent.is_none()) => {
                self.finish_tools("failed");
                let turn = self.turn.take().map(|t| t.id);
                self.emit(ProviderEvent::TurnFailed {
                    turn,
                    error: message,
                    code: Some("PROCESS_EXIT".into()),
                    diagnostic: None,
                });
            }
            Phase::Ready => warn!(%message, "agy exited; it restarts with the next prompt"),
        }
        // A prompt that waited for this process to go starts the next one.
        self.advance();
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_ids_follow_paseo() {
        let id = persistence_id("e5cf1e2d");
        assert_eq!(
            id,
            r#"plugin:{"version":1,"data":{"conversationId":"e5cf1e2d"}}"#
        );
        assert_eq!(conversation_id(&id).as_deref(), Some("e5cf1e2d"));
        assert_eq!(conversation_id("bare-id").as_deref(), Some("bare-id"));
        assert_eq!(conversation_id("plugin:{}"), None);
        assert_eq!(conversation_id(""), None);
    }
}
