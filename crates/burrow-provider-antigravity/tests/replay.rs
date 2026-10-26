//! The Antigravity adapter against scripted CLI transcripts: deterministic,
//! no process, no model.
//!
//! A fixture is JSONL. Each line is one of:
//! - `{"out":{...}}`: the CLI prints this frame;
//! - `{"in":{...}}`: the adapter must write a frame that contains this one
//!   (`"$ANY"` matches any value, `"$PREFIX:<text>"` any string starting so);
//! - `{"cmd":{...}}`: the daemon sends a command (`prompt`, `interrupt`,
//!   `model`);
//! - `{"stop":"interrupt"|"close"}`: the adapter must ask the CLI to stop;
//! - `{"exit":{"status":1,"stderr":"..."}}`: the CLI exits;
//! - `{"launch":{"conversation":...,"model":...}}`: the adapter must start
//!   the CLI again, with these arguments.
//!
//! The first launch, when the session starts, is implicit. Every command
//! that the CLI should see is followed by the `in` or `stop` it causes,
//! which keeps the two streams in order. Lines starting `//` are comments.
//!
//! The scripted transcripts are written for these tests, in the frame format
//! of agy 1.2 to 1.4; none is copied from Paseo. `real_flash_*.jsonl` were
//! recorded from agy 1.4.2 with `BURROW_AGY_TRACE` (see their headers).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_provider::{Command, Image, ProviderEvent};
use burrow_provider_antigravity::process::{Output, Stop, Transport};
use burrow_provider_antigravity::session::{Launcher, Session, Settings, persistence_id};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);
const CONVERSATION: &str = "0b7c3f8e-2d41-4a6b-9c55-1f0e2a3b4c5d";
/// The conversation of the recorded session.
const REAL: &str = "eb504908-f832-45e1-8a82-1b3a7da4288e";

/// A launch's `--conversation` and `--model`.
type LaunchArgs = (Option<String>, Option<String>);

struct FakeLauncher {
    launches: mpsc::UnboundedSender<Launch>,
    args: Arc<Mutex<Vec<LaunchArgs>>>,
}

struct Launch {
    written: mpsc::UnboundedReceiver<Value>,
    stops: mpsc::UnboundedReceiver<Stop>,
    cli: mpsc::UnboundedSender<Output>,
}

impl Launcher for FakeLauncher {
    fn launch(
        &mut self,
        conversation: Option<&str>,
        settings: &Settings,
    ) -> std::io::Result<Transport> {
        self.args
            .lock()
            .unwrap()
            .push((conversation.map(str::to_string), settings.model.clone()));
        let (input, written) = mpsc::unbounded_channel();
        let (stop, stops) = mpsc::unbounded_channel();
        let (cli, output) = mpsc::unbounded_channel();
        let _ = self.launches.send(Launch {
            written,
            stops,
            cli,
        });
        Ok(Transport {
            input,
            stop,
            output,
        })
    }
}

fn contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (_, Value::String(s)) if s == "$ANY" => true,
        (Value::String(a), Value::String(s)) if s.starts_with("$PREFIX:") => {
            a.starts_with(&s["$PREFIX:".len()..])
        }
        (Value::Object(a), Value::Object(e)) => e
            .iter()
            .all(|(k, v)| a.get(k).is_some_and(|av| contains(av, v))),
        (Value::Array(a), Value::Array(e)) => {
            a.len() == e.len() && a.iter().zip(e).all(|(av, ev)| contains(av, ev))
        }
        _ => actual == expected,
    }
}

fn command(spec: &Value) -> Command {
    let (name, args) = spec.as_object().unwrap().iter().next().unwrap();
    match name.as_str() {
        "prompt" => Command::Prompt {
            turn_id: args["turn"].as_str().unwrap().into(),
            message_id: "8c81baa4-28ba-48dc-9b9d-84a5bf4aa31a".into(),
            text: args["text"].as_str().unwrap_or_default().into(),
            images: args["images"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|image| Image {
                    data: image["data"].as_str().unwrap().into(),
                    mime_type: image["mimeType"].as_str().unwrap().into(),
                })
                .collect(),
        },
        "interrupt" => Command::Interrupt,
        "model" => Command::SetModel(args.as_str().map(str::to_string)),
        "mode" => Command::SetMode(args.as_str().unwrap().into()),
        other => panic!("unknown command {other}"),
    }
}

struct Played {
    events: Vec<ProviderEvent>,
    launches: Vec<LaunchArgs>,
}

struct Options {
    resume: Option<String>,
    model: Option<String>,
    system_prompt: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            resume: None,
            model: Some("gemini-3.8-flash-low".into()),
            system_prompt: None,
        }
    }
}

