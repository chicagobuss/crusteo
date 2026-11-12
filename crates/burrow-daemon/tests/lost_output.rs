//! Burrow keeps serving when its standard output and error go away, as when
//! it was started from a terminal that has since closed: logging must not
//! take connections (or the daemon) down with it.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(10);

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

fn scratch_home() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("burrow-lost-output-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("scratch home");
    dir
}

#[tokio::test]
async fn burrow_serves_after_its_output_is_gone() {
    let home = scratch_home();
    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_burrow"))
        .args(["--home"])
        .arg(&home)
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
        .env("BURROW_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start burrow");
    // Close the reading ends at once: every write Burrow makes now fails.
    drop(child.stdout.take());
    drop(child.stderr.take());
    let mut daemon = Daemon(child);

    let url = format!("ws://127.0.0.1:{port}/ws");
    let deadline = Instant::now() + WAIT;
    let mut ws = loop {
        if let Some(status) = daemon.0.try_wait().expect("poll burrow") {
            panic!("burrow exited with {status} once its output was gone");
        }
        match connect_async(url.as_str()).await {
            Ok((ws, _)) => break ws,
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(50)).await,
            Err(err) => panic!("connect to {url}: {err}"),
        }
    };

    let hello = json!({
        "type": "hello", "clientId": "cid_lost_output", "clientType": "cli",
        "protocolVersion": 1, "appVersion": "lost-output-test", "capabilities": {},
    });
    ws.send(Message::text(hello.to_string()))
        .await
        .expect("hello");
    let request = json!({"type": "session", "message":
        {"type": "fetch_agents_request", "requestId": "r1"}});
    ws.send(Message::text(request.to_string()))
        .await
        .expect("request");

    // Both the server info and the agents reply arrive: the connection lived
    // through the log lines it caused.
    let (mut info, mut agents) = (false, false);
    while !(info && agents) {
        let frame = timeout(WAIT, ws.next())
            .await
            .expect("a frame in time")
            .expect("the connection stays open")
            .expect("a frame");
        let Message::Text(text) = frame else { continue };
        let value: Value = serde_json::from_str(text.as_str()).expect("json");
        let message = &value["message"];
        info |= message["payload"]["status"] == "server_info";
        agents |= message["type"] == "fetch_agents_response";
    }
    assert!(
        daemon.0.try_wait().expect("poll").is_none(),
        "burrow still runs"
    );
    drop(daemon);
    let _ = std::fs::remove_dir_all(&home);
}
