//! A coding-agent CLI upgrading on hosts, and models appearing: told once for
//! the fleet, and shown on the hosts.

mod support;

use std::time::Duration;

use reef::fake::{self, FakeDaemon};
use serde_json::{Value, json};
use support::*;

async fn fleet_of_three() -> ([FakeDaemon; 3], Phone) {
    let a = FakeDaemon::start("srv_a").await.unwrap();
    let b = FakeDaemon::start("srv_b").await.unwrap();
    let c = FakeDaemon::start("srv_c").await.unwrap();
    let addr = reef_configured(
        &[("example-a", &a), ("example-b", &b), ("example-c", &c)],
        |config| config.limits.updates = Duration::from_millis(400),
    )
    .await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    // Every host's first snapshot is the baseline.
    phone
        .until("every host's CLIs", |p| {
            p.hosts
                .as_array()
                .is_some_and(|h| h.len() == 3 && h.iter().all(|h| h["providers"].is_array()))
        })
        .await;
    ([a, b, c], phone)
}

fn claude(phone: &Phone, host: &str) -> Value {
    let host = phone
        .hosts
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == host);
    let providers = host.unwrap()["providers"].as_array().unwrap();
    providers
        .iter()
        .find(|p| p["id"] == "claude")
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upgrading_fleet_is_one_notice_that_fills_in() {
    let ([a, b, c], mut phone) = fleet_of_three().await;
    assert_eq!(claude(&phone, "example-a")["version"], "2.1.317");
    assert_eq!(claude(&phone, "example-a")["install"], "native");
    assert!(phone.notifications.is_empty(), "a first look is no news");

    a.update_providers(fake::providers_upgraded("2.1.320", true));
    b.update_providers(fake::providers_upgraded("2.1.320", true));
    phone
        .until("the notice", |p| !p.notifications.is_empty())
        .await;
    let notice = &phone.notifications[0];
    assert_eq!(notice["kind"], "cli_update");
    assert_eq!(notice["id"], "cli-claude-2.1.320");
    assert_eq!(notice["title"], "Claude Code 2.1.320");
    assert_eq!(notice["body"], "on 2 of 3 hosts \u{b7} new: Fable 5.2");
    assert_eq!(
        (notice["hosts_on"].as_u64(), notice["hosts_total"].as_u64()),
        (Some(2), Some(3))
    );

    // The host still on the old one is marked, and the others are not.
    phone
        .until("the hosts to say who is behind", |p| {
            claude(p, "example-c")["behind"] == true
        })
        .await;
    assert_eq!(claude(&phone, "example-a")["behind"], false);
    assert_eq!(claude(&phone, "example-a")["version"], "2.1.320");

    // The last one catches up: the same notice, new counts.
    c.update_providers(fake::providers_upgraded("2.1.320", true));
    phone
        .until("the notice again", |p| p.notifications.len() == 2)
        .await;
    let again = &phone.notifications[1];
    assert_eq!(again["id"], "cli-claude-2.1.320");
    assert_eq!(again["body"], "on 3 of 3 hosts \u{b7} new: Fable 5.2");
    assert_eq!(claude(&phone, "example-c")["behind"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_with_no_new_version_is_told_on_its_own() {
    let ([a, _b, _c], mut phone) = fleet_of_three().await;
    let mut entries = fake::providers();
    entries[0]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "claude-fable-5-2", "label": "Fable 5.2"}));
    a.update_providers(entries);
    phone
        .until("the notice", |p| !p.notifications.is_empty())
        .await;
    let notice = &phone.notifications[0];
    assert_eq!(notice["kind"], "models");
    assert_eq!(notice["title"], "New Claude Code model");
    assert_eq!(notice["body"], "new: Fable 5.2 on example-a");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_setting_silences_it_but_the_hosts_still_show() {
    let ([a, _b, _c], mut phone) = fleet_of_three().await;
    assert_eq!(phone.settings["notify_updates"], true);
    let req = phone
        .ask(json!({"type": "action", "action": "settings", "notify_updates": false}))
        .await;
    phone
        .until("the answer", |p| p.results.contains_key(&req))
        .await;
    assert_eq!(phone.settings["notify_updates"], false);
    a.update_providers(fake::providers_upgraded("2.1.320", false));
    phone
        .until("the host to show it", |p| {
            claude(p, "example-a")["version"] == "2.1.320"
        })
        .await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    let pongs = phone.pongs;
    phone.send(json!({"type": "ping", "t": 0})).await;
    phone.until("the pong", |p| p.pongs > pongs).await;
    assert!(phone.notifications.is_empty(), "{:?}", phone.notifications);
}
