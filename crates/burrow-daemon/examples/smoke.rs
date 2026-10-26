//! Drives one real agent through a daemon (burrow or Paseo) the way
//! crusteo would, and logs every frame for comparison.
//!
//! ```text
//! cargo run -p burrow-daemon --example smoke -- <endpoint> <cwd> <log.jsonl> first [provider] [model]
//! cargo run -p burrow-daemon --example smoke -- <endpoint> <cwd> <log.jsonl> pi <model>
//! cargo run -p burrow-daemon --example smoke -- <endpoint> <cwd> <log.jsonl> cursor <model>
//! cargo run -p burrow-daemon --example smoke -- <endpoint> <cwd> <log.jsonl> resume <agent-id> [question]
//! ```
//!
//! `first` creates an agent and runs four short turns: a plain answer, a
//! shell command that needs permission, a question, and an interrupted turn.
//! With the `codex` provider the question is left out (Codex asks only in
//! plan mode), the mode is `auto` and the reasoning effort `low`.
//! `agy` does the same for Antigravity, which runs every tool without asking
//! and has no questions: a plain answer, a shell command, and an interrupted
//! turn (`cargo run ... -- <endpoint> <cwd> <log.jsonl> agy [model]`).
//! `pi` does the same with a pi agent, without the permission and the
//! question: pi asks for neither.
//! `cursor` runs a Cursor agent (ACP) in its own default mode: a plain answer,
//! a shell command allowed with the agent's own allow action, and an
//! interrupted turn. ACP has no question tool.
//! `resume` (after the daemon restarted) checks the agent is still there with
//! the same history, asks it about its first answer, and archives it. Keep the
//! model cheap: every turn is a real model call.

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use paseo_client::{Client, ConnState, Event};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::timeout;

const TURN: Duration = Duration::from_secs(180);

struct Log {
    file: Mutex<std::fs::File>,
    seen: Mutex<Vec<Value>>,
    changed: Notify,
}

impl Log {
    fn write(&self, dir: &str, frame: &Value) {
        let mut file = self.file.lock().unwrap();
        let _ = writeln!(file, "{}", json!({"dir": dir, "frame": frame}));
    }

    /// Waits for a pushed message after `from` that matches; returns its index.
    async fn wait(
        &self,
        what: &str,
        from: usize,
        matches: impl Fn(&Value) -> bool,
    ) -> (usize, Value) {
        let found = timeout(TURN, async {
            loop {
                let notified = self.changed.notified();
                {
                    let seen = self.seen.lock().unwrap();
                    if let Some(at) = (from..seen.len()).find(|&i| matches(&seen[i])) {
                        return (at, seen[at].clone());
                    }
                }
                notified.await;
            }
        })
        .await;
        match found {
            Ok(found) => {
                eprintln!("  ✓ {what}");
                found
            }
            Err(_) => panic!("no {what} within {TURN:?}"),
        }
    }

