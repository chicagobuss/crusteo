//! Crusteo's `paseo-client`, unmodified, against Burrow: the client must
//! report Connected and get answers to the requests crusteo makes on
//! connect, in crusteo's own request shapes (`crates/fleet/src/requests.rs`).

use std::time::Duration;

use burrow_daemon::{Config, Daemon, seed, start};
use paseo_client::{Client, ConnState, Error, Event, EventReceiver, ServerInfo};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

async fn connected(events: &EventReceiver) -> ServerInfo {
    timeout(WAIT, async {
        loop {
            match events.recv().await.expect("event stream") {
                Event::State(ConnState::Connected(info)) => return info,
                Event::State(ConnState::Disconnected { reason, .. }) => {
                    panic!("disconnected: {reason}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("connected in time")
}

/// The next session push whose type is `kind`.
async fn push(events: &EventReceiver, kind: &str) -> Value {
    timeout(WAIT, async {
        loop {
            if let Event::Message(message) = events.recv().await.expect("event stream")
                && message["type"] == kind
            {
                return message;
            }
        }
    })
    .await
    .expect("push in time")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crusteo_client_connects_and_bootstraps() {
    let mut config = Config::ephemeral();
    config.hostname = Some("example-host".into());
    let daemon = Daemon::new(config);
    let agent_id = seed::demo(&daemon);
    let running = start(daemon).await.expect("start");

    let client = Client::connect(paseo_client::Config::new(
        running.local_addr.to_string(),
        paseo_client::generate_client_id(),
    ));
    let events = client.events();
    let info = connected(&events).await;
    assert_eq!(info.server_id, running.daemon.config.server_id);
    assert_eq!(info.hostname, "example-host");
    assert!(info.version.ends_with("-burrow"));
    for feature in [
        "directorySync",
        "projectList",
        "workspaceLabels",
        "ownedSubscriptions",
    ] {
        assert!(info.has(feature), "{feature}");
    }

    // crusteo's bootstrap, in order (src/app/store.rs `bootstrap`).
    let events_sub = client
        .request(json!({
            "type": "session.events.set_subscription.request",
            "events": ["project.update", "script_status_update", "agent_permission_request",
                       "agent_permission_resolved", "agent_attention_required"],
        }))
        .await
        .expect("events subscription");
    assert!(events_sub["subscriptionId"].is_string());

    let projects = client
        .request(json!({"type": "project.list.request", "sync": {}}))
        .await
        .expect("project list");
    assert_eq!(projects["projects"].as_array().unwrap().len(), 1);

    let workspaces = client
        .request(json!({
            "type": "fetch_workspaces_request",
            "sort": [{"key": "activity_at", "direction": "desc"}],
            "page": {"limit": 200}, "subscribe": {}, "sync": {},
        }))
        .await
        .expect("workspaces");
    let workspace: paseo_client::proto::Workspace =
        serde_json::from_value(workspaces["entries"][0].clone()).expect("workspace decodes");
    assert_eq!(workspace.directory(), seed::DEMO_ROOT);
    let sync: paseo_client::proto::SyncMeta =
        serde_json::from_value(workspaces["sync"].clone()).expect("sync decodes");
    assert!(sync.is_snapshot());

    let agents = client
        .request(json!({
            "type": "fetch_agents_request", "scope": "active",
            "sort": [{"key": "updated_at", "direction": "desc"}],
            "page": {"limit": 200}, "subscribe": {}, "sync": {},
        }))
        .await
        .expect("agents");
    let subscription = agents["subscriptionId"]
        .as_str()
        .expect("subscribed")
        .to_string();
    let entries = agents["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    let agent = paseo_client::proto::Agent::from_value(entries[0]["agent"].clone()).unwrap();
    assert_eq!(agent.id, agent_id);
    assert_eq!(agent.workspace_id.as_deref(), Some(workspace.id.as_str()));
    assert_eq!(agent.bucket(), paseo_client::proto::Bucket::Done);

    let labels = client
        .request(json!({"type": "workspace.label.list.request", "subscribe": {}}))
        .await
        .expect("labels");
    assert!(labels["labels"].as_array().unwrap().is_empty());

    // Opening the conversation.
    client
        .request(json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent_id]}))
        .await
        .expect("timeline subscription");
    let tail = client
        .request(json!({
            "type": "fetch_agent_timeline_request", "agentId": agent_id,
            "direction": "tail", "limit": 40, "projection": "projected",
        }))
        .await
        .expect("timeline");
    assert_eq!(tail["entries"].as_array().unwrap().len(), 2);
    assert_eq!(tail["endCursor"]["seq"], 2);

    // A directory change reaches the open subscription.
    let mut renamed = running
        .daemon
        .read(|store| store.agents.get(&agent_id).cloned())
        .unwrap();
    renamed.title = Some("Renamed".into());
    running
        .daemon
        .apply(|store| ((), store.upsert_agent(renamed)));
    let update = push(&events, "agent_update").await;
    assert_eq!(update["payload"]["subscriptionId"], subscription.as_str());
    assert_eq!(update["payload"]["agent"]["title"], "Renamed");
    assert!(
        update["payload"]["seq"].as_u64().is_some(),
        "synced subscriptions carry seq"
    );

    // Something burrow does not implement yet.
    let refused = client
        .request(json!({"type": "checkout_status_request", "cwd": "/tmp"}))
        .await;
    assert!(
        matches!(&refused, Err(Error::Rpc { code: Some(code), .. }) if code == "unknown_schema"),
        "{refused:?}"
    );

    drop(client);
    running.shutdown().await;
}
