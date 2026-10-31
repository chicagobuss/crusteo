//! Notifications to Paseo's apps, end to end: a client that is there is
//! told to show one, and with nobody there the registered phones get a
//! push, here through a stand-in for Expo's push service.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::{Config, Daemon, Running, start};
use burrow_provider::echo::EchoProvider;
use paseo_client::{Client, ConnState, Event, EventReceiver};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);
const TOKEN: &str = "ExponentPushToken[test-phone]";

/// Answers every POST with `reply` and hands over its body.
async fn push_service(reply: Value) -> (String, mpsc::UnboundedReceiver<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/--/api/v2/push/send",
        listener.local_addr().unwrap()
    );
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let body = loop {
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                let Some((head, body)) = text.split_once("\r\n\r\n") else {
                    continue;
                };
                let length: usize = (head.lines())
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if body.len() >= length {
                    break Some(body[..length].to_string());
                }
            };
            if let Some(body) = body {
                let _ = tx.send(serde_json::from_str(&body).unwrap_or(Value::Null));
            }
            let text = reply.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (url, rx)
}

struct Harness {
    running: Running,
    daemon: Arc<Daemon>,
    client: Client,
    events: EventReceiver,
    cwd: PathBuf,
}

async fn harness(push_url: String) -> Harness {
    let mut config = Config::ephemeral();
    config.push_url = Some(push_url);
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
    let cwd = std::env::temp_dir().join(format!("burrow-push-{}", burrow_daemon::ids::agent_id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let h = Harness {
        running,
        daemon,
        client,
        events,
        cwd,
    };
    // As Paseo's app subscribes.
    h.request(json!({"type": "session.events.set_subscription.request",
        "events": ["agent_attention_required"], "notifications": true}))
        .await;
    h
}

impl Harness {
    async fn request(&self, message: Value) -> Value {
        self.client.request(message).await.expect("request")
    }

    async fn heartbeat(&self, focused: Option<&str>, visible: bool) {
        self.client
            .send(json!({"type": "client_heartbeat", "deviceType": "mobile",
            "focusedAgentId": focused, "lastActivityAt": burrow_daemon::clock::now_iso(),
            "appVisible": visible}));
        // A heartbeat has no reply: a ping behind it shows it was read.
        self.request(json!({"type": "ping"})).await;
    }

    /// Makes an echo agent and runs one turn; returns its id and the
    /// attention it raised.
    async fn finish_a_turn(&self) -> (String, Value) {
        let reply = self
            .request(json!({"type": "agent.create.request", "labels": {},
                "config": {"provider": "echo", "cwd": self.cwd.to_string_lossy()}}))
            .await;
        let agent = reply["agent"]["id"].as_str().unwrap().to_string();
        let prompt = fleet::prompt::Prompt {
            text: "all done here".into(),
            images: Vec::new(),
        };
        self.request(fleet::requests::send_message(&agent, "m-1", &prompt, None))
            .await;
        let attention = timeout(WAIT, async {
            loop {
                if let Event::Message(m) = self.events.recv().await.unwrap()
                    && m["type"] == "agent_attention_required"
                    && m["payload"]["agentId"] == agent
                {
                    return m["payload"].clone();
                }
            }
        })
        .await
        .expect("attention");
        (agent, attention)
    }

    async fn stop(self) {
        drop(self.client);
        self.running.shutdown().await;
        self.daemon.close();
        let _ = std::fs::remove_dir_all(&self.cwd);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_nobody_in_the_app_the_phone_gets_a_push() {
    let (url, mut pushes) = push_service(json!({"data": [{"status": "ok", "id": "t1"}]})).await;
    let h = harness(url).await;
    h.client
        .send(json!({"type": "register_push_token", "token": TOKEN}));
    let (agent, attention) = h.finish_a_turn().await;
    // No heartbeat: nobody is looking, so no client shows it.
    assert_eq!(attention["shouldNotify"], false);
    let sent = timeout(WAIT, pushes.recv()).await.expect("a push").unwrap();
    assert_eq!(sent[0]["to"], TOKEN);
    assert_eq!(sent[0]["title"], "Agent finished");
    assert_eq!(sent[0]["body"], "echo: all done here");
    assert_eq!(sent[0]["sound"], "default");
    assert_eq!(sent[0]["data"]["agentId"], agent.as_str());
    assert_eq!(sent[0]["data"]["reason"], "finished");
    assert_eq!(
        sent[0]["data"]["serverId"],
        h.daemon.config.server_id.as_str()
    );
    assert!(!sent[0]["data"]["workspaceId"].is_null());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_in_the_app_shows_it_instead_and_one_on_the_agent_hears_nothing() {
    let (url, mut pushes) = push_service(json!({"data": [{"status": "ok"}]})).await;
    let h = harness(url).await;
    h.client
        .send(json!({"type": "register_push_token", "token": TOKEN}));
    h.heartbeat(None, true).await;
    let (agent, attention) = h.finish_a_turn().await;
    assert_eq!(attention["shouldNotify"], true);
    assert!(attention.get("\u{0}notifyConnection").is_none());
    // Looking at that agent, which clears its attention: no notification
    // at all for its next turn.
    h.heartbeat(Some(&agent), true).await;
    h.request(json!({"type": "clear_agent_attention", "agentId": agent}))
        .await;
    let prompt = fleet::prompt::Prompt {
        text: "again".into(),
        images: Vec::new(),
    };
    h.request(fleet::requests::send_message(&agent, "m-2", &prompt, None))
        .await;
    let second = timeout(WAIT, async {
        loop {
            if let Event::Message(m) = h.events.recv().await.unwrap()
                && m["type"] == "agent_attention_required"
            {
                return m["payload"].clone();
            }
        }
    })
    .await
    .expect("the second attention");
    assert_eq!(second["shouldNotify"], false);
    assert!(
        timeout(Duration::from_millis(500), pushes.recv())
            .await
            .is_err(),
        "no push while someone is in the app"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_expo_rejects_or_the_phone_unregisters_is_dropped() {
    let rejected = json!({"data": [{"status": "error", "message": "gone",
        "details": {"error": "DeviceNotRegistered"}}]});
    let (url, mut pushes) = push_service(rejected).await;
    let h = harness(url).await;
    h.client
        .send(json!({"type": "register_push_token", "token": TOKEN}));
    h.request(json!({"type": "ping"})).await;
    assert_eq!(h.daemon.push_tokens.active(), [TOKEN]);
    h.finish_a_turn().await;
    timeout(WAIT, pushes.recv()).await.expect("a push");
    timeout(WAIT, async {
        while !h.daemon.push_tokens.active().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the rejected token dropped");

    h.client
        .send(json!({"type": "register_push_token", "token": "other"}));
    let reply = h
        .request(json!({"type": "push.unregister.request", "token": "other"}))
        .await;
    assert!(reply.get("requestId").is_some(), "{reply}");
    assert!(h.daemon.push_tokens.active().is_empty());
    let info = h.daemon.server_info().frame();
    assert_eq!(
        info["message"]["payload"]["features"]["pushTokenRevocation"],
        true
    );
    h.stop().await;
}