    fn len(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

fn stream(message: &Value, kind: &str, agent: &str) -> bool {
    message["type"] == "agent_stream"
        && message["payload"]["agentId"] == agent
        && message["payload"]["event"]["type"] == kind
}

fn turn_end(message: &Value, agent: &str) -> bool {
    ["turn_completed", "turn_failed", "turn_canceled"]
        .iter()
        .any(|kind| stream(message, kind, agent))
}

struct Driver {
    client: Client,
    log: Arc<Log>,
}

impl Driver {
    async fn request(&self, mut message: Value) -> Value {
        // A permission answer's requestId is the permission id: keep it.
        if message.get("requestId").is_none() {
            message["requestId"] = paseo_client::new_request_id().into();
        }
        self.log.write("request", &message);
        let reply = self
            .client
            .request_with_timeout(message.clone(), Duration::from_secs(120))
            .await;
        let reply = match reply {
            Ok(payload) => payload,
            Err(err) => json!({"error": err.to_string()}),
        };
        self.log
            .write("reply", &json!({"to": message["type"], "payload": reply}));
        reply
    }

    async fn send(&self, agent: &str, text: &str) -> usize {
        let from = self.log.len();
        let prompt = fleet::prompt::Prompt {
            text: text.into(),
            images: Vec::new(),
        };
        let id = uuid::Uuid::new_v4().to_string();
        let reply = self
            .request(fleet::requests::send_message(agent, &id, &prompt, None))
            .await;
        assert_eq!(reply["accepted"], true, "send refused: {reply}");
        from
    }

    async fn tail(&self, agent: &str) -> Value {
        self.request(
            json!({"type": "fetch_agent_timeline_request", "agentId": agent,
            "direction": "tail", "limit": 40, "projection": "projected"}),
        )
        .await
    }
}

fn reply_text(page: &Value) -> Vec<String> {
    let mut timeline = fleet::timeline::Timeline::default();
    timeline.apply_page(page);
    timeline
        .items()
        .map(|item| match &item.kind {
            fleet::timeline::ItemKind::User { text, .. } => format!("user: {text}"),
            fleet::timeline::ItemKind::Assistant { text, .. } => format!("assistant: {text}"),
            fleet::timeline::ItemKind::Tool(call) => format!("tool: {} {}", call.name, call.status),
            fleet::timeline::ItemKind::Thought { .. } => "thought".into(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [endpoint, cwd, log_path, phase] = [&args[0], &args[1], &args[2], &args[3]];
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .unwrap();
    let log = Arc::new(Log {
        file: Mutex::new(file),
        seen: Mutex::new(Vec::new()),
        changed: Notify::new(),
    });
    let client = Client::connect(paseo_client::Config::new(
        endpoint.clone(),
        paseo_client::generate_client_id(),
    ));
    let events = client.events();
    let info = loop {
        if let Event::State(ConnState::Connected(info)) = events.recv().await.unwrap() {
            break info;
        }
    };
    eprintln!("connected to {} {}", info.server_id, info.version);
    log.write("server_info", &info.raw);
    let pump = log.clone();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if let Event::Message(message) = event {
                pump.write("push", &message);
                pump.seen.lock().unwrap().push(message);
                pump.changed.notify_waiters();
            }
        }
    });
    let d = Driver { client, log };
    d.request(
        json!({"type": "session.events.set_subscription.request", "events": [
        "agent_permission_request", "agent_permission_resolved", "agent_attention_required"]}),
    )
    .await;
    d.request(
        json!({"type": "fetch_agents_request", "scope": "active", "subscribe": {},
        "sort": [{"key": "updated_at", "direction": "desc"}], "page": {"limit": 200}}),
    )
    .await;

    match phase.as_str() {
        "first" => first(&d, cwd, args.get(4), args.get(5), false).await,
        // Just the first turn: a cheap check of shapes.
        "one" => first(&d, cwd, args.get(4), args.get(5), true).await,
        "agy" => agy(&d, cwd, args.get(4)).await,
        "pi" => pi(&d, cwd, &args[4]).await,
        "cursor" => cursor(&d, cwd, &args[4]).await,
        "resume" => resume(&d, &args[4], args.get(5)).await,
        other => panic!("unknown phase {other}"),
    }
}

/// Waits until the daemon offers `provider` as ready, with its models.
async fn offered(d: &Driver, provider: &str) -> fleet::providers::Provider {
    // Paseo probes providers in the background and says "loading" at first.
    for _ in 0..60 {
        let snapshot = d
            .request(json!({"type": "get_providers_snapshot_request"}))
            .await;
        if let Some(offered) = fleet::providers::parse(&snapshot)
            .into_iter()
            .find(|p| p.id == provider && p.ready() && !p.models.is_empty())
        {
            eprintln!("provider {provider}: {} models", offered.models.len());
            return offered;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    panic!("provider {provider} not ready within two minutes");
}

async fn agy(d: &Driver, cwd: &str, model: Option<&String>) {
    let model = model.map_or("gemini-3.8-flash-low", String::as_str);
    let offered = offered(d, "antigravity").await;
    assert!(
        offered.models.iter().any(|m| m.id == model),
        "{model} is not offered"
    );
    let created = d
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "antigravity", "cwd": cwd, "model": model, "modeId": "full-access"}}))
        .await;
    let agent = created["agent"]["id"]
        .as_str()
        .expect("created")
        .to_string();
    eprintln!("AGENT {agent}");
    d.request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent]}))
        .await;

    eprintln!("turn 1: a plain answer");
    let from = d.send(&agent, "Reply with exactly one word: ready").await;
    d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;

    eprintln!("turn 2: a shell command, run without asking");
    let from = d
        .send(
            &agent,
            "Use your command tool to run exactly this command: echo smoke > smoke.txt\nThen reply with the word done.",
        )
        .await;
    let (_, call) = d
        .log
        .wait("shell tool call", from, |m| {
            stream(m, "timeline", &agent)
                && m["payload"]["event"]["item"]["type"] == "tool_call"
                && m["payload"]["event"]["item"]["detail"]["type"] == "shell"
        })
        .await;
    eprintln!(
        "  {} {}",
        call["payload"]["event"]["item"]["name"], call["payload"]["event"]["item"]["detail"]
    );
    d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;
    let written = std::fs::read_to_string(std::path::Path::new(cwd).join("smoke.txt"));
    eprintln!("  smoke.txt: {:?}", written.map(|s| s.trim().to_string()));

    // Only full access exists; a model change applies from the next turn.
    for mode in ["plan", "full-access"] {
        let reply = d
            .request(json!({"type": "set_agent_mode_request", "agentId": agent, "modeId": mode}))
            .await;
        eprintln!(
            "  mode {mode}: accepted {} {}",
            reply["accepted"], reply["error"]
        );
    }
    let switched = model.replace("-low", "-medium");
    let reply = d
        .request(json!({"type": "set_agent_model_request", "agentId": agent, "modelId": switched}))
        .await;
    eprintln!("  model {switched}: accepted {}", reply["accepted"]);

    eprintln!("turn 3: interrupted, on the new model");
    let from = d
        .send(
            &agent,
            "Count from 1 to 300, one number per line, with no other text.",
        )
        .await;
    d.log
        .wait("first text", from, |m| {
            stream(m, "timeline", &agent)
                && m["payload"]["event"]["item"]["type"] == "assistant_message"
        })
        .await;
    let canceled = d
        .request(json!({"type": "cancel_agent_request", "agentId": agent}))
        .await;
    eprintln!("  status after cancel: {}", canceled["agent"]["status"]);
    let (_, end) = d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;
    eprintln!("  {}", end["payload"]["event"]);

    let page = d.tail(&agent).await;
    eprintln!(
        "epoch {} maxSeq {}",
        page["epoch"], page["window"]["maxSeq"]
    );
    for line in reply_text(&page) {
        eprintln!(
            "  | {}",
            line.chars()
                .take(100)
                .collect::<String>()
                .replace('\n', " / ")
        );
    }
    let agent_now = d
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    eprintln!(
        "status {} mode {} model {} usage {} session {}",
        agent_now["agent"]["status"],
        agent_now["agent"]["currentModeId"],
        agent_now["agent"]["model"],
        agent_now["agent"]["lastUsage"],
        agent_now["agent"]["persistence"]["sessionId"]
    );
}

