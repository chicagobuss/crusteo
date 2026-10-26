//! The ACP session against scripted agent transcripts: deterministic, no
//! process, no model.
//!
//! A fixture is JSONL. Each line is one of:
//! - `{"dir":"out","frame":{...}}`: the agent sends this;
//! - `{"dir":"in","frame":{...}}`: the session must write a frame that
//!   contains this one (`"$ANY"` matches any value);
//! - `{"cmd":{...}}`: the daemon sends a command (`prompt`, `interrupt`,
//!   `respond`, `mode`, `model`, `thinking`); `respond` answers the last
//!   permission request;
//! - `{"exit":{"code":1,"stderr":"..."}}`: the agent process ends.
//!
//! `"$REQ"` in an `out` frame stands for the id of the last request the
//! session wrote. Recordings made with `BURROW_ACP_TRACE` have the same
//! `dir`/`frame` shape.

use std::time::Duration;

use burrow_provider::{Command, ProviderEvent};
use burrow_provider_acp::process::{Output, Transport};
use burrow_provider_acp::session::{Launcher, Session, Settings};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

struct FakeLauncher {
    launches: mpsc::UnboundedSender<Launch>,
}

struct Launch {
    written: mpsc::UnboundedReceiver<Value>,
    agent: mpsc::UnboundedSender<Output>,
}

impl Launcher for FakeLauncher {
    fn launch(&mut self) -> std::io::Result<Transport> {
        let (input, written) = mpsc::unbounded_channel();
        let (agent, output) = mpsc::unbounded_channel();
        let _ = self.launches.send(Launch { written, agent });
        Ok(Transport { input, output })
    }
}

fn contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (_, Value::String(s)) if s == "$ANY" => true,
        (Value::Object(a), Value::Object(e)) => e
            .iter()
            .all(|(k, v)| a.get(k).is_some_and(|av| contains(av, v))),
        (Value::Array(a), Value::Array(e)) => {
            a.len() == e.len() && a.iter().zip(e).all(|(av, ev)| contains(av, ev))
        }
        _ => actual == expected,
    }
}

fn substitute(frame: &Value, request: &Value) -> Value {
    match frame {
        Value::String(s) if s == "$REQ" => request.clone(),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute(v, request)))
                .collect(),
        ),
        Value::Array(list) => list.iter().map(|v| substitute(v, request)).collect(),
        other => other.clone(),
    }
}

fn settings() -> Settings {
    Settings {
        provider: "cursor".into(),
        permission_provider: "acp".into(),
        cwd: "/home/user/project".into(),
        capability_meta: Some(json!({"parameterizedModelPicker": true})),
        ..Settings::default()
    }
}

struct Played {
    events: Vec<ProviderEvent>,
}

impl Played {
    fn items(&self) -> Vec<&Value> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::Item { item, .. } => Some(item),
                _ => None,
            })
            .collect()
    }

    fn of_type(&self, kind: &str) -> Vec<&Value> {
        self.items()
            .into_iter()
            .filter(|item| item["type"] == kind)
            .collect()
    }

    fn runtime(&self) -> Vec<&Value> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::RuntimeInfo { info } => Some(info),
                _ => None,
            })
            .collect()
    }
}

/// Drains events into `seen` until `stop` matches one (or the wait ends).
async fn events_until(
    events: &mut mpsc::UnboundedReceiver<ProviderEvent>,
    seen: &mut Vec<ProviderEvent>,
    stop: impl Fn(&ProviderEvent) -> bool,
) {
    while let Ok(Some(event)) = timeout(WAIT, events.recv()).await {
        let done = stop(&event);
        seen.push(event);
        if done {
            return;
        }
    }
    panic!("the awaited event never came; saw {seen:#?}");
}

