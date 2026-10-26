//! The pi adapter against scripted RPC transcripts: deterministic, no
//! process, no model.
//!
//! A fixture is JSONL. Each line is one of:
//! - `{"dir":"out","frame":{...}}`: pi sends this;
//! - `{"dir":"in","frame":{...}}`: the adapter must write a frame that
//!   contains this one (`"$ANY"` matches any value);
//! - `{"exit":{"status":1,"stderr":"..."}}`: pi exits;
//! - `{"cmd":{...}}`: the daemon sends a command (`prompt`, `steer`,
//!   `interrupt`, `respond`, `model`, `thinking`).
//!
//! `"$REQ"` in an `out` frame stands for the `id` of the last command the
//! adapter wrote. Real transcripts recorded with `BURROW_PI_TRACE` have the
//! same `dir`/`frame` shape.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_provider::{Command, ProviderEvent};
use burrow_provider_pi::process::Output;
use burrow_provider_pi::session::{Launcher, Session, Settings, Transport};
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
    pi: mpsc::UnboundedSender<Output>,
}

impl Launcher for FakeLauncher {
    fn launch(&mut self, resume: Option<&str>, _: &Settings) -> std::io::Result<Transport> {
        self.resumes
            .lock()
            .unwrap()
            .push(resume.map(str::to_string));
        let (input, written) = mpsc::unbounded_channel();
        let (pi, output) = mpsc::unbounded_channel();
        let _ = self.launches.send(Launch { written, pi });
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
            message_id: "m1".into(),
            text: args["text"].as_str().unwrap().into(),
            images: Vec::new(),
        },
        "steer" => Command::Steer {
            message_id: "m2".into(),
            text: args.as_str().unwrap().into(),
            images: Vec::new(),
        },
        "interrupt" => Command::Interrupt,
        "model" => Command::SetModel(args.as_str().map(str::to_string)),
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

async fn play(fixture: &str, resume: Option<&str>, settings: Settings) -> Played {
    let (launches_tx, mut launches) = mpsc::unbounded_channel();
    let resumes = Arc::new(Mutex::new(Vec::new()));
    let launcher = FakeLauncher {
        launches: launches_tx,
        resumes: resumes.clone(),
    };
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let session = Session::new(launcher, settings, resume.map(str::to_string), events_tx);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(session.run(command_rx));

    let mut launch: Option<Launch> = None;
    let mut last_request = String::new();
    for (number, line) in fixture.lines().enumerate() {
        let number = number + 1;
        if line.trim().is_empty() || line.starts_with("//") {
            continue;
        }
        let step: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {number}: {e}"));
        if let Some(spec) = step.get("cmd") {
            commands.send(command(spec)).unwrap();
            continue;
        }
        // A command or an exit may relaunch pi; take the newest launch.
        while let Ok(next) = launches.try_recv() {
            launch = Some(next);
        }
        if launch.is_none() {
            launch = Some(timeout(WAIT, launches.recv()).await.unwrap().unwrap());
        }
        let current = launch.as_mut().unwrap();
        if let Some(exit) = step.get("exit") {
            current
                .pi
                .send(Output::Exited {
                    status: exit["status"].as_i64().map(|s| s as i32),
                    stderr: exit["stderr"].as_str().unwrap_or_default().into(),
                })
                .unwrap();
            continue;
        }
        match step["dir"].as_str() {
            Some("out") => {
                let frame = substitute(&step["frame"], &last_request);
                current.pi.send(Output::Message(frame)).unwrap();
            }
            Some("in") => {
                let expected = &step["frame"];
                let written = timeout(WAIT, async {
                    loop {
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
                            // The adapter relaunched pi: follow it.
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
                if let Some(id) = written["id"].as_str() {
                    last_request = id.into();
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

fn ends(events: &[ProviderEvent]) -> Vec<&'static str> {
    events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TurnCompleted { .. } => Some("completed"),
            ProviderEvent::TurnCanceled { .. } => Some("canceled"),
            ProviderEvent::TurnFailed { .. } => Some("failed"),
            _ => None,
        })
        .collect()
}

fn permission(events: &[ProviderEvent]) -> (String, Value) {
    events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::PermissionRequested { id, request } => {
                Some((id.clone(), request.clone()))
            }
            _ => None,
        })
        .expect("a permission request")
}

#[tokio::test]
async fn a_text_turn_streams_and_reports_usage() {
    let played = play(
        include_str!("fixtures/text_turn.jsonl"),
        None,
        Settings::default(),
    )
    .await;
    let e = &played.events;
    let started: Vec<&ProviderEvent> = e
        .iter()
        .filter(|x| matches!(x, ProviderEvent::SessionStarted { .. }))
        .collect();
    assert!(
        matches!(
            started[..],
            [ProviderEvent::SessionStarted { session_id, model: Some(model) }]
                if session_id == "sess-1" && model == "google/gemini-2.5-flash-lite"
        ),
        "the thread is reported once, when the run starts: {started:?}"
    );
    assert!(e.iter().any(
        |x| matches!(x, ProviderEvent::TurnStarted { turn: Some(t) } if t == "foreground-turn-1")
    ));
    assert_eq!(text(e), "Hello there.");
    let ids: Vec<Value> = items(e).iter().map(|i| i["messageId"].clone()).collect();
    assert!(
        ids[0].is_string() && ids.iter().all(|id| *id == ids[0]),
        "one message id for the streamed message"
    );
    let usage = e
        .iter()
        .find_map(|x| match x {
            ProviderEvent::TurnCompleted { usage: Some(u), .. } => Some(u.clone()),
            _ => None,
        })
        .expect("completed with usage");
    assert_eq!(
        usage,
        json!({"inputTokens": 3240, "cachedInputTokens": 0, "outputTokens": 3,
               "totalCostUsd": 0.000334, "contextWindowMaxTokens": 1048576,
               "contextWindowUsedTokens": 3243})
    );
    assert_eq!(ends(e), ["completed"]);
}

#[tokio::test]
async fn a_tool_runs_and_finishes() {
    let played = play(
        include_str!("fixtures/tool_turn.jsonl"),
        None,
        Settings::default(),
    )
    .await;
    let calls: Vec<Value> = items(&played.events)
        .into_iter()
        .filter(|i| i["callId"] == "bash_1")
        .collect();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[0],
        json!({"type": "tool_call", "callId": "bash_1", "name": "bash",
               "detail": {"type": "shell", "command": "ls"}, "status": "running", "error": null})
    );
    assert_eq!(calls[1]["detail"]["output"], "a.txt\n");
    assert_eq!(calls[1]["status"], "running");
    assert_eq!(calls[2]["status"], "completed");
    assert_eq!(
        calls[2]["detail"],
        json!({"type": "shell", "command": "ls", "output": "a.txt\nb.txt\n", "exitCode": null})
    );
    let failed = items(&played.events)
        .into_iter()
        .find(|i| i["status"] == "failed")
        .expect("a failed read");
    assert_eq!(failed["detail"]["type"], "read");
    assert_eq!(failed["error"]["content"][0]["text"], "ENOENT");
    assert_eq!(text(&played.events), "Two files.");
    assert_eq!(ends(&played.events), ["completed"]);
}

#[tokio::test]
async fn an_extension_dialog_is_a_question() {
    let played = play(
        include_str!("fixtures/question.jsonl"),
        None,
        Settings::default(),
    )
    .await;
    let (id, request) = permission(&played.events);
    assert_eq!(id, "ui-1");
    assert_eq!(
        request,
        json!({"provider": "pi", "name": "Pi select", "kind": "question",
               "title": "Which color?",
               "input": {"questions": [{"question": "Which color?", "header": "Response",
                   "options": [{"label": "red"}, {"label": "blue"}], "multiSelect": false}]},
               "metadata": {"extensionUiMethod": "select", "answerHeader": "Response"}})
    );
    let requests: Vec<Value> = played
        .events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::PermissionRequested { request, .. } => Some(request.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(requests[1]["title"], "Delete it?\n\nThere is no undo.");
    assert_eq!(requests[1]["name"], "Pi confirm");
    // The fixture's `in` lines check what was written back.
    let notice = items(&played.events)
        .into_iter()
        .find(|i| i["type"] == "notification")
        .expect("a notification");
    assert_eq!(
        notice,
        json!({"type": "notification", "level": "warning", "message": "Low disk"})
    );
    assert_eq!(text(&played.events), "blue it is");
}

#[tokio::test]
async fn an_interrupt_clears_the_queue_aborts_and_cancels() {
    let played = play(
        include_str!("fixtures/interrupt.jsonl"),
        None,
        Settings::default(),
    )
    .await;
    let e = &played.events;
    assert_eq!(
        ends(e),
        ["canceled"],
        "the aborted run is not also a failure"
    );
    assert!(e.iter().any(
        |x| matches!(x, ProviderEvent::TurnCanceled { reason, .. } if reason == "interrupted")
    ));
    let tool = items(e)
        .into_iter()
        .rfind(|i| i["type"] == "tool_call")
        .unwrap();
    assert_eq!(tool["status"], "canceled");
}

#[tokio::test]
async fn a_failed_run_reports_the_providers_error() {
    let played = play(
        include_str!("fixtures/failure.jsonl"),
        None,
        Settings::default(),
    )
    .await;
    let error = played
        .events
        .iter()
        .find_map(|x| match x {
            ProviderEvent::TurnFailed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .expect("failed");
    assert_eq!(
        error,
        "429 quota exceeded (stopReason=error, model=google/gemini-2.5-flash-lite)"
    );
    assert!(
        played.events.iter().any(|x| matches!(
            x,
            ProviderEvent::Item { item, .. } if item["type"] == "error"
                && item["message"] == "Provider retry (attempt 1): 429 quota exceeded"
        )),
        "the retry shows"
    );
}

#[tokio::test]
async fn a_lost_session_starts_over_and_keeps_the_prompt() {
    let played = play(
        include_str!("fixtures/resume_lost.jsonl"),
        Some("gone"),
        Settings::default(),
    )
    .await;
    assert_eq!(played.resumes, [Some("gone".to_string()), None]);
    assert!(played.events.iter().any(
        |x| matches!(x, ProviderEvent::SessionStarted { session_id, .. } if session_id == "sess-2")
    ));
    assert_eq!(text(&played.events), "fresh");
    assert_eq!(ends(&played.events), ["completed"]);
}

#[tokio::test]
async fn a_resumed_session_switches_to_the_wanted_model() {
    let settings = Settings {
        model: Some("google/gemini-3.1-flash-lite".into()),
        ..Settings::default()
    };
    let played = play(
        include_str!("fixtures/resume_model.jsonl"),
        Some("sess-1"),
        settings,
    )
    .await;
    // The fixture's `in` lines check the switch comes before the prompt.
    assert_eq!(played.resumes, [Some("sess-1".to_string())]);
    assert_eq!(ends(&played.events), ["completed"]);
}

#[tokio::test]
async fn pi_can_start_a_run_by_itself() {
    let played = play(
        include_str!("fixtures/autonomous.jsonl"),
        Some("sess-1"),
        Settings::default(),
    )
    .await;
    let e = &played.events;
    assert!(
        e.iter()
            .any(|x| matches!(x, ProviderEvent::TurnStarted { turn: None }))
    );
    let custom = items(e)
        .into_iter()
        .find(|i| i["type"] == "tool_call")
        .expect("the extension's message");
    assert_eq!(custom["name"], "build-watch");
    assert_eq!(
        custom["detail"],
        json!({"type": "plain_text", "text": "Build finished"})
    );
    assert_eq!(custom["metadata"]["synthetic"], true);
    assert_eq!(text(e), "The build passed.");
    assert!(matches!(
        e.iter()
            .rev()
            .find(|x| !matches!(x, ProviderEvent::Exited { .. })),
        Some(ProviderEvent::TurnCompleted { turn: None, .. })
    ));
}

/// Replays a transcript recorded with `BURROW_PI_TRACE`: the daemon's
/// side (prompts, the interrupt) is rebuilt from its `in` frames, and every
/// `out` frame is what pi really sent. The adapter numbers its requests the
/// same way, so pi's answers find their commands.
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
            launch.pi.send(Output::Message(frame.clone())).unwrap();
            continue;
        }
        // The daemon's side: issue what made the adapter write this frame.
        match frame["type"].as_str() {
            Some("prompt") => {
                turns += 1;
                commands
                    .send(Command::Prompt {
                        turn_id: format!("foreground-turn-{turns}"),
                        message_id: format!("m{turns}"),
                        text: frame["message"].as_str().unwrap().into(),
                        images: Vec::new(),
                    })
                    .unwrap();
            }
            // An interrupt writes clear_queue, then abort.
            Some("clear_queue") => commands.send(Command::Interrupt).unwrap(),
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
        assert_eq!(
            written["id"], frame["id"],
            "wrote {written}\nexpected {frame}"
        );
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
async fn a_recorded_gemini_session_replays_to_the_same_conversation() {
    let events = play_recorded(include_str!("fixtures/real_gemini_session.jsonl")).await;
    assert_eq!(ends(&events), ["completed", "completed", "canceled"]);
    assert_eq!(text(&events), "ready\ndone\nThe");
    let bash = items(&events)
        .into_iter()
        .find(|i| i["type"] == "tool_call" && i["status"] == "completed")
        .expect("a completed bash call");
    assert_eq!(
        bash["detail"],
        json!({"type": "shell", "command": "echo smoke > smoke.txt", "output": "(no output)",
               "exitCode": null})
    );
    let usage = events
        .iter()
        .rev()
        .find_map(|e| match e {
            ProviderEvent::TurnCompleted { usage: Some(u), .. } => Some(u.clone()),
            _ => None,
        })
        .expect("usage");
    assert_eq!(usage["inputTokens"], 9743);
    assert_eq!(usage["contextWindowUsedTokens"], 3280);
    assert!(events.iter().any(|e| matches!(
        e,
        ProviderEvent::SessionStarted { session_id, .. }
            if session_id == "01a156a4-d4ac-73a4-966d-83f46b2481f8"
    )));
}

#[tokio::test]
async fn settings_and_steers_reach_pi() {
    let played = play(
        include_str!("fixtures/settings_and_steer.jsonl"),
        None,
        Settings::default(),
    )
    .await;
    // The fixture's `in` lines check what was written.
    let compaction: Vec<Value> = items(&played.events)
        .into_iter()
        .filter(|i| i["type"] == "compaction")
        .collect();
    assert_eq!(
        compaction,
        [
            json!({"type": "compaction", "status": "loading", "trigger": "manual"}),
            json!({"type": "compaction", "status": "completed", "trigger": "manual"}),
        ]
    );
    assert!(
        matches!(
            played
                .events
                .iter()
                .rfind(|e| matches!(e, ProviderEvent::TurnCompleted { .. })),
            Some(ProviderEvent::TurnCompleted { usage: None, .. })
        ),
        "a pi without session stats still completes the turn"
    );
}
