//! Paseo 0.12's background workspaces: Reef asks the daemons that have
//! them for them, rows say which agents are in one, and each list shows or
//! hides them by the phone's own choice, else Reef's setting.

mod support;

use reef::fake::{self, FakeDaemon};
use serde_json::{Value, json};
use support::*;

/// A 0.12 daemon with the usual seed, plus a background workspace holding
/// one running agent.
async fn background_daemon(advertised: bool) -> FakeDaemon {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    daemon.set_background_workspaces(advertised);
    fake::seed(&daemon, "a");
    fake::seed_background(&daemon, "a");
    daemon
}

fn keys_of(phone: &Phone, view: &str) -> Vec<String> {
    let mut keys = phone.keys(view);
    keys.sort();
    keys
}

fn row<'a>(phone: &'a Phone, view: &str, key: &str) -> Option<&'a Value> {
    phone.view(view).parts.iter().find(|r| r["key"] == key)
}

#[tokio::test(flavor = "multi_thread")]
async fn background_agents_show_tagged_by_default_and_hide_per_phone_or_reef_wide() {
    let daemon = background_daemon(true).await;
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    assert_eq!(phone.settings["show_background"], true);

    // Shown, tagged: Reef asked for them.
    phone
        .send(json!({"type": "subscribe", "id": "all", "view": "agents", "limit": 50}))
        .await;
    phone
        .until("the fleet with its background agent", |p| {
            p.keys("all").len() == 6
        })
        .await;
    let helper = row(&phone, "all", "example-a/agt_a_bg").unwrap();
    assert_eq!(helper["background"], true);
    assert_eq!(helper["workspace"], "shop-helper");
    assert!(row(&phone, "all", "example-a/agt_a_run").unwrap()["background"].is_null());
    let reads = daemon.directory_reads();
    assert!(!reads.is_empty());
    for read in &reads {
        assert_eq!(read["includeBackground"], true, "{read}");
        assert!(read["filter"].is_null(), "{read}");
    }

    // This phone hides them; the other list keeps them.
    phone
        .send(
            json!({"type": "subscribe", "id": "mine", "view": "agents", "limit": 50,
                     "filter": {"background": false}}),
        )
        .await;
    phone
        .until("the list without it", |p| {
            p.views.get("mine").is_some_and(|v| v.snapshots > 0)
        })
        .await;
    assert_eq!(keys_of(&phone, "mine").len(), 5);
    assert!(row(&phone, "mine", "example-a/agt_a_bg").is_none());
    assert_eq!(phone.view("mine").fields["total"], 5);

    // Reef-wide off: a phone that has not chosen follows; one that has keeps
    // its choice.
    let req = phone
        .ask(json!({"type": "action", "action": "settings", "show_background": false}))
        .await;
    phone
        .until("the answer", |p| p.results.contains_key(&req))
        .await;
    phone
        .until("the settings", |p| p.settings["show_background"] == false)
        .await;
    phone
        .until("the default list to drop it", |p| p.keys("all").len() == 5)
        .await;
    phone
        .send(
            json!({"type": "subscribe", "id": "chosen", "view": "agents", "limit": 50,
                     "filter": {"background": true}}),
        )
        .await;
    phone
        .until("the list that chose them", |p| {
            p.views.get("chosen").is_some_and(|v| v.parts.len() == 6)
        })
        .await;

    // Hidden or not, a background agent's finished turn is announced, as
    // its notify setting says.
    daemon.finish_turn(
        "agt_a_bg",
        "2026-10-17T10:00:00.000Z",
        "Nothing unusual in the logs.",
    );
    phone
        .until("the notification", |p| {
            p.notifications
                .iter()
                .any(|n| n["key"] == "example-a/agt_a_bg")
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_without_background_workspaces_is_not_asked_for_them() {
    let daemon = background_daemon(false).await;
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "all", "view": "agents", "limit": 50}))
        .await;
    // Before 0.12 nothing is hidden, so the flag only marks the row.
    phone.until("the fleet", |p| p.keys("all").len() == 6).await;
    let reads = daemon.directory_reads();
    assert!(!reads.is_empty());
    for read in &reads {
        assert!(read.get("includeBackground").is_none(), "{read}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_hides_them_leaves_out_what_was_not_asked_for() {
    // What a client that never asks sees from a 0.12 daemon: the fake
    // behaves as Paseo does, so Reef's request is what makes them show.
    let daemon = background_daemon(true).await;
    let client =
        paseo_client::Client::connect(paseo_client::Config::new(daemon.endpoint(), "cid_probe"));
    let events = client.events();
    while let Ok(event) = events.recv().await {
        if let paseo_client::Event::State(paseo_client::ConnState::Connected(info)) = event {
            assert!(info.has("backgroundWorkspaces"));
            break;
        }
    }
    let asked = fleet::requests::fetch_agents(fleet::requests::DirectoryRead::default(), None);
    let plain = client.request(asked).await.unwrap();
    let ids: Vec<&str> = (plain["entries"].as_array().unwrap().iter())
        .map(|e| e["agent"]["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&"agt_a_bg"), "{ids:?}");
    let read = fleet::requests::DirectoryRead {
        sync: false,
        background: true,
    };
    let all = client
        .request(fleet::requests::fetch_agents(read, None))
        .await
        .unwrap();
    let entries = all["entries"].as_array().unwrap();
    assert_eq!(entries.len(), ids.len() + 1);
    assert!(entries.iter().any(|e| e["agent"]["id"] == "agt_a_bg"));
}
