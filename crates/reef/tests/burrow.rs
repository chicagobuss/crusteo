//! The phone's create, relabel and delete actions against a real burrow,
//! in-process, with its echo provider: the requests Reef sends are the
//! ones burrow serves, and what burrow pushes back is what the phone
//! then shows.

mod support;

use std::sync::Arc;

use burrow_daemon::{Config, Daemon};
use burrow_provider::echo::EchoProvider;
use reef::{Device, ReefConfig};
use serde_json::{Value, json};
use support::*;

const HOST: &str = "example-a";

async fn act(phone: &mut Phone, mut action: Value) -> Value {
    action["type"] = json!("action");
    let req = phone.ask(action).await;
    phone
        .until("the answer", |p| p.results.contains_key(&req))
        .await;
    phone.results[&req].clone()
}

/// The labels burrow has on an agent, by name: the ids under crusteo's
/// key, named as the phone's colours name them, once the phone has them.
async fn labels_of(daemon: &Daemon, phone: &mut Phone, agent: &str) -> Vec<String> {
    let value = daemon
        .read(|store| (store.agent(agent)).and_then(|a| a.labels.get(fleet::labels::KEY).cloned()));
    let value = value.unwrap_or_else(|| panic!("{agent} has no labels"));
    let ids: Vec<String> = (value.strip_prefix("v1:").unwrap().split(','))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    phone
        .until("the labels' names", |p| {
            ids.iter().all(|id| p.labels[id].is_object())
        })
        .await;
    ids.iter()
        .map(|id| phone.labels[id]["name"].as_str().unwrap().to_string())
        .collect()
}

/// The labels burrow has on an agent's workspace.
fn workspace_labels(daemon: &Daemon, agent: &str) -> Vec<String> {
    daemon.read(|store| {
        let workspace = store.agent(agent).and_then(|a| a.workspace_id.clone());
        (workspace.and_then(|w| store.workspaces.get(&w).map(|w| w.labels.clone())))
            .unwrap_or_default()
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn create_relabel_and_delete_work_on_burrow() {
    let daemon = Daemon::new(Config::ephemeral());
    daemon.agents.register(Arc::new(EchoProvider::default()));
    let running = burrow_daemon::start(daemon.clone()).await.unwrap();
    let cwd = std::env::temp_dir().join(format!("reef-burrow-{}", burrow_daemon::ids::agent_id()));
    std::fs::create_dir_all(&cwd).unwrap();

    let host = fleet::HostConfig {
        id: reef::config::host_id(HOST),
        name: HOST.into(),
        endpoint: Some(running.local_addr.to_string()),
        password: None,
    };
    let device = Device {
        name: "test phone".into(),
        token: TOKEN.into(),
    };
    let mut config = ReefConfig::new(vec![host], "cid_reef_burrow", vec![device]);
    config.bind = "127.0.0.1:0".parse().unwrap();
    let addr = reef::start(config).await.unwrap().addr;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "all", "view": "agents"}))
        .await;

    // The form's choices: burrow's echo provider, and labels.
    let options = loop {
        let req = phone
            .ask(json!({"type": "create_options", "host": HOST}))
            .await;
        phone
            .until("the options", |p| p.results.contains_key(&req))
            .await;
        let options = phone.results[&req].clone();
        if options["ok"] == true {
            break options;
        }
        // The host is still connecting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(options["labels"], true, "{options}");
    assert!(
        (options["providers"].as_array().unwrap().iter())
            .any(|p| p["id"] == "echo" && p["ready"] == true),
        "{options}"
    );

    // Create, with a label new to the host.
    let made = act(
        &mut phone,
        json!({"action": "create", "host": HOST, "provider": "echo",
               "cwd": cwd.to_string_lossy(), "labels": ["release"],
               "text": "hello", "message_id": "msg-1"}),
    )
    .await;
    assert_eq!(made["ok"], true, "{made}");
    assert!(made.get("labels_error").is_none(), "{made}");
    let agent = made["agent"].as_str().unwrap().to_string();
    let key = format!("{HOST}/{agent}");
    // The agent carries it; burrow's workspace labels are left alone.
    assert_eq!(labels_of(&daemon, &mut phone, &agent).await, ["release"]);
    assert!(workspace_labels(&daemon, &agent).is_empty());
    let catalog = daemon.read(|store| serde_json::to_value(store.labels.list()).unwrap());
    assert_eq!(catalog, json!([]));

    // Relabel: one on, one new, one off.
    let relabelled = act(
        &mut phone,
        json!({"action": "relabel", "host": HOST, "agent": agent,
               "labels": ["infra", " Ops   team "]}),
    )
    .await;
    assert_eq!(relabelled["ok"], true, "{relabelled}");
    assert_eq!(
        labels_of(&daemon, &mut phone, &agent).await,
        ["infra", "Ops team"]
    );
    phone
        .until("the row's new labels", |p| {
            (p.views.get("all").into_iter())
                .flat_map(|v| v.parts.iter())
                .any(|r| r["key"] == key.as_str() && r["labels"] == json!(["infra", "Ops team"]))
        })
        .await;
    // Reef's catalog keeps the one taken off.
    let mut names: Vec<&str> = (phone.labels.as_object().unwrap().values())
        .filter_map(|l| l["name"].as_str())
        .collect();
    names.sort();
    assert_eq!(names, ["Ops team", "infra", "release"]);

    // Delete: gone from burrow, and from the phone's list.
    let deleted = act(
        &mut phone,
        json!({"action": "delete", "host": HOST, "agent": agent}),
    )
    .await;
    assert_eq!(deleted["ok"], true, "{deleted}");
    assert!(daemon.read(|store| store.agent(&agent).is_none()));
    phone
        .until("the row to go", |p| !p.keys("all").contains(&key))
        .await;
    assert!(phone.errors.is_empty(), "{:?}", phone.errors);

    running.shutdown().await;
    daemon.close();
    std::fs::remove_dir_all(&cwd).unwrap();
}
