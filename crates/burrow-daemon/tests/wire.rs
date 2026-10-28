//! Protocol-level tests with a bare WebSocket client: the frames on the
//! wire, close codes and timers, checked against what Paseo does.

use std::time::Duration;

use burrow_daemon::daemon::Timing;
use burrow_daemon::{Config, Daemon, Running, seed, start};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const WAIT: Duration = Duration::from_secs(5);

async fn daemon_with(timing: Timing) -> (Running, String) {
    let mut config = Config::ephemeral();
    config.timing = timing;
    let daemon = Daemon::new(config);
    let agent = seed::demo(&daemon);
    (start(daemon).await.expect("start"), agent)
}

async fn daemon() -> (Running, String) {
    daemon_with(Timing::default()).await
}

/// `start` hands the daemon back already listening, but under a full
/// parallel test run a loopback connect on Windows now and then fails with
/// an I/O error anyway, so those are retried briefly.
async fn connect(running: &Running) -> Ws {
    let url = running.url();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut pause = Duration::from_millis(10);
    loop {
        match connect_async(url.as_str()).await {
            Ok((ws, _)) => return ws,
            Err(Error::Io(err)) if Instant::now() < deadline => {
                eprintln!("connect to {url} failed, retrying: {err}");
                sleep(pause).await;
                pause = (pause * 2).min(Duration::from_millis(200));
            }
            Err(err) => panic!("connect to {url}: {err}"),
        }
    }
}

fn hello_frame(version: i64) -> Value {
    json!({
        "type": "hello", "clientId": "cid_wire_test", "clientType": "cli",
        "protocolVersion": version, "appVersion": "wire-test",
        "capabilities": {"hello_rejection": true, "owned_subscriptions": true,
                         "selective_agent_timeline": true, "explicit_event_subscriptions": true},
    })
}

async fn send(ws: &mut Ws, frame: Value) {
    ws.send(Message::text(frame.to_string()))
        .await
        .expect("send");
}

/// The next text frame as JSON, skipping control frames.
async fn next_json(ws: &mut Ws) -> Value {
    loop {
        let frame = timeout(WAIT, ws.next())
            .await
            .expect("frame in time")
            .expect("open")
            .expect("ok");
        match frame {
            Message::Text(text) => return serde_json::from_str(text.as_str()).expect("json"),
            Message::Close(frame) => panic!("closed: {frame:?}"),
            _ => {}
        }
    }
}

/// The close code and reason the server sends next.
async fn close_of(ws: &mut Ws) -> (u16, String) {
    loop {
        match timeout(WAIT, ws.next()).await.expect("close in time") {
            Some(Ok(Message::Close(Some(frame)))) => {
                return (frame.code.into(), frame.reason.to_string());
            }
            Some(Ok(Message::Close(None))) | None | Some(Err(_)) => panic!("no close frame"),
            Some(Ok(_)) => {}
        }
    }
}

async fn admitted(running: &Running) -> (Ws, Value) {
    let mut ws = connect(running).await;
    send(&mut ws, hello_frame(1)).await;
    let info = next_json(&mut ws).await;
    (ws, info)
}

/// Sends a session request and returns the message that carries its id.
async fn request(ws: &mut Ws, mut message: Value, id: &str) -> Value {
    message["requestId"] = id.into();
    send(ws, json!({"type": "session", "message": message})).await;
    loop {
        let frame = next_json(ws).await;
        if frame["message"]["payload"]["requestId"] == id {
            return frame["message"].clone();
        }
    }
}

#[tokio::test]
async fn hello_is_answered_with_server_info() {
    let (running, _) = daemon().await;
    let (_ws, info) = admitted(&running).await;
    assert_eq!(info["type"], "session");
    assert_eq!(info["message"]["type"], "status");
    let payload = &info["message"]["payload"];
    assert_eq!(payload["status"], "server_info");
    assert_eq!(payload["protocolVersion"], 1);
    assert_eq!(
        payload["serverId"],
        running.daemon.config.server_id.as_str()
    );
    assert_eq!(payload["features"]["ownedSubscriptions"], true);
    assert_eq!(payload["features"]["directorySync"], true);
}

