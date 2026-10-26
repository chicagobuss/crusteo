//! Reef end to end: fake Paseo daemons upstream, a phone downstream
//! speaking the protocol over a real WebSocket. The phone here keeps its
//! own copy of each view by applying snapshots and deltas exactly as the
//! spec says, checking every delta's sequence number, so the tests prove
//! the deltas add up to the right picture, not only that some arrived.

mod support;

use futures_util::{SinkExt, StreamExt};
use reef::fake::{self, FakeDaemon};
use serde_json::json;
use support::*;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_needs_its_token() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, answer) = Phone::connect(addr, "wrong").await;
    assert_eq!(answer["type"], "error");
    assert_eq!(answer["code"], "unauthorized");
    assert!(phone.recv().await.is_none(), "Reef hangs up");

    // A first frame that is not hello is refused the same way.
    let mut ws = connect_ws(addr).await;
    ws.send(Message::text(
        r#"{"type":"subscribe","id":"s","view":"needs_you"}"#,
    ))
    .await
    .unwrap();
    let Some(Ok(Message::Text(text))) = ws.next().await else {
        panic!("no answer")
    };
    assert!(text.as_str().contains("unauthorized"));

    // No token at all is refused too, unless Reef is open.
    let (_, answer) = Phone::connect(addr, "").await;
    assert_eq!(answer["code"], "unauthorized");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_reef_takes_any_phone() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    let addr = open_reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, welcome) = Phone::connect(addr, "").await;
    assert_eq!(welcome["type"], "welcome");
    // Named as it named itself.
    assert_eq!(welcome["device"], "test");
    phone
        .send(json!({"type": "subscribe", "id": "ny", "view": "needs_you"}))
        .await;
    phone
        .until("the needs-you list", |p| p.keys("ny").len() == 3)
        .await;

    // A token, even a made-up one, does no harm.
    let (_, welcome) = Phone::connect(addr, "anything").await;
    assert_eq!(welcome["type"], "welcome");
}