async fn first(
    d: &Driver,
    cwd: &str,
    provider: Option<&String>,
    model: Option<&String>,
    only_one: bool,
) {
    let provider = provider.map_or("claude", String::as_str);
    let model = model.map_or("claude-haiku-4-5", String::as_str);
    // Paseo probes providers in the background and says "loading" at first.
    let mut ready = None;
    for _ in 0..60 {
        let snapshot = d
            .request(json!({"type": "get_providers_snapshot_request"}))
            .await;
        let providers = fleet::providers::parse(&snapshot);
        if let Some(offered) = providers
            .into_iter()
            .find(|p| p.id == provider && p.ready())
        {
            ready = Some(offered);
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let offered = ready.expect("provider ready within two minutes");
    eprintln!("provider {provider}: {} models", offered.models.len());

    let codex = provider == "codex";
    let mut config = json!({"provider": provider, "cwd": cwd, "model": model, "modeId": "default"});
    if codex {
        config["modeId"] = "auto".into();
        config["thinkingOptionId"] = "low".into();
    }
    let created = d
        .request(json!({"type": "agent.create.request", "labels": {}, "config": config}))
        .await;
    let agent = created["agent"]["id"]
        .as_str()
        .expect("created")
        .to_string();
    eprintln!("AGENT {agent}");
    d.request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent]}))
        .await;

    eprintln!("turn 1: a plain answer");
    let from = d.send(&agent, "Reply with exactly one word: ready").await;
    d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;
    if only_one {
        d.tail(&agent).await;
        d.request(json!({"type": "fetch_agent_request", "agentId": agent}))
            .await;
        return;
    }

    eprintln!("turn 2: a command that needs permission");
    let command = if codex {
        // The workspace sandbox lets Codex write here without asking, so ask it to ask.
        "Run this shell command to create a file: echo smoke > smoke.txt (it needs approval; ask for it). Then reply with the word done."
    } else {
        "Use the Bash tool to run exactly this command: echo smoke > smoke.txt\nThen reply with the word done."
    };
    let from = d.send(&agent, command).await;
    let (_, asked) = d
        .log
        .wait("permission request", from, |m| {
            m["type"] == "agent_permission_request" && m["payload"]["agentId"] == agent
        })
        .await;
    let request = asked["payload"]["request"].clone();
    eprintln!("  kind {} name {}", request["kind"], request["name"]);
    d.request(json!({"type": "agent_permission_response", "agentId": agent,
        "requestId": request["id"], "response": {"behavior": "allow", "selectedActionId": "accept"}}))
        .await;
    d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;
    let written = std::fs::read_to_string(std::path::Path::new(cwd).join("smoke.txt"));
    eprintln!("  smoke.txt: {:?}", written.map(|s| s.trim().to_string()));

    if !codex {
        question(d, &agent).await;
    }

    eprintln!("turn 4: interrupted");
    let from = d
        .send(
            &agent,
            "Count from 1 to 300, one number per line, with no other text.",
        )
        .await;
    d.log
        .wait("first text", from, |m| {
            stream(m, "timeline", &agent)
                && m["payload"]["event"]["item"]["type"] == "assistant_message"
        })
        .await;
    let canceled = d
        .request(json!({"type": "cancel_agent_request", "agentId": agent}))
        .await;
    eprintln!("  status after cancel: {}", canceled["agent"]["status"]);
    d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;

    let page = d.tail(&agent).await;
    eprintln!(
        "epoch {} maxSeq {}",
        page["epoch"], page["window"]["maxSeq"]
    );
    for line in reply_text(&page) {
        eprintln!(
            "  | {}",
            line.chars()
                .take(100)
                .collect::<String>()
                .replace('\n', " / ")
        );
    }
    let agent_now = d
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    eprintln!(
        "status {} attention {} session {}",
        agent_now["agent"]["status"],
        agent_now["agent"]["attentionReason"],
        agent_now["agent"]["persistence"]["sessionId"]
    );
}