#[tokio::test]
async fn frames_before_hello_close_the_socket() {
    let (running, _) = daemon().await;
    let mut ws = connect(&running).await;
    send(&mut ws, json!({"type": "ping"})).await;
    assert_eq!(
        close_of(&mut ws).await,
        (4002, "Session message before hello".into())
    );

    let mut ws = connect(&running).await;
    ws.send(Message::text("this is not json")).await.unwrap();
    assert_eq!(close_of(&mut ws).await, (4002, "Invalid hello".into()));
}

#[tokio::test]
async fn an_old_protocol_is_rejected_with_a_frame_then_4003() {
    let (running, _) = daemon().await;
    let mut ws = connect(&running).await;
    send(&mut ws, hello_frame(0)).await;
    let rejected = next_json(&mut ws).await;
    assert_eq!(
        rejected,
        json!({"type": "hello.rejected", "reason": "incompatible_protocol", "accepts": ["password"]})
    );
    assert_eq!(
        close_of(&mut ws).await,
        (4003, "Incompatible protocol version".into())
    );
}

#[tokio::test]
async fn a_second_hello_closes_the_socket() {
    let (running, _) = daemon().await;
    let (mut ws, _) = admitted(&running).await;
    send(&mut ws, hello_frame(1)).await;
    assert_eq!(close_of(&mut ws).await, (4002, "Unexpected hello".into()));
}

#[tokio::test]
async fn hello_must_arrive_in_time() {
    let (running, _) = daemon_with(Timing {
        hello_timeout: Duration::from_millis(300),
        ..Timing::default()
    })
    .await;
    let mut ws = connect(&running).await;
    assert_eq!(close_of(&mut ws).await, (4001, "Hello timeout".into()));
}

#[tokio::test]
async fn a_further_address_is_served_once_it_can_be_bound() {
    // Held by something else, as a VPN's address is missing before it is up.
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let further = held.local_addr().unwrap();
    let mut config = Config::ephemeral();
    config.also_listen = vec![further];
    config.timing.listen_retry = Duration::from_millis(50);
    let running = start(Daemon::new(config)).await.expect("start");
    // The first address serves meanwhile.
    let (_ws, info) = admitted(&running).await;
    assert_eq!(info["message"]["payload"]["status"], "server_info");

    drop(held);
    let url = format!("ws://{further}/ws");
    let deadline = Instant::now() + WAIT;
    let mut ws = loop {
        match connect_async(url.as_str()).await {
            Ok((ws, _)) => break ws,
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(50)).await,
            Err(err) => panic!("connect to {url}: {err}"),
        }
    };
    send(&mut ws, hello_frame(1)).await;
    let info = next_json(&mut ws).await;
    assert_eq!(
        info["message"]["payload"]["serverId"],
        running.daemon.config.server_id.as_str()
    );
    running.shutdown().await;
}

#[tokio::test]
async fn the_path_must_be_ws() {
    let (running, _) = daemon().await;
    let url = format!("ws://{}/other", running.local_addr);
    assert!(connect_async(url).await.is_err());
}

