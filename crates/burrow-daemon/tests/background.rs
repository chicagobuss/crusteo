//! Paseo 0.12's background workspaces end to end through `paseo-client`:
//! made by `workspace.create` or `agent.create` with `background`, kept out
//! of directory reads, sync and pushes unless a client sends
//! `includeBackground`, in their own sync generation, and kept across a
//! restart. Reads go in the shape crusteo and Reef send
//! (`fleet::requests::fetch_*`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::{Config, Daemon, Running, start};
use burrow_provider::echo::EchoProvider;
use fleet::requests::{self, DirectoryRead};
use paseo_client::{Client, ConnState, Event, EventReceiver};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

struct Harness {
    running: Running,
    daemon: Arc<Daemon>,
    client: Client,
    events: EventReceiver,
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "burrow-background-{name}-{}",
        burrow_daemon::ids::agent_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn harness(home: &Path, server_id: &str) -> Harness {
    let mut config = Config::ephemeral();
    config.home = Some(home.to_path_buf());
    config.server_id = server_id.into();
    let daemon = Daemon::open(config).unwrap();
    daemon.agents.register(Arc::new(EchoProvider::default()));
    let running = start(daemon.clone()).await.unwrap();
    let client = Client::connect(paseo_client::Config::new(
        running.local_addr.to_string(),
        paseo_client::generate_client_id(),
    ));
    let events = client.events();
    timeout(WAIT, async {
        loop {
            if let Event::State(ConnState::Connected(info)) = events.recv().await.unwrap() {
                assert!(info.has("backgroundWorkspaces"));
                return;
            }
        }
    })
    .await
    .expect("connected");
    Harness {
        running,
        daemon,
        client,
        events,
    }
}

impl Harness {
    async fn request(&self, message: Value) -> Value {
        self.client.request(message).await.expect("request")
    }

    async fn agent(&self, cwd: &Path, extra: Value) -> Value {
        let mut message = json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "echo", "cwd": cwd.to_string_lossy()}});
        if let (Some(target), Value::Object(extra)) = (message.as_object_mut(), extra) {
            target.extend(extra);
        }
        let reply = self.request(message).await;
        assert!(reply["error"].is_null(), "{reply}");
        reply["agent"].clone()
    }

    /// Every push that arrives within a short while.
    async fn pushes(&self) -> Vec<Value> {
        let mut seen = Vec::new();
        while let Ok(Ok(event)) = timeout(Duration::from_millis(400), self.events.recv()).await {
            if let Event::Message(message) = event {
                seen.push(message);
            }
        }
        seen
    }

    async fn stop(self) {
        drop(self.client);
        self.running.shutdown().await;
        self.daemon.close();
    }
}

fn ids(payload: &Value, key: &str) -> Vec<String> {
    let mut ids: Vec<String> = (payload["entries"].as_array().unwrap().iter())
        .map(|entry| {
            let entry = if key.is_empty() { entry } else { &entry[key] };
            entry["id"].as_str().unwrap().to_string()
        })
        .collect();
    ids.sort();
    ids
}