async fn question(d: &Driver, agent: &str) {
    let agent = agent.to_string();
    eprintln!("turn 3: a question");
    let from = d
        .send(
            &agent,
            "Use the AskUserQuestion tool to ask me one question, with header Color and the options red and blue. After I answer, reply with only the color I picked.",
        )
        .await;
    let (_, asked) = d
        .log
        .wait("question", from, |m| {
            m["type"] == "agent_permission_request" && m["payload"]["agentId"] == agent
        })
        .await;
    let request = asked["payload"]["request"].clone();
    assert!(
        fleet::questions::is_question(&request),
        "not a question: {request}"
    );
    let questions = fleet::questions::parse(&request["input"]).expect("questions");
    let blue = questions[0]
        .options
        .iter()
        .position(|o| o.label.eq_ignore_ascii_case("blue"))
        .unwrap_or(0);
    let selected = vec![BTreeSet::from([blue]); questions.len()];
    let answers =
        fleet::questions::answers(&questions, &selected, &vec![String::new(); questions.len()]);
    let response = fleet::questions::submit(&request, answers);
    d.request(
        json!({"type": "agent_permission_response", "agentId": agent,
        "requestId": request["id"], "response": response}),
    )
    .await;
    d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;
}

/// Ends the next turn after the first streamed text.
async fn interrupted(d: &Driver, agent: &str, text: &str) {
    let from = d.send(agent, text).await;
    d.log
        .wait("first text", from, |m| {
            stream(m, "timeline", agent)
                && m["payload"]["event"]["item"]["type"] == "assistant_message"
        })
        .await;
    let canceled = d
        .request(json!({"type": "cancel_agent_request", "agentId": agent}))
        .await;
    eprintln!("  status after cancel: {}", canceled["agent"]["status"]);
    d.log.wait("turn end", from, |m| turn_end(m, agent)).await;
}

/// Prints the agent's timeline and state.
async fn summary(d: &Driver, agent: &str) {
    let page = d.tail(agent).await;
    eprintln!(
        "epoch {} maxSeq {}",
        page["epoch"], page["window"]["maxSeq"]
    );
    for line in reply_text(&page) {
        eprintln!(
            "  | {}",
            line.chars()
                .take(100)
                .collect::<String>()
                .replace('\n', " / ")
        );
    }
    let agent_now = d
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    eprintln!(
        "status {} attention {} session {} usage {}",
        agent_now["agent"]["status"],
        agent_now["agent"]["attentionReason"],
        agent_now["agent"]["persistence"]["sessionId"],
        agent_now["agent"]["lastUsage"]
    );
}