#[tokio::test(flavor = "multi_thread")]
async fn needs_you_and_the_agent_list_follow_the_fleet() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, welcome) = Phone::connect(addr, TOKEN).await;
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["v"], 0);
    assert_eq!(welcome["device"], "test phone");

    phone
        .send(json!({"type": "subscribe", "id": "ny", "view": "needs_you"}))
        .await;
    phone
        .send(json!({"type": "subscribe", "id": "busy", "view": "agents", "filter": {"states": ["running"]}}))
        .await;
    phone
        .send(json!({"type": "subscribe", "id": "all", "view": "agents", "limit": 2}))
        .await;
    phone
        .until("the needs-you list", |p| p.keys("ny").len() == 3)
        .await;
    // The orchestrator finished and waits "for review", which Reef
    // does not count as needing anyone.
    assert_eq!(
        phone.keys("ny"),
        [
            "example-a/agt_a_asks",
            "example-a/agt_a_perm",
            "example-a/agt_a_failed",
        ]
    );
    let ny = phone.view("ny");
    assert_eq!(ny.parts[0]["ask"]["kind"], "question");
    assert_eq!(
        ny.parts[0]["ask"]["question"],
        "Ship the fix alone, or with the refactor?"
    );
    assert_eq!(ny.parts[1]["ask"]["title"], "Bash");
    assert_eq!(ny.parts[1]["workspace"], "shop-search");
    assert_eq!(ny.parts[2]["state"], "failed");
    assert_eq!(ny.parts[2]["labels"], json!(["infra", "billing"]));
    assert_eq!(ny.parts[2]["notify"], true);
    phone
        .until("the lists", |p| {
            p.keys("busy").len() == 1 && p.views.get("all").is_some_and(|v| v.fields["total"] == 5)
        })
        .await;
    assert_eq!(phone.keys("busy"), ["example-a/agt_a_run"]);
    assert_eq!(phone.view("all").parts.len(), 2, "a window of two");
    assert!(phone.hosts[0]["state"] == "online" || phone.hosts[0]["state"] == "connecting");

    // A permission answered at the desk: the agent leaves the list.
    daemon.patch_agent(
        "agt_a_perm",
        json!({"pendingPermissions": [], "requiresAttention": true, "attentionReason": "finished",
               "attentionTimestamp": "2026-10-17T09:55:00.000Z", "updatedAt": "2026-10-17T09:55:00.000Z"}),
    );
    phone
        .until("the answered permission to go", |p| {
            p.keys("ny") == ["example-a/agt_a_asks", "example-a/agt_a_failed"]
        })
        .await;

    // The running agent finishes: it leaves the running list and is done.
    daemon.patch_agent(
        "agt_a_run",
        json!({"status": "idle", "activeTurn": null, "requiresAttention": true, "attentionReason": "finished",
               "attentionTimestamp": "2026-10-17T10:00:00.000Z", "updatedAt": "2026-10-17T10:00:00.000Z"}),
    );
    phone
        .until("the finished agent", |p| p.keys("busy").is_empty())
        .await;
    let ny = phone.view("ny");
    assert_eq!(ny.snapshots, 1, "one snapshot, then deltas");
    assert!(ny.deltas >= 1);

    // A bigger window comes as a fresh snapshot.
    phone
        .send(json!({"type": "window", "id": "all", "offset": 0, "limit": 10}))
        .await;
    phone
        .until("the wider window", |p| p.view("all").parts.len() == 5)
        .await;

    // An action Reef does not know is refused at Reef.
    let req = phone
        .ask(json!({"type": "action", "action": "rename", "host": "example-a", "agent": "agt_a_asks"}))
        .await;
    phone
        .until("the action's answer", |p| p.results.contains_key(&req))
        .await;
    assert_eq!(phone.results[&req]["ok"], false);
    assert_eq!(phone.results[&req]["error"]["code"], "bad_request");

    // Watching sends nothing but reads.
    assert!(daemon.refused().is_empty(), "{:?}", daemon.refused());
    for request in daemon.requests() {
        assert!(
            reef::upstream::READ_REQUESTS.contains(&request.as_str()),
            "Reef sent {request}"
        );
    }
    assert_eq!(daemon.clients(), ["cid_reef_test"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_comes_as_turns_with_footers_and_streams() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    // The agent list first, so Reef knows the agent.
    phone
        .send(json!({"type": "subscribe", "id": "ny", "view": "needs_you"}))
        .await;
    phone.until("the fleet", |p| p.keys("ny").len() == 3).await;

    phone
        .send(
            json!({"type": "subscribe", "id": "c", "view": "conversation",
                     "host": "example-a", "agent": "agt_a_orch", "turns": 10}),
        )
        .await;
    phone
        .until("the conversation", |p| {
            p.views
                .get("c")
                .is_some_and(|v| v.fields["loading"] == false && v.parts.len() == 4)
        })
        .await;
    let view = phone.view("c");
    assert_eq!(
        view.fields["agent"]["title"],
        "Orchestrate the billing migration"
    );
    assert_eq!(view.fields["has_older"], false);
    assert!(view.fields["working"].is_null());
    let turns = &view.parts;
    // One turn from a message, two the orchestrator ran on its own, and
    // one more from a message: each with its footer.
    assert_eq!(
        turns
            .iter()
            .map(|t| t["by"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "agent", "agent", "user"]
    );
    assert_eq!(
        texts(&turns[0]),
        [
            "user:Migrate billing to the new ledger, one service at a time.",
            "tool:cargo test -p ledger",
            "assistant:The ledger tests pass. I'll start with invoices."
        ]
    );
    assert_eq!(
        turns[0]["footer"],
        json!({"at": "2026-10-17T09:01:12.000Z", "secs": 72})
    );
    assert_eq!(
        texts(&turns[1]),
        ["assistant:Invoices moved; the subagent is checking the totals."]
    );
    assert_eq!(
        turns[1]["footer"],
        json!({"at": "2026-10-17T09:10:00.000Z"})
    );
    assert_eq!(turns[2]["footer"]["secs"], 40);
    assert_eq!(texts(&turns[2])[0], "tool:billing/totals.rs");
    assert_eq!(turns[3]["footer"]["secs"], 120);
    let tool = &turns[0]["items"][1];
    assert_eq!(tool["name"], "Shell");
    assert_eq!(tool["icon"], "terminal");
    assert_eq!(tool["status"], "completed");
    assert_eq!(tool["detail"], true);

    // The detail of a tool call comes on request.
    let tool_id = tool["id"].as_str().unwrap().to_string();
    let req = phone
        .ask(json!({"type": "detail", "id": "c", "item": tool_id}))
        .await;
    phone
        .until("the detail", |p| p.results.contains_key(&req))
        .await;
    let detail = &phone.results[&req]["detail"];
    assert_eq!(
        detail["sections"][0]["text"],
        "$ cargo test -p ledger\n\ntest result: ok. 42 passed"
    );

    // The orchestrator resumes on its own and streams a reply.
    let t = |s: &str| format!("2026-10-17T10:00:{s}.000Z");
    daemon.stream_event(
        "agt_a_orch",
        &t("00"),
        json!({"type": "turn_started", "provider": "claude", "turnId": "autonomous-turn-5"}),
    );
    daemon.stream_row(
        "agt_a_orch",
        "autonomous-turn-5",
        &t("01"),
        json!({"type": "assistant_message", "text": "Refunds", "messageId": "r-5"}),
    );
    phone
        .until("the new turn", |p| {
            p.view("c").parts.len() == 5 && !p.view("c").fields["working"].is_null()
        })
        .await;
    assert_eq!(phone.view("c").fields["working"]["since"], t("00"));
    assert!(
        phone.view("c").parts[4].get("footer").is_none(),
        "no footer while it runs"
    );
    daemon.stream_row(
        "agt_a_orch",
        "autonomous-turn-5",
        &t("09"),
        json!({"type": "assistant_message", "text": " are done.", "messageId": "r-5"}),
    );
    phone
        .until("the rest of the reply", |p| {
            texts(&p.view("c").parts[4]) == ["assistant:Refunds are done."]
        })
        .await;
    assert!(
        phone.view("c").appends >= 1,
        "streamed text goes as appends"
    );
    daemon.stream_event(
        "agt_a_orch",
        &t("10"),
        json!({"type": "turn_completed", "provider": "claude", "turnId": "autonomous-turn-5"}),
    );
    phone
        .until("the footer", |p| {
            p.view("c").parts[4].get("footer").is_some()
        })
        .await;
    let view = phone.view("c");
    assert!(view.fields["working"].is_null());
    // A reply streamed as one message is one item, stamped with its last
    // chunk: a turn of one item shows when it was, without a duration.
    assert_eq!(view.parts[4]["footer"], json!({"at": t("09")}));
    assert_eq!(view.parts[4]["by"], "agent");
    assert_eq!(view.snapshots, 1);

    // Closing the view stops nothing upstream at once, and Reef still
    // only read.
    phone.send(json!({"type": "unsubscribe", "id": "c"})).await;
    assert!(daemon.refused().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn older_turns_page_in_from_the_daemon() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    daemon.add_workspace(fake::workspace("wks_1", "shop", "shop-long", &[]));
    daemon.upsert_agent(fake::agent(
        "agt_long",
        "wks_1",
        "A long conversation",
        "2026-10-17T12:00:00.000Z",
        json!({}),
    ));
    // Thirty turns of a question and an answer: sixty rows, more than one
    // page of forty.
    for n in 0..30 {
        let turn = format!("foreground-turn-{n}");
        let at = |s: u32| format!("2026-10-17T10:{n:02}:{s:02}.000Z");
        daemon.seed_row("agt_long", &turn, &at(0), json!({"type": "user_message",
            "text": format!("question {n}"), "messageId": format!("q{n}"), "clientMessageId": format!("q{n}")}));
        daemon.seed_row(
            "agt_long",
            &turn,
            &at(30),
            json!({"type": "assistant_message",
            "text": format!("answer {n}"), "messageId": format!("a{n}")}),
        );
    }
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "all", "view": "agents"}))
        .await;
    phone.until("the fleet", |p| p.keys("all").len() == 1).await;
    phone
        .send(
            json!({"type": "subscribe", "id": "c", "view": "conversation",
                     "host": "example-a", "agent": "agt_long", "turns": 2}),
        )
        .await;
    phone
        .until("the last two turns", |p| {
            p.views.get("c").is_some_and(|v| v.parts.len() == 2)
        })
        .await;
    assert_eq!(phone.keys("c"), ["q28", "q29"]);
    assert_eq!(phone.view("c").fields["has_older"], true);

    // Older turns already loaded come at once.
    let req = phone
        .ask(json!({"type": "older", "id": "c", "turns": 5}))
        .await;
    phone
        .until("older turns", |p| p.results.contains_key(&req))
        .await;
    assert_eq!(phone.keys("c").len(), 7);
    assert_eq!(phone.keys("c")[0], "q23");

    // Asking past what is loaded shows the rest of it...
    let req = phone
        .ask(json!({"type": "older", "id": "c", "turns": 20}))
        .await;
    phone
        .until("all that is loaded", |p| p.results.contains_key(&req))
        .await;
    assert_eq!(phone.keys("c")[0], "q10");
    assert_eq!(phone.results[&req]["has_older"], true);
    let fetches_before = daemon
        .requests()
        .iter()
        .filter(|r| *r == "fetch_agent_timeline_request")
        .count();

    // ...and asking again fetches the page before it from the daemon.
    let req = phone
        .ask(json!({"type": "older", "id": "c", "turns": 4}))
        .await;
    phone
        .until("a page from the daemon", |p| p.results.contains_key(&req))
        .await;
    assert_eq!(phone.keys("c")[0], "q6");
    let fetches_after = daemon
        .requests()
        .iter()
        .filter(|r| *r == "fetch_agent_timeline_request")
        .count();
    assert_eq!(fetches_after, fetches_before + 1);
    assert_eq!(phone.keys("c").len(), 24);
    assert_eq!(phone.view("c").snapshots, 1, "older turns come as deltas");
    let first = &phone.view("c").parts[0];
    assert_eq!(texts(first), ["user:question 6", "assistant:answer 6"]);
}

