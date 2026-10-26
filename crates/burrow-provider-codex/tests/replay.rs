//! The Codex adapter against scripted app-server transcripts: deterministic,
//! no process, no model.
//!
//! A fixture is JSONL. Each line is one of:
//! - `{"dir":"out","frame":{...}}`: app-server sends this;
//! - `{"dir":"in","frame":{...}}`: the adapter must write a frame that
//!   contains this one (`"$ANY"` matches any value);
//! - `{"cmd":{...}}`: the daemon sends a command (`prompt`, `steer`,
//!   `interrupt`, `respond`, `mode`, `model`, `thinking`);
//! - `{"setup":"start"|"resume"}`: the usual handshake, ending in a new
//!   thread `thr-1` or the resumed `thr-old`;
//! - `{"exit":"<stderr>"}`: app-server dies.
//!
//! `"$ID"` in an `out` frame stands for the id of the last request the
//! adapter wrote. Real transcripts recorded with `BURROW_CODEX_TRACE` have
//! the same `dir`/`frame` shape.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_provider::{Command, ProviderEvent};
use burrow_provider_codex::catalog::Catalog;
use burrow_provider_codex::process::Output;
use burrow_provider_codex::session::{Launcher, Session, Settings, Transport};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);
const CWD: &str = "/home/user/repo";

/// Each launch hands the test a new pair of channels.
struct FakeLauncher {
    launches: mpsc::UnboundedSender<Launch>,
}

struct Launch {
    written: mpsc::UnboundedReceiver<Value>,
    server: mpsc::UnboundedSender<Output>,
}

impl Launcher for FakeLauncher {
    fn launch(&mut self) -> std::io::Result<Transport> {
        let (input, written) = mpsc::unbounded_channel();
        let (server, output) = mpsc::unbounded_channel();
        let _ = self.launches.send(Launch { written, server });
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

fn substitute(frame: &Value, id: &Value) -> Value {
    match frame {
        Value::String(s) if s == "$ID" => id.clone(),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute(v, id)))
                .collect(),
        ),
        Value::Array(list) => list.iter().map(|v| substitute(v, id)).collect(),
        other => other.clone(),
    }
}

fn command(spec: &Value) -> Command {
    let (name, args) = spec.as_object().unwrap().iter().next().unwrap();
    match name.as_str() {
        "prompt" => Command::Prompt {
            turn_id: args["turn"].as_str().unwrap().into(),
            message_id: "m-1".into(),
            text: args["text"].as_str().unwrap().into(),
            images: Vec::new(),
        },
        "steer" => Command::Steer {
            message_id: "m-2".into(),
            text: args.as_str().unwrap().into(),
            images: Vec::new(),
        },
        "interrupt" => Command::Interrupt,
        "mode" => Command::SetMode(args.as_str().unwrap().into()),
        "model" => Command::SetModel(args.as_str().map(str::to_string)),
        "thinking" => Command::SetThinking(args.as_str().map(str::to_string)),
        "respond" => Command::RespondPermission {
            id: args["id"].as_str().unwrap().into(),
            response: args["response"].clone(),
        },
        other => panic!("unknown command {other}"),
    }
}

