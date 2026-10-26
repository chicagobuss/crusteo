//! A Codex agent end to end, against a scripted app-server (no process, no
//! model), through Crusteo's `paseo-client` and `fleet`: what the providers
//! snapshot, the agent snapshot and its timeline carry for Codex.

use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::{Config, Daemon, Running, start};
use burrow_provider_codex::CodexProvider;
use burrow_provider_codex::catalog::Catalog;
use burrow_provider_codex::process::Output;
use burrow_provider_codex::session::Transport;
use fleet::timeline::{ItemKind, Timeline};
use paseo_client::{Client, ConnState, Event, EventReceiver};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

fn models() -> Value {
    json!({"data": [{"id": "gpt-6-luna", "model": "gpt-6-luna", "displayName": "GPT-6-Luna",
        "description": "Fast and affordable.", "isDefault": true, "defaultReasoningEffort": "medium",
        "supportedReasoningEfforts": [
            {"reasoningEffort": "low", "description": "Fast responses"},
            {"reasoningEffort": "medium", "description": "Balanced"}]}]})
}

/// app-server as a task: answers the handshake, and plays a turn per prompt.
/// A prompt that starts with `run` asks to run a command first.
fn app_server() -> std::io::Result<Transport> {
    let (input, mut written) = mpsc::unbounded_channel::<Value>();
    let (server, output) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let say = |frame: Value| {
            let _ = server.send(Output::Message(frame));
        };
        let mut turns = 0;
        let mut waiting: Option<(String, String)> = None;
        while let Some(frame) = written.recv().await {
            let id = frame["id"].clone();
            let params = &frame["params"];
            match frame["method"].as_str() {
                Some("initialize") => {
                    say(json!({"id": id, "result": {"userAgent": "codex/0.162.1"}}))
                }
                Some("model/list") => say(json!({"id": id, "result": models()})),
                Some("config/read") => say(json!({"id": id, "result": {"config": {}}})),
                Some("collaborationMode/list") => say(json!({"id": id, "result": {"data": [
                    {"name": "Default", "mode": "default"}]}})),
                Some("thread/start" | "thread/resume") => say(json!({"id": id, "result": {
                    "thread": {"id": "thr-1"}, "sandbox": {"type": "workspaceWrite"}}})),
                Some("turn/start") => {
                    turns += 1;
                    let turn = format!("turn-{turns}");
                    let text = params["input"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    say(json!({"id": id, "result": {"turn": {"id": turn}}}));
                    say(
                        json!({"method": "turn/started", "params": {"threadId": "thr-1",
                        "turn": {"id": turn, "status": "inProgress"}}}),
                    );
                    if text.starts_with("run") {
                        say(
                            json!({"method": "item/started", "params": {"threadId": "thr-1",
                            "turnId": turn, "item": {"type": "commandExecution", "id": "exec-1",
                            "command": "/bin/bash -lc ls", "cwd": "/home/user/repo",
                            "status": "inProgress", "aggregatedOutput": null, "exitCode": null}}}),
                        );
                        say(
                            json!({"method": "item/commandExecution/requestApproval", "id": 0,
                            "params": {"threadId": "thr-1", "turnId": turn, "itemId": "exec-1",
                            "command": "/bin/bash -lc ls", "cwd": "/home/user/repo",
                            "reason": "List the files?"}}),
                        );
                        waiting = Some((turn, text));
                        continue;
                    }
                    finish(&say, &turn, &format!("echo: {text}"));
                }
                None if id == 0 => {
                    let (turn, _) = waiting.take().expect("an approval was asked");
                    let accepted = frame["result"]["decision"] == "accept";
                    say(
                        json!({"method": "item/completed", "params": {"threadId": "thr-1",
                        "turnId": turn, "item": {"type": "commandExecution", "id": "exec-1",
                        "command": "/bin/bash -lc ls", "cwd": "/home/user/repo",
                        "status": if accepted { "completed" } else { "declined" },
                        "aggregatedOutput": if accepted { json!("a.txt\n") } else { Value::Null },
                        "exitCode": if accepted { json!(0) } else { Value::Null }}}}),
                    );
                    finish(&say, &turn, if accepted { "listed" } else { "skipped" });
                }
                _ => {}
            }
        }
    });
    Ok(Transport { input, output })
}

