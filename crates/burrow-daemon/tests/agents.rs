//! The agent manager end to end, with the echo provider (no model) and
//! Crusteo's own client code: `paseo-client` for the wire and `fleet` for the
//! conversation model the desktop app and Reef render from.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::{Config, Daemon, Running, start};
use burrow_provider::echo::EchoProvider;
use fleet::timeline::{ItemKind, Timeline};
use paseo_client::{Client, ConnState, Error, Event, EventReceiver};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

struct Harness {
    running: Running,
    daemon: Arc<Daemon>,
    client: Client,
    events: EventReceiver,
    cwd: PathBuf,
}

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("burrow-{name}-{}", burrow_daemon::ids::agent_id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn harness_at(home: Option<&Path>, cwd: PathBuf, server_id: &str) -> Harness {
    let mut config = Config::ephemeral();
    config.home = home.map(Path::to_path_buf);
    config.server_id = server_id.into();
    let daemon = Daemon::open(config).unwrap();
    daemon.agents.register(Arc::new(EchoProvider {
        chunk_delay: Duration::from_millis(5),
        slow_delay: Duration::from_secs(30),
    }));
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
    let harness = Harness {
        running,
        daemon,
        client,
        events,
        cwd,
    };
    harness
        .request(json!({"type": "session.events.set_subscription.request",
            "events": ["agent_permission_request", "agent_permission_resolved",
                       "agent_attention_required"]}))
        .await;
    harness
        .request(json!({"type": "fetch_agents_request", "scope": "active", "subscribe": {}}))
        .await;
    harness
}

async fn harness() -> Harness {
    harness_at(None, scratch("cwd"), &burrow_daemon::ids::server_id()).await
}

impl Harness {
    async fn request(&self, message: Value) -> Value {
        self.client.request(message).await.expect("request")
    }

    /// Every pushed message up to and including the first that matches.
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

    async fn create(&self) -> String {
        let reply = self
            .request(json!({"type": "agent.create.request", "labels": {},
                "config": {"provider": "echo", "cwd": self.cwd.to_string_lossy()}}))
            .await;
        assert!(reply["error"].is_null(), "{reply}");
        assert_eq!(reply["creation"]["phase"], "completed");
        let id = reply["agent"]["id"].as_str().unwrap().to_string();
        self.request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [id]}))
            .await;
        id
    }

    async fn send(&self, agent: &str, text: &str, message_id: &str, behavior: Option<&str>) {
        let prompt = fleet::prompt::Prompt {
            text: text.into(),
            images: Vec::new(),
        };
        let reply = self
            .request(fleet::requests::send_message(
                agent, message_id, &prompt, behavior,
            ))
            .await;
        assert_eq!(reply["accepted"], true, "{reply}");
    }

    async fn tail(&self, agent: &str) -> Value {
        self.request(
            json!({"type": "fetch_agent_timeline_request", "agentId": agent,
            "direction": "tail", "limit": 40, "projection": "projected"}),
        )
        .await
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

/// What the conversation model shows, one line per item.
fn summary(timeline: &Timeline) -> Vec<String> {
    timeline
        .items()
        .map(|item| match &item.kind {
            ItemKind::User { text, .. } => format!("user:{text}"),
            ItemKind::Assistant { text, .. } => format!("assistant:{text}"),
            ItemKind::Tool(call) => format!("tool:{}:{}", call.name, call.status),
            other => format!("{other:?}"),
        })
        .collect()
}

/// A prompt's images are kept and listed on its `user_message`, so a client
/// other than the sender can read them back; nothing else is served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_images_are_kept_for_every_client() {
    let home = scratch("home");
    let h = harness_at(
        Some(&home),
        scratch("cwd"),
        &burrow_daemon::ids::server_id(),
    )
    .await;
    let agent = h.create().await;
    let png = fleet::prompt::ImageAttachment::new("iVBORw0KGgo=", "image/png");
    let prompt = fleet::prompt::Prompt {
        text: "look".into(),
        images: vec![png.clone()],
    };
    let reply = h
        .request(fleet::requests::send_message(
            &agent, "m-img", &prompt, None,
        ))
        .await;
    assert_eq!(reply["accepted"], true, "{reply}");
    h.until("turn_completed", |m| stream_event(m, "turn_completed"))
        .await;

    let mut history = Timeline::default();
    history.apply_page(&h.tail(&agent).await);
    let files = match &history.items().next().unwrap().kind {
        ItemKind::User {
            text,
            images,
            image_files,
            ..
        } => {
            assert_eq!(text, "look");
            assert!(images.is_empty(), "the bytes are not sent inline");
            image_files.clone()
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(files.len(), 1);
    let kept = home.join("attachments");
    assert!(
        files[0].path.starts_with(&*kept.to_string_lossy()),
        "{files:?}"
    );
    let read = h.request(files[0].request()).await;
    assert_eq!(fleet::images::image_from_reply(&read), Ok(png));

    std::fs::write(h.cwd.join("secret.png"), b"x").unwrap();
    let outside = fleet::requests::read_file(&h.cwd.to_string_lossy(), "secret.png", 1024);
    let refused = h.request(outside).await;
    assert!(refused["file"].is_null(), "{refused}");
    assert!(refused["error"].is_string(), "{refused}");
    h.stop().await;
    std::fs::remove_dir_all(home).unwrap();
}

/// Paseo 0.12's provenance: a message one agent sends another reaches the
/// provider in an envelope naming the sender, and the timeline has it as an
/// `agent_message` call from the sender, not as the user's prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_from_another_agent_says_who_sent_it() {
    let h = harness().await;
    let sender = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "echo", "cwd": h.cwd.to_string_lossy(), "title": "Plan & ship"}}))
        .await["agent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let agent = h.create().await;
    let reply = h
        .request(
            json!({"type": "send_agent_message_request", "agentId": agent,
            "text": "Check the <ledger>", "messageId": "m-from", "sourceAgentId": sender}),
        )
        .await;
    assert_eq!(reply["accepted"], true, "{reply}");
    h.until("turn_completed", |m| stream_event(m, "turn_completed"))
        .await;

    let page = h.tail(&agent).await;
    let items: Vec<&Value> = (page["entries"].as_array().unwrap().iter())
        .map(|entry| &entry["item"])
        .collect();
    assert_eq!(items[0]["type"], "tool_call");
    assert_eq!(items[0]["name"], "agent_message");
    assert_eq!(items[0]["callId"], "paseo-agent-message:m-from");
    assert_eq!(
        items[0]["agentMessage"],
        json!({"event": "message", "sender": {"id": sender, "title": "Plan & ship"},
               "text": "Check the <ledger>"})
    );
    // The conversation model shows who it came from, not a prompt of the
    // user's; the agent was handed the envelope, escaped as Paseo escapes it.
    let mut history = Timeline::default();
    history.apply_page(&page);
    assert_eq!(
        summary(&history),
        [
            "tool:agent_message:completed".to_string(),
            format!(
                "assistant:echo: <paseo-system version=\"1\" kind=\"agent-message\" \
                 source-agent-id=\"{sender}\" message-id=\"m-from\" \
                 source-agent-title=\"Plan &amp; ship\">\nCheck the &lt;ledger&gt;\n</paseo-system>"
            )
        ]
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_reads_the_same_live_and_from_history() {
    let h = harness().await;
    let agent = h.create().await;

    // The live model starts from the (empty) tail page, then takes the stream.
    let mut live = Timeline::default();
    live.apply_page(&h.tail(&agent).await);
    h.send(&agent, "hello  world", "m-1", None).await;
    let pushes = h
        .until("turn_completed", |m| stream_event(m, "turn_completed"))
        .await;
    let streamed: Vec<&Value> = pushes
        .iter()
        .filter(|m| m["type"] == "agent_stream")
        .collect();
    assert_eq!(streamed[0]["payload"]["event"]["type"], "turn_started");
    assert_eq!(
        streamed[0]["payload"]["event"]["turnId"],
        "foreground-turn-1"
    );
    let user = &streamed[1]["payload"];
    assert_eq!(user["event"]["item"]["type"], "user_message");
    assert_eq!(user["event"]["item"]["clientMessageId"], "m-1");
    assert_eq!(user["seq"], 1);
    for message in &streamed {
        live.apply_live(&message["payload"]);
    }

    let mut history = Timeline::default();
    history.apply_page(&h.tail(&agent).await);
    assert_eq!(
        summary(&history),
        ["user:hello  world", "assistant:echo: hello  world"]
    );
    assert_eq!(summary(&live), summary(&history));

    let finished = h
        .until("finished attention", |m| {
            m["type"] == "agent_attention_required"
        })
        .await;
    let attention = &finished.last().unwrap()["payload"];
    assert_eq!(attention["reason"], "finished");
    assert_eq!(attention["notification"]["title"], "Agent finished");
    assert_eq!(attention["notification"]["body"], "echo: hello world");
    let agent_now = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    assert_eq!(agent_now["agent"]["status"], "idle");
    assert_eq!(agent_now["agent"]["title"], "hello world");
    assert_eq!(agent_now["agent"]["requiresAttention"], true);
    assert!(agent_now["agent"]["activeTurn"].is_null());

    let cleared = h
        .request(json!({"type": "clear_agent_attention", "agentId": agent}))
        .await;
    assert_eq!(cleared["agents"][0]["requiresAttention"], false);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permission_waits_for_its_answer() {
    let h = harness().await;
    let agent = h.create().await;
    h.send(&agent, "tool ls -la", "m-1", None).await;
    let pushes = h
        .until("permission request", |m| {
            m["type"] == "agent_permission_request"
        })
        .await;
    let request = pushes.last().unwrap()["payload"]["request"].clone();
    assert_eq!(request["kind"], "tool");
    assert!(request["id"].as_str().unwrap().starts_with("permission-"));
    let snapshot = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    assert_eq!(
        snapshot["agent"]["pendingPermissions"][0]["id"],
        request["id"]
    );
    assert_eq!(
        paseo_client::proto::Agent::from_value(snapshot["agent"].clone())
            .unwrap()
            .bucket(),
        paseo_client::proto::Bucket::NeedsInput
    );

    // Reef's way: wait for the resolution as the reply.
    let resolved = h
        .client
        .request_with_timeout(
            json!({"type": "agent_permission_response", "agentId": agent,
                   "requestId": request["id"],
                   "response": {"behavior": "allow", "selectedActionId": "accept"}}),
            Duration::from_secs(5),
        )
        .await
        .expect("resolution");
    assert_eq!(resolved["requestId"], request["id"]);
    assert_eq!(resolved["resolution"]["behavior"], "allow");

    h.until("turn_completed", |m| stream_event(m, "turn_completed"))
        .await;
    let mut history = Timeline::default();
    history.apply_page(&h.tail(&agent).await);
    assert_eq!(
        summary(&history),
        ["user:tool ls -la", "tool:Bash:completed", "assistant:done"]
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_takes_answers_by_header_and_refuses_empty_ones() {
    let h = harness().await;
    let agent = h.create().await;
    h.send(&agent, "ask Proceed?", "m-1", None).await;
    let pushes = h
        .until("question", |m| m["type"] == "agent_permission_request")
        .await;
    let request = pushes.last().unwrap()["payload"]["request"].clone();
    assert!(fleet::questions::is_question(&request));

    let empty = json!({"type": "agent_permission_response", "agentId": agent,
        "requestId": request["id"], "response": {"behavior": "allow",
        "updatedInput": {"answers": {}}}});
    let refused = h.client.request(empty).await;
    assert!(
        matches!(refused, Err(Error::Rpc { ref message, .. }) if message.contains("question_answer_required")),
        "{refused:?}"
    );

    let questions = fleet::questions::parse(&request["input"]).unwrap();
    let answers = fleet::questions::answers(
        &questions,
        &[std::collections::BTreeSet::from([0])],
        &[String::new()],
    );
    let response = fleet::questions::submit(&request, answers);
    h.request(
        json!({"type": "agent_permission_response", "agentId": agent,
        "requestId": request["id"], "response": response}),
    )
    .await;
    h.until("turn_completed", |m| stream_event(m, "turn_completed"))
        .await;
    let mut history = Timeline::default();
    history.apply_page(&h.tail(&agent).await);
    assert_eq!(
        summary(&history).last().unwrap(),
        r#"assistant:answered: {"Answer":"Yes"}"#
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_for_an_agent_answers_as_paseo_does() {
    let h = harness().await;
    let agent = h.create().await;
    let wait = |timeout_ms: Option<u64>| {
        let mut request = json!({"type": "wait_for_finish_request", "agentId": agent});
        if let Some(ms) = timeout_ms {
            request["timeoutMs"] = ms.into();
        }
        h.request(request)
    };
    // `paseo send` waits right after the send: it answers once the turn ends.
    h.send(&agent, "hi", "m-1", None).await;
    let done = wait(None).await;
    assert_eq!(done["status"], "idle", "{done}");
    assert_eq!(done["lastMessage"], "echo: hi");
    assert_eq!(done["final"]["id"], agent.as_str());
    assert!(done["error"].is_null());
    // Its workspace now has a last prompt, for Paseo 0.13's sort.
    let workspaces = h.request(json!({"type": "fetch_workspaces_request"})).await;
    let workspace = (workspaces["entries"].as_array().unwrap().iter())
        .find(|w| w["id"] == done["final"]["workspaceId"])
        .unwrap();
    assert_eq!(
        workspace["lastUserMessageAt"], done["final"]["lastUserMessageAt"],
        "{workspace}"
    );
    assert!(workspace["lastUserMessageAt"].is_string());

    // A turn still going when the time is up.
    h.send(&agent, "slow x", "m-2", None).await;
    let late = wait(Some(200)).await;
    assert_eq!(late["status"], "timeout", "{late}");
    assert_eq!(late["final"]["status"], "running");
    h.request(json!({"type": "cancel_agent_request", "agentId": agent}))
        .await;

    // A permission ends the wait, as it does in Paseo.
    h.send(&agent, "tool ls", "m-3", None).await;
    let asked = wait(Some(5000)).await;
    assert_eq!(asked["status"], "permission", "{asked}");

    let unknown = h
        .request(json!({"type": "wait_for_finish_request", "agentId": "no-such-agent"}))
        .await;
    assert_eq!(unknown["status"], "error", "{unknown}");
    assert!(unknown["final"].is_null());

    // Paseo 0.13's `sessionMode`: the mode changes before the prompt, and
    // one the agent cannot take refuses the send.
    h.request(
        json!({"type": "agent_permission_response", "agentId": agent,
        "requestId": asked["final"]["pendingPermissions"][0]["id"],
        "response": {"behavior": "deny", "message": "no"}}),
    )
    .await;
    wait(Some(5000)).await;
    let switched = h
        .request(
            json!({"type": "send_agent_message_request", "agentId": agent,
            "text": "hi", "messageId": "m-4", "sessionMode": "bypassPermissions"}),
        )
        .await;
    assert_eq!(switched["accepted"], true, "{switched}");
    let agent_now = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    assert_eq!(agent_now["agent"]["currentModeId"], "bypassPermissions");
    let refused = h
        .request(
            json!({"type": "send_agent_message_request", "agentId": agent,
            "text": "hi", "messageId": "m-5", "sessionMode": "no-such-mode"}),
        )
        .await;
    assert_eq!(refused["accepted"], false, "{refused}");
    h.stop().await;
}

/// What `paseo delete --all` and `paseo detach` send.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_lists_archived_agents_and_detach_drops_the_parent() {
    let h = harness().await;
    let parent = h.create().await;
    let child = h
        .request(
            json!({"type": "agent.create.request", "labels": {}, "callerAgentId": parent,
            "config": {"provider": "echo", "cwd": h.cwd.to_string_lossy()}}),
        )
        .await["agent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let archived = h.create().await;
    h.request(json!({"type": "archive_agent_request", "agentId": archived}))
        .await;

    let page = |cursor: Option<&str>| {
        let mut page = json!({"limit": 2});
        if let Some(cursor) = cursor {
            page["cursor"] = cursor.into();
        }
        h.request(json!({"type": "fetch_agent_history_request", "page": page}))
    };
    let first = page(None).await;
    let cursor = first["pageInfo"]["nextCursor"]
        .as_str()
        .unwrap()
        .to_string();
    let second = page(Some(&cursor)).await;
    assert!(second["pageInfo"]["nextCursor"].is_null(), "{second}");
    let mut ids: Vec<String> = (first["entries"].as_array().unwrap().iter())
        .chain(second["entries"].as_array().unwrap())
        .map(|entry| entry["agent"]["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    let mut all = vec![parent.clone(), child.clone(), archived.clone()];
    all.sort();
    assert_eq!(ids, all, "archived agents are in the history too");
    // `paseo ls -a` asks the directory for them instead.
    let listed =
        |filter: Value| h.request(json!({"type": "fetch_agents_request", "filter": filter}));
    let has = |page: &Value, id: &str| {
        (page["entries"].as_array().unwrap().iter()).any(|e| e["agent"]["id"] == id)
    };
    assert!(has(
        &listed(json!({"includeArchived": true})).await,
        &archived
    ));
    assert!(!has(&listed(json!({})).await, &archived));

    let parent_of = |agent: &Value| agent["labels"]["paseo.parent-agent-id"].clone();
    let before = h
        .request(json!({"type": "fetch_agent_request", "agentId": child}))
        .await;
    assert_eq!(parent_of(&before["agent"]), parent.as_str());
    let detached = h
        .request(json!({"type": "agent.detach.request", "agentId": child}))
        .await;
    assert_eq!(detached["accepted"], true, "{detached}");
    let after = h
        .request(json!({"type": "fetch_agent_request", "agentId": child}))
        .await;
    assert!(parent_of(&after["agent"]).is_null(), "{after}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_joins_the_turn_and_a_plain_send_replaces_it() {
    let h = harness().await;
    let agent = h.create().await;
    h.send(&agent, "slow a", "m-1", None).await;
    h.send(&agent, "b", "m-2", Some("steer")).await;
    let pushes = h
        .until("steered turn end", |m| stream_event(m, "turn_completed"))
        .await;
    assert_eq!(
        pushes
            .iter()
            .filter(|m| stream_event(m, "turn_started"))
            .count(),
        1,
        "steering starts no new turn"
    );

    h.send(&agent, "slow c", "m-3", None).await;
    h.send(&agent, "d", "m-4", None).await;
    let pushes = h
        .until("replacement turn end", |m| {
            stream_event(m, "turn_completed")
        })
        .await;
    let canceled = pushes
        .iter()
        .find(|m| stream_event(m, "turn_canceled"))
        .expect("the old turn is canceled");
    assert_eq!(canceled["payload"]["event"]["turnId"], "foreground-turn-2");

    let mut history = Timeline::default();
    history.apply_page(&h.tail(&agent).await);
    assert_eq!(
        summary(&history),
        [
            // The first chunk left on the leading edge, before the steer.
            "user:slow a",
            "assistant:echo: ",
            "user:b",
            "assistant:a steered: b",
            "user:slow c",
            "assistant:echo: ",
            "user:d",
            "assistant:echo: d",
        ]
    );
    let turns: Vec<_> = history
        .items()
        .map(|i| i.turn_id.clone().unwrap_or_default())
        .collect();
    assert_eq!(turns[0], turns[2], "the steer joined turn 1");
    assert_eq!(turns[6], "foreground-turn-3");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_failure_archive_and_refresh() {
    let h = harness().await;
    let agent = h.create().await;

    h.send(&agent, "slow x", "m-1", None).await;
    let canceled = h
        .request(json!({"type": "cancel_agent_request", "agentId": agent}))
        .await;
    assert_eq!(canceled["agent"]["status"], "idle");
    assert_eq!(canceled["agent"]["attentionReason"], "finished");
    h.until("turn_canceled", |m| stream_event(m, "turn_canceled"))
        .await;

    h.request(json!({"type": "clear_agent_attention", "agentId": agent}))
        .await;
    h.send(&agent, "fail boom", "m-2", None).await;
    let pushes = h
        .until("turn_failed", |m| stream_event(m, "turn_failed"))
        .await;
    let system = pushes
        .iter()
        .find(|m| {
            m["payload"]["event"]["item"]["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("[System Error]"))
        })
        .expect("a system error row");
    assert_eq!(
        system["payload"]["event"]["item"]["text"],
        "[System Error] boom\n\ncode: echo_failure"
    );
    let failed = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    assert_eq!(failed["agent"]["status"], "error");
    assert_eq!(failed["agent"]["attentionReason"], "error");

    let archived = h
        .request(json!({"type": "archive_agent_request", "agentId": agent}))
        .await;
    assert!(archived["archivedAt"].is_string());
    h.until("directory removal", |m| {
        m["type"] == "agent_update" && m["payload"]["kind"] == "remove"
    })
    .await;
    let stored = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    assert!(stored["agent"]["archivedAt"].is_string());
    assert_eq!(stored["agent"]["requiresAttention"], false);

    let refreshed = h
        .request(json!({"type": "refresh_agent_request", "agentId": agent}))
        .await;
    assert_eq!(refreshed["status"], "agent_refreshed");
    // slow x, its first chunk, fail boom, the system error.
    assert_eq!(refreshed["timelineSize"], 4);
    let listed = h
        .request(json!({"type": "fetch_agents_request", "scope": "active"}))
        .await;
    assert_eq!(listed["entries"][0]["agent"]["id"], agent.as_str());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_in_a_git_checkout_is_placed_like_paseo_places_it() {
    let cwd = scratch("repo");
    let initialized = std::process::Command::new("git")
        .args(["init", "-q", "-b", "trunk"])
        .current_dir(&cwd)
        .status()
        .is_ok_and(|s| s.success());
    if !initialized {
        return; // no git here: nothing to check
    }
    let h = harness_at(None, cwd, &burrow_daemon::ids::server_id()).await;
    let agent = h.create().await;
    let found = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    let checkout = &found["project"]["checkout"];
    assert_eq!(checkout["isGit"], true);
    assert_eq!(checkout["currentBranch"], "trunk");
    assert!(checkout["worktreeRoot"].is_string());
    assert!(found["agent"]["runtimeInfo"]["provider"] == "echo");
    assert!(
        found["agent"]
            .as_object()
            .unwrap()
            .contains_key("thinkingOptionId")
    );
    let workspaces = h.request(json!({"type": "fetch_workspaces_request"})).await;
    let workspace = &workspaces["entries"][0];
    assert_eq!(workspace["workspaceKind"], "local_checkout");
    assert_eq!(workspace["gitRuntime"]["currentBranch"], "trunk");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn providers_are_offered_in_both_snapshot_shapes() {
    let h = harness().await;
    // Crusteo advertises compact snapshots.
    let compact = h
        .request(json!({"type": "get_providers_snapshot_request"}))
        .await;
    assert!(compact["entries"].as_array().unwrap().is_empty());
    let providers = fleet::providers::parse(&compact);
    let echo = providers.iter().find(|p| p.id == "echo").unwrap();
    assert!(echo.ready());
    assert_eq!(echo.models[0].id, "echo-1");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_resumes_after_a_restart_with_its_timeline_intact() {
    let home = scratch("home");
    let cwd = scratch("cwd");
    let server_id = burrow_daemon::ids::server_id();

    let h = harness_at(Some(&home), cwd.clone(), &server_id).await;
    let agent = h.create().await;
    h.send(&agent, "one", "m-1", None).await;
    h.until("first turn", |m| stream_event(m, "turn_completed"))
        .await;
    let before = h.tail(&agent).await;
    let session = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await["agent"]["persistence"]["sessionId"]
        .clone();
    h.stop().await;

    let h = harness_at(Some(&home), cwd, &server_id).await;
    let listed = h
        .request(json!({"type": "fetch_agents_request", "scope": "active"}))
        .await;
    assert_eq!(listed["entries"][0]["agent"]["status"], "closed");
    h.request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent]}))
        .await;
    h.send(&agent, "two", "m-2", None).await;
    h.until("second turn", |m| stream_event(m, "turn_completed"))
        .await;
    let after = h.tail(&agent).await;
    assert_eq!(after["epoch"], before["epoch"], "the epoch survives");
    let mut turns: Vec<_> = after["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["turnId"].as_str().unwrap_or_default().to_string())
        .collect();
    turns.dedup();
    assert_eq!(turns, ["foreground-turn-1", "foreground-turn-2"]);
    let seqs: Vec<u64> = after["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seqStart"].as_u64().unwrap())
        .collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "seqs continue"
    );
    let resumed = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    assert_eq!(resumed["agent"]["persistence"]["sessionId"], session);
    h.stop().await;
    std::fs::remove_dir_all(&home).unwrap();
}

fn removal_of(message: &Value, agent: &str) -> bool {
    message["type"] == "agent_update"
        && message["payload"]["kind"] == "remove"
        && message["payload"]["agentId"] == agent
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_running_agent_stops_it_and_forgets_it_for_good() {
    let home = scratch("home");
    let cwd = scratch("cwd");
    let server_id = burrow_daemon::ids::server_id();
    let h = harness_at(Some(&home), cwd.clone(), &server_id).await;
    let agent = h.create().await;
    let workspaces = h
        .request(json!({"type": "fetch_workspaces_request", "subscribe": {}}))
        .await;
    let workspace = workspaces["entries"][0]["id"].as_str().unwrap().to_string();
    h.send(&agent, "one", "m-1", None).await;
    h.until("the first turn's attention", |m| {
        m["type"] == "agent_attention_required"
    })
    .await;
    // Mid-turn: the slow reply waits 30 s before its last chunk.
    h.send(&agent, "slow two", "m-2", None).await;
    assert!(h.daemon.agents.is_live(&agent));
    h.daemon.flush();
    let record = home.join(format!("agents/{agent}.json"));
    let timeline = home.join(format!("timelines/{agent}.jsonl"));
    assert!(record.is_file() && timeline.is_file());

    // Reef's and crusteo's request; Paseo's reply.
    let reply = h.request(fleet::requests::delete_agent(&agent)).await;
    assert_eq!(
        reply,
        json!({"agentId": agent, "requestId": reply["requestId"]})
    );
    let pushes = h
        .until("the directory removal", |m| removal_of(m, &agent))
        .await;
    assert!(!h.daemon.agents.is_live(&agent), "its session is stopped");
    // Closed, not interrupted: no turn end and no attention for it.
    assert!(
        !pushes.iter().any(|m| stream_event(m, "turn_canceled")
            || m["type"] == "agent_attention_required" && m["payload"]["agentId"] == agent),
        "{pushes:#?}"
    );
    // Its workspace stays, described again without it.
    let pushes = h
        .until("its workspace", |m| {
            m["type"] == "workspace_update" && m["payload"]["workspace"]["id"] == workspace
        })
        .await;
    assert_eq!(
        pushes.last().unwrap()["payload"]["workspace"]["status"],
        "done"
    );

    h.daemon.flush();
    assert!(!record.exists(), "its record is gone");
    assert!(!timeline.exists(), "its timeline is gone");
    let found = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    assert!(found["agent"].is_null(), "{found}");
    assert!(h.tail(&agent).await["error"].is_string());
    let send = h
        .client
        .request(
            json!({"type": "send_agent_message_request", "agentId": agent,
                        "text": "three", "messageId": "m-3"}),
        )
        .await
        .unwrap();
    assert_eq!(send["accepted"], false);
    h.stop().await;

    // Not back after a restart.
    let h = harness_at(Some(&home), cwd, &server_id).await;
    let listed = h
        .request(json!({"type": "fetch_agents_request", "scope": "active"}))
        .await;
    assert!(listed["entries"].as_array().unwrap().is_empty(), "{listed}");
    let workspaces = h.request(json!({"type": "fetch_workspaces_request"})).await;
    assert_eq!(workspaces["entries"][0]["id"], workspace.as_str());
    h.stop().await;
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_an_archived_or_unknown_agent_answers_the_same() {
    let home = scratch("home");
    let h = harness_at(
        Some(&home),
        scratch("cwd"),
        &burrow_daemon::ids::server_id(),
    )
    .await;
    let agent = h.create().await;
    h.request(json!({"type": "archive_agent_request", "agentId": agent}))
        .await;
    h.until("the archive's removal", |m| removal_of(m, &agent))
        .await;

    let reply = h.request(fleet::requests::delete_agent(&agent)).await;
    assert_eq!(reply["agentId"], agent.as_str());
    let found = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    assert!(found["agent"].is_null(), "an archived agent goes too");
    h.daemon.flush();
    assert!(!home.join(format!("agents/{agent}.json")).exists());

    // Paseo answers an id it does not know as it answers any other.
    let unknown = h
        .request(fleet::requests::delete_agent("no-such-agent"))
        .await;
    assert_eq!(unknown["agentId"], "no-such-agent");
    // A prefix is not enough to delete anything.
    let other = h.create().await;
    h.request(fleet::requests::delete_agent(&other[..8])).await;
    let kept = h
        .request(json!({"type": "fetch_agent_request", "agentId": other}))
        .await;
    assert_eq!(kept["agent"]["id"], other.as_str());
    h.stop().await;
    std::fs::remove_dir_all(&home).unwrap();
}

/// Echo, with an opening notice as Antigravity has.
struct WithNotice(EchoProvider);

impl burrow_provider::Provider for WithNotice {
    fn info(&self) -> burrow_provider::ProviderInfo {
        self.0.info()
    }
    fn start(
        &self,
        spec: burrow_provider::SessionSpec,
    ) -> Result<burrow_provider::Session, burrow_provider::ProviderError> {
        self.0.start(spec)
    }
    fn opening_items(&self, _spec: &burrow_provider::SessionSpec) -> Vec<Value> {
        vec![json!({"type": "notification", "level": "warning", "message": "Heads up"})]
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_items_come_before_the_first_prompt() {
    let h = harness().await;
    h.daemon
        .agents
        .register(Arc::new(WithNotice(EchoProvider::default())));
    let reply = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "echo", "cwd": h.cwd.to_string_lossy()},
            "initialPrompt": "hello"}))
        .await;
    let agent = reply["agent"]["id"].as_str().unwrap().to_string();
    let rows = timeout(WAIT, async {
        loop {
            let rows: Vec<(Value, Option<String>)> = h.daemon.read(|store| {
                store
                    .timeline(&agent)
                    .map(|t| {
                        t.rows()
                            .iter()
                            .map(|r| (r.item.clone(), r.turn_id.clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            });
            if rows
                .iter()
                .any(|(item, _)| item["type"] == "assistant_message")
            {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a reply");
    assert_eq!(rows[0].0["type"], "notification");
    assert_eq!(rows[0].1, None, "outside any turn");
    assert_eq!(rows[1].0["type"], "user_message");
    h.stop().await;
}

/// A provider that reports what an ACP session does: its own modes, runtime
/// and usage, its own turn and permission ids; and one still probing.
struct SessionReported;

impl burrow_provider::Provider for SessionReported {
    fn info(&self) -> burrow_provider::ProviderInfo {
        burrow_provider::ProviderInfo {
            id: "reported".into(),
            label: "Reported".into(),
            ..Default::default()
        }
    }

    fn start(
        &self,
        spec: burrow_provider::SessionSpec,
    ) -> Result<burrow_provider::Session, burrow_provider::ProviderError> {
        use burrow_provider::{Command, Mode, ProviderEvent};
        let (commands, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, event_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let _ = spec;
            let mode = |id: &str| Mode {
                id: id.into(),
                label: id.to_uppercase(),
                ..Mode::default()
            };
            let _ = events.send(ProviderEvent::ModesAvailable {
                modes: vec![mode("agent"), mode("plan")],
            });
            let _ = events.send(ProviderEvent::ModeChanged {
                mode_id: "agent".into(),
            });
            let _ = events.send(ProviderEvent::RuntimeInfo {
                info: json!({"provider": "reported", "sessionId": "s-1", "model": "m-1",
                             "thinkingOptionId": "high", "modeId": "agent", "extra": {"title": null}}),
            });
            while let Some(command) = command_rx.recv().await {
                match command {
                    Command::Prompt { turn_id, .. } => {
                        let _ = events.send(ProviderEvent::PermissionRequested {
                            id: "p-1".into(),
                            request: json!({"id": "agent-chosen-id", "kind": "tool", "name": "execute"}),
                        });
                        let _ = events.send(ProviderEvent::UsageUpdated {
                            usage: json!({"contextWindowMaxTokens": 100, "contextWindowUsedTokens": 7}),
                        });
                        let _ = events.send(ProviderEvent::TurnCompleted {
                            turn: Some(turn_id),
                            usage: None,
                        });
                    }
                    Command::Shutdown => break,
                    _ => {}
                }
            }
        });
        Ok(burrow_provider::Session {
            commands,
            events: event_rx,
        })
    }

    fn foreground_turn_id(&self, number: u64) -> String {
        format!("custom-{number}")
    }
}

/// Loading until its flag is cleared.
struct Probing(Arc<std::sync::atomic::AtomicBool>);

impl burrow_provider::Provider for Probing {
    fn info(&self) -> burrow_provider::ProviderInfo {
        burrow_provider::ProviderInfo {
            id: "probing".into(),
            label: "Probing".into(),
            ..Default::default()
        }
    }

    fn start(
        &self,
        _: burrow_provider::SessionSpec,
    ) -> Result<burrow_provider::Session, burrow_provider::ProviderError> {
        Err(burrow_provider::ProviderError::Unavailable(
            "probing".into(),
        ))
    }

    fn loading(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_reports_its_own_modes_runtime_and_ids() {
    let h = harness().await;
    h.daemon.agents.register(Arc::new(SessionReported));
    let loading = Arc::new(std::sync::atomic::AtomicBool::new(true));
    h.daemon.agents.register(Arc::new(Probing(loading)));
    let snapshot = h
        .request(json!({"type": "get_providers_snapshot_request"}))
        .await;
    let probing = &snapshot["compactSnapshot"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["provider"] == "probing")
        .unwrap()
        .clone();
    assert_eq!(probing["status"], "loading");
    assert!(probing.get("models").is_none());

    let reply = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "reported", "cwd": h.cwd.to_string_lossy()}}))
        .await;
    let agent = reply["agent"]["id"].as_str().unwrap().to_string();
    h.request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent]}))
        .await;
    h.send(&agent, "go", "m-1", None).await;
    let seen = h
        .until("turn end", |m| stream_event(m, "turn_completed"))
        .await;
    let started = seen
        .iter()
        .find(|m| stream_event(m, "turn_started"))
        .unwrap();
    assert_eq!(started["payload"]["event"]["turnId"], "custom-1");
    let asked = seen
        .iter()
        .find(|m| stream_event(m, "permission_requested"))
        .unwrap();
    assert_eq!(
        asked["payload"]["event"]["request"]["id"],
        "agent-chosen-id"
    );
    let found = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    let snapshot = &found["agent"];
    let modes: Vec<&str> = snapshot["availableModes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(modes, ["agent", "plan"]);
    assert_eq!(snapshot["currentModeId"], "agent");
    assert_eq!(snapshot["runtimeInfo"]["model"], "m-1");
    assert_eq!(snapshot["effectiveThinkingOptionId"], "high");
    assert_eq!(snapshot["lastUsage"]["contextWindowUsedTokens"], 7);
    // A mode only the session listed can be chosen.
    let set = h
        .request(json!({"type": "set_agent_mode_request", "agentId": agent, "modeId": "plan"}))
        .await;
    assert_eq!(set["accepted"], true, "{set}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_that_finishes_loading_is_pushed_to_subscribers() {
    let h = harness().await;
    let loading = Arc::new(std::sync::atomic::AtomicBool::new(true));
    h.daemon.agents.register(Arc::new(Probing(loading.clone())));
    h.request(json!({"type": "session.events.set_subscription.request",
        "events": ["providers_snapshot_update"]}))
        .await;
    burrow_daemon::agents::announce_when_loaded(&h.daemon);
    loading.store(false, std::sync::atomic::Ordering::SeqCst);
    let seen = h
        .until("providers update", |m| {
            m["type"] == "providers_snapshot_update"
        })
        .await;
    let update = &seen.last().unwrap()["payload"];
    assert!(update.get("requestId").is_none());
    // Crusteo reads compact snapshots, and the push comes in its encoding.
    let providers = fleet::providers::parse(update);
    let probing = providers.iter().find(|p| p.id == "probing").unwrap();
    assert!(probing.ready());
    h.stop().await;
}

/// The echo provider as one install of a CLI: its models are those its
/// version offers, and it counts the sessions it starts.
struct Install {
    models: Vec<&'static str>,
    starts: Arc<std::sync::atomic::AtomicUsize>,
}

impl burrow_provider::Provider for Install {
    fn info(&self) -> burrow_provider::ProviderInfo {
        let mut info = EchoProvider::default().info();
        info.id = "upgraded".into();
        info.models = (self.models.iter())
            .map(|id| burrow_provider::Model {
                id: (*id).into(),
                label: (*id).into(),
                ..Default::default()
            })
            .collect();
        info
    }

    fn start(
        &self,
        spec: burrow_provider::SessionSpec,
    ) -> Result<burrow_provider::Session, burrow_provider::ProviderError> {
        self.starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        EchoProvider::default().start(spec)
    }
}

fn models_of(update: &Value, provider: &str) -> Vec<String> {
    let providers = fleet::providers::parse(update);
    let found = providers.iter().find(|p| p.id == provider).unwrap();
    found.models.iter().map(|m| m.id.clone()).collect()
}

/// A CLI upgraded under a running daemon is found again at the next check:
/// clients are pushed its new models, and an idle agent's next prompt runs
/// on the new install. A check that finds the same version finds nothing
/// again, and a client's refresh probes anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgraded_cli_is_offered_without_a_restart() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let h = harness().await;
    let version = Arc::new(Mutex::new("1.0.0".to_string()));
    let finds = Arc::new(AtomicUsize::new(0));
    let (old_starts, new_starts) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let discovery = burrow_provider::Discovery {
        id: "upgraded".into(),
        stamp: Box::new({
            let version = version.clone();
            move || Some(version.lock().unwrap().clone())
        }),
        find: Box::new({
            let (version, finds) = (version.clone(), finds.clone());
            let (old_starts, new_starts) = (old_starts.clone(), new_starts.clone());
            move || {
                finds.fetch_add(1, Ordering::SeqCst);
                let install = match version.lock().unwrap().as_str() {
                    "1.0.0" => Install {
                        models: vec!["m-1"],
                        starts: old_starts.clone(),
                    },
                    _ => Install {
                        models: vec!["m-1", "m-2"],
                        starts: new_starts.clone(),
                    },
                };
                Some(Arc::new(install) as Arc<dyn burrow_provider::Provider>)
            }
        }),
    };
    burrow_daemon::discovery::start(&h.daemon, vec![discovery]).await;
    h.request(json!({"type": "session.events.set_subscription.request",
        "events": ["providers_snapshot_update"]}))
        .await;

    let reply = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "upgraded", "cwd": h.cwd.to_string_lossy()}}))
        .await;
    let agent = reply["agent"]["id"].as_str().unwrap().to_string();
    h.request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent]}))
        .await;
    h.send(&agent, "one", "m-1", None).await;
    h.until("first turn", |m| stream_event(m, "turn_completed"))
        .await;
    assert_eq!(old_starts.load(Ordering::SeqCst), 1);

    // Unchanged: nothing is found again.
    burrow_daemon::discovery::recheck(&h.daemon, None, false).await;
    assert_eq!(finds.load(Ordering::SeqCst), 1);

    *version.lock().unwrap() = "1.1.0".into();
    burrow_daemon::discovery::recheck(&h.daemon, None, false).await;
    assert_eq!(finds.load(Ordering::SeqCst), 2);
    let seen = h
        .until("providers update", |m| {
            m["type"] == "providers_snapshot_update"
        })
        .await;
    assert_eq!(
        models_of(&seen.last().unwrap()["payload"], "upgraded"),
        ["m-1", "m-2"]
    );

    h.send(&agent, "two", "m-2", None).await;
    let seen = h
        .until("second turn", |m| stream_event(m, "turn_completed"))
        .await;
    assert!(seen.iter().any(|m| stream_event(m, "turn_completed")));
    assert_eq!(old_starts.load(Ordering::SeqCst), 1);
    assert_eq!(new_starts.load(Ordering::SeqCst), 1);

    // A client's refresh probes the unchanged CLI, and the snapshot follows.
    let reply = h
        .request(json!({"type": "refresh_providers_snapshot_request", "providers": ["upgraded"]}))
        .await;
    assert_eq!(reply["acknowledged"], true, "{reply}");
    h.until("refreshed providers", |m| {
        m["type"] == "providers_snapshot_update"
    })
    .await;
    assert_eq!(finds.load(Ordering::SeqCst), 3);
    h.stop().await;
}