async fn play(fixture: &str, settings: Settings) -> Played {
    let (launches_tx, mut launches) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let session = Session::new(
        FakeLauncher {
            launches: launches_tx,
        },
        settings,
        events_tx,
    );
    tokio::spawn(session.run(command_rx));
    let mut launch = timeout(WAIT, launches.recv()).await.unwrap().unwrap();
    let mut seen = Vec::new();
    let mut last_request = Value::Null;
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/{fixture}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let step: Value = serde_json::from_str(line).unwrap();
        if let Some(cmd) = step.get("cmd") {
            let (name, args) = cmd.as_object().unwrap().iter().next().unwrap();
            let command = match name.as_str() {
                "prompt" => Command::Prompt {
                    turn_id: args["turn"].as_str().unwrap().into(),
                    message_id: args["messageId"].as_str().unwrap_or("msg-1").into(),
                    text: args["text"].as_str().unwrap().into(),
                    images: Vec::new(),
                },
                "interrupt" => Command::Interrupt,
                "mode" => Command::SetMode(args.as_str().unwrap().into()),
                "model" => Command::SetModel(args.as_str().map(str::to_string)),
                "thinking" => Command::SetThinking(args.as_str().map(str::to_string)),
                "respond" => {
                    events_until(&mut events, &mut seen, |e| {
                        matches!(e, ProviderEvent::PermissionRequested { .. })
                    })
                    .await;
                    let id = seen
                        .iter()
                        .rev()
                        .find_map(|e| match e {
                            ProviderEvent::PermissionRequested { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .unwrap();
                    Command::RespondPermission {
                        id,
                        response: args.clone(),
                    }
                }
                other => panic!("unknown command {other}"),
            };
            commands.send(command).unwrap();
        } else if let Some(exit) = step.get("exit") {
            launch
                .agent
                .send(Output::Exited {
                    code: exit["code"].as_i64().map(|c| c as i32),
                    signal: None,
                    stderr: exit["stderr"].as_str().unwrap_or_default().into(),
                })
                .unwrap();
        } else if step["dir"] == "out" {
            let frame = substitute(&step["frame"], &last_request);
            launch.agent.send(Output::Message(frame)).unwrap();
        } else if step["dir"] == "in" {
            let expected = &step["frame"];
            let written = timeout(WAIT, launch.written.recv())
                .await
                .unwrap_or_else(|_| panic!("line {}: nothing written, expected {expected}", n + 1))
                .unwrap();
            assert!(
                contains(&written, expected),
                "line {}: wrote {written}\nexpected {expected}",
                n + 1
            );
            if written.get("method").is_some()
                && let Some(id) = written.get("id")
            {
                last_request = id.clone();
            }
        }
    }
    let _ = commands.send(Command::Shutdown);
    events_until(&mut events, &mut seen, |e| {
        matches!(e, ProviderEvent::Exited { .. })
    })
    .await;
    Played { events: seen }
}

#[tokio::test]
async fn a_text_turn_waits_for_the_model_and_streams() {
    let played = play(
        "text_turn.jsonl",
        Settings {
            model: Some("gpt-5-mini".into()),
            ..settings()
        },
    )
    .await;
    let events = &played.events;
    assert_eq!(
        events[0],
        ProviderEvent::SessionStarted {
            session_id: "sess-1".into(),
            model: Some("grok-4.7".into())
        }
    );
    match &events[1] {
        ProviderEvent::ModesAvailable { modes } => {
            let ids: Vec<&str> = modes.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, ["agent", "plan", "ask"]);
            assert_eq!(modes[2].label, "Ask");
        }
        other => panic!("expected modes, got {other:?}"),
    }
    assert_eq!(
        events[2],
        ProviderEvent::ModeChanged {
            mode_id: "agent".into()
        }
    );
    // Applied before the prompt went out.
    let runtime = played.runtime();
    assert_eq!(runtime[0]["model"], "grok-4.7");
    assert_eq!(
        runtime[1],
        &json!({"provider": "cursor", "sessionId": "sess-1", "model": "gpt-5-mini",
                "thinkingOptionId": "high", "modeId": "agent",
                "extra": {"title": null, "updatedAt": null}})
    );
    assert_eq!(runtime.last().unwrap()["extra"]["title"], "Word Reply");
    assert!(
        played.of_type("user_message").is_empty(),
        "the echo is dropped"
    );
    assert_eq!(played.of_type("reasoning")[0]["text"], "Thinking it over");
    let text = played.of_type("assistant_message");
    assert_eq!(text.len(), 2);
    assert_eq!(text[0]["messageId"], text[1]["messageId"], "one message");
    assert!(text[0]["messageId"].as_str().unwrap().len() >= 32);
    assert!(events.contains(&ProviderEvent::UsageUpdated {
        usage: json!({"contextWindowMaxTokens": 200000, "contextWindowUsedTokens": 1200})
    }));
    assert!(events.contains(&ProviderEvent::TurnCompleted {
        turn: Some("t1".into()),
        usage: Some(json!({"inputTokens": 10, "outputTokens": 2, "cachedInputTokens": 1})),
    }));
    let turn_items = events
        .iter()
        .filter(|e| matches!(e, ProviderEvent::Item { turn, .. } if turn.as_deref() == Some("t1")));
    assert_eq!(turn_items.count(), 3);
}

#[tokio::test]
async fn a_tool_asks_and_runs() {
    let played = play("tool_permission.jsonl", settings()).await;
    let request = played
        .events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::PermissionRequested { id, request } => Some((id, request)),
            _ => None,
        })
        .unwrap();
    let (id, request) = request;
    assert_eq!(request["id"], id.as_str());
    assert_eq!(request["provider"], "acp");
    assert_eq!(request["kind"], "tool");
    assert_eq!(request["name"], "execute");
    assert_eq!(request["title"], "`echo smoke > smoke.txt`");
    assert_eq!(
        request["detail"],
        json!({"type": "shell", "command": "echo smoke > smoke.txt"})
    );
    assert_eq!(
        request["actions"],
        json!([
            {"id": "allow-once", "label": "Allow once", "behavior": "allow"},
            {"id": "allow-always", "label": "Allow always", "behavior": "allow"},
            {"id": "reject-once", "label": "Reject", "behavior": "deny"},
        ])
    );
    assert_eq!(request["metadata"]["toolCallId"], "call-1");
    let calls = played.of_type("tool_call");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["status"], "running");
    assert_eq!(
        calls[1],
        &json!({"type": "tool_call", "callId": "call-1", "name": "execute",
                "detail": {"type": "shell", "command": "echo smoke > smoke.txt", "exitCode": 0},
                "metadata": {"kind": "execute", "title": "`echo smoke > smoke.txt`"},
                "status": "completed", "error": null})
    );
    assert_eq!(
        played.of_type("todo")[0],
        &json!({"type": "todo", "items": [
            {"text": "Write the file", "completed": true},
            {"text": "Say done", "completed": false}]})
    );
    assert!(played.events.contains(&ProviderEvent::ModeChanged {
        mode_id: "plan".into()
    }));
}

