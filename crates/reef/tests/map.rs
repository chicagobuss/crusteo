//! The map view end to end: every agent as a tile, cards only for what
//! the phone looks at up close, and replies read only for those.

mod support;

use reef::fake::{self, FakeDaemon};
use serde_json::json;
use support::*;

fn timeline_reads(daemon: &FakeDaemon) -> usize {
    daemon
        .requests()
        .iter()
        .filter(|r| *r == "fetch_agent_timeline_request")
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_map_sends_tiles_for_all_and_cards_for_its_focus() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    fake::seed_many(&daemon, "a", 30);
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "map", "view": "map", "group": "label"}))
        .await;
    phone
        .until("the tiles", |p| {
            p.views.get("map").is_some_and(|v| v.parts.len() >= 30)
        })
        .await;
    let view = phone.view("map");
    assert!(
        view.cards.is_empty(),
        "no cards until the phone looks closer"
    );
    let tile = &view.parts[0];
    assert!(tile["key"].as_str().unwrap().contains('|'));
    assert!(tile.get("title").is_none(), "a tile is small: {tile}");
    assert!(view.fields["lanes"].as_array().unwrap().len() >= 5);
    // Nothing was read for tiles.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(timeline_reads(&daemon), 0);

    // Looking at two cards up close brings their text, and reads only
    // their replies.
    phone
        .send(json!({"type": "focus", "id": "map",
                     "agents": ["example-a/agt_a_orch", "example-a/agt_a_m1"]}))
        .await;
    phone
        .until("the cards with replies", |p| {
            let cards = &p.view("map").cards;
            cards.len() == 2 && cards.iter().all(|c| c.get("reply").is_some())
        })
        .await;
    let orch = phone
        .view("map")
        .cards
        .iter()
        .find(|c| c["id"] == "agt_a_orch")
        .unwrap();
    assert_eq!(orch["title"], "Orchestrate the billing migration");
    assert_eq!(timeline_reads(&daemon), 2);

    // A turn ends: the tile's state changes in a small delta.
    let tiles_before = phone.view("map").deltas;
    daemon.finish_turn("agt_a_run", "2026-10-17T10:00:00.000Z", "Done.");
    phone
        .until("the tile to settle", |p| {
            p.view("map")
                .parts
                .iter()
                .filter(|t| t["agent"] == "example-a/agt_a_run")
                .all(|t| t["state"] == "done")
        })
        .await;
    assert!(phone.view("map").deltas > tiles_before);
    assert_eq!(phone.view("map").snapshots, 1);

    // Looking away drops the cards.
    phone
        .send(json!({"type": "focus", "id": "map", "agents": []}))
        .await;
    phone
        .until("no cards", |p| p.view("map").cards.is_empty())
        .await;
}
