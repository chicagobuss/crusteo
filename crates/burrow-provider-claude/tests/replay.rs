//! The Claude adapter against scripted CLI transcripts: deterministic, no
//! process, no model.
//!
//! A fixture is JSONL. Each line is one of:
//! - `{"dir":"out","frame":{...}}`: the CLI sends this;
//! - `{"dir":"in","frame":{...}}`: the adapter must write a frame that
//!   contains this one (`"$ANY"` matches any value);
//! - `{"cmd":{...}}`: the daemon sends a command (`prompt`, `steer`,
//!   `interrupt`, `respond`, `mode`, `thinking`).
//!
//! `"$REQ"` in an `out` frame stands for the `request_id` of the last
//! control request the adapter wrote. Real transcripts recorded with
//! `BURROW_CLAUDE_TRACE` have the same `dir`/`frame` shape.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_provider::{Command, ProviderEvent};
use burrow_provider_claude::process::Output;
use burrow_provider_claude::session::{Launcher, Session, Settings, Transport};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

/// Each launch hands the test a new pair of channels.
struct FakeLauncher {
    launches: mpsc::UnboundedSender<Launch>,
    resumes: Arc<Mutex<Vec<Option<String>>>>,
}

struct Launch {
    written: mpsc::UnboundedReceiver<Value>,
    cli: mpsc::UnboundedSender<Output>,
}

impl Launcher for FakeLauncher {
    fn launch(&mut self, resume: Option<&str>, _: &Settings) -> std::io::Result<Transport> {
        self.resumes
            .lock()
            .unwrap()
            .push(resume.map(str::to_string));
        let (input, written) = mpsc::unbounded_channel();
        let (cli, output) = mpsc::unbounded_channel();
        let _ = self.launches.send(Launch { written, cli });
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

fn substitute(frame: &Value, request: &str) -> Value {
    match frame {
        Value::String(s) if s == "$REQ" => request.into(),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute(v, request)))
                .collect(),
        ),
        Value::Array(list) => list.iter().map(|v| substitute(v, request)).collect(),
        other => other.clone(),
    }
}

fn command(spec: &Value) -> Command {
    let (name, args) = spec.as_object().unwrap().iter().next().unwrap();
    match name.as_str() {
        "prompt" => Command::Prompt {
            turn_id: args["turn"].as_str().unwrap().into(),
            message_id: args["messageId"]
                .as_str()
                .unwrap_or("8c81baa4-28ba-48dc-9b9d-84a5bf4aa31a")
                .into(),
            text: args["text"].as_str().unwrap().into(),
            images: Vec::new(),
        },
        "steer" => Command::Steer {
            message_id: "11111111-2222-3333-4444-555555555555".into(),
            text: args.as_str().unwrap().into(),
            images: Vec::new(),
        },
        "interrupt" => Command::Interrupt,
        "mode" => Command::SetMode(args.as_str().unwrap().into()),
        "thinking" => Command::SetThinking(args.as_str().map(str::to_string)),
        "respond" => Command::RespondPermission {
            id: args["id"].as_str().unwrap().into(),
            response: args["response"].clone(),
        },
        other => panic!("unknown command {other}"),
    }
}

struct Played {
    events: Vec<ProviderEvent>,
    resumes: Vec<Option<String>>,
}

