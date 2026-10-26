//! The agent list end to end: sorting and grouping on Reef, and each
//! row's latest reply, read without a burst of reads.

mod support;

use reef::fake::{self, FakeDaemon};
use serde_json::{Value, json};
use support::*;

fn timeline_reads(daemon: &FakeDaemon) -> usize {
    daemon
        .requests()
        .iter()
        .filter(|r| *r == "fetch_agent_timeline_request")
        .count()
}

fn row<'a>(phone: &'a Phone, id: &str, agent: &str) -> Option<&'a Value> {
    phone.views.get(id)?.parts.iter().find(|r| r["id"] == agent)
}

#[tokio::test(flavor = "multi_thread")]
async fn groups_come_with_counts_and_follow_the_fleet() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(
            json!({"type": "subscribe", "id": "g", "view": "agents", "group": "state",
                     "sort": "recent", "collapsed": ["state:done"]}),
        )
        .await;
    phone
        .until("the groups", |p| {
            p.views.get("g").is_some_and(|v| {
                v.fields
                    .get("groups")
                    .and_then(Value::as_array)
                    .is_some_and(|g| g.len() == 4)
            })
        })
        .await;
    let groups: Vec<(String, u64)> = phone.view("g").fields["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["label"].as_str().unwrap().into(),
                g["total"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        [
            ("Needs input".to_string(), 2),
            ("Failed".to_string(), 1),
            ("Working".to_string(), 1),
            ("Done".to_string(), 1)
        ]
    );
    // The done group is folded: counted, no rows.
    assert_eq!(phone.view("g").parts.len(), 4);
    assert_eq!(phone.view("g").parts[0]["group"], "state:needs_input");

    // The running agent finishes: the counts move in a delta.
    daemon.finish_turn("agt_a_run", "2026-10-17T10:00:00.000Z", "Drafted.");
    phone
        .until("the new counts", |p| {
            p.view("g").fields["groups"]
                .as_array()
                .unwrap()
                .iter()
                .all(|g| g["label"] != "Working")
        })
        .await;
    assert_eq!(phone.view("g").snapshots, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_show_the_latest_reply_read_once_per_shown_agent() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    // Newest first by default; a window of two shows the two newest.
    phone
        .send(json!({"type": "subscribe", "id": "l", "view": "agents", "limit": 2}))
        .await;
    phone
        .until("the rows", |p| {
            p.views.get("l").is_some_and(|v| v.parts.len() == 2)
        })
        .await;
    assert_eq!(phone.view("l").parts[0]["id"], "agt_a_asks");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(timeline_reads(&daemon), 2, "only the agents shown are read");

    // A wider window reads the newly shown ones, once each.
    phone
        .send(json!({"type": "window", "id": "l", "offset": 0, "limit": 10}))
        .await;
    phone
        .until("the orchestrator's reply", |p| {
            row(p, "l", "agt_a_orch").is_some_and(|r| r.get("reply").is_some())
        })
        .await;
    assert_eq!(
        row(&phone, "l", "agt_a_orch").unwrap()["reply"],
        "Refunds are next; I'll report back when they are done."
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(timeline_reads(&daemon), 5);

    // A turn ends: its reply is read again, with notifications off for
    // it too.
    let req = phone
        .ask(
            json!({"type": "action", "action": "notify", "host": "example-a",
                    "agent": "agt_a_run", "on": false}),
        )
        .await;
    phone
        .until("the answer", |p| p.results.contains_key(&req))
        .await;
    daemon.finish_turn(
        "agt_a_run",
        "2026-10-17T11:00:00.000Z",
        "### The guide is ready\nIt has five parts.",
    );
    phone
        .until("the new reply", |p| {
            row(p, "l", "agt_a_run").is_some_and(|r| r["reply"] == "The guide is ready")
        })
        .await;
    // It went to the top, as the agent that did something last.
    phone
        .until("the reorder", |p| p.view("l").parts[0]["id"] == "agt_a_run")
        .await;
    assert_eq!(timeline_reads(&daemon), 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn hosts_say_since_when() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .until("the host online", |p| p.hosts[0]["state"] == "online")
        .await;
    assert!(
        phone.hosts[0]["since"]
            .as_str()
            .is_some_and(|s| s.starts_with("20"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn labels_come_with_their_colours() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed_many(&daemon, "a", 12);
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    // By label id, from Reef's catalog: each name once, in its colour.
    let color = |p: &Phone, name: &str| {
        (p.labels.as_object().into_iter().flat_map(|l| l.values()))
            .find(|l| l["name"] == name)
            .map(|l| l["color"].clone())
    };
    phone
        .until("the label colours", |p| {
            color(p, "billing") == Some(json!("#cc8f64"))
        })
        .await;
    assert!(
        phone
            .labels
            .as_object()
            .unwrap()
            .keys()
            .all(|id| id.starts_with("lbl_"))
    );
    assert_eq!(color(&phone, "mobile"), Some(json!("#a392d5")));
    // perf has no definition: a colour picked from its name.
    assert_eq!(
        color(&phone, "perf"),
        Some(json!(reef::lists::label_color("perf", None)))
    );
}
