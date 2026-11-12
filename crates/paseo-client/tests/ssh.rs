//! `ssh://` endpoints, through a stand-in for ssh (testing/fake_ssh.rs) that
//! pipes to a daemon on this machine.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use paseo_client::{Client, Config, ConnState, Event};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

fn fake_ssh() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-ssh"))
}

/// A daemon that answers each hello with its server info and then keeps
/// the connection, counting the connections it took.
async fn daemon(server_id: &'static str) -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen, seen_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                use tokio_tungstenite::tungstenite::handshake::server::{
                    ErrorResponse, Request, Response,
                };
                let mut host = String::new();
                // tungstenite's callback type, error response and all.
                #[allow(clippy::result_large_err)]
                let callback = |request: &Request, response: Response| {
                    host = (request.headers().get("host"))
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    Ok::<Response, ErrorResponse>(response)
                };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                    return;
                };
                let _ = seen.send(host);
                let Some(Ok(_hello)) = ws.next().await else {
                    return;
                };
                let info = json!({"type": "status", "payload": {
                    "status": "server_info", "serverId": server_id, "hostname": "far", "version": "test"
                }});
                if ws.send(Message::text(info.to_string())).await.is_err() {
                    return;
                }
                while ws.next().await.is_some_and(|frame| frame.is_ok()) {}
            });
        }
    });
    (port, seen_rx)
}

async fn until_state(client: &Client, wanted: impl Fn(&ConnState) -> bool) -> ConnState {
    let events = client.events();
    timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(Event::State(state)) = events.recv().await
                && wanted(&state)
            {
                return state;
            }
        }
    })
    .await
    .expect("the state in time")
}

/// Removes the stand-in's notes of its runs for `host`.
fn forget_runs(host: &str) {
    let name = host.replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    let _ = std::fs::remove_file(std::env::temp_dir().join(format!("fake-ssh-{name}.log")));
}

fn config(endpoint: String) -> Config {
    let mut config = Config::new(endpoint, "cid_ssh_test");
    config.ssh_program = Some(fake_ssh());
    config
}

#[test]
fn a_daemon_is_reached_through_ssh() {
    paseo_client::block_on(async {
        let (port, mut seen) = daemon("srv_far").await;
        // The stand-in writes each run down by host (testing/fake_ssh.rs).
        let host = format!("dev@far{}.example", std::process::id());
        let log = std::env::temp_dir().join(format!(
            "fake-ssh-{}.log",
            host.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
        ));
        let _ = std::fs::remove_file(&log);
        let client = Client::connect(config(format!("ssh://{host}?daemonPort={port}")));
        let ConnState::Connected(info) =
            until_state(&client, |s| matches!(s, ConnState::Connected(_))).await
        else {
            unreachable!()
        };
        assert_eq!(info.server_id, "srv_far");
        assert_eq!(
            seen.recv().await.as_deref(),
            Some(format!("127.0.0.1:{port}").as_str()),
            "the handshake asks for the daemon on the far host's loopback"
        );
        let args = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            args.trim(),
            format!(
                "-T -o BatchMode=yes -o ConnectTimeout=10 -o ClearAllForwardings=yes \
                 -o ExitOnForwardFailure=yes -W 127.0.0.1:{port} {host}"
            )
        );
        // A reconnect starts ssh again.
        client.reconnect();
        until_state(&client, |s| matches!(s, ConnState::Connected(_))).await;
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);
        let _ = std::fs::remove_file(&log);
    });
}

#[test]
fn what_ssh_says_is_the_reason_a_host_is_down() {
    paseo_client::block_on(async {
        let client = Client::connect(config("ssh://dev@denied.example".into()));
        let ConnState::Disconnected { reason, auth } =
            until_state(&client, |s| matches!(s, ConnState::Disconnected { .. })).await
        else {
            unreachable!()
        };
        assert_eq!(reason, "dev@denied.example: Permission denied (publickey).");
        assert!(!auth, "ssh failing is not the daemon refusing the hello");

        let (port, _seen) = daemon("srv_unused").await;
        let client = Client::connect(config(format!("ssh://closed.example?daemonPort={port}")));
        let ConnState::Disconnected { reason, .. } =
            until_state(&client, |s| matches!(s, ConnState::Disconnected { .. })).await
        else {
            unreachable!()
        };
        assert_eq!(
            reason,
            "channel 0: open failed: connect failed: Connection refused; stdio forwarding failed"
        );
        drop(client);
        forget_runs("dev@denied.example");
        forget_runs("closed.example");
    });
}

#[test]
fn a_missing_ssh_and_a_malformed_endpoint_say_so() {
    paseo_client::block_on(async {
        let mut missing = config("ssh://box".into());
        missing.ssh_program = Some(PathBuf::from("no-such-ssh-program"));
        let client = Client::connect(missing);
        let ConnState::Disconnected { reason, .. } =
            until_state(&client, |s| matches!(s, ConnState::Disconnected { .. })).await
        else {
            unreachable!()
        };
        assert!(reason.contains("was not found"), "{reason}");

        let client = Client::connect(config("ssh://dev:secret@box".into()));
        let ConnState::Disconnected { reason, .. } =
            until_state(&client, |s| matches!(s, ConnState::Disconnected { .. })).await
        else {
            unreachable!()
        };
        assert_eq!(reason, "Invalid SSH host URI");
        forget_runs("box");
    });
}