async fn play(fixture: &str, resume: Option<&str>, mode: Option<&str>) -> Played {
    let (launches_tx, mut launches) = mpsc::unbounded_channel();
    let resumes = Arc::new(Mutex::new(Vec::new()));
    let launcher = FakeLauncher {
        launches: launches_tx,
        resumes: resumes.clone(),
    };
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let settings = Settings {
        mode: mode.map(str::to_string),
        ..Settings::default()
    };
    let session = Session::new(launcher, settings, resume.map(str::to_string), events_tx);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(session.run(command_rx));

    let mut launch: Option<Launch> = None;
    let mut last_request = String::new();
    for (number, line) in fixture.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with("//") {
            continue;
        }
        let step: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {number}: {e}"));
        if let Some(spec) = step.get("cmd") {
            commands.send(command(spec)).unwrap();
            continue;
        }
        // A command may relaunch the CLI; take the newest launch.
        while let Ok(next) = launches.try_recv() {
            launch = Some(next);
        }
        if launch.is_none() {
            launch = Some(timeout(WAIT, launches.recv()).await.unwrap().unwrap());
        }
        let current = launch.as_mut().unwrap();
        match step["dir"].as_str() {
            Some("out") => {
                let frame = substitute(&step["frame"], &last_request);
                current.cli.send(Output::Message(frame)).unwrap();
            }
            Some("in") => {
                let expected = &step["frame"];
                let written = timeout(WAIT, async {
                    loop {
                        // Pick up a relaunch while waiting.
                        if let Ok(next) = launches.try_recv() {
                            *current = next;
                        }
                        match tokio::time::timeout(
                            Duration::from_millis(20),
                            current.written.recv(),
                        )
                        .await
                        {
                            Ok(Some(frame)) => return frame,
                            // The adapter relaunched the CLI: follow it.
                            Ok(None) => *current = launches.recv().await.expect("a relaunch"),
                            Err(_) => continue,
                        }
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("line {number}: nothing written; expected {expected}"));
                assert!(
                    contains(&written, expected),
                    "line {number}: wrote {written}\nexpected {expected}"
                );
                if written["type"] == "control_request" {
                    last_request = written["request_id"].as_str().unwrap_or_default().into();
                }
            }
            other => panic!("line {number}: unknown step {other:?}"),
        }
    }
    // Let the adapter finish emitting, then stop it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    commands.send(Command::Shutdown).unwrap();
    timeout(WAIT, task).await.unwrap().unwrap();
    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }
    let resumes = resumes.lock().unwrap().clone();
    Played { events, resumes }
}

fn items(events: &[ProviderEvent]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::Item { item, .. } => Some(item.clone()),
            _ => None,
        })
        .collect()
}

fn text(events: &[ProviderEvent]) -> String {
    items(events)
        .iter()
        .filter(|i| i["type"] == "assistant_message")
        .filter_map(|i| i["text"].as_str())
        .collect()
}

#[tokio::test]
async fn a_text_turn_streams_once() {
    let played = play(include_str!("fixtures/text_turn.jsonl"), None, None).await;
    let e = &played.events;
    assert!(
        matches!(&e[0], ProviderEvent::TurnStarted { turn: Some(t) } if t == "foreground-turn-1")
    );
    assert!(e.iter().any(
        |x| matches!(x, ProviderEvent::SessionStarted { session_id, .. } if session_id == "sess-1")
    ));
    assert_eq!(
        text(e),
        "Hello there.",
        "deltas, not repeated by the full frame"
    );
    let usage = e
        .iter()
        .find_map(|x| match x {
            ProviderEvent::TurnCompleted { usage: Some(u), .. } => Some(u.clone()),
            _ => None,
        })
        .expect("completed");
    assert_eq!(usage["inputTokens"], 12);
    assert_eq!(usage["cachedInputTokens"], 300);
    assert_eq!(usage["outputTokens"], 7);
    assert_eq!(usage["contextWindowMaxTokens"], 200000);
    assert_eq!(usage["contextWindowUsedTokens"], 12 + 300 + 7);
}