/// The handshake every session starts with.
fn setup(kind: &str) -> Vec<Value> {
    let mut steps = vec![
        json!({"dir": "in", "frame": {"method": "initialize",
            "params": {"clientInfo": {"name": "codex_app_server_daemon"},
                       "capabilities": {"experimentalApi": true}}}}),
        json!({"dir": "out", "frame": {"id": "$ID", "result": {"userAgent": "codex_cli_rs/0.162.1"}}}),
        json!({"dir": "in", "frame": {"method": "initialized"}}),
        json!({"dir": "in", "frame": {"id": 2, "method": "model/list"}}),
        json!({"dir": "in", "frame": {"id": 3, "method": "config/read", "params": {"cwd": CWD}}}),
        json!({"dir": "in", "frame": {"id": 4, "method": "collaborationMode/list"}}),
        json!({"dir": "out", "frame": {"id": 2, "result": {"data": [
            {"id": "gpt-6-luna", "model": "gpt-6-luna", "displayName": "GPT-6-Luna",
             "isDefault": true, "defaultReasoningEffort": "medium",
             "supportedReasoningEfforts": [
                {"reasoningEffort": "low", "description": "Fast"},
                {"reasoningEffort": "medium", "description": "Balanced"}]}]}}}),
        json!({"dir": "out", "frame": {"id": 3, "result": {"config": {"model": null}}}}),
        json!({"dir": "out", "frame": {"id": 4, "result": {"data": [
            {"name": "Plan", "mode": "plan"}, {"name": "Default", "mode": "default"}]}}}),
    ];
    let sandbox = json!({"type": "workspaceWrite", "writableRoots": [], "networkAccess": false,
                         "excludeTmpdirEnvVar": false, "excludeSlashTmp": false});
    if kind == "resume" {
        steps.push(json!({"dir": "in", "frame": {"method": "thread/resume",
            "params": {"threadId": "thr-old"}}}));
        steps.push(json!({"dir": "out", "frame": {"id": "$ID", "result": {
            "thread": {"id": "thr-old"}, "sandbox": sandbox}}}));
    } else {
        steps.push(
            json!({"dir": "in", "frame": {"method": "thread/start", "params": {
            "model": "gpt-6-luna", "cwd": CWD, "approvalPolicy": "on-request",
            "sandbox": "workspace-write", "approvalsReviewer": "user"}}}),
        );
        steps.push(json!({"dir": "out", "frame": {"id": "$ID", "result": {
            "thread": {"id": "thr-1"}, "sandbox": sandbox}}}));
        steps.push(json!({"dir": "out", "frame": {"method": "thread/started",
            "params": {"thread": {"id": "thr-1"}}}}));
    }
    steps
}