#[tokio::test]
async fn errors_follow_paseo() {
    let (running, _) = daemon().await;
    let (mut ws, _) = admitted(&running).await;

    let unknown = request(&mut ws, json!({"type": "definitely.not.a.request"}), "r1").await;
    assert_eq!(unknown["type"], "rpc_error");
    assert_eq!(unknown["payload"]["code"], "unknown_schema");
    assert_eq!(
        unknown["payload"]["requestType"],
        "definitely.not.a.request"
    );

    // A known type with a missing field is also "unknown_schema".
    let malformed = request(&mut ws, json!({"type": "fetch_agent_request"}), "r2").await;
    assert_eq!(malformed["payload"]["code"], "unknown_schema");

    // Garbage after hello is an uncorrelated status error; the socket stays.
    ws.send(Message::text("this is not json")).await.unwrap();
    let error = next_json(&mut ws).await;
    assert_eq!(error["message"]["payload"]["status"], "error");
    let pong = request(&mut ws, json!({"type": "ping", "clientSentAt": 7}), "r3").await;
    assert_eq!(pong["type"], "pong");
    assert_eq!(pong["payload"]["clientSentAt"], 7);
    assert!(pong["payload"]["serverSentAt"].as_i64().unwrap() > 0);

    // Subscription ids are the host's to assign.
    let chosen = json!({"type": "fetch_agents_request", "subscribe": {"subscriptionId": "mine"}});
    let refused = request(&mut ws, chosen, "r4").await;
    assert_eq!(refused["payload"]["code"], "handler_error");

    // Releasing an id that never existed is acknowledged.
    let release = json!({"type": "subscription.release.request", "subscriptionId": "no-such-sub"});
    let ack = request(&mut ws, release, "r5").await;
    assert_eq!(ack["type"], "subscription.release.response");
}

#[tokio::test]
async fn bare_ping_gets_bare_pong() {
    let (running, _) = daemon().await;
    let (mut ws, _) = admitted(&running).await;
    send(&mut ws, json!({"type": "ping"})).await;
    assert_eq!(next_json(&mut ws).await, json!({"type": "pong"}));
}

#[tokio::test]
async fn directory_sync_cursors() {
    let (running, _) = daemon().await;
    let (mut ws, _) = admitted(&running).await;
    let read = |sync: Value| json!({"type": "fetch_workspaces_request", "sync": sync});

    let first = request(&mut ws, read(json!({})), "s1").await;
    let sync = &first["payload"]["sync"];
    assert_eq!(sync["mode"], "snapshot");
    assert_eq!(sync["reason"], "no_cursor");
    assert_eq!(first["payload"]["entries"].as_array().unwrap().len(), 1);
    assert!(first["payload"]["entries"][0]["syncSeq"].as_u64().is_some());
    let generation = sync["generation"].clone();
    let head = sync["headSeq"].as_u64().unwrap();

    let current = request(
        &mut ws,
        read(json!({"generation": generation, "afterSeq": head})),
        "s2",
    )
    .await;
    assert_eq!(current["payload"]["sync"]["mode"], "changes");
    assert!(current["payload"]["entries"].as_array().unwrap().is_empty());

    let restarted = request(
        &mut ws,
        read(json!({"generation": "bogus", "afterSeq": 1})),
        "s3",
    )
    .await;
    assert_eq!(restarted["payload"]["sync"]["reason"], "generation_changed");

    let future = json!({"generation": generation, "afterSeq": head + 1000});
    let expired = request(&mut ws, read(future), "s4").await;
    assert_eq!(expired["payload"]["sync"]["reason"], "cursor_expired");

    let filtered = json!({"type": "fetch_agents_request", "scope": "active",
                          "filter": {"statuses": ["idle"]}, "sync": {}});
    let refused = request(&mut ws, filtered, "s5").await;
    assert_eq!(refused["type"], "rpc_error");
    assert_eq!(refused["payload"]["code"], "invalid_request");
}