fn items_in(phone: &Phone, id: &str) -> usize {
    phone
        .view(id)
        .parts
        .iter()
        .map(|turn| turn["items"].as_array().map_or(0, Vec::len))
        .sum()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_lived_conversation_window_stays_bounded() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    daemon.add_workspace(fake::workspace("wks_1", "shop", "shop-long", &[]));
    daemon.upsert_agent(fake::agent(
        "agt_long",
        "wks_1",
        "A long conversation",
        "2026-10-17T12:00:00.000Z",
        json!({}),
    ));
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "all", "view": "agents"}))
        .await;
    phone.until("the fleet", |p| p.keys("all").len() == 1).await;
    phone
        .send(
            json!({"type": "subscribe", "id": "c", "view": "conversation",
                     "host": "example-a", "agent": "agt_long"}),
        )
        .await;
    phone
        .until("the conversation", |p| {
            p.views
                .get("c")
                .is_some_and(|v| v.fields["loading"] == false)
        })
        .await;
    // 200 turns stream in while the phone watches: 400 items, far past
    // what one window holds (60 a snapshot, four times that as it grows).
    for n in 0..200 {
        let turn = format!("foreground-turn-{n}");
        let at = format!("2026-10-17T11:{:02}:{:02}.000Z", n / 60, n % 60);
        daemon.stream_row("agt_long", &turn, &at, json!({"type": "user_message",
            "text": format!("question {n}"), "messageId": format!("q{n}"), "clientMessageId": format!("q{n}")}));
        daemon.stream_row(
            "agt_long",
            &turn,
            &at,
            json!({"type": "assistant_message", "text": format!("answer {n}"), "messageId": format!("a{n}")}),
        );
        if n == 0 {
            // The window starts here once the phone has seen it.
            phone
                .until("the first turn", |p| p.keys("c") == ["q0"])
                .await;
        }
    }

    phone
        .until("the last turn", |p| {
            p.keys("c").last().map(String::as_str) == Some("q199")
        })
        .await;
    let grown = items_in(&phone, "c");
    assert!(grown <= 240, "the window grew to {grown} items");
    assert!(grown >= 200, "it still keeps a long stretch: {grown}");
    assert_eq!(phone.view("c").fields["has_older"], true);

    // A resync sends the end of it only.
    let before = phone.view("c").snapshots;
    phone.send(json!({"type": "resync", "id": "c"})).await;
    phone
        .until("a fresh snapshot", |p| p.view("c").snapshots > before)
        .await;
    assert_eq!(items_in(&phone, "c"), 60);
    assert_eq!(phone.keys("c")[0], "q170");

    // Earlier turns are still there for the asking.
    let req = phone
        .ask(json!({"type": "older", "id": "c", "turns": 5}))
        .await;
    phone
        .until("older turns", |p| p.results.contains_key(&req))
        .await;
    assert_eq!(phone.keys("c")[0], "q165");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_restart_reconnects_and_resyncs() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    let other = FakeDaemon::start("srv_b").await.unwrap();
    fake::seed(&other, "b");
    let addr = reef_with(&[("example-a", &daemon), ("example-b", &other)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "ny", "view": "needs_you"}))
        .await;
    phone.until("both hosts", |p| p.keys("ny").len() == 6).await;

    // The daemon drops every connection; a change made while Reef is
    // away shows once it is back.
    daemon.drop_connections();
    phone
        .until("the host to go offline", |p| {
            p.hosts[0]["state"] != "online" || p.keys("ny").len() == 3
        })
        .await;
    daemon.patch_agent(
        "agt_a_failed",
        json!({"status": "idle", "requiresAttention": false, "attentionReason": null,
               "updatedAt": "2026-10-17T10:30:00.000Z"}),
    );
    phone
        .until("the host to come back", |p| {
            p.hosts[0]["state"] == "online" && p.keys("ny").len() == 5
        })
        .await;
    assert!(
        !phone
            .keys("ny")
            .contains(&"example-a/agt_a_failed".to_string())
    );
    assert!(daemon.clients().len() >= 2, "Reef dialed again");
    assert!(daemon.refused().is_empty() && other.refused().is_empty());
}
