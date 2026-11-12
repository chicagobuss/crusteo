//! Prompts for an agent mid-turn: steered in where its provider can be
//! steered, else queued behind the turn and started when it ends, each
//! waited on by its own turn; dropped when the agent is stopped first.

use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::client::Client;
use burrow_daemon::{Config, Daemon, Running, start};
use burrow_provider::echo::EchoProvider;
use burrow_provider::{Provider, ProviderError, ProviderInfo, Session, SessionSpec};
use serde_json::{Value, json};

const WITHIN: Option<Duration> = Some(Duration::from_secs(20));

/// The echo provider as one that cannot be steered (as ACP's and
/// Antigravity's cannot), under its own id.
struct Unsteerable(EchoProvider);

impl Provider for Unsteerable {
    fn info(&self) -> ProviderInfo {
        let mut info = self.0.info();
        info.id = "unsteerable".into();
        info.capabilities.steer = false;
        info
    }

    fn start(&self, spec: SessionSpec) -> Result<Session, ProviderError> {
        self.0.start(spec)
    }
}

struct Harness {
    running: Running,
    client: Client,
    cwd: std::path::PathBuf,
}

async fn harness(slow: Duration) -> Harness {
    let cwd = std::env::temp_dir().join(format!("burrow-queue-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let daemon = Daemon::new(Config::ephemeral());
    let echo = || EchoProvider {
        chunk_delay: Duration::from_millis(2),
        slow_delay: slow,
    };
    daemon.agents.register(Arc::new(echo()));
    daemon.agents.register(Arc::new(Unsteerable(echo())));
    let running = start(daemon).await.expect("start");
    let client = Client::connect(&running.local_addr.to_string())
        .await
        .expect("connect");
    Harness {
        running,
        client,
        cwd,
    }
}

impl Harness {
    async fn create(&mut self, provider: &str) -> String {
        let created = self
            .client
            .request(
                json!({"type": "agent.create.request", "labels": {},
                       "config": {"provider": provider, "cwd": self.cwd.to_string_lossy()}}),
                WITHIN,
            )
            .await
            .expect("create");
        created["agent"]["id"].as_str().expect("an id").to_string()
    }

    async fn send(&mut self, agent: &str, text: &str, behavior: Option<&str>) -> Value {
        let mut message = json!({"type": "send_agent_message_request", "agentId": agent,
                                 "text": text, "messageId": uuid::Uuid::new_v4().to_string()});
        if let Some(behavior) = behavior {
            message["activeTurnBehavior"] = behavior.into();
        }
        let reply = self.client.request(message, WITHIN).await.expect("send");
        assert_eq!(reply["accepted"], true, "{reply}");
        reply
    }

    async fn wait(&mut self, agent: &str, on: (&str, &Value)) -> Value {
        let mut ask = json!({"type": "wait_for_finish_request", "agentId": agent,
                             "timeoutMs": 15_000});
        ask[on.0] = on.1.clone();
        self.client.request(ask, WITHIN).await.expect("wait")
    }

    async fn stop(self) {
        self.client.close().await;
        self.running.shutdown().await;
        let _ = std::fs::remove_dir_all(&self.cwd);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsteerable_agent_takes_queued_prompts_in_order_after_its_turn() {
    let mut h = harness(Duration::from_millis(1500)).await;
    let agent = h.create("unsteerable").await;

    let first = h.send(&agent, "slow a", None).await;
    assert_eq!(first["disposition"], "started", "{first}");
    // An agent's (or Burrow's CLI's) default: it cannot steer, so it waits.
    let second = h.send(&agent, "b", Some("auto")).await;
    assert_eq!(second["disposition"], "queued", "{second}");
    assert_eq!(second["queuePosition"], 1);
    assert!(second["turnId"].is_null(), "no turn yet: {second}");
    let third = h.send(&agent, "c", Some("auto")).await;
    assert_eq!(third["queuePosition"], 2, "{third}");

    // Each wait is for its own turn, though the next starts at once.
    let a = h.wait(&agent, ("turnId", &first["turnId"])).await;
    assert_eq!(a["status"], "idle", "{a}");
    assert_eq!(a["lastMessage"], "echo: a", "{a}");
    let b = h.wait(&agent, ("messageId", &second["messageId"])).await;
    assert_eq!(b["lastMessage"], "echo: b", "{b}");
    let c = h.wait(&agent, ("messageId", &third["messageId"])).await;
    assert_eq!(c["status"], "idle", "{c}");
    assert_eq!(c["lastMessage"], "echo: c", "{c}");
    assert_eq!(c["final"]["status"], "idle", "{c}");

    // Paseo's steer still falls back to interrupting such an agent.
    let slow = h.send(&agent, "slow d", None).await;
    let steer = h.send(&agent, "e", Some("steer")).await;
    assert_eq!(steer["disposition"], "interrupted", "{steer}");
    let d = h.wait(&agent, ("turnId", &slow["turnId"])).await;
    assert_eq!(d["status"], "idle", "{d}");
    let e = h.wait(&agent, ("turnId", &steer["turnId"])).await;
    assert_eq!(e["lastMessage"], "echo: e", "{e}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steerable_agent_is_steered_by_default_and_queued_on_request() {
    let mut h = harness(Duration::from_millis(1500)).await;
    let agent = h.create("echo").await;

    let first = h.send(&agent, "slow p", None).await;
    let steered = h.send(&agent, "q", Some("auto")).await;
    assert_eq!(steered["disposition"], "steered", "{steered}");
    assert_eq!(steered["turnId"], first["turnId"], "it joined the turn");
    let p = h.wait(&agent, ("turnId", &steered["turnId"])).await;
    assert_eq!(p["lastMessage"], "p steered: q", "{p}");

    let slow = h.send(&agent, "slow r", None).await;
    let queued = h.send(&agent, "s", Some("queue")).await;
    assert_eq!(queued["disposition"], "queued", "{queued}");
    let r = h.wait(&agent, ("turnId", &slow["turnId"])).await;
    assert_eq!(r["lastMessage"], "echo: r", "not steered: {r}");
    let s = h.wait(&agent, ("messageId", &queued["messageId"])).await;
    assert_eq!(s["lastMessage"], "echo: s", "{s}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_the_agent_drops_what_waits_behind_its_turn() {
    let mut h = harness(Duration::from_secs(30)).await;
    let agent = h.create("unsteerable").await;

    h.send(&agent, "slow x", None).await;
    let queued = h.send(&agent, "slow y", None).await;
    // A user's prompt with no behavior replaces the turn, as in Paseo.
    assert_eq!(queued["disposition"], "interrupted", "{queued}");
    let queued = h.send(&agent, "slow z", Some("queue")).await;
    assert_eq!(queued["disposition"], "queued", "{queued}");

    let canceled = h
        .client
        .request(
            json!({"type": "cancel_agent_request", "agentId": agent}),
            WITHIN,
        )
        .await
        .expect("cancel");
    assert_eq!(canceled["agent"]["status"], "idle", "{canceled}");
    let z = h.wait(&agent, ("messageId", &queued["messageId"])).await;
    assert_eq!(z["status"], "error", "{z}");
    assert!(
        z["error"].as_str().is_some_and(|e| e.contains("dropped")),
        "{z}"
    );
    // Nothing started after the stop.
    let after = h
        .client
        .request(
            json!({"type": "fetch_agent_request", "agentId": agent}),
            WITHIN,
        )
        .await
        .expect("fetch");
    assert_eq!(after["agent"]["status"], "idle", "{after}");
    h.stop().await;
}