fn finish(say: &impl Fn(Value), turn: &str, reply: &str) {
    let (head, tail) = reply.split_at(reply.len() / 2);
    for delta in [head, tail] {
        say(
            json!({"method": "item/agentMessage/delta", "params": {"threadId": "thr-1",
            "turnId": turn, "itemId": format!("msg-{turn}"), "delta": delta}}),
        );
    }
    say(
        json!({"method": "item/completed", "params": {"threadId": "thr-1", "turnId": turn,
        "item": {"type": "agentMessage", "id": format!("msg-{turn}"), "text": reply}}}),
    );
    say(
        json!({"method": "thread/tokenUsage/updated", "params": {"threadId": "thr-1",
        "turnId": turn, "tokenUsage": {"last": {"inputTokens": 900, "cachedInputTokens": 100,
        "outputTokens": 12, "totalTokens": 912}, "total": {}, "modelContextWindow": 258400}}}),
    );
    say(
        json!({"method": "turn/completed", "params": {"threadId": "thr-1",
        "turn": {"id": turn, "status": "completed", "error": null}}}),
    );
}

struct Harness {
    running: Running,
    daemon: Arc<Daemon>,
    client: Client,
    events: EventReceiver,
    cwd: std::path::PathBuf,
}

impl Harness {
    async fn new() -> Self {
        let daemon = Daemon::open(Config::ephemeral()).unwrap();
        let catalog = Catalog::from_responses(&models(), &json!({"config": {}}));
        daemon
            .agents
            .register(Arc::new(CodexProvider::with_launcher(
                Some(vec![0, 162, 1]),
                catalog,
                Arc::new(app_server),
            )));
        let running = start(daemon.clone()).await.unwrap();
        let client = Client::connect(paseo_client::Config::new(
            running.local_addr.to_string(),
            paseo_client::generate_client_id(),
        ));
        let events = client.events();
        timeout(WAIT, async {
            loop {
                if let Event::State(ConnState::Connected(_)) = events.recv().await.unwrap() {
                    return;
                }
            }
        })
        .await
        .expect("connected");
        let cwd =
            std::env::temp_dir().join(format!("burrow-codex-{}", burrow_daemon::ids::agent_id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let h = Self {
            running,
            daemon,
            client,
            events,
            cwd,
        };
        h.request(json!({"type": "session.events.set_subscription.request",
            "events": ["agent_permission_request", "agent_permission_resolved"]}))
            .await;
        h.request(json!({"type": "fetch_agents_request", "scope": "active", "subscribe": {}}))
            .await;
        h
    }

    async fn request(&self, message: Value) -> Value {
        self.client.request(message).await.expect("request")
    }

    async fn until(&self, what: &str, matches: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        timeout(WAIT, async {
            loop {
                if let Event::Message(message) = self.events.recv().await.unwrap() {
                    let hit = matches(&message);
                    seen.push(message);
                    if hit {
                        return;
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {what} within {WAIT:?}; saw {seen:#?}"));
        seen
    }

    async fn send(&self, agent: &str, text: &str, message_id: &str) {
        let prompt = fleet::prompt::Prompt {
            text: text.into(),
            images: Vec::new(),
        };
        let reply = self
            .request(fleet::requests::send_message(
                agent, message_id, &prompt, None,
            ))
            .await;
        assert_eq!(reply["accepted"], true, "{reply}");
    }

    async fn agent(&self, agent: &str) -> Value {
        self.request(json!({"type": "fetch_agent_request", "agentId": agent}))
            .await["agent"]
            .clone()
    }

    async fn stop(self) {
        drop(self.client);
        self.running.shutdown().await;
        self.daemon.close();
        let _ = std::fs::remove_dir_all(&self.cwd);
    }
}

fn stream_event(message: &Value, kind: &str) -> bool {
    message["type"] == "agent_stream" && message["payload"]["event"]["type"] == kind
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_providers_snapshot_offers_codex_as_paseo_does() {
    let h = Harness::new().await;
    let full = burrow_daemon::agents::providers_snapshot(&h.daemon, false);
    let codex = &full["entries"][0];
    assert_eq!(codex["provider"], "codex");
    assert_eq!(codex["defaultModeId"], "auto-review");
    let modes: Vec<&str> = codex["modes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(modes, ["auto", "auto-review", "full-access"]);
    assert_eq!(
        codex["models"][0],
        json!({"id": "gpt-6-luna", "label": "GPT-6-Luna", "description": "Fast and affordable.",
               "isDefault": true, "defaultThinkingOptionId": "medium",
               "thinkingOptions": [
                   {"id": "low", "label": "low", "description": "Fast responses", "isDefault": false},
                   {"id": "medium", "label": "medium", "description": "Balanced", "isDefault": true}],
               "metadata": {"model": "gpt-6-luna", "defaultReasoningEffort": "medium",
                            "supportedReasoningEfforts": models()["data"][0]["supportedReasoningEfforts"]}})
    );
    let compact = burrow_daemon::agents::providers_snapshot(&h.daemon, true);
    let model = &compact["compactSnapshot"]["entries"][0]["models"][0];
    assert_eq!(model["thinkingSet"], 0);
    assert!(model.get("thinkingOptions").is_none());
    assert_eq!(
        compact["compactSnapshot"]["thinkingSets"][0]["defaultOptionId"],
        "medium"
    );
    // What Crusteo reads from it.
    let offered = fleet::providers::parse(&compact);
    assert_eq!(offered[0].models[0].id, "gpt-6-luna");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_agent_runs_turns_and_asks_before_commands() {
    let h = Harness::new().await;
    let reply = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "codex", "cwd": h.cwd.to_string_lossy(), "modeId": "auto"}}))
        .await;
    assert!(reply["error"].is_null(), "{reply}");
    let agent = reply["agent"]["id"].as_str().unwrap().to_string();
    h.request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent]}))
        .await;

    h.send(&agent, "hello", "m-1").await;
    let pushes = h
        .until("turn_completed", |m| stream_event(m, "turn_completed"))
        .await;
    let started = pushes
        .iter()
        .find(|m| stream_event(m, "turn_started"))
        .unwrap();
    assert_eq!(started["payload"]["event"]["turnId"], "codex-turn-1");
    let completed = pushes.last().unwrap();
    assert_eq!(
        completed["payload"]["event"]["usage"]["contextWindowMaxTokens"],
        258400
    );
    let snapshot = h.agent(&agent).await;
    assert_eq!(snapshot["persistence"]["sessionId"], "thr-1");
    assert_eq!(
        snapshot["runtimeInfo"],
        json!({"provider": "codex", "sessionId": "thr-1", "model": "gpt-6-luna",
               "thinkingOptionId": "medium", "modeId": "auto",
               "extra": {"collaborationMode": "Default"}})
    );
    assert_eq!(snapshot["effectiveThinkingOptionId"], "medium");
    assert_eq!(snapshot["lastUsage"]["contextWindowUsedTokens"], 912);

    h.send(&agent, "run ls", "m-2").await;
    let pushes = h
        .until("permission", |m| m["type"] == "agent_permission_request")
        .await;
    let request = pushes.last().unwrap()["payload"]["request"].clone();
    assert_eq!(request["name"], "CodexBash");
    assert_eq!(request["kind"], "tool");
    assert_eq!(
        request["detail"],
        json!({"type": "shell", "command": "ls", "cwd": "/home/user/repo"})
    );
    h.request(
        json!({"type": "agent_permission_response", "agentId": agent,
        "requestId": request["id"], "response": {"behavior": "allow"}}),
    )
    .await;
    h.until("turn_completed", |m| stream_event(m, "turn_completed"))
        .await;

    let page = h
        .request(
            json!({"type": "fetch_agent_timeline_request", "agentId": agent,
            "direction": "tail", "limit": 40, "projection": "projected"}),
        )
        .await;
    let mut timeline = Timeline::default();
    timeline.apply_page(&page);
    let shown: Vec<String> = timeline
        .items()
        .map(|item| match &item.kind {
            ItemKind::User { text, .. } => format!("user:{text}"),
            ItemKind::Assistant { text, .. } => format!("assistant:{text}"),
            ItemKind::Tool(call) => format!("tool:{}:{}", call.name, call.status),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        shown,
        [
            "user:hello",
            "assistant:echo: hello",
            "user:run ls",
            "tool:shell:completed",
            "assistant:listed"
        ]
    );
    h.stop().await;
}