async fn play(fixture: &str, resume: Option<&str>) -> Vec<ProviderEvent> {
    let (launches_tx, mut launches) = mpsc::unbounded_channel();
    let launcher = FakeLauncher {
        launches: launches_tx,
    };
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let settings = Settings {
        cwd: CWD.into(),
        mode: Some("auto".into()),
        ..Settings::default()
    };
    let catalog = Arc::new(Mutex::new(Catalog::default()));
    let session = Session::new(
        launcher,
        settings,
        resume.map(str::to_string),
        catalog,
        events_tx,
    );
    let (commands, command_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(session.run(command_rx));

    let mut steps = Vec::new();
    for (number, line) in fixture.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with("//") {
            continue;
        }
        let step: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {number}: {e}"));
        match step["setup"].as_str() {
            Some(kind) => steps.extend(setup(kind).into_iter().map(|s| (number, s))),
            None => steps.push((number, step)),
        }
    }

    let mut launch: Option<Launch> = None;
    let mut last_id = Value::Null;
    for (number, step) in steps {
        if let Some(spec) = step.get("cmd") {
            commands.send(command(spec)).unwrap();
            continue;
        }
        // A command may relaunch app-server; take the newest launch.
        while let Ok(next) = launches.try_recv() {
            launch = Some(next);
        }
        if launch.is_none() {
            launch = Some(timeout(WAIT, launches.recv()).await.unwrap().unwrap());
        }
        let current = launch.as_mut().unwrap();
        if let Some(stderr) = step.get("exit") {
            current
                .server
                .send(Output::Exited {
                    status: Some(1),
                    stderr: stderr.as_str().unwrap_or_default().into(),
                })
                .unwrap();
            continue;
        }
        match step["dir"].as_str() {
            Some("out") => {
                let frame = substitute(&step["frame"], &last_id);
                current.server.send(Output::Message(frame)).unwrap();
            }
            Some("in") => {
                let expected = &step["frame"];
                let written = timeout(WAIT, async {
                    loop {
                        if let Ok(next) = launches.try_recv() {
                            *current = next;
                        }
                        if let Ok(Some(frame)) =
                            timeout(Duration::from_millis(20), current.written.recv()).await
                        {
                            return frame;
                        }
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("line {number}: nothing written; expected {expected}"));
                assert!(
                    contains(&written, expected),
                    "line {number}: wrote {written}\nexpected {expected}"
                );
                if written.get("method").is_some() && written.get("id").is_some() {
                    last_id = written["id"].clone();
                }
            }
            other => panic!("line {number}: unknown step {other:?}"),
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

fn items(events: &[ProviderEvent]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::Item { item, .. } => Some(item.clone()),
            _ => None,
        })
        .collect()
}

fn texts(events: &[ProviderEvent]) -> Vec<String> {
    items(events)
        .iter()
        .filter(|i| i["type"] == "assistant_message")
        .filter_map(|i| i["text"].as_str().map(str::to_string))
        .collect()
}

fn permissions(events: &[ProviderEvent]) -> Vec<(String, Value)> {
    events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::PermissionRequested { id, request } => {
                Some((id.clone(), request.clone()))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_text_turn_streams_once() {
    let e = play(include_str!("fixtures/text_turn.jsonl"), None).await;
    assert!(matches!(
        &e[0],
        ProviderEvent::SessionStarted { session_id, model: Some(m) }
            if session_id == "thr-1" && m == "gpt-6-luna"
    ));
    let info = e
        .iter()
        .find_map(|x| match x {
            ProviderEvent::RuntimeInfo { info } => Some(info.clone()),
            _ => None,
        })
        .expect("runtime info");
    assert_eq!(
        info,
        json!({"provider": "codex", "sessionId": "thr-1", "model": "gpt-6-luna",
               "thinkingOptionId": "medium", "modeId": "auto",
               "extra": {"collaborationMode": "Default"}})
    );
    assert!(
        e.iter().any(
            |x| matches!(x, ProviderEvent::TurnStarted { turn: Some(t) } if t == "codex-turn-1")
        )
    );
    assert_eq!(
        texts(&e),
        ["Hel", "lo.", "\n\n---\n\nBye.", " Done"],
        "deltas, a rule before the second message, then only the missing suffix"
    );
    let reasoning: Vec<Value> = items(&e)
        .into_iter()
        .filter(|i| i["type"] == "reasoning")
        .collect();
    assert_eq!(
        reasoning,
        [json!({"type": "reasoning", "text": "Thinking"})]
    );
    assert!(
        !items(&e).iter().any(|i| i["type"] == "user_message"),
        "the daemon records the prompt itself"
    );
    let usage = json!({"inputTokens": 17142, "cachedInputTokens": 16896, "outputTokens": 65,
                       "contextWindowMaxTokens": 258400, "contextWindowUsedTokens": 17207});
    assert!(
        e.iter()
            .any(|x| matches!(x, ProviderEvent::UsageUpdated { usage: u } if *u == usage))
    );
    assert!(matches!(
        e.iter().rev().find(|x| !matches!(x, ProviderEvent::Exited { .. })),
        Some(ProviderEvent::TurnCompleted { turn: Some(t), usage: Some(u) })
            if t == "codex-turn-1" && *u == usage
    ));
}

#[tokio::test]
async fn commands_wait_for_approval_and_a_denial_shows() {
    let e = play(include_str!("fixtures/command_approval.jsonl"), None).await;
    let asked = permissions(&e);
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].0, "0");
    assert_eq!(
        asked[0].1,
        json!({
            "provider": "codex", "name": "CodexBash", "kind": "tool",
            "title": "Run command: /bin/bash -lc 'echo hi > a.txt'",
            "description": "May I write a.txt?",
            "input": {"command": "/bin/bash -lc 'echo hi > a.txt'", "cwd": CWD},
            "detail": {"type": "shell", "command": "echo hi > a.txt", "cwd": CWD},
            "metadata": {"itemId": "exec-1", "threadId": "thr-1", "turnId": "turn-a"},
        })
    );
    assert!(
        !e.iter()
            .any(|x| matches!(x, ProviderEvent::PermissionCanceled { .. })),
        "an answered request is not withdrawn"
    );
    let calls: Vec<Value> = items(&e)
        .into_iter()
        .filter(|i| i["type"] == "tool_call")
        .collect();
    let exec1: Vec<&Value> = calls.iter().filter(|c| c["callId"] == "exec-1").collect();
    assert_eq!(exec1[0]["status"], "running");
    assert_eq!(
        exec1[1]["detail"],
        json!({"type": "shell", "command": "echo hi > a.txt", "cwd": CWD, "exitCode": 0})
    );
    let denied = calls
        .iter()
        .find(|c| c["callId"] == "permission-exec-2")
        .expect("the denial shows");
    assert_eq!(denied["name"], "shell");
    assert_eq!(denied["status"], "failed");
    assert_eq!(denied["error"], json!({"message": "Not now"}));
    assert_eq!(
        denied["metadata"],
        json!({"permissionRequestId": "permission-exec-2", "denied": true})
    );
    let exec2 = calls.iter().rfind(|c| c["callId"] == "exec-2").unwrap();
    assert_eq!(exec2["status"], "failed");
}

#[tokio::test]
async fn an_interrupt_waits_for_codex_to_name_the_turn() {
    let e = play(include_str!("fixtures/interrupt.jsonl"), None).await;
    let ends: Vec<&ProviderEvent> = e
        .iter()
        .filter(|x| {
            matches!(
                x,
                ProviderEvent::TurnCanceled { .. }
                    | ProviderEvent::TurnCompleted { .. }
                    | ProviderEvent::TurnFailed { .. }
            )
        })
        .collect();
    assert_eq!(ends.len(), 2);
    for end in ends {
        assert!(
            matches!(end, ProviderEvent::TurnCanceled { reason, .. } if reason == "interrupted"),
            "{end:?}"
        );
    }
}

#[tokio::test]
async fn a_question_is_answered_by_header() {
    let e = play(include_str!("fixtures/question.jsonl"), None).await;
    let asked = permissions(&e);
    assert_eq!(asked[0].1["kind"], "question");
    assert_eq!(asked[0].1["name"], "request_user_input");
    assert_eq!(
        asked[0].1["input"]["questions"][0],
        json!({"id": "color", "header": "Color", "question": "Which color?",
               "options": [{"label": "red"}, {"label": "blue"}]})
    );
    let calls: Vec<Value> = items(&e)
        .into_iter()
        .filter(|i| i["callId"] == "call-q")
        .collect();
    assert_eq!(calls[0]["status"], "running");
    assert_eq!(calls[1]["status"], "completed");
    assert_eq!(
        calls[1]["detail"]["text"],
        "Color: Which color?\nOptions: red, blue\n\nAnswers:\n\ncolor: blue"
    );
    assert_eq!(texts(&e), ["blue"]);
}

#[tokio::test]
async fn a_resumed_thread_and_a_steer() {
    let e = play(include_str!("fixtures/resume_steer.jsonl"), Some("thr-old")).await;
    assert!(
        !e.iter()
            .any(|x| matches!(x, ProviderEvent::SessionStarted { .. })),
        "a resumed thread is already known"
    );
    assert_eq!(texts(&e), ["Counting 1 2 3", " and steered"]);
}

#[tokio::test]
async fn an_archived_thread_is_unarchived_to_resume() {
    let e = play(
        include_str!("fixtures/resume_archived.jsonl"),
        Some("thr-old"),
    )
    .await;
    assert!(
        e.iter()
            .any(|x| matches!(x, ProviderEvent::TurnCompleted { .. }))
    );
}

#[tokio::test]
async fn a_crash_fails_the_turn_and_the_next_prompt_resumes() {
    let e = play(include_str!("fixtures/crash.jsonl"), None).await;
    assert!(e.iter().any(|x| matches!(
        x,
        ProviderEvent::TurnFailed { error, .. } if error == "Codex app-server exited: boom"
    )));
    assert!(e.iter().any(
        |x| matches!(x, ProviderEvent::TurnCompleted { turn: Some(t), .. } if t == "codex-turn-2")
    ));
}

#[tokio::test]
async fn files_and_plans_map_like_paseo() {
    let e = play(include_str!("fixtures/file_change.jsonl"), None).await;
    let all = items(&e);
    let patch = all
        .iter()
        .rfind(|i| i["callId"] == "patch-1")
        .expect("the file change");
    assert_eq!(patch["status"], "completed");
    assert_eq!(
        patch["detail"],
        json!({"type": "edit", "filePath": "notes.txt", "newString": "hello\n"})
    );
    assert_eq!(
        permissions(&e)[0].1,
        json!({"provider": "codex", "name": "CodexFileChange", "kind": "tool",
               "title": "Apply file changes",
               "detail": {"type": "unknown", "input": {"reason": null}, "output": null},
               "metadata": {"itemId": "patch-1", "threadId": "thr-1", "turnId": "turn-a"}})
    );
    let todo = all.iter().find(|i| i["type"] == "todo").expect("a todo");
    assert_eq!(todo["items"][0]["status"], "in_progress");
    let compactions: Vec<&Value> = all.iter().filter(|i| i["type"] == "compaction").collect();
    assert_eq!(
        compactions,
        [
            &json!({"type": "compaction", "status": "loading"}),
            &json!({"type": "compaction", "status": "completed"}),
        ],
        "one row for the item and the notification together"
    );
}

/// Replays a transcript recorded with `BURROW_CODEX_TRACE`: the daemon's
/// side (prompts, answers, the interrupt) is rebuilt from its `in` frames,
/// and every `out` frame is what app-server really sent.
async fn play_recorded(trace: &str, resume: Option<&str>, first_turn: u64) -> Vec<ProviderEvent> {
    let (launches_tx, mut launches) = mpsc::unbounded_channel();
    let launcher = FakeLauncher {
        launches: launches_tx,
    };
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let settings = Settings {
        cwd: CWD.into(),
        model: Some("gpt-6-luna".into()),
        mode: Some("auto".into()),
        thinking: Some("low".into()),
        system_prompt: None,
    };
    let catalog = Arc::new(Mutex::new(Catalog::default()));
    let session = Session::new(
        launcher,
        settings,
        resume.map(str::to_string),
        catalog,
        events_tx,
    );
    let (commands, command_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(session.run(command_rx));
    let mut launch = timeout(WAIT, launches.recv()).await.unwrap().unwrap();
    let mut turn = first_turn;
    for line in trace.lines().filter(|l| !l.starts_with("//")) {
        let record: Value = serde_json::from_str(line).unwrap();
        let frame = &record["frame"];
        if record["dir"] == "out" {
            launch.server.send(Output::Message(frame.clone())).unwrap();
            continue;
        }
        // The daemon's side: issue what made the adapter write this frame.
        match frame["method"].as_str() {
            Some("turn/start") => {
                commands
                    .send(Command::Prompt {
                        turn_id: format!("codex-turn-{turn}"),
                        message_id: format!("m-{turn}"),
                        text: frame["params"]["input"][0]["text"].as_str().unwrap().into(),
                        images: Vec::new(),
                    })
                    .unwrap();
                turn += 1;
            }
            Some("turn/interrupt") => commands.send(Command::Interrupt).unwrap(),
            None => {
                let behavior = if frame["result"]["decision"] == "accept" {
                    "allow"
                } else {
                    "deny"
                };
                commands
                    .send(Command::RespondPermission {
                        id: frame["id"].to_string(),
                        response: json!({"behavior": behavior}),
                    })
                    .unwrap();
            }
            Some(_) => {}
        }
        let written = timeout(WAIT, launch.written.recv())
            .await
            .unwrap_or_else(|_| panic!("the adapter did not write {frame}"))
            .unwrap();
        assert_eq!(
            written["method"], frame["method"],
            "wrote {written}\nexpected {frame}"
        );
        if frame.get("method").is_none() {
            assert_eq!(written["result"], frame["result"], "the answer Codex got");
        } else if frame["method"] != "thread/resume" {
            assert_eq!(written["params"], frame["params"], "what Codex got");
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

#[tokio::test]
async fn a_recorded_session_replays_to_the_same_conversation() {
    let e = play_recorded(include_str!("fixtures/real_session.jsonl"), None, 1).await;
    assert_eq!(ends(&e), ["completed", "completed", "canceled"]);
    let thread = "01a156aa-24f0-7062-9cba-e094066a3975";
    assert!(e.iter().any(
        |x| matches!(x, ProviderEvent::SessionStarted { session_id, .. } if session_id == thread)
    ));
    let asked = permissions(&e);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1["name"], "CodexBash");
    assert_eq!(
        asked[0].1["detail"],
        json!({"type": "shell", "command": "echo smoke > smoke.txt", "cwd": CWD})
    );
    let all = texts(&e).concat();
    assert!(all.starts_with("ready"), "{all}");
    assert!(all.contains("\n\n---\n\ndone"), "{all}");
    let shell = items(&e)
        .into_iter()
        .rfind(|i| i["type"] == "tool_call")
        .unwrap();
    assert_eq!(
        shell["detail"],
        json!({"type": "shell", "command": "echo smoke > smoke.txt", "cwd": CWD, "exitCode": 0})
    );
}

#[tokio::test]
async fn a_recorded_resume_remembers_the_conversation() {
    let trace = include_str!("fixtures/real_session_resumed.jsonl");
    let e = play_recorded(trace, Some("01a156aa-24f0-7062-9cba-e094066a3975"), 4).await;
    assert_eq!(ends(&e), ["completed"]);
    assert_eq!(texts(&e), ["ready"]);
    assert!(
        !e.iter()
            .any(|x| matches!(x, ProviderEvent::SessionStarted { .. }))
    );
    // The same thread as `thread/read` would return it, read back as history.
    let resumed: Value = trace
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|r| r["frame"]["result"]["thread"]["turns"].is_array())
        .unwrap();
    let history =
        burrow_provider_codex::history::from_thread(&resumed["frame"]["result"], Some(CWD));
    let shown: Vec<String> = history
        .iter()
        .map(|h| match h.item["type"].as_str().unwrap() {
            "tool_call" => format!("tool:{}:{}", h.item["name"], h.item["status"]),
            kind => format!("{kind}:{}", h.item["text"].as_str().unwrap_or_default()),
        })
        .collect();
    assert_eq!(
        shown,
        [
            "user_message:Reply with exactly one word: ready",
            "assistant_message:ready",
            "user_message:Run this shell command to create a file: echo smoke > smoke.txt (it needs approval; ask for it). Then reply with the word done.",
            "assistant_message:I’ll request approval to run the file-creating command.",
            "tool:\"shell\":\"completed\"",
            "assistant_message:done",
            "user_message:Count from 1 to 300, one number per line, with no other text.",
        ]
    );
    assert_eq!(
        history[0].timestamp.as_deref(),
        Some("2026-10-20T00:15:53.000Z")
    );
}

#[tokio::test]
async fn settings_apply_to_the_next_turn() {
    let e = play(include_str!("fixtures/settings.jsonl"), None).await;
    let last = e
        .iter()
        .rev()
        .find_map(|x| match x {
            ProviderEvent::RuntimeInfo { info } => Some(info.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(last["modeId"], "full-access");
    assert_eq!(last["thinkingOptionId"], "low");
    assert!(e.iter().any(|x| matches!(
        x,
        ProviderEvent::TurnFailed { error, .. } if error == "usage limit reached"
    )));
}