#[tokio::test]
async fn subscriptions_push_changes_to_their_socket_only() {
    let (running, agent_id) = daemon().await;
    let (mut ws, _) = admitted(&running).await;
    let (mut other, _) = admitted(&running).await;

    let opened = request(
        &mut ws,
        json!({"type": "fetch_agents_request", "scope": "active", "subscribe": {}}),
        "a1",
    )
    .await;
    let subscription = opened["payload"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        opened["payload"]["entries"][0]["agent"]["id"],
        agent_id.as_str()
    );
    let timeline =
        json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent_id]});
    let timeline_ack = request(&mut ws, timeline, "a2").await;
    let timeline_subscription = timeline_ack["payload"]["subscriptionId"].clone();

    let new_agent = seed::agent("agent-two", "/tmp/elsewhere", None, "Second");
    running
        .daemon
        .apply(|store| ((), store.upsert_agent(new_agent)));
    let update = next_json(&mut ws).await;
    assert_eq!(update["message"]["type"], "agent_update");
    let payload = &update["message"]["payload"];
    assert_eq!(payload["subscriptionId"], subscription.as_str());
    assert_eq!(payload["kind"], "upsert");
    assert_eq!(payload["agent"]["id"], "agent-two");
    assert!(payload.get("seq").is_none(), "no sync, no seq");

    let item = json!({"type": "assistant_message", "text": "more"});
    running.daemon.apply(|store| {
        let event = store.append_timeline(
            &agent_id,
            item,
            Some("foreground-turn-2".into()),
            "t".into(),
        );
        ((), event.into_iter().collect())
    });
    let stream = next_json(&mut ws).await;
    let payload = &stream["message"]["payload"];
    assert_eq!(stream["message"]["type"], "agent_stream");
    assert_eq!(payload["subscriptionId"], timeline_subscription);
    assert_eq!(payload["seq"], 3);
    assert_eq!(payload["event"]["type"], "timeline");
    assert_eq!(payload["event"]["turnId"], "foreground-turn-2");

    // The other socket subscribed to nothing and hears nothing.
    send(&mut other, json!({"type": "ping"})).await;
    assert_eq!(next_json(&mut other).await, json!({"type": "pong"}));
}

#[tokio::test]
async fn timeline_pages_read_history() {
    let (running, agent_id) = daemon().await;
    let (mut ws, _) = admitted(&running).await;
    let tail = json!({"type": "fetch_agent_timeline_request", "agentId": agent_id,
                      "direction": "tail", "limit": 40, "projection": "projected"});
    let page = request(&mut ws, tail, "t1").await;
    let payload = &page["payload"];
    assert_eq!(page["type"], "fetch_agent_timeline_response");
    assert_eq!(payload["entries"].as_array().unwrap().len(), 2);
    assert_eq!(payload["entries"][0]["item"]["type"], "user_message");
    assert_eq!(payload["entries"][1]["turnId"], "foreground-turn-1");
    assert_eq!(
        payload["window"],
        json!({"minSeq": 1, "maxSeq": 2, "nextSeq": 3})
    );
    assert_eq!(payload["hasOlder"], false);
    assert!(payload["error"].is_null());

    let missing = json!({"type": "fetch_agent_timeline_request", "agentId": "nope"});
    let missing = request(&mut ws, missing, "t2").await;
    assert!(missing["payload"]["error"].is_string());
}

#[tokio::test]
async fn one_ping_commits_the_socket_to_the_lease() {
    let (running, _) = daemon_with(Timing {
        lease: Duration::from_millis(600),
        lease_check: Duration::from_millis(100),
        ..Timing::default()
    })
    .await;

    // Never pinged: never leased, stays open through the silence.
    let (mut quiet, _) = admitted(&running).await;
    // Pinged once, then silent: terminated without a close frame.
    let (mut leased, _) = admitted(&running).await;
    send(&mut leased, json!({"type": "ping"})).await;
    assert_eq!(next_json(&mut leased).await, json!({"type": "pong"}));
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let ended = timeout(WAIT, leased.next()).await.expect("ended in time");
    assert!(
        !matches!(ended, Some(Ok(Message::Close(Some(_))))),
        "a terminated socket gets no close code: {ended:?}"
    );
    send(&mut quiet, json!({"type": "ping"})).await;
    assert_eq!(next_json(&mut quiet).await, json!({"type": "pong"}));
}

#[tokio::test]
async fn shutdown_closes_sockets_with_1001() {
    let (running, _) = daemon().await;
    let (mut ws, _) = admitted(&running).await;
    let stop = tokio::spawn(running.shutdown());
    assert_eq!(
        close_of(&mut ws).await,
        (1001, "Server shutting down".into())
    );
    drop(ws);
    timeout(WAIT, stop).await.unwrap().unwrap();
}
