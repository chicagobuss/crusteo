//! A provider that runs no model. It answers each prompt from its text, which
//! makes every path of the agent manager testable for free:
//!
//! | Prompt | Turn |
//! |---|---|
//! | `tool <command>` | a running Bash tool call, a permission request; allowed: completed, denied: failed |
//! | `ask <question>` | a question (`kind: "question"`); the reply quotes the answers |
//! | `slow <text>` | like plain text, but waits `slow_delay` before the last chunk (to steer or interrupt) |
//! | `fail <error>` | the turn fails with that error |
//! | anything else | `echo: <text>` in two chunks |
//!
//! A steer during a turn adds `steered: <text>` to the reply and ends the wait.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    Capabilities, Command, Mode, Model, Provider, ProviderError, ProviderEvent, ProviderInfo,
    Session, SessionSpec, ThinkingOption,
};

pub struct EchoProvider {
    /// Pause between the two chunks of a plain reply.
    pub chunk_delay: Duration,
    /// How long a `slow` turn waits before its last chunk.
    pub slow_delay: Duration,
}

impl Default for EchoProvider {
    fn default() -> Self {
        Self {
            chunk_delay: Duration::from_millis(5),
            slow_delay: Duration::from_secs(30),
        }
    }
}

impl Provider for EchoProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: "echo".into(),
            label: "Echo".into(),
            description: Some("Answers from the prompt text; runs no model".into()),
            modes: vec![
                Mode {
                    id: "default".into(),
                    label: "Always Ask".into(),
                    description: Some("Asks before running tools".into()),
                    icon: Some("Shield".into()),
                    color_tier: Some("safe".into()),
                },
                Mode {
                    id: "bypassPermissions".into(),
                    label: "Bypass".into(),
                    description: Some("Runs tools without asking".into()),
                    icon: Some("ShieldOff".into()),
                    color_tier: Some("dangerous".into()),
                },
            ],
            default_mode_id: Some("default".into()),
            models: vec![Model {
                id: "echo-1".into(),
                label: "Echo".into(),
                description: None,
                is_default: true,
                context_window_max_tokens: None,
                thinking: vec![ThinkingOption {
                    id: "none".into(),
                    label: "None".into(),
                    is_default: true,
                }],
            }],
            capabilities: Capabilities {
                streaming: true,
                session_persistence: true,
                dynamic_modes: true,
                tool_invocations: true,
                steer: true,
                ..Capabilities::default()
            },
        }
    }

    fn start(&self, spec: SessionSpec) -> Result<Session, ProviderError> {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let session_id = spec
            .resume
            .clone()
            .unwrap_or_else(|| format!("echo-session-{}", unique()));
        let worker = Worker {
            events,
            commands: command_rx,
            chunk_delay: self.chunk_delay,
            slow_delay: self.slow_delay,
            bypass: spec.mode_id.as_deref() == Some("bypassPermissions"),
            next_id: 0,
        };
        tokio::spawn(worker.run(session_id));
        Ok(Session {
            commands,
            events: event_rx,
        })
    }
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

struct Worker {
    events: mpsc::UnboundedSender<ProviderEvent>,
    commands: mpsc::UnboundedReceiver<Command>,
    chunk_delay: Duration,
    slow_delay: Duration,
    bypass: bool,
    next_id: u64,
}

/// What interrupted a wait.
enum Wake {
    Elapsed,
    Steer(String),
    Answer(Value),
    Interrupt,
    Shutdown,
}

impl Worker {
    fn emit(&self, event: ProviderEvent) {
        let _ = self.events.send(event);
    }

    fn text(&self, turn: &Option<String>, message_id: &str, text: &str) {
        self.emit(ProviderEvent::Item {
            turn: turn.clone(),
            item: json!({"type": "assistant_message", "text": text, "messageId": message_id}),
        });
    }