#[tokio::test]
async fn an_interrupt_cancels_the_prompt_and_its_tools() {
    let played = play("cancel.jsonl", settings()).await;
    let calls = played.of_type("tool_call");
    assert_eq!(calls.last().unwrap()["status"], "canceled");
    assert!(calls.last().unwrap()["error"].is_null());
    assert!(played.events.contains(&ProviderEvent::TurnCanceled {
        turn: Some("t1".into()),
        reason: "Interrupted".into()
    }));
}

#[tokio::test]
async fn a_resumed_session_loads_without_replaying_history() {
    let played = play(
        "resume.jsonl",
        Settings {
            resume: Some("sess-old".into()),
            ..settings()
        },
    )
    .await;
    assert_eq!(
        played.events[0],
        ProviderEvent::SessionStarted {
            session_id: "sess-old".into(),
            model: Some("gpt-5-mini".into())
        }
    );
    // Only the new turn shows; the replayed one is Burrow's already.
    let text = played.of_type("assistant_message");
    assert_eq!(text.len(), 1, "{:#?}", played.events);
    assert_eq!(text[0]["text"], "ready");
    assert!(played.of_type("tool_call").is_empty());
    assert!(played.of_type("user_message").is_empty());
}

#[tokio::test]
async fn a_lost_session_carries_on_in_a_new_one() {
    let played = play(
        "resume_lost.jsonl",
        Settings {
            resume: Some("sess-gone".into()),
            ..settings()
        },
    )
    .await;
    assert_eq!(
        played.events[0],
        ProviderEvent::SessionStarted {
            session_id: "sess-2".into(),
            model: None
        }
    );
}

