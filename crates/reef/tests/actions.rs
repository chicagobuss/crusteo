//! The phone's actions, end to end: each sends exactly the daemon request
//! it should, in the format crusteo and the Paseo app send, and nothing
//! else.

mod support;

use reef::fake::{self, FakeDaemon};
use serde_json::{Value, json};
use support::*;

/// A seeded daemon, a Reef on it, and a phone that has seen the fleet.
async fn fleet() -> (FakeDaemon, Phone) {
    let (daemon, phone, _) = fleet_at().await;
    (daemon, phone)
}

/// [`fleet`], and where Reef listens.
async fn fleet_at() -> (FakeDaemon, Phone, std::net::SocketAddr) {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "ny", "view": "needs_you"}))
        .await;
    phone.until("the fleet", |p| p.keys("ny").len() == 3).await;
    (daemon, phone, addr)
}

/// Sends an action and waits for its answer.
async fn act(phone: &mut Phone, action: Value) -> Value {
    let mut message = action;
    message["type"] = json!("action");
    let req = phone.ask(message).await;
    phone
        .until("the action's answer", |p| p.results.contains_key(&req))
        .await;
    phone.results[&req].clone()
}

/// The requests the daemon got that are not reads, by type.
fn writes(daemon: &FakeDaemon) -> Vec<String> {
    daemon
        .requests()
        .into_iter()
        .filter(|r| !reef::upstream::READ_REQUESTS.contains(&r.as_str()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn allowing_a_permission_sends_one_permission_response() {
    let (daemon, mut phone) = fleet().await;
    let answer = act(
        &mut phone,
        json!({"action": "permission", "host": "example-a", "agent": "agt_a_perm",
               "ask": "perm_a_s", "decision": "allow"}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(writes(&daemon), ["agent_permission_response"]);
    let sent = &daemon.writes()[0];
    assert_eq!(sent["agentId"], "agt_a_perm");
    assert_eq!(sent["requestId"], "perm_a_s");
    assert_eq!(
        sent["response"],
        json!({"behavior": "allow", "selectedActionId": "accept"})
    );
    phone
        .until("the agent to leave needs-you", |p| {
            !p.keys("ny").contains(&"example-a/agt_a_perm".to_string())
        })
        .await;

    // The same ask again is no longer pending: refused at Reef.
    let again = act(
        &mut phone,
        json!({"action": "permission", "host": "example-a", "agent": "agt_a_perm",
               "ask": "perm_a_s", "decision": "deny"}),
    )
    .await;
    assert_eq!(again["ok"], false);
    assert_eq!(again["error"]["code"], "not_found");
    assert_eq!(writes(&daemon).len(), 1);
    assert!(daemon.refused().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn denying_a_permission_says_so_as_the_paseo_app_does() {
    let (daemon, mut phone) = fleet().await;
    let answer = act(
        &mut phone,
        json!({"action": "permission", "host": "example-a", "agent": "agt_a_perm",
               "ask": "perm_a_s", "decision": "deny"}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(
        daemon.writes()[0]["response"],
        json!({"behavior": "deny", "selectedActionId": "reject", "message": "Denied by user"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_cancels_the_turn_and_is_no_news() {
    let (daemon, mut phone) = fleet().await;
    let answer = act(
        &mut phone,
        json!({"action": "stop", "host": "example-a", "agent": "agt_a_run"}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(writes(&daemon), ["cancel_agent_request"]);
    assert_eq!(
        daemon.writes()[0],
        json!({"type": "cancel_agent_request", "agentId": "agt_a_run",
               "requestId": daemon.writes()[0]["requestId"]})
    );
    phone
        .send(json!({"type": "subscribe", "id": "busy", "view": "agents", "filter": {"states": ["running"]}}))
        .await;
    phone
        .until("the agent to stop", |p| {
            p.views.get("busy").is_some_and(|v| v.parts.is_empty())
        })
        .await;
    // The phone stopped it: no notification that its turn ended.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    phone.send(json!({"type": "ping", "t": 1})).await;
    phone.until("the pong", |p| p.pongs > 0).await;
    assert!(phone.notifications.is_empty(), "{:?}", phone.notifications);
}

/// Opens an agent's conversation and waits for it to load.
async fn open(phone: &mut Phone, agent: &str) {
    phone
        .send(
            json!({"type": "subscribe", "id": "c", "view": "conversation",
                     "host": "example-a", "agent": agent}),
        )
        .await;
    phone
        .until("the conversation", |p| {
            p.views
                .get("c")
                .is_some_and(|v| v.fields["loading"] == false)
        })
        .await;
}

fn queued(phone: &Phone) -> Vec<String> {
    phone.view("c").fields["queued"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap().to_string())
        .collect()
}

fn user_texts(phone: &Phone) -> Vec<String> {
    phone
        .view("c")
        .parts
        .iter()
        .flat_map(|t| t["items"].as_array().unwrap().clone())
        .filter(|i| i["kind"] == "user")
        .map(|i| i["text"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_to_an_idle_agent_goes_at_once() {
    let (daemon, mut phone) = fleet().await;
    open(&mut phone, "agt_a_orch").await;
    let answer = act(
        &mut phone,
        json!({"action": "send", "host": "example-a", "agent": "agt_a_orch",
               "text": "Do the refunds next.", "message_id": "msg-1"}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(answer["sent"], true);
    assert_eq!(writes(&daemon), ["send_agent_message_request"]);
    let sent = &daemon.writes()[0];
    assert_eq!(sent["agentId"], "agt_a_orch");
    assert_eq!(sent["text"], "Do the refunds next.");
    assert_eq!(sent["messageId"], "msg-1");
    assert!(sent.get("activeTurnBehavior").is_none());
    phone
        .until("the message in the conversation", |p| {
            user_texts(p).last().map(String::as_str) == Some("Do the refunds next.")
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn images_go_with_a_message_or_alone_in_the_daemons_format() {
    let (daemon, mut phone) = fleet().await;
    open(&mut phone, "agt_a_orch").await;
    // Bigger than a plain frame may be: only a message with images may.
    let big = "A".repeat(100 * 1024);
    let answer = act(
        &mut phone,
        json!({"action": "send", "host": "example-a", "agent": "agt_a_orch",
               "text": "What is wrong here?", "message_id": "msg-img",
               "images": [{"data": big, "mime_type": "image/jpeg"}]}),
    )
    .await;
    assert_eq!(answer["sent"], true, "{answer}");
    let sent = &daemon.writes()[0];
    assert_eq!(sent["text"], "What is wrong here?");
    assert_eq!(
        sent["images"],
        json!([{"data": big, "mimeType": "image/jpeg"}])
    );

    let answer = act(
        &mut phone,
        // The first started a turn; steering sends into it now.
        json!({"action": "steer", "host": "example-a", "agent": "agt_a_orch",
               "text": "", "message_id": "msg-img2",
               "images": [{"data": "cG5n", "mime_type": "image/png"}]}),
    )
    .await;
    assert_eq!(answer["sent"], true, "{answer}");
    assert_eq!(daemon.writes()[1]["images"][0]["mimeType"], "image/png");

    let answer = act(
        &mut phone,
        json!({"action": "send", "host": "example-a", "agent": "agt_a_orch",
               "text": "x", "message_id": "msg-bad",
               "images": [{"data": "cG5n", "mime_type": "text/html"}]}),
    )
    .await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(daemon.writes().len(), 2);
}

/// A plain HTTP GET: the status line and headers, and the body.
async fn get(addr: std::net::SocketAddr, path: &str) -> (String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let end = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&response[..end]).to_string();
    (head, response[end + 4..].to_vec())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_desktop_gets_the_hosts_with_a_device_token_and_no_passwords() {
    let (daemon, _phone, addr) = fleet_at().await;
    let (head, body) = get(addr, &format!("/api/hosts?t={TOKEN}")).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("Content-Type: application/json"), "{head}");
    let answer: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        answer["hosts"],
        json!([{"id": "example-a", "name": "example-a",
                "endpoint": daemon.endpoint(), "password": false}])
    );
    assert!(answer["reef"].is_string());
    let (head, _) = get(addr, "/api/hosts").await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
    let (head, _) = get(addr, "/api/hosts?t=wrong").await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversations_images_are_fetched_by_key_with_a_device_token() {
    use base64::Engine;
    let (_daemon, mut phone, addr) = fleet_at().await;
    open(&mut phone, "agt_a_orch").await;
    let png: Vec<u8> = b"\x89PNG\r\n\x1a\n not really a picture".to_vec();
    let data = base64::engine::general_purpose::STANDARD.encode(&png);
    let answer = act(
        &mut phone,
        json!({"action": "send", "host": "example-a", "agent": "agt_a_orch",
               "text": "look", "message_id": "msg-pic",
               "images": [{"data": data, "mime_type": "image/png"}]}),
    )
    .await;
    assert_eq!(answer["sent"], true, "{answer}");
    phone
        .until("the image's key", |p| {
            p.view("c").parts.iter().any(|t| {
                t["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|i| i["image_keys"].is_array())
            })
        })
        .await;
    let item = phone
        .view("c")
        .parts
        .iter()
        .flat_map(|t| t["items"].as_array().unwrap().clone())
        .find(|i| i["image_keys"].is_array())
        .unwrap();
    assert_eq!(item["images"], 1);
    let key = item["image_keys"][0].as_str().unwrap().to_string();

    let (head, body) = get(addr, &format!("/image/{key}?t={TOKEN}")).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("Content-Type: image/png"), "{head}");
    assert!(head.contains("immutable"), "{head}");
    assert_eq!(body, png);

    // No token, a wrong one, or a key nobody has: nothing.
    let (head, _) = get(addr, &format!("/image/{key}")).await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
    let (head, _) = get(addr, &format!("/image/{key}?t=wrong")).await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
    let (head, _) = get(addr, &format!("/image/0123456789abcdef?t={TOKEN}")).await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
}

#[tokio::test(flavor = "multi_thread")]
async fn images_a_daemon_keeps_in_files_are_read_from_it_by_key() {
    use base64::Engine;
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let (daemon, mut phone, addr) = fleet_at().await;
    let (shot, chart) = (
        b"\x89PNG a prompt's".to_vec(),
        b"\x89PNG a reply's".to_vec(),
    );
    let kept = "/home/dev/.burrow/attachments/ab12.png";
    daemon.add_file(kept, "image/png", &encode(&shot));
    daemon.add_file(
        "/tmp/paseo-attachments-x/chart.png",
        "image/png",
        &encode(&chart),
    );
    // History from another client: a Burrow lists a prompt's images by
    // path, and a reply names the file an image was saved to.
    daemon.seed_row(
        "agt_a_orch",
        "turn-pic",
        "2026-10-26T10:00:00.000Z",
        json!({"type": "user_message", "text": "", "messageId": "msg-kept",
               "images": [{"path": kept, "mimeType": "image/png"}]}),
    );
    daemon.seed_row(
        "agt_a_orch",
        "turn-pic",
        "2026-10-26T10:00:05.000Z",
        json!({"type": "assistant_message", "messageId": "msg-kept-reply",
               "text": "Here:\n\n![Image](file:///tmp/paseo-attachments-x/chart.png)\n"}),
    );
    open(&mut phone, "agt_a_orch").await;
    let items = |phone: &Phone| -> Vec<Value> {
        (phone.view("c").parts.iter())
            .flat_map(|t| t["items"].as_array().cloned().unwrap_or_default())
            .collect()
    };
    phone
        .until("both images' keys", |p| {
            let items = items(p);
            items
                .iter()
                .any(|i| i["id"] == "msg-kept" && i["image_keys"].is_array())
                && items.iter().any(|i| i["image_sources"].is_object())
        })
        .await;
    let items = items(&phone);
    let prompt = items.iter().find(|i| i["id"] == "msg-kept").unwrap();
    assert_eq!(prompt["images"], 1);
    let reply = items
        .iter()
        .find(|i| i["image_sources"].is_object())
        .unwrap();
    let sources = reply["image_sources"].as_object().unwrap();
    assert_eq!(
        sources.keys().collect::<Vec<_>>(),
        ["file:///tmp/paseo-attachments-x/chart.png"]
    );

    let keys = [
        (prompt["image_keys"][0].as_str().unwrap(), shot),
        (sources.values().next().unwrap().as_str().unwrap(), chart),
    ];
    for (key, bytes) in keys {
        let (head, body) = get(addr, &format!("/image/{key}?t={TOKEN}")).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Content-Type: image/png"), "{head}");
        // Named by where it is, so kept for a day rather than for good.
        assert!(
            head.contains("max-age=86400") && !head.contains("immutable"),
            "{head}"
        );
        assert_eq!(body, bytes);
    }
    // Only the files the conversation names are read.
    let reads = || (daemon.requests().into_iter()).filter(|r| r == "file_explorer_request");
    assert_eq!(reads().count(), 2);
    let (head, _) = get(addr, &format!("/image/0123456789abcdef?t={TOKEN}")).await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
    assert_eq!(reads().count(), 2);
    assert!(daemon.refused().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_to_a_busy_agent_waits_for_its_turn_to_end() {
    let (daemon, mut phone) = fleet().await;
    open(&mut phone, "agt_a_run").await;
    let answer = act(
        &mut phone,
        json!({"action": "send", "host": "example-a", "agent": "agt_a_run",
               "text": "Add a section on tests.", "message_id": "msg-q"}),
    )
    .await;
    assert_eq!(answer["queued"], true, "{answer}");
    phone
        .until("the queued message", |p| {
            queued(p) == ["Add a section on tests."]
        })
        .await;
    assert!(writes(&daemon).is_empty(), "nothing goes while it runs");

    // The turn ends; the message goes, as a plain send.
    daemon.finish_turn(
        "agt_a_run",
        "2026-10-17T10:00:00.000Z",
        "Drafted the guide.",
    );
    phone
        .until("the queue to drain", |p| queued(p).is_empty())
        .await;
    phone
        .until("the message in the conversation", |p| {
            user_texts(p).last().map(String::as_str) == Some("Add a section on tests.")
        })
        .await;
    assert_eq!(writes(&daemon), ["send_agent_message_request"]);
    let sent = &daemon.writes()[0];
    assert_eq!(sent["messageId"], "msg-q");
    assert!(sent.get("activeTurnBehavior").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn steering_goes_into_the_running_turn_and_a_queued_message_can_be_dropped() {
    let (daemon, mut phone) = fleet().await;
    open(&mut phone, "agt_a_run").await;
    let steer = act(
        &mut phone,
        json!({"action": "steer", "host": "example-a", "agent": "agt_a_run",
               "text": "Keep it short.", "message_id": "msg-s"}),
    )
    .await;
    assert_eq!(steer["sent"], true, "{steer}");
    assert_eq!(daemon.writes()[0]["activeTurnBehavior"], "steer");

    // Queued, then steered in after all.
    act(
        &mut phone,
        json!({"action": "send", "host": "example-a", "agent": "agt_a_run",
               "text": "Mention the chat room.", "message_id": "msg-2"}),
    )
    .await;
    let now = act(
        &mut phone,
        json!({"action": "steer", "host": "example-a", "agent": "agt_a_run", "message_id": "msg-2"}),
    )
    .await;
    assert_eq!(now["sent"], true, "{now}");
    assert_eq!(daemon.writes()[1]["messageId"], "msg-2");
    assert_eq!(daemon.writes()[1]["activeTurnBehavior"], "steer");

    // Queued, then dropped: nothing goes.
    act(
        &mut phone,
        json!({"action": "send", "host": "example-a", "agent": "agt_a_run",
               "text": "Never mind.", "message_id": "msg-3"}),
    )
    .await;
    phone.until("the queue", |p| queued(p).len() == 1).await;
    let dropped = act(
        &mut phone,
        json!({"action": "unqueue", "host": "example-a", "agent": "agt_a_run", "message_id": "msg-3"}),
    )
    .await;
    assert_eq!(dropped["ok"], true);
    phone
        .until("an empty queue", |p| queued(p).is_empty())
        .await;
    assert_eq!(writes(&daemon).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn archiving_sends_one_archive_request_and_the_agent_goes() {
    let (daemon, mut phone) = fleet().await;
    let answer = act(
        &mut phone,
        json!({"action": "archive", "host": "example-a", "agent": "agt_a_failed"}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(writes(&daemon), ["archive_agent_request"]);
    assert_eq!(daemon.writes()[0]["agentId"], "agt_a_failed");
    phone
        .until("the agent to go", |p| {
            !p.keys("ny").contains(&"example-a/agt_a_failed".to_string())
        })
        .await;
    // Gone, so nothing more can be done to it.
    let again = act(
        &mut phone,
        json!({"action": "archive", "host": "example-a", "agent": "agt_a_failed"}),
    )
    .await;
    assert_eq!(again["error"]["code"], "not_found");
    assert_eq!(writes(&daemon).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_is_answered_by_header_or_dismissed() {
    let (daemon, mut phone) = fleet().await;
    // Every question needs an answer before anything is sent.
    let empty = act(
        &mut phone,
        json!({"action": "answer", "host": "example-a", "agent": "agt_a_asks",
               "ask": "perm_a_q", "answers": [{"selected": []}]}),
    )
    .await;
    assert_eq!(empty["ok"], false);
    assert_eq!(empty["error"]["code"], "bad_request");
    assert!(writes(&daemon).is_empty());

    let answer = act(
        &mut phone,
        json!({"action": "answer", "host": "example-a", "agent": "agt_a_asks",
               "ask": "perm_a_q", "answers": [{"selected": [1]}]}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(writes(&daemon), ["agent_permission_response"]);
    let response = &daemon.writes()[0]["response"];
    assert_eq!(response["behavior"], "allow");
    assert_eq!(
        response["updatedInput"]["answers"],
        json!({"Scope": "With the refactor"})
    );
    // The question's own input goes back with the answers, as crusteo
    // sends it.
    assert_eq!(response["updatedInput"]["questions"][0]["header"], "Scope");

    // Another daemon's question, dismissed.
    let other = FakeDaemon::start("srv_b").await.unwrap();
    fake::seed(&other, "b");
    let addr = reef_with(&[("example-b", &other)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "ny", "view": "needs_you"}))
        .await;
    phone.until("the fleet", |p| p.keys("ny").len() == 3).await;
    let dismissed = act(
        &mut phone,
        json!({"action": "answer", "host": "example-b", "agent": "agt_b_asks",
               "ask": "perm_b_q", "dismiss": true}),
    )
    .await;
    assert_eq!(dismissed["ok"], true, "{dismissed}");
    assert_eq!(
        other.writes()[0]["response"],
        json!({"behavior": "deny", "message": "Dismissed by user"})
    );
}

/// The texts of the sends the daemon got, in order.
fn sent_texts(daemon: &FakeDaemon) -> Vec<String> {
    daemon
        .writes()
        .iter()
        .filter(|w| w["type"] == "send_agent_message_request")
        .map(|w| w["text"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn send(agent: &str, text: &str, id: &str) -> Value {
    json!({"action": "send", "host": "example-a", "agent": agent, "text": text, "message_id": id})
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_message_stays_marked_and_the_next_one_still_goes() {
    let (daemon, mut phone) = fleet().await;
    open(&mut phone, "agt_a_run").await;
    act(&mut phone, send("agt_a_run", "first", "m1")).await;
    act(&mut phone, send("agt_a_run", "second", "m2")).await;
    phone.until("both queued", |p| queued(p).len() == 2).await;
    // The daemon refuses the first; the second must not be stuck behind it.
    daemon.fail_next_sends(1);
    daemon.finish_turn("agt_a_run", "2026-10-17T10:00:00.000Z", "Done.");
    phone
        .until("the second sent", |p| {
            user_texts(p).last().map(String::as_str) == Some("second")
        })
        .await;
    assert_eq!(sent_texts(&daemon), ["first", "second"]);
    let waiting = phone.view("c").fields["queued"].clone();
    assert_eq!(waiting[0]["text"], "first");
    assert!(waiting[0]["error"].is_string(), "{waiting}");
    // It is not tried again by itself.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(sent_texts(&daemon).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_waits_behind_one_in_flight_and_goes_after_it() {
    let (daemon, mut phone) = fleet().await;
    open(&mut phone, "agt_a_orch").await;
    daemon.hold_sends(true);
    // The agent is idle: the first goes at once, and its reply is held.
    let req = phone
        .ask(
            json!({"type": "action", "action": "send", "host": "example-a",
                    "agent": "agt_a_orch", "text": "first", "message_id": "m1"}),
        )
        .await;
    phone
        .until("the turn to start", |p| {
            !p.view("c").fields["working"].is_null()
        })
        .await;
    // Its turn ends while the reply is still on its way: that idle edge
    // must not be lost.
    // Now, not a fixed time: older news than the send is ignored.
    daemon.finish_turn("agt_a_orch", &fleet::clock::now_iso(), "Done.");
    phone
        .until("the turn to end", |p| {
            p.view("c").fields["working"].is_null()
        })
        .await;
    // The agent looks idle, but the first is still in flight: the second
    // waits instead of jumping ahead.
    let second = act(&mut phone, send("agt_a_orch", "second", "m2")).await;
    assert_eq!(second["queued"], true, "{second}");
    assert_eq!(sent_texts(&daemon), ["first"]);
    daemon.release_sends();
    phone
        .until("the first's answer", |p| p.results.contains_key(&req))
        .await;
    phone
        .until("the second sent", |p| {
            user_texts(p).last().map(String::as_str) == Some("second")
        })
        .await;
    assert_eq!(sent_texts(&daemon), ["first", "second"]);
    assert!(daemon.writes()[1].get("activeTurnBehavior").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queue_holds_twenty_and_goes_with_its_agent() {
    let (_daemon, mut phone) = fleet().await;
    open(&mut phone, "agt_a_run").await;
    for n in 0..reef::upstream::QUEUE_MAX {
        let answer = act(
            &mut phone,
            send("agt_a_run", &format!("m{n}"), &format!("id{n}")),
        )
        .await;
        assert_eq!(answer["queued"], true, "{answer}");
    }
    let refused = act(&mut phone, send("agt_a_run", "one more", "id-extra")).await;
    assert_eq!(refused["ok"], false);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("waiting already")
    );
    phone
        .until("the full queue", |p| {
            queued(p).len() == reef::upstream::QUEUE_MAX
        })
        .await;
    // Archived: its queue goes too.
    act(
        &mut phone,
        json!({"action": "archive", "host": "example-a", "agent": "agt_a_run"}),
    )
    .await;
    phone
        .until("an empty queue", |p| queued(p).is_empty())
        .await;
}