    fn id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{}", self.next_id)
    }

    async fn run(mut self, session_id: String) {
        self.emit(ProviderEvent::SessionStarted {
            session_id,
            model: None,
        });
        while let Some(command) = self.commands.recv().await {
            match command {
                Command::Prompt { turn_id, text, .. } => {
                    if !self.turn(Some(turn_id), text).await {
                        break;
                    }
                }
                Command::SetMode(mode) => {
                    self.bypass = mode == "bypassPermissions";
                    self.emit(ProviderEvent::ModeChanged { mode_id: mode });
                }
                Command::Shutdown => break,
                // Nothing running: steer, interrupt and answers have nothing to act on.
                _ => {}
            }
        }
        self.emit(ProviderEvent::Exited { error: None });
    }

    /// Waits for `delay`, or for a command that matters to a running turn.
    async fn wait(&mut self, delay: Option<Duration>) -> Wake {
        loop {
            let next = async {
                match delay {
                    Some(delay) => tokio::time::timeout(delay, self.commands.recv()).await.ok(),
                    None => Some(self.commands.recv().await),
                }
            };
            match next.await {
                None => return Wake::Elapsed,
                Some(None) | Some(Some(Command::Shutdown)) => return Wake::Shutdown,
                Some(Some(Command::Interrupt)) => return Wake::Interrupt,
                Some(Some(Command::Steer { text, .. })) => return Wake::Steer(text),
                Some(Some(Command::RespondPermission { response, .. })) => {
                    return Wake::Answer(response);
                }
                Some(Some(Command::SetMode(mode))) => {
                    self.bypass = mode == "bypassPermissions";
                    self.emit(ProviderEvent::ModeChanged { mode_id: mode });
                }
                Some(Some(_)) => {}
            }
        }
    }

    /// Runs one turn. Returns false when the session should end.
    async fn turn(&mut self, turn: Option<String>, prompt: String) -> bool {
        self.emit(ProviderEvent::TurnStarted { turn: turn.clone() });
        let message_id = self.id("echo-msg");
        let (verb, rest) = prompt.split_once(' ').unwrap_or((prompt.as_str(), ""));
        let rest = rest.to_string();
        let outcome = match verb {
            "tool" => self.tool(&turn, &message_id, &rest).await,
            "ask" => self.ask(&turn, &message_id, &rest).await,
            "fail" => {
                self.emit(ProviderEvent::TurnFailed {
                    turn,
                    error: rest,
                    code: Some("echo_failure".into()),
                    diagnostic: None,
                });
                return true;
            }
            "slow" => {
                self.text(&turn, &message_id, "echo: ");
                match self.wait(Some(self.slow_delay)).await {
                    Wake::Steer(steer) => {
                        self.text(&turn, &message_id, &format!("{rest} steered: {steer}"));
                        Wake::Elapsed
                    }
                    Wake::Elapsed => {
                        self.text(&turn, &message_id, &rest);
                        Wake::Elapsed
                    }
                    other => other,
                }
            }
            _ => {
                self.text(&turn, &message_id, "echo: ");
                match self.wait(Some(self.chunk_delay)).await {
                    Wake::Steer(steer) => {
                        self.text(&turn, &message_id, &format!("{prompt} steered: {steer}"));
                        Wake::Elapsed
                    }
                    Wake::Elapsed => {
                        self.text(&turn, &message_id, &prompt);
                        Wake::Elapsed
                    }
                    other => other,
                }
            }
        };
        match outcome {
            Wake::Interrupt => {
                self.emit(ProviderEvent::TurnCanceled {
                    turn,
                    reason: "Interrupted".into(),
                });
                true
            }
            Wake::Shutdown => false,
            _ => {
                self.emit(ProviderEvent::TurnCompleted {
                    turn,
                    usage: Some(json!({"inputTokens": 1, "outputTokens": 1})),
                });
                true
            }
        }
    }

    async fn tool(&mut self, turn: &Option<String>, message_id: &str, command: &str) -> Wake {
        let call_id = self.id("echo-call");
        let call = |status: &str, output: Option<&str>, error: Value| {
            let mut detail = json!({"type": "shell", "command": command});
            if let Some(output) = output {
                detail["output"] = output.into();
            }
            json!({"type": "tool_call", "callId": call_id, "name": "Bash",
                   "detail": detail, "status": status, "error": error})
        };
        self.emit(ProviderEvent::Item {
            turn: turn.clone(),
            item: call("running", None, Value::Null),
        });
        let answer = if self.bypass {
            json!({"behavior": "allow"})
        } else {
            let permission = self.id("echo-permission");
            self.emit(ProviderEvent::PermissionRequested {
                id: permission,
                request: json!({
                    "provider": "echo", "name": "Bash", "kind": "tool",
                    "title": "Run a command", "input": {"command": command},
                    "detail": {"type": "shell", "command": command},
                }),
            });
            match self.wait(None).await {
                Wake::Answer(answer) => answer,
                Wake::Interrupt => {
                    self.emit(ProviderEvent::Item {
                        turn: turn.clone(),
                        item: call("canceled", None, Value::Null),
                    });
                    return Wake::Interrupt;
                }
                other => return other,
            }
        };
        if answer["behavior"] == "allow" {
            let output = format!("ran: {command}");
            self.emit(ProviderEvent::Item {
                turn: turn.clone(),
                item: call("completed", Some(&output), Value::Null),
            });
            self.text(turn, message_id, "done");
        } else {
            let message = answer["message"].as_str().unwrap_or("Denied").to_string();
            self.emit(ProviderEvent::Item {
                turn: turn.clone(),
                item: call("failed", None, json!({"message": message})),
            });
            self.text(turn, message_id, &format!("denied: {message}"));
        }
        Wake::Elapsed
    }

    async fn ask(&mut self, turn: &Option<String>, message_id: &str, question: &str) -> Wake {
        let permission = self.id("echo-question");
        self.emit(ProviderEvent::PermissionRequested {
            id: permission,
            request: json!({
                "provider": "echo", "name": "AskUserQuestion", "kind": "question",
                "title": "Question",
                "input": {"questions": [{
                    "question": question, "header": "Answer", "multiSelect": false,
                    "options": [{"label": "Yes"}, {"label": "No"}],
                }]},
            }),
        });
        match self.wait(None).await {
            Wake::Answer(answer) if answer["behavior"] == "allow" => {
                let answers = answer["updatedInput"]["answers"].clone();
                self.text(turn, message_id, &format!("answered: {answers}"));
                Wake::Elapsed
            }
            Wake::Answer(_) => {
                self.text(turn, message_id, "dismissed");
                Wake::Elapsed
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn until_turn_end(session: &mut Session) -> Vec<ProviderEvent> {
        let mut seen = Vec::new();
        while let Some(event) = session.events.recv().await {
            let done = matches!(
                event,
                ProviderEvent::TurnCompleted { .. }
                    | ProviderEvent::TurnCanceled { .. }
                    | ProviderEvent::TurnFailed { .. }
            );
            seen.push(event);
            if done {
                break;
            }
        }
        seen
    }

    async fn next_permission(session: &mut Session) -> (String, Value) {
        while let Some(event) = session.events.recv().await {
            if let ProviderEvent::PermissionRequested { id, request } = event {
                return (id, request);
            }
        }
        panic!("no permission request");
    }

    fn prompt(turn: &str, text: &str) -> Command {
        Command::Prompt {
            turn_id: turn.into(),
            message_id: "m".into(),
            text: text.into(),
            images: Vec::new(),
        }
    }

    fn text_of(events: &[ProviderEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::Item { item, .. } if item["type"] == "assistant_message" => {
                    item["text"].as_str().map(str::to_string)
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn echoes_a_prompt_as_one_turn() {
        let mut session = EchoProvider::default()
            .start(SessionSpec::default())
            .unwrap();
        session
            .commands
            .send(prompt("foreground-turn-1", "hi"))
            .unwrap();
        let events = until_turn_end(&mut session).await;
        assert!(matches!(events[0], ProviderEvent::SessionStarted { .. }));
        assert_eq!(
            events[1],
            ProviderEvent::TurnStarted {
                turn: Some("foreground-turn-1".into())
            }
        );
        assert_eq!(text_of(&events), "echo: hi");
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::TurnCompleted { .. })
        ));
    }

    #[tokio::test]
    async fn a_tool_waits_for_permission() {
        let mut session = EchoProvider::default()
            .start(SessionSpec::default())
            .unwrap();
        session.commands.send(prompt("t1", "tool ls")).unwrap();
        let (id, request) = next_permission(&mut session).await;
        assert_eq!(request["kind"], "tool");
        session
            .commands
            .send(Command::RespondPermission {
                id,
                response: json!({"behavior": "deny", "message": "no"}),
            })
            .unwrap();
        let events = until_turn_end(&mut session).await;
        assert_eq!(text_of(&events), "denied: no");
    }

    #[tokio::test]
    async fn steer_and_interrupt_reach_a_slow_turn() {
        let mut session = EchoProvider::default()
            .start(SessionSpec::default())
            .unwrap();
        session.commands.send(prompt("t1", "slow a")).unwrap();
        session
            .commands
            .send(Command::Steer {
                message_id: "m2".into(),
                text: "b".into(),
                images: Vec::new(),
            })
            .unwrap();
        let events = until_turn_end(&mut session).await;
        assert_eq!(text_of(&events), "echo: a steered: b");

        session.commands.send(prompt("t2", "slow c")).unwrap();
        session.commands.send(Command::Interrupt).unwrap();
        let events = until_turn_end(&mut session).await;
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::TurnCanceled { reason, .. }) if reason == "Interrupted"
        ));
    }
}