async fn play(fixture: &str, options: Options) -> Played {
    let (launches_tx, mut launches) = mpsc::unbounded_channel();
    let args = Arc::new(Mutex::new(Vec::new()));
    let launcher = FakeLauncher {
        launches: launches_tx,
        args: args.clone(),
    };
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let settings = Settings {
        model: options.model,
    };
    let session = Session::new(
        launcher,
        settings,
        options.system_prompt,
        options.resume,
        events_tx,
    );
    let (commands, command_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(session.run(command_rx));

    let mut current: Launch = timeout(WAIT, launches.recv()).await.unwrap().unwrap();
    for (number, line) in fixture.lines().enumerate() {
        let number = number + 1;
        if line.trim().is_empty() || line.starts_with("//") {
            continue;
        }
        let step: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {number}: {e}"));
        let (kind, body) = step.as_object().unwrap().iter().next().unwrap();
        match kind.as_str() {
            "cmd" => commands.send(command(body)).unwrap(),
            "out" => current.cli.send(Output::Message(body.clone())).unwrap(),
            "exit" => current
                .cli
                .send(Output::Exited {
                    status: body["status"].as_i64().map(|s| s as i32),
                    stderr: body["stderr"].as_str().unwrap_or_default().into(),
                })
                .unwrap(),
            "in" => {
                let written = timeout(WAIT, current.written.recv())
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| panic!("line {number}: nothing written; expected {body}"));
                assert!(
                    contains(&written, body),
                    "line {number}: wrote {written}\nexpected {body}"
                );
            }
            "stop" => {
                let stop = timeout(WAIT, current.stops.recv())
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| panic!("line {number}: no stop; expected {body}"));
                let expected = match body.as_str() {
                    Some("interrupt") => Stop::Interrupt,
                    Some("close") => Stop::Close,
                    other => panic!("line {number}: unknown stop {other:?}"),
                };
                assert_eq!(stop, expected, "line {number}");
            }
            "launch" => {
                current = timeout(WAIT, launches.recv())
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| panic!("line {number}: no new launch"));
                let last = args.lock().unwrap().last().cloned().unwrap();
                let expected = (
                    body["conversation"].as_str().map(str::to_string),
                    body["model"].as_str().map(str::to_string),
                );
                assert_eq!(last, expected, "line {number}: launch arguments");
            }
            other => panic!("line {number}: unknown step {other}"),
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
    let launches = args.lock().unwrap().clone();
    Played { events, launches }
}

fn items(events: &[ProviderEvent]) -> Vec<(Option<String>, Value)> {
    events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::Item { turn, item } => Some((turn.clone(), item.clone())),
            _ => None,
        })
        .collect()
}

fn text(events: &[ProviderEvent], turn: &str) -> String {
    items(events)
        .iter()
        .filter(|(t, i)| t.as_deref() == Some(turn) && i["type"] == "assistant_message")
        .filter_map(|(_, i)| i["text"].as_str().map(str::to_string))
        .collect()
}

fn tools(events: &[ProviderEvent]) -> Vec<Value> {
    items(events)
        .into_iter()
        .map(|(_, item)| item)
        .filter(|item| item["type"] == "tool_call")
        .collect()
}

