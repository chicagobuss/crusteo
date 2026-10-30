//! State survives a restart: directory records, and timelines with the same
//! epoch and seqs, so a client's cursor is still good afterwards.

use std::path::{Path, PathBuf};
use std::time::Duration;

use burrow_daemon::{Config, Daemon, seed, start};
use paseo_client::{Client, ConnState, Event, EventReceiver};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

fn scratch_home() -> PathBuf {
    std::env::temp_dir().join(format!("burrow-restart-{}", burrow_daemon::ids::agent_id()))
}

async fn connected(events: &EventReceiver) {
    timeout(WAIT, async {
        loop {
            if let Event::State(ConnState::Connected(_)) = events.recv().await.unwrap() {
                return;
            }
        }
    })
    .await
    .expect("connected in time");
}

async fn tail(home: &Path, server_id: &str, agent_id: &str) -> (Value, Value) {
    let mut config = Config::ephemeral();
    config.home = Some(home.to_path_buf());
    config.server_id = server_id.into();
    let daemon = Daemon::open(config).expect("open home");
    let running = start(daemon.clone()).await.expect("start");
    let client = Client::connect(paseo_client::Config::new(
        running.local_addr.to_string(),
        paseo_client::generate_client_id(),
    ));
    connected(&client.events()).await;
    let page = client
        .request(
            json!({"type": "fetch_agent_timeline_request", "agentId": agent_id,
                        "direction": "tail", "limit": 40, "projection": "canonical"}),
        )
        .await
        .expect("timeline");
    let agents = client
        .request(json!({"type": "fetch_agents_request", "scope": "active"}))
        .await
        .expect("agents");
    drop(client);
    running.shutdown().await;
    daemon.close();
    (page, agents)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timelines_and_records_survive_a_restart() {
    let home = scratch_home();
    let server_id = burrow_daemon::ids::server_id();

    // First run: seed, then add a row while "running".
    let mut config = Config::ephemeral();
    config.home = Some(home.clone());
    let daemon = Daemon::open(config).unwrap();
    let agent_id = seed::demo(&daemon);
    daemon.apply(|store| {
        let mut agent = store.agents.get(&agent_id).cloned().unwrap();
        agent.status = burrow_protocol::directory::AgentStatus::Running;
        let mut events = store.upsert_agent(agent);
        events.extend(store.append_timeline(
            &agent_id,
            json!({"type": "assistant_message", "text": "third"}),
            Some("foreground-turn-1".into()),
            "2026-01-01T00:00:00.000Z".into(),
        ));
        ((), events)
    });
    daemon.close();
    drop(daemon);

    let (first, agents) = tail(&home, &server_id, &agent_id).await;
    assert_eq!(first["entries"].as_array().unwrap().len(), 3);
    assert_eq!(first["window"]["maxSeq"], 3);
    let agent = &agents["entries"][0]["agent"];
    assert_eq!(agent["id"], agent_id.as_str());
    assert_eq!(agent["status"], "closed", "nothing runs after a restart");
    assert!(agent["activeTurn"].is_null());

    // Second restart: the same epoch and seqs, so cursors stay valid.
    let (second, _) = tail(&home, &server_id, &agent_id).await;
    assert_eq!(second["epoch"], first["epoch"]);
    assert_eq!(second["window"], first["window"]);
    assert_eq!(second["entries"][2]["item"]["text"], "third");

    std::fs::remove_dir_all(&home).unwrap();
}