async fn pi(d: &Driver, cwd: &str, model: &str) {
    offered(d, "pi").await;
    let created = d
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "pi", "cwd": cwd, "model": model, "thinkingOptionId": "off"}}))
        .await;
    let agent = created["agent"]["id"]
        .as_str()
        .expect("created")
        .to_string();
    eprintln!("AGENT {agent}");
    d.request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent]}))
        .await;

    eprintln!("turn 1: a plain answer");
    let from = d.send(&agent, "Reply with exactly one word: ready").await;
    d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;

    eprintln!("turn 2: a shell command");
    let from = d
        .send(
            &agent,
            "Use the bash tool to run exactly this command: echo smoke > smoke.txt\nThen reply with the word done.",
        )
        .await;
    d.log
        .wait("tool call", from, |m| {
            stream(m, "timeline", &agent)
                && m["payload"]["event"]["item"]["type"] == "tool_call"
                && m["payload"]["event"]["item"]["status"] == "completed"
        })
        .await;
    d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;
    let written = std::fs::read_to_string(std::path::Path::new(cwd).join("smoke.txt"));
    eprintln!("  smoke.txt: {:?}", written.map(|s| s.trim().to_string()));

    eprintln!("turn 3: interrupted");
    interrupted(
        d,
        &agent,
        "Write a 600-word story about a lighthouse keeper. Do not use any tools.",
    )
    .await;
    summary(d, &agent).await;
}

async fn cursor(d: &Driver, cwd: &str, model: &str) {
    offered(d, "cursor").await;
    // No mode: the session's own (`agent`) stands, as in Paseo.
    let created = d
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "cursor", "cwd": cwd, "model": model}}))
        .await;
    let agent = created["agent"]["id"]
        .as_str()
        .expect("created")
        .to_string();
    eprintln!("AGENT {agent}");
    d.request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent]}))
        .await;

    eprintln!("turn 1: a plain answer");
    let from = d.send(&agent, "Reply with exactly one word: ready").await;
    d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;

    eprintln!("turn 2: a command that needs permission");
    let from = d
        .send(
            &agent,
            "Use your shell tool to run exactly this command: echo smoke > smoke.txt\nThen reply with the word done.",
        )
        .await;
    let (_, asked) = d
        .log
        .wait("permission request", from, |m| {
            m["type"] == "agent_permission_request" && m["payload"]["agentId"] == agent
        })
        .await;
    let request = asked["payload"]["request"].clone();
    eprintln!("  kind {} name {}", request["kind"], request["name"]);
    // The agent's own first allow action (`allow-once` for Cursor).
    let action = request["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["behavior"] == "allow")
        .and_then(|a| a["id"].as_str())
        .expect("an allow action")
        .to_string();
    d.request(
        json!({"type": "agent_permission_response", "agentId": agent,
        "requestId": request["id"], "response": {"behavior": "allow", "selectedActionId": action}}),
    )
    .await;
    d.log.wait("turn end", from, |m| turn_end(m, &agent)).await;
    let written = std::fs::read_to_string(std::path::Path::new(cwd).join("smoke.txt"));
    eprintln!("  smoke.txt: {:?}", written.map(|s| s.trim().to_string()));

    eprintln!("turn 3: interrupted");
    interrupted(
        d,
        &agent,
        "Count from 1 to 300, one number per line, with no other text.",
    )
    .await;
    summary(d, &agent).await;
}

async fn resume(d: &Driver, agent: &str, question: Option<&String>) {
    let found = d
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    eprintln!("after restart: status {}", found["agent"]["status"]);
    let before = d.tail(agent).await;
    eprintln!(
        "epoch {} maxSeq {}",
        before["epoch"], before["window"]["maxSeq"]
    );
    d.request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent]}))
        .await;
    eprintln!("turn 5: memory across the restart");
    let question = question.map_or(
        "What single word did you reply with in your very first answer in this conversation? Reply with only that word.",
        String::as_str,
    );
    let from = d.send(agent, question).await;
    d.log.wait("turn end", from, |m| turn_end(m, agent)).await;
    let after = d.tail(agent).await;
    eprintln!(
        "epoch {} (same: {}) maxSeq {}",
        after["epoch"],
        after["epoch"] == before["epoch"],
        after["window"]["maxSeq"]
    );
    for line in reply_text(&after).iter().rev().take(3) {
        eprintln!("  | {}", line.chars().take(100).collect::<String>());
    }
    let archived = d
        .request(json!({"type": "archive_agent_request", "agentId": agent}))
        .await;
    eprintln!("archived at {}", archived["archivedAt"]);
}
