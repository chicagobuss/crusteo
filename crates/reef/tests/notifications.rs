//! Turn-finished notifications and the settings that choose them.

mod support;

use std::time::Duration;

use reef::fake::{self, FakeDaemon};
use serde_json::{Value, json};
use support::*;

async fn fleet_saving_to(dir: Option<std::path::PathBuf>) -> (FakeDaemon, Phone) {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    let addr = reef_saving_to(&[("example-a", &daemon)], dir).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "all", "view": "agents", "limit": 50}))
        .await;
    phone.until("the fleet", |p| p.keys("all").len() == 5).await;
    (daemon, phone)
}

/// Waits a little longer than Reef could take to say more.
async fn settle(phone: &mut Phone) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    let pongs = phone.pongs;
    phone.send(json!({"type": "ping", "t": 0})).await;
    phone.until("the pong", |p| p.pongs > pongs).await;
}

fn row<'a>(phone: &'a Phone, key: &str) -> &'a Value {
    phone
        .view("all")
        .parts
        .iter()
        .find(|r| r["key"] == key)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_turn_is_announced_with_the_first_line_of_the_reply() {
    let (daemon, mut phone) = fleet_saving_to(None).await;
    assert_eq!(
        phone.settings,
        json!({"notify_default": true, "show_background": true})
    );
    daemon.finish_turn(
        "agt_a_run",
        "2026-10-17T10:00:00.000Z",
        "## Drafted the guide\n\nIt has five sections.",
    );
    phone
        .until("the notification", |p| !p.notifications.is_empty())
        .await;
    let notice = &phone.notifications[0];
    assert_eq!(notice["kind"], "turn_finished");
    assert_eq!(notice["state"], "done");
    assert_eq!(notice["key"], "example-a/agt_a_run");
    assert_eq!(notice["host"], "example-a");
    assert_eq!(notice["agent"], "agt_a_run");
    assert_eq!(notice["title"], "Write the onboarding guide");
    assert_eq!(notice["body"], "Drafted the guide");
    settle(&mut phone).await;
    assert_eq!(phone.notifications.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_conversation_hears_the_end_once() {
    let (daemon, mut phone) = fleet_saving_to(None).await;
    phone
        .send(
            json!({"type": "subscribe", "id": "c", "view": "conversation",
                     "host": "example-a", "agent": "agt_a_run"}),
        )
        .await;
    phone
        .until("the conversation", |p| {
            p.views
                .get("c")
                .is_some_and(|v| v.fields["loading"] == false)
        })
        .await;
    daemon.finish_turn("agt_a_run", "2026-10-17T10:00:00.000Z", "Done drafting.");
    phone
        .until("the notification", |p| !p.notifications.is_empty())
        .await;
    assert_eq!(phone.notifications[0]["body"], "Done drafting.");
    settle(&mut phone).await;
    assert_eq!(
        phone.notifications.len(),
        1,
        "the stream and the directory agree"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_turn_says_so() {
    let (daemon, mut phone) = fleet_saving_to(None).await;
    daemon.patch_agent(
        "agt_a_run",
        json!({"status": "error", "activeTurn": null, "lastError": "Provider quit",
               "updatedAt": "2026-10-17T10:00:00.000Z"}),
    );
    phone
        .until("the notification", |p| !p.notifications.is_empty())
        .await;
    assert_eq!(phone.notifications[0]["state"], "failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn notify_settings_are_kept_on_the_reef_for_every_phone() {
    let dir = std::env::temp_dir().join(format!(
        "reef-notify-{}",
        reef::config::new_token().unwrap()
    ));
    let (daemon, mut phone) = fleet_saving_to(Some(dir.clone())).await;
    assert_eq!(row(&phone, "example-a/agt_a_run")["notify"], true);

    // This agent's turns stay quiet.
    let req = phone
        .ask(
            json!({"type": "action", "action": "notify", "host": "example-a",
                    "agent": "agt_a_run", "on": false}),
        )
        .await;
    phone
        .until("the answer", |p| p.results.contains_key(&req))
        .await;
    phone
        .until("the row to say so", |p| {
            row(p, "example-a/agt_a_run")["notify"] == false
        })
        .await;
    daemon.finish_turn("agt_a_run", "2026-10-17T10:00:00.000Z", "Quietly done.");
    settle(&mut phone).await;
    assert!(phone.notifications.is_empty(), "{:?}", phone.notifications);

    // The default goes off: every phone hears, and it is saved.
    let req = phone
        .ask(json!({"type": "action", "action": "settings", "notify_default": false}))
        .await;
    phone
        .until("the answer", |p| p.results.contains_key(&req))
        .await;
    phone
        .until("the settings", |p| p.settings["notify_default"] == false)
        .await;
    phone
        .until("every row to follow", |p| {
            row(p, "example-a/agt_a_orch")["notify"] == false
        })
        .await;
    let saved: Value =
        serde_json::from_slice(&std::fs::read(dir.join("reef-settings.json")).unwrap()).unwrap();
    assert_eq!(saved["notify_default"], false);
    assert_eq!(saved["notify"]["example-a/agt_a_run"], false);

    // An agent switched on stays on with the default off.
    let req = phone
        .ask(
            json!({"type": "action", "action": "notify", "host": "example-a",
                    "agent": "agt_a_orch", "on": true}),
        )
        .await;
    phone
        .until("the answer", |p| p.results.contains_key(&req))
        .await;
    daemon.start_turn("agt_a_orch", "2026-10-17T10:01:00.000Z");
    daemon.finish_turn(
        "agt_a_orch",
        "2026-10-17T10:02:00.000Z",
        "Refunds are done.",
    );
    phone
        .until("the notification", |p| !p.notifications.is_empty())
        .await;
    assert_eq!(phone.notifications[0]["agent"], "agt_a_orch");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gone_agents_choice_is_forgotten() {
    let dir =
        std::env::temp_dir().join(format!("reef-prune-{}", reef::config::new_token().unwrap()));
    let (_daemon, mut phone) = fleet_saving_to(Some(dir.clone())).await;
    let saved = || -> Value {
        serde_json::from_slice(&std::fs::read(dir.join("reef-settings.json")).unwrap()).unwrap()
    };
    for agent in ["agt_a_run", "agt_a_orch"] {
        let req = phone
            .ask(
                json!({"type": "action", "action": "notify", "host": "example-a",
                        "agent": agent, "on": false}),
            )
            .await;
        phone
            .until("the answer", |p| p.results.contains_key(&req))
            .await;
    }
    settle(&mut phone).await;
    // Every host is up and loaded: choices for agents it has stay.
    assert_eq!(saved()["notify"]["example-a/agt_a_run"], false);
    assert_eq!(saved()["notify"]["example-a/agt_a_orch"], false);

    let req = phone
        .ask(json!({"type": "action", "action": "archive", "host": "example-a", "agent": "agt_a_run"}))
        .await;
    phone
        .until("the answer", |p| p.results.contains_key(&req))
        .await;
    phone
        .until("the agent to go", |p| p.keys("all").len() == 4)
        .await;
    settle(&mut phone).await;
    let notify = saved()["notify"].clone();
    assert!(notify.get("example-a/agt_a_run").is_none(), "{notify}");
    assert_eq!(notify["example-a/agt_a_orch"], false);
    let _ = std::fs::remove_dir_all(&dir);
}