#[tokio::test]
async fn settings_apply_in_order_and_thinking_uses_its_option() {
    let played = play(
        "setup.jsonl",
        Settings {
            mode: Some("plan".into()),
            thinking: Some("low".into()),
            ..settings()
        },
    )
    .await;
    let runtime = played.runtime();
    let last = runtime.last().unwrap();
    assert_eq!(last["modeId"], "plan");
    assert_eq!(last["thinkingOptionId"], "low");
    assert!(played.events.contains(&ProviderEvent::ModeChanged {
        mode_id: "plan".into()
    }));
}

#[tokio::test]
async fn an_exit_fails_the_running_turn() {
    let played = play("exit.jsonl", settings()).await;
    assert!(played.events.contains(&ProviderEvent::TurnFailed {
        turn: Some("t1".into()),
        error: "ACP agent exited unexpectedly (1)".into(),
        code: None,
        diagnostic: Some("panic: boom".into()),
    }));
    assert_eq!(
        played.events.last(),
        Some(&ProviderEvent::Exited {
            error: Some("ACP agent exited unexpectedly (1)".into())
        })
    );
}

/// Recorded from `cursor-agent` 2026.10.01 on GPT-5 Mini through the smoke
/// driver (a plain turn, an approved shell command, an interrupted turn),
/// then scrubbed.
#[tokio::test]
async fn a_recorded_cursor_session_replays() {
    let played = play(
        "real_cursor_session.jsonl",
        Settings {
            cwd: "/home/user/project".into(),
            model: Some("gpt-5-mini".into()),
            ..settings()
        },
    )
    .await;
    let text: Vec<&str> = played
        .of_type("assistant_message")
        .iter()
        .map(|item| item["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, ["ready", "done", "1", "\n"]);
    let calls = played.of_type("tool_call");
    assert_eq!(
        calls.last().unwrap(),
        &&json!({"type": "tool_call",
            "callId": "call_lRXi2UrfXNRZ6qGOyv9KQFaR\nfc_0c43f07092218029016ad6b2dffd9c87d1b6d22a3dca1ec652",
            "name": "execute",
            "detail": {"type": "shell", "command": "echo smoke > smoke.txt", "exitCode": 0},
            "metadata": {"kind": "execute", "title": "`echo smoke > smoke.txt`"},
            "status": "completed", "error": null})
    );
    let request = played
        .events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::PermissionRequested { request, .. } => Some(request),
            _ => None,
        })
        .unwrap();
    // The stored call, not the request's "Not in allowlist" note, is shown.
    assert_eq!(
        request["detail"],
        json!({"type": "shell", "command": "echo smoke > smoke.txt"})
    );
    let ends: Vec<&ProviderEvent> = played
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                ProviderEvent::TurnCompleted { .. } | ProviderEvent::TurnCanceled { .. }
            )
        })
        .collect();
    assert_eq!(ends.len(), 3);
    assert!(matches!(ends[2], ProviderEvent::TurnCanceled { .. }));
    assert_eq!(
        played.runtime().last().unwrap()["extra"]["title"],
        "Word Reply"
    );
}