const PLAIN: DirectoryRead = DirectoryRead {
    sync: true,
    background: false,
};
const WITH_BACKGROUND: DirectoryRead = DirectoryRead {
    sync: true,
    background: true,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_workspaces_show_only_to_clients_that_ask() {
    let home = scratch("home");
    let server_id = burrow_daemon::ids::server_id();
    let h = harness(&home, &server_id).await;
    let (shop, chores) = (scratch("shop"), scratch("chores"));

    let front = h.agent(&shop, json!({})).await;
    // Background is how a workspace is made: never the shop's own.
    let helper = h.agent(&shop, json!({"background": true})).await;
    let (front_ws, helper_ws) = (
        front["workspaceId"].as_str().unwrap().to_string(),
        helper["workspaceId"].as_str().unwrap().to_string(),
    );
    assert_ne!(front_ws, helper_ws);
    // A plain create in the same directory still finds the shop's own.
    let again = h.agent(&shop, json!({})).await;
    assert_eq!(again["workspaceId"], front_ws.as_str());
    let refused = h
        .request(json!({"type": "agent.create.request", "labels": {}, "background": true,
            "workspaceId": front_ws, "config": {"provider": "echo", "cwd": shop.to_string_lossy()}}))
        .await;
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("workspace creation intent")
    );

    let made = h
        .request(
            json!({"type": "workspace.create.request", "background": true,
            "title": "Nightly chores",
            "source": {"kind": "directory", "path": chores.to_string_lossy()}}),
        )
        .await;
    assert!(made["error"].is_null(), "{made}");
    assert_eq!(made["workspace"]["background"], true);
    assert_eq!(made["workspace"]["name"], "Nightly chores");
    assert_eq!(made["creation"]["phase"], "completed");
    let chores_ws = made["workspace"]["id"].as_str().unwrap().to_string();
    let worktree = h
        .request(json!({"type": "workspace.create.request",
            "source": {"kind": "worktree", "cwd": shop.to_string_lossy()}}))
        .await;
    assert!(worktree["workspace"].is_null());
    assert!(worktree["error"].is_string());

    // Without asking: neither the workspaces nor their agents, and the
    // chores project, with only a background workspace, reads as empty.
    let plain = h.request(requests::fetch_workspaces(PLAIN, None)).await;
    assert_eq!(ids(&plain, ""), std::slice::from_ref(&front_ws));
    assert_eq!(plain["emptyProjects"].as_array().unwrap().len(), 1);
    assert_eq!(plain["entries"][0]["background"], false);
    let generation = plain["sync"]["generation"].as_str().unwrap().to_string();
    assert!(!generation.ends_with(":background"));
    let plain_agents = h.request(requests::fetch_agents(PLAIN, None)).await;
    let mut expected = vec![
        front["id"].as_str().unwrap().to_string(),
        again["id"].as_str().unwrap().to_string(),
    ];
    expected.sort();
    assert_eq!(ids(&plain_agents, "agent"), expected);

    // Asking: everything, in a generation of its own.
    let all = h
        .request(requests::fetch_workspaces(WITH_BACKGROUND, None))
        .await;
    let mut every = vec![front_ws.clone(), helper_ws.clone(), chores_ws.clone()];
    every.sort();
    assert_eq!(ids(&all, ""), every);
    assert!(all["emptyProjects"].as_array().unwrap().is_empty());
    assert_eq!(
        all["sync"]["generation"],
        format!("{generation}:background")
    );
    let background: Vec<&Value> = (all["entries"].as_array().unwrap().iter())
        .filter(|w| w["background"] == true)
        .collect();
    assert_eq!(background.len(), 2);
    let all_agents = h
        .request(requests::fetch_agents(WITH_BACKGROUND, None))
        .await;
    assert_eq!(all_agents["entries"].as_array().unwrap().len(), 3);
    // In the filter, as Paseo also takes it, on an unsequenced read.
    let filtered = h
        .request(json!({"type": "fetch_agents_request", "filter": {"includeBackground": true}}))
        .await;
    assert_eq!(filtered["entries"].as_array().unwrap().len(), 3);

    // A plain cursor does not resume a background read, nor the reverse.
    let cursor = paseo_client::proto::Cursor {
        generation: generation.clone(),
        after_seq: plain["sync"]["headSeq"].as_u64().unwrap(),
    };
    let crossed = h
        .request(requests::fetch_workspaces(WITH_BACKGROUND, Some(&cursor)))
        .await;
    assert_eq!(crossed["sync"]["mode"], "snapshot");
    assert_eq!(crossed["sync"]["reason"], "generation_changed");

    // Pushes: a new agent in the background workspace reaches only the
    // subscription that asked, sequenced in the background generation.
    let quiet = h
        .request(requests::fetch_agents(
            DirectoryRead {
                sync: false,
                background: false,
            },
            None,
        ))
        .await;
    let loud = h
        .request(requests::fetch_agents(WITH_BACKGROUND, None))
        .await;
    let (quiet, loud) = (
        quiet["subscriptionId"].as_str().unwrap().to_string(),
        loud["subscriptionId"].as_str().unwrap().to_string(),
    );
    h.pushes().await;
    let late = h
        .agent(&shop, json!({"workspaceId": helper_ws.clone()}))
        .await;
    let updates: Vec<Value> = (h.pushes().await.into_iter())
        .filter(|m| m["type"] == "agent_update" && m["payload"]["agent"]["id"] == late["id"])
        .collect();
    // Every read above subscribed too: only those that asked hear of it.
    let plain_sub = plain_agents["subscriptionId"].as_str().unwrap();
    for update in &updates {
        let sub = update["payload"]["subscriptionId"].as_str().unwrap();
        assert!(sub != quiet && sub != plain_sub, "{update}");
        assert_eq!(
            update["payload"]["generation"],
            format!("{generation}:background")
        );
    }
    assert!(
        updates
            .iter()
            .any(|u| u["payload"]["subscriptionId"] == loud.as_str())
    );
    h.stop().await;

    // The flag is saved with the workspace.
    let h = harness(&home, &server_id).await;
    let restarted = h
        .request(requests::fetch_workspaces(WITH_BACKGROUND, None))
        .await;
    let flagged: Vec<String> = (restarted["entries"].as_array().unwrap().iter())
        .filter(|w| w["background"] == true)
        .map(|w| w["id"].as_str().unwrap().to_string())
        .collect();
    let mut flagged = flagged;
    flagged.sort();
    let mut expected = vec![helper_ws, chores_ws];
    expected.sort();
    assert_eq!(flagged, expected);
    let plain = h.request(requests::fetch_workspaces(PLAIN, None)).await;
    assert_eq!(ids(&plain, ""), [front_ws]);
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}
