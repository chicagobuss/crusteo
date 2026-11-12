//! `burrow status`: what a running Burrow reports, and what it says when
//! nothing answers.

use std::net::TcpListener;
use std::process::Command;

use burrow_daemon::{Config, Daemon, seed, start};

#[tokio::test]
async fn status_reports_a_running_daemon() {
    let daemon = Daemon::new(Config::ephemeral());
    seed::demo(&daemon);
    let server_id = daemon.config.server_id.clone();
    let running = start(daemon).await.expect("start");
    let endpoint = running.local_addr.to_string();
    let report = burrow_daemon::status::fetch(&endpoint)
        .await
        .expect("a report");
    assert_eq!(report.endpoint, endpoint);
    assert_eq!(report.server_id, server_id);
    assert_eq!(report.pid, Some(u64::from(std::process::id())));
    assert_eq!(report.total(), 1, "the demo's agent: {report:?}");
    let text = report.render();
    assert!(
        text.contains(&format!("answering at ws://{endpoint}/ws")),
        "{text}"
    );
    assert!(text.contains("Agents: 1 ("), "{text}");
}

#[test]
fn status_fails_plainly_when_nothing_answers() {
    // A port that was free a moment ago: nothing listens there.
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("a free port");
    let out = Command::new(env!("CARGO_BIN_EXE_burrow"))
        .args(["status", "--host", &format!("127.0.0.1:{port}")])
        .env_remove("PASEO_HOST")
        .output()
        .expect("run burrow status");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with(&format!(
            "burrow status: nothing answers at ws://127.0.0.1:{port}/ws"
        )),
        "{err}"
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn help_says_how_to_drive_burrow() {
    let out = Command::new(env!("CARGO_BIN_EXE_burrow"))
        .arg("--help")
        .output()
        .expect("run burrow --help");
    let help = String::from_utf8_lossy(&out.stdout);
    for line in [
        "burrow ls",
        "burrow send <agent>",
        "burrow rename <agent>",
        "burrow archive <agent>",
        "burrow status",
        "--host <host[:port]>",
        "Paseo's CLI does everything else",
    ] {
        assert!(help.contains(line), "--help mentions {line:?}:\n{help}");
    }
}
