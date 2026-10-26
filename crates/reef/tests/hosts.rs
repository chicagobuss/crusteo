//! Hosts added and removed from a phone: dialed or closed at once, and
//! saved to the hosts file for the next start.

mod support;

use reef::fake::{self, FakeDaemon};
use serde_json::{Value, json};
use support::*;

async fn act(phone: &mut Phone, action: Value) -> Value {
    let mut message = action;
    message["type"] = json!("action");
    let req = phone.ask(message).await;
    phone
        .until("the action's answer", |p| p.results.contains_key(&req))
        .await;
    phone.results[&req].clone()
}

fn names(phone: &Phone) -> Vec<String> {
    phone
        .hosts
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| h["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn saved(path: &std::path::Path) -> Vec<Value> {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_added_from_a_phone_connects_and_is_saved_and_one_removed_goes() {
    let a = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&a, "a");
    let b = FakeDaemon::start("srv_b").await.unwrap();
    fake::seed(&b, "b");
    let dir = std::env::temp_dir().join(format!("reef-hosts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("hosts.json");
    let addr = reef_with_hosts_file(&[("example-a", &a)], file.clone()).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "all", "view": "agents", "limit": 50}))
        .await;
    phone
        .until("host a's agents", |p| p.keys("all").len() == 5)
        .await;

    let answer = act(
        &mut phone,
        json!({"action": "host_add", "name": "example-b", "endpoint": b.endpoint()}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    phone
        .until("host b online", |p| {
            p.hosts
                .as_array()
                .into_iter()
                .flatten()
                .any(|h| h["name"] == "example-b" && h["state"] == "online")
        })
        .await;
    phone
        .until("both hosts' agents", |p| p.keys("all").len() == 10)
        .await;
    let file_hosts = saved(&file);
    assert_eq!(file_hosts.len(), 2);
    assert_eq!(file_hosts[1]["name"], "example-b");
    assert_eq!(file_hosts[1]["endpoint"], json!(b.endpoint()));

    // A name in use, or no address, is refused and nothing is saved.
    for bad in [
        json!({"action": "host_add", "name": "EXAMPLE-B", "endpoint": "10.0.0.9"}),
        json!({"action": "host_add", "name": "example-c", "endpoint": " "}),
        json!({"action": "host_remove", "name": "nowhere"}),
    ] {
        let answer = act(&mut phone, bad).await;
        assert_eq!(answer["ok"], false, "{answer}");
    }
    assert_eq!(saved(&file).len(), 2);

    let answer = act(
        &mut phone,
        json!({"action": "host_remove", "name": "example-a"}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    phone
        .until("host a gone", |p| names(p) == ["example-b"])
        .await;
    phone
        .until("only host b's agents", |p| p.keys("all").len() == 5)
        .await;
    assert_eq!(saved(&file).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_address_without_a_port_gets_the_daemons() {
    let a = FakeDaemon::start("srv_a").await.unwrap();
    let dir = std::env::temp_dir().join(format!("reef-port-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("hosts.json");
    let addr = reef_with_hosts_file(&[("example-a", &a)], file.clone()).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    let answer = act(
        &mut phone,
        json!({"action": "host_add", "name": "example-z", "endpoint": "192.0.2.7"}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(saved(&file)[1]["endpoint"], "192.0.2.7:6767");
    let _ = std::fs::remove_dir_all(&dir);
}
