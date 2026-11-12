//! `restart_server_request`, as crusteo's Restart sends it: Burrow answers
//! `restart_requested`, stops as it does on SIGTERM and starts again in the
//! same process, so a service manager watching that process sees no change.
//! What it saved is there after.
#![cfg(unix)]

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const WAIT: Duration = Duration::from_secs(15);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Kills the daemon however the test ends.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("a free port")
}

/// Connects once the daemon listens, and says hello.
async fn connect(daemon: &mut Daemon, port: u16) -> Ws {
    let url = format!("ws://127.0.0.1:{port}/ws");
    let deadline = Instant::now() + WAIT;
    let mut ws = loop {
        if let Some(status) = daemon.0.try_wait().expect("poll burrow") {
            panic!("burrow exited with {status}");
        }
        match connect_async(url.as_str()).await {
            Ok((ws, _)) => break ws,
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(50)).await,
            Err(err) => panic!("connect to {url}: {err}"),
        }
    };
    let hello = json!({
        "type": "hello", "clientId": "cid_restart", "clientType": "cli",
        "protocolVersion": 1, "appVersion": "restart-test", "capabilities": {},
    });
    ws.send(Message::text(hello.to_string()))
        .await
        .expect("hello");
    ws
}

/// Sends `message` and returns the session message answering it.
async fn ask(ws: &mut Ws, message: Value) -> Value {
    let id = message["requestId"].as_str().unwrap().to_string();
    let frame = json!({"type": "session", "message": message});
    ws.send(Message::text(frame.to_string()))
        .await
        .expect("send");
    loop {
        let frame = timeout(WAIT, ws.next())
            .await
            .expect("an answer in time")
            .expect("the connection stays open")
            .expect("a frame");
        let Message::Text(text) = frame else { continue };
        let value: Value = serde_json::from_str(text.as_str()).expect("json");
        let message = &value["message"];
        if message["payload"]["requestId"] == id.as_str() {
            return message.clone();
        }
    }
}

#[tokio::test]
async fn a_restart_starts_burrow_again_in_the_same_process_with_its_state() {
    let home = std::env::temp_dir().join(format!("burrow-restart-{}", uuid::Uuid::new_v4()));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_burrow"))
        .arg("--home")
        .arg(home.join("state"))
        .args(["--listen", &format!("127.0.0.1:{port}")])
        .args([
            "--no-claude",
            "--no-codex",
            "--no-agy",
            "--no-pi",
            "--no-cursor",
        ])
        .args([
            "--no-opencode",
            "--no-gemini",
            "--no-paseo-lock",
            "--no-push",
        ])
        .arg("--echo-provider")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start burrow");
    let pid = child.id();
    let mut daemon = Daemon(child);

    let mut ws = connect(&mut daemon, port).await;
    let before = ask(
        &mut ws,
        json!({"type": "daemon.get_status.request", "requestId": "s1"}),
    )
    .await;
    let started = before["payload"]["startedAt"].clone();
    let created = ask(
        &mut ws,
        json!({"type": "create_agent_request", "requestId": "c1", "labels": {},
               "config": {"provider": "echo", "cwd": cwd.to_string_lossy()}}),
    )
    .await;
    let agent = created["payload"]["agentId"]
        .as_str()
        .or_else(|| created["payload"]["agent"]["id"].as_str())
        .unwrap_or_else(|| panic!("an agent: {created}"))
        .to_string();

    let restarting = ask(
        &mut ws,
        json!({"type": "restart_server_request", "requestId": "r1", "reason": "test"}),
    )
    .await;
    assert_eq!(restarting["type"], "status", "{restarting}");
    assert_eq!(restarting["payload"]["status"], "restart_requested");
    assert_eq!(restarting["payload"]["reason"], "test");
    // The socket closes as Burrow stops.
    let closed = timeout(WAIT, async {
        while let Some(Ok(frame)) = ws.next().await {
            if let Message::Close(_) = frame {
                break;
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the socket closed");

    // Back on the same port, in the same process, started anew.
    sleep(Duration::from_millis(300)).await;
    let mut ws = connect(&mut daemon, port).await;
    assert_eq!(daemon.0.id(), pid);
    let after = ask(
        &mut ws,
        json!({"type": "daemon.get_status.request", "requestId": "s2"}),
    )
    .await;
    assert_ne!(after["payload"]["startedAt"], started, "{after}");
    let found = ask(
        &mut ws,
        json!({"type": "fetch_agent_request", "requestId": "f1", "agentId": agent}),
    )
    .await;
    assert_eq!(found["payload"]["agent"]["id"], agent.as_str(), "{found}");
    assert!(
        daemon.0.try_wait().expect("poll").is_none(),
        "still running"
    );
    drop(daemon);
    let _ = std::fs::remove_dir_all(&home);
}