fn notices(events: &[ProviderEvent]) -> Vec<String> {
    items(events)
        .into_iter()
        .filter(|(_, item)| item["type"] == "notification")
        .map(|(_, item)| item["message"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_text_turn_streams_deltas_and_reports_usage() {
    let played = play(
        include_str!("fixtures/text_turn.jsonl"),
        Options {
            system_prompt: Some("Be brief.".into()),
            ..Options::default()
        },
    )
    .await;
    let e = &played.events;
    assert!(matches!(
        &e[0],
        ProviderEvent::SessionStarted { session_id, model: None }
            if *session_id == persistence_id(CONVERSATION)
    ));
    assert!(
        matches!(&e[1], ProviderEvent::TurnStarted { turn: Some(t) } if t == "foreground-turn-1")
    );
    assert_eq!(text(e, "foreground-turn-1"), "Hello there.\n");
    assert_eq!(text(e, "foreground-turn-2"), "Again.\n");
    // One message per step, keyed `<turn key>:<step index>`, as in Paseo.
    let ids: Vec<String> = items(e)
        .iter()
        .filter(|(t, _)| t.as_deref() == Some("foreground-turn-1"))
        .map(|(_, i)| i["messageId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 3, "three deltas of one message");
    assert!(ids.iter().all(|id| *id == ids[0] && id.ends_with(":1")));
    let usage: Vec<Value> = e
        .iter()
        .filter_map(|x| match x {
            ProviderEvent::TurnCompleted { usage, .. } => usage.clone(),
            _ => None,
        })
        .collect();
    // Each DONE generation counts once; the cumulative result is not added.
    assert_eq!(
        usage[0],
        json!({"inputTokens": 1200, "outputTokens": 9, "cachedInputTokens": 100})
    );
    assert_eq!(
        usage[1],
        json!({"inputTokens": 1300, "outputTokens": 4, "cachedInputTokens": 0})
    );
    assert_eq!(played.launches.len(), 1, "one process for both turns");
}

#[tokio::test]
async fn tools_map_to_paseo_details_and_denials_are_reported_once() {
    let played = play(include_str!("fixtures/tools.jsonl"), Options::default()).await;
    let e = &played.events;
    let calls = tools(e);
    let last = |name: &str| {
        calls
            .iter()
            .rev()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap()
    };
    let read = last("view_file");
    assert_eq!(read["status"], "completed");
    assert_eq!(
        read["detail"],
        json!({"type": "read", "filePath": "/home/user/repo/notes.txt", "content": "2 lines, 19 bytes"})
    );
    assert!(read["callId"].as_str().unwrap().ends_with(":2"));
    assert_eq!(
        last("write_to_file")["detail"],
        json!({"type": "write", "filePath": "/home/user/repo/out.txt"})
    );
    assert_eq!(
        last("replace_file_content")["detail"],
        json!({"type": "edit", "filePath": "/home/user/repo/out.txt"})
    );
    let shell = last("run_command");
    assert_eq!(shell["status"], "failed");
    assert_eq!(
        shell["detail"],
        json!({"type": "shell", "command": "printf hi", "output": ""})
    );
    assert!(
        shell["error"]
            .as_str()
            .unwrap()
            .starts_with("permission check failed")
    );
    // The second result repeats the first denial: still one notice.
    assert_eq!(
        notices(e),
        vec!["Shell command denied\nAntigravity's own policy denied it."]
    );
    let notice_turn = items(e)
        .into_iter()
        .find(|(_, i)| i["type"] == "notification")
        .unwrap()
        .0;
    assert_eq!(notice_turn.as_deref(), Some("foreground-turn-1"));
    let completed = e
        .iter()
        .filter(|x| matches!(x, ProviderEvent::TurnCompleted { .. }))
        .count();
    assert_eq!(completed, 2);
}

#[tokio::test]
async fn an_interrupt_cancels_then_the_conversation_continues() {
    let played = play(include_str!("fixtures/interrupt.jsonl"), Options::default()).await;
    let e = &played.events;
    let canceled: Vec<&ProviderEvent> = e
        .iter()
        .filter(|x| matches!(x, ProviderEvent::TurnCanceled { .. }))
        .collect();
    assert_eq!(
        canceled.len(),
        1,
        "the CLI's own `interrupted` result is ignored"
    );
    assert!(matches!(
        canceled[0],
        ProviderEvent::TurnCanceled { turn: Some(t), reason }
            if t == "foreground-turn-1" && reason == "Canceled by provider"
    ));
    let sleep: Vec<Value> = tools(e)
        .into_iter()
        .filter(|c| c["name"] == "run_command")
        .collect();
    assert_eq!(sleep.len(), 2);
    assert_eq!(sleep[0]["status"], "running");
    assert_eq!(sleep[1]["status"], "canceled");
    assert!(sleep[1]["error"].is_null());
    assert_eq!(text(e, "foreground-turn-2"), "Still here.");
    assert!(e.iter().any(
        |x| matches!(x, ProviderEvent::TurnCompleted { turn: Some(t), .. } if t == "foreground-turn-2")
    ));
    let started = e
        .iter()
        .filter(|x| matches!(x, ProviderEvent::SessionStarted { .. }))
        .count();
    assert_eq!(started, 1, "the same conversation, announced once");
}

#[tokio::test]
async fn a_model_change_restarts_the_cli_on_the_next_prompt() {
    let played = play(
        include_str!("fixtures/model_change.jsonl"),
        Options::default(),
    )
    .await;
    assert_eq!(
        played.launches,
        vec![
            (None, Some("gemini-3.8-flash-low".into())),
            (
                Some(CONVERSATION.into()),
                Some("gemini-3.8-flash-high".into())
            ),
        ]
    );
    assert_eq!(text(&played.events, "foreground-turn-1"), "High now.");
}

#[tokio::test]
async fn a_resumed_session_keeps_its_conversation_and_skips_the_system_prompt() {
    let played = play(
        include_str!("fixtures/resume.jsonl"),
        Options {
            resume: Some(CONVERSATION.into()),
            system_prompt: Some("Be brief.".into()),
            ..Options::default()
        },
    )
    .await;
    assert_eq!(played.launches[0].0.as_deref(), Some(CONVERSATION));
    assert!(
        !played
            .events
            .iter()
            .any(|x| matches!(x, ProviderEvent::SessionStarted { .. })),
        "nothing new to persist"
    );
    assert_eq!(text(&played.events, "foreground-turn-7"), "COBALT");
}

#[tokio::test]
async fn a_different_conversation_on_resume_fails_the_prompt() {
    let played = play(
        include_str!("fixtures/resume_mismatch.jsonl"),
        Options {
            resume: Some(CONVERSATION.into()),
            ..Options::default()
        },
    )
    .await;
    let failed = played.events.iter().find_map(|x| match x {
        ProviderEvent::TurnFailed { turn, error, .. } => Some((turn.clone(), error.clone())),
        _ => None,
    });
    assert_eq!(
        failed,
        Some((
            Some("foreground-turn-3".into()),
            "Antigravity restored a different conversation".into()
        ))
    );
}

#[tokio::test]
async fn signed_out_fails_the_turn_with_paseos_diagnostic() {
    let played = play(
        include_str!("fixtures/not_signed_in.jsonl"),
        Options::default(),
    )
    .await;
    let failed: Vec<String> = played
        .events
        .iter()
        .filter_map(|x| match x {
            ProviderEvent::TurnFailed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        failed,
        vec!["Antigravity authentication failed. Run `agy` and sign in, then retry."]
    );
    assert_eq!(played.launches.len(), 2, "the prompt tried again");
}

#[tokio::test]
async fn a_crash_mid_turn_fails_it_and_its_running_tools() {
    let played = play(include_str!("fixtures/crash.jsonl"), Options::default()).await;
    let e = &played.events;
    assert!(e.iter().any(|x| matches!(
        x,
        ProviderEvent::TurnFailed { turn: Some(t), error, code: Some(code), .. }
            if t == "foreground-turn-1" && error == "panic: runtime error" && code == "PROCESS_EXIT"
    )));
    let last = tools(e).pop().unwrap();
    assert_eq!(last["status"], "failed");
    assert_eq!(last["error"], "Antigravity turn failed");
}

#[tokio::test]
async fn a_subagent_step_is_a_sub_agent_tool_call() {
    let played = play(include_str!("fixtures/subagent.jsonl"), Options::default()).await;
    let call = tools(&played.events).pop().unwrap();
    assert_eq!(call["name"], "invoke_subagent");
    assert_eq!(call["status"], "completed");
    assert_eq!(call["detail"]["type"], "sub_agent");
    assert_eq!(
        call["detail"]["childSessionId"],
        "9d1e2f3a-4b5c-4d6e-8f70-81a2b3c4d5e6"
    );
    assert_eq!(call["metadata"]["subagents"][0]["type_name"], "counter");
}

#[tokio::test]
async fn images_are_attached_as_files() {
    let played = play(include_str!("fixtures/image.jsonl"), Options::default()).await;
    assert!(
        played
            .events
            .iter()
            .any(|x| matches!(x, ProviderEvent::TurnCompleted { .. }))
    );
}

#[tokio::test]
async fn a_real_flash_session_replays() {
    let played = play(
        include_str!("fixtures/real_flash_session.jsonl"),
        Options::default(),
    )
    .await;
    let e = &played.events;
    assert!(e.iter().any(|x| matches!(
        x,
        ProviderEvent::SessionStarted { session_id, .. } if *session_id == persistence_id(REAL)
    )));
    assert_eq!(text(e, "foreground-turn-1"), "ready\n");
    let shell = tools(e).pop().expect("a shell call");
    assert_eq!(shell["status"], "completed");
    assert_eq!(
        shell["detail"],
        json!({"type": "shell", "command": "echo smoke > smoke.txt",
               "output": "Command exited with code 0.\n\nNo output from the command."})
    );
    assert_eq!(text(e, "foreground-turn-2"), "done\n");
    let usage = e.iter().find_map(|x| match x {
        ProviderEvent::TurnCompleted {
            turn: Some(t),
            usage,
        } if t == "foreground-turn-2" => usage.clone(),
        _ => None,
    });
    assert_eq!(
        usage,
        Some(json!({"inputTokens": 8470 + 8599, "outputTokens": 43, "cachedInputTokens": 0}))
    );
    assert!(text(e, "foreground-turn-3").starts_with("1\n2\n3"));
    assert!(e.iter().any(|x| matches!(
        x,
        ProviderEvent::TurnCanceled { turn: Some(t), .. } if t == "foreground-turn-3"
    )));
}

#[tokio::test]
async fn a_real_resume_after_a_restart_replays() {
    let played = play(
        include_str!("fixtures/real_flash_resume.jsonl"),
        Options {
            resume: Some(REAL.into()),
            ..Options::default()
        },
    )
    .await;
    assert_eq!(played.launches[0].0.as_deref(), Some(REAL));
    assert_eq!(text(&played.events, "foreground-turn-4"), "ready\n");
}