#[tokio::test]
async fn a_tool_runs_after_its_permission() {
    let played = play(
        include_str!("fixtures/tool_permission.jsonl"),
        None,
        Some("default"),
    )
    .await;
    let e = &played.events;
    let request = e
        .iter()
        .find_map(|x| match x {
            ProviderEvent::PermissionRequested { id, request } => {
                Some((id.clone(), request.clone()))
            }
            _ => None,
        })
        .expect("a permission request");
    assert_eq!(request.0, "cli-1");
    assert_eq!(request.1["kind"], "tool");
    assert_eq!(
        request.1["detail"],
        json!({"type": "shell", "command": "ls"})
    );
    assert_eq!(request.1["metadata"]["toolUseId"], "toolu_1");
    let calls: Vec<Value> = items(e)
        .into_iter()
        .filter(|i| i["type"] == "tool_call")
        .collect();
    assert_eq!(calls[0]["detail"]["type"], "unknown", "opens as unknown");
    assert_eq!(calls[1]["detail"]["type"], "shell");
    assert_eq!(calls.last().unwrap()["status"], "completed");
    assert_eq!(calls.last().unwrap()["detail"]["output"], "a.txt\nb.txt");
}

#[tokio::test]
async fn a_question_reaches_claude_keyed_by_question_text() {
    let played = play(include_str!("fixtures/question.jsonl"), None, None).await;
    let request = played
        .events
        .iter()
        .find_map(|x| match x {
            ProviderEvent::PermissionRequested { request, .. } => Some(request.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(request["kind"], "question");
    assert_eq!(request["title"], "Which file should I change?");
    assert_eq!(request["input"]["questions"][0]["allowOther"], true);
    // The fixture's `in` line checks what was written back.
    assert_eq!(text(&played.events), "Editing a.txt.");
}

#[tokio::test]
async fn an_interrupt_cancels_and_drops_the_trailing_result() {
    let played = play(include_str!("fixtures/interrupt.jsonl"), None, None).await;
    let e = &played.events;
    let canceled = e
        .iter()
        .position(
            |x| matches!(x, ProviderEvent::TurnCanceled { reason, .. } if reason == "Interrupted"),
        )
        .expect("canceled");
    let tool = items(&e[..canceled])
        .into_iter()
        .rfind(|i| i["type"] == "tool_call")
        .unwrap();
    assert_eq!(tool["status"], "canceled");
    assert!(
        !e.iter().any(|x| matches!(
            x,
            ProviderEvent::TurnFailed { .. } | ProviderEvent::TurnCompleted { .. }
        )),
        "the aborted result after the interrupt opens or closes nothing"
    );
}

#[tokio::test]
async fn the_cli_can_start_a_turn_by_itself() {
    let played = play(
        include_str!("fixtures/autonomous.jsonl"),
        Some("sess-1"),
        None,
    )
    .await;
    let e = &played.events;
    assert!(matches!(e[0], ProviderEvent::TurnStarted { turn: None }));
    assert_eq!(text(e), "The background build finished.");
    assert!(matches!(
        e.iter()
            .rev()
            .find(|x| !matches!(x, ProviderEvent::Exited { .. })),
        Some(ProviderEvent::TurnCompleted { turn: None, .. })
    ));
    assert_eq!(played.resumes, [Some("sess-1".to_string())]);
}

#[tokio::test]
async fn a_lost_session_starts_over_without_resume() {
    let played = play(
        include_str!("fixtures/resume_lost.jsonl"),
        Some("gone"),
        None,
    )
    .await;
    assert!(played
        .events
        .iter()
        .any(|x| matches!(x, ProviderEvent::TurnFailed { error, .. } if error.contains("No conversation found"))));
    assert_eq!(played.resumes, [Some("gone".to_string()), None]);
}

/// Replays a transcript recorded with `BURROW_CLAUDE_TRACE`: the daemon's
/// side (prompts, answers, the interrupt) is rebuilt from its `in` frames,
/// and every `out` frame is what Claude Code really sent.
async fn play_recorded(trace: &str) -> Vec<ProviderEvent> {
    let (launches_tx, mut launches) = mpsc::unbounded_channel();
    let launcher = FakeLauncher {
        launches: launches_tx,
        resumes: Arc::new(Mutex::new(Vec::new())),
    };
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let session = Session::new(launcher, Settings::default(), None, events_tx);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(session.run(command_rx));
    let mut launch = timeout(WAIT, launches.recv()).await.unwrap().unwrap();
    let mut turns = 0;
    for line in trace.lines().filter(|l| !l.starts_with("//")) {
        let record: Value = serde_json::from_str(line).unwrap();
        let frame = &record["frame"];
        if record["dir"] == "out" {
            launch.cli.send(Output::Message(frame.clone())).unwrap();
            continue;
        }
        // The daemon's side: issue what made the adapter write this frame.
        match (frame["type"].as_str(), frame["request"]["subtype"].as_str()) {
            (Some("user"), _) if frame.get("priority").is_none() => {
                turns += 1;
                let text = frame["message"]["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<String>();
                commands
                    .send(Command::Prompt {
                        turn_id: format!("foreground-turn-{turns}"),
                        message_id: frame["uuid"].as_str().unwrap().into(),
                        text,
                        images: Vec::new(),
                    })
                    .unwrap();
            }
            (Some("control_request"), Some("interrupt")) => {
                commands.send(Command::Interrupt).unwrap();
            }
            (Some("control_response"), _) => {
                let body = &frame["response"]["response"];
                let response = if body["behavior"] == "allow" {
                    json!({"behavior": "allow", "updatedInput": body["updatedInput"]})
                } else {
                    json!({"behavior": "deny", "message": body["message"]})
                };
                commands
                    .send(Command::RespondPermission {
                        id: frame["response"]["request_id"].as_str().unwrap().into(),
                        response,
                    })
                    .unwrap();
            }
            _ => {}
        }
        let written = timeout(WAIT, launch.written.recv())
            .await
            .unwrap_or_else(|_| panic!("the adapter did not write {frame}"))
            .unwrap();
        assert_eq!(
            written["type"], frame["type"],
            "wrote {written}\nexpected {frame}"
        );
        if frame["type"] == "control_response" {
            assert_eq!(
                written["response"]["response"], frame["response"]["response"],
                "the answer Claude got"
            );
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    commands.send(Command::Shutdown).unwrap();
    timeout(WAIT, task).await.unwrap().unwrap();
    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn a_recorded_haiku_session_replays_to_the_same_conversation() {
    let events = play_recorded(include_str!("fixtures/real_haiku_session.jsonl")).await;
    let ends: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TurnCompleted { .. } => Some("completed"),
            ProviderEvent::TurnCanceled { .. } => Some("canceled"),
            ProviderEvent::TurnFailed { .. } => Some("failed"),
            _ => None,
        })
        .collect();
    assert_eq!(ends, ["completed", "completed", "completed", "canceled"]);
    let kinds: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::PermissionRequested { request, .. } => {
                Some(request["kind"].as_str().unwrap_or_default().to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["tool", "question"]);
    let all = text(&events);
    assert!(all.starts_with("ready"), "{all}");
    assert!(all.contains("done") && all.contains("blue"), "{all}");
    let bash = items(&events)
        .into_iter()
        .find(|i| i["type"] == "tool_call" && i["status"] == "completed" && i["name"] == "Bash")
        .expect("a completed Bash call");
    assert_eq!(bash["detail"]["command"], "echo smoke > smoke.txt");
}

#[tokio::test]
async fn steer_plan_approval_and_mode_names() {
    let played = play(
        include_str!("fixtures/steer_and_plan.jsonl"),
        None,
        Some("plan"),
    )
    .await;
    let e = &played.events;
    let plan = e
        .iter()
        .find_map(|x| match x {
            ProviderEvent::PermissionRequested { request, .. } => Some(request.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(plan["kind"], "plan");
    assert_eq!(plan["actions"][1]["id"], "implement");
    assert!(
        e.iter().any(
            |x| matches!(x, ProviderEvent::ModeChanged { mode_id } if mode_id == "acceptEdits")
        )
    );
}