/// The same session after a daemon restart: loaded, its history not shown
/// again, and the model already the configured one.
#[tokio::test]
async fn a_recorded_cursor_resume_replays() {
    let played = play(
        "real_cursor_resume.jsonl",
        Settings {
            cwd: "/home/user/project".into(),
            model: Some("gpt-5-mini".into()),
            resume: Some("d5fc846f-c811-41e9-a384-8e5b5b7bfcd8".into()),
            ..settings()
        },
    )
    .await;
    let text: Vec<&Value> = played
        .of_type("assistant_message")
        .iter()
        .map(|item| &item["text"])
        .collect();
    assert_eq!(text, [&json!("ready")]);
    assert!(played.of_type("tool_call").is_empty());
    assert!(matches!(
        &played.events[0],
        ProviderEvent::SessionStarted { session_id, model }
            if session_id == "d5fc846f-c811-41e9-a384-8e5b5b7bfcd8"
                && model.as_deref() == Some("gpt-5-mini")
    ));
}

/// The catalog probe: a session's modes, and Cursor's own model list with
/// each model's thinking options; what the agent asks meanwhile is refused.
#[tokio::test]
async fn the_cursor_probe_reads_modes_and_models() {
    let (input, mut written) = mpsc::unbounded_channel::<Value>();
    let (agent, output) = mpsc::unbounded_channel();
    let refused = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = refused.clone();
    tokio::spawn(async move {
        while let Some(frame) = written.recv().await {
            if frame.get("method").is_none() {
                seen.lock().unwrap().push(frame);
                continue;
            }
            let result = match frame["method"].as_str().unwrap() {
                "initialize" => json!({"agentCapabilities": {"loadSession": true}}),
                "session/new" => {
                    let ask = json!({"jsonrpc": "2.0", "id": 90, "method": "fs/read_text_file",
                                     "params": {"path": "/home/user/x"}});
                    agent.send(Output::Message(ask)).unwrap();
                    json!({"sessionId": "probe-1",
                           "modes": {"currentModeId": "agent", "availableModes": [
                               {"id": "agent", "name": "Agent"}, {"id": "ask", "name": "Ask"}]},
                           "models": {"currentModelId": "grok-4.7", "availableModels": [
                               {"modelId": "default", "name": "Auto"},
                               {"modelId": "grok-4.7", "name": "Grok 4.7"}]}})
                }
                "cursor/list_available_models" => json!({"models": [
                    {"value": "default", "name": "Auto", "configOptions": []},
                    {"value": "grok-4.7", "name": "Grok 4.7", "configOptions": [
                        {"id": "reasoning_effort", "category": "thought_level", "type": "select",
                         "currentValue": "high", "options": [
                            {"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]}]},
                ]}),
                other => panic!("unexpected {other}"),
            };
            let answer = json!({"jsonrpc": "2.0", "id": frame["id"], "result": result});
            agent.send(Output::Message(answer)).unwrap();
        }
    });
    let catalog = burrow_provider_acp::probe::run(
        Transport { input, output },
        "/home/user",
        Some(&json!({"parameterizedModelPicker": true})),
        burrow_provider_acp::Models::CursorExtension,
    )
    .await
    .unwrap();
    let modes: Vec<&str> = catalog.modes.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(modes, ["agent", "ask"]);
    assert_eq!(catalog.models.len(), 2);
    assert!(!catalog.models[0].is_default && catalog.models[1].is_default);
    assert_eq!(catalog.models[1].thinking.len(), 2);
    assert!(catalog.models[1].thinking[1].is_default);
    let refused = refused.lock().unwrap();
    assert_eq!(refused[0]["id"], 90);
    assert_eq!(refused[0]["error"]["code"], -32601);
}

#[tokio::test]
async fn a_failed_prompt_reports_the_agent_error() {
    let played = play("prompt_error.jsonl", settings()).await;
    let failed = played.events.iter().find_map(|e| match e {
        ProviderEvent::TurnFailed {
            error,
            code,
            diagnostic,
            ..
        } => Some((error, code, diagnostic)),
        _ => None,
    });
    let (error, code, diagnostic) = failed.unwrap();
    assert_eq!(error, "Internal error: Rate limited");
    assert_eq!(code.as_deref(), Some("-32603"));
    assert!(diagnostic.as_deref().unwrap().contains("code=-32603"));
}
