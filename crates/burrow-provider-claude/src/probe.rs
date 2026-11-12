//! Asks the CLI itself which models it offers.
//!
//! Claude Code answers a bare `initialize` control request (no user message,
//! so no model call) with its version, account and `models`. Burrow reads
//! that at discovery, so a new Claude model needs no Burrow release.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use burrow_provider::Model;
use serde_json::{Value, json};

use crate::catalog;

/// How long the CLI may take to answer. It loads the user's settings first.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the CLI has to exit by itself once its input is closed (Claude
/// Code 2.1.317 takes a third of a second) before it is killed.
///
/// It is never killed sooner: a CLI that starts once its sign-in has expired
/// renews it first, and killed mid-renewal it leaves the old refresh token
/// spent and the new one unsaved, which signs out every Claude agent on the
/// host. On a host where no agent has run for longer than a sign-in lasts,
/// the probe is the first to renew it.
const EXIT_WAIT: Duration = Duration::from_secs(60);

/// The launch flags of the probe: print mode with stream-json on both ends,
/// no transcript written for a session that never had a turn, and none of
/// the user's hooks run. The probe is not a session: a `SessionStart` hook
/// that records sessions would record one per probe, and with the probe
/// ending before any turn, no `SessionEnd` might close it. `--bare` would
/// skip hooks too, but it also skips the subscription sign-in.
pub fn arguments() -> [&'static str; 9] {
    [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
        "--settings",
        r#"{"disableAllHooks":true}"#,
    ]
}

/// The models `binary` reports, or why it did not say.
pub fn models(binary: &Path) -> Result<Vec<Model>, String> {
    let mut command = std::process::Command::new(binary);
    command
        .args(arguments())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("NODE_OPTIONS");
    for key in crate::PARENT_SESSION_ENV {
        command.env_remove(key);
    }
    command.env("CLAUDE_CODE_ENTRYPOINT", crate::ENTRYPOINT);
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = std::path::PathBuf::from(home);
        if home.is_dir() {
            command.current_dir(home);
        }
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdin = child.stdin.take();
    let result = ask(&mut child, stdin);
    // Its input is closed now (`ask` took it): the CLI ends by itself, after
    // any renewal of the sign-in it started. Waited for off this thread.
    let _ = std::thread::Builder::new()
        .name("claude-probe-exit".into())
        .spawn(move || settle(child, EXIT_WAIT));
    result
}

/// Waits up to `wait` for the CLI to exit, then kills it.
fn settle(mut child: std::process::Child, wait: Duration) {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    tracing::warn!(
        seconds = wait.as_secs(),
        "Claude's CLI was still running after its models were read; stopping it"
    );
    let _ = child.kill();
    let _ = child.wait();
}

fn ask(
    child: &mut std::process::Child,
    stdin: Option<std::process::ChildStdin>,
) -> Result<Vec<Model>, String> {
    let mut stdin = stdin.ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, lines) = mpsc::channel();
    // Reads to the end, after the answer too, so the CLI never blocks on a
    // full pipe while it exits.
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&buffer) {
                let _ = tx.send(value);
            }
        }
    });
    let request = json!({"type": "control_request", "request_id": "probe",
        "request": {"subtype": "initialize"}});
    writeln!(stdin, "{request}")
        .and_then(|()| stdin.flush())
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let frame = lines
            .recv_timeout(left)
            .map_err(|_| "no answer to initialize".to_string())?;
        if frame["type"] != "control_response" || frame["response"]["request_id"] != "probe" {
            continue;
        }
        let response = &frame["response"];
        if response["subtype"] == "error" {
            return Err(response["error"].as_str().unwrap_or("error").to_string());
        }
        return catalog::from_initialize(&response["response"])
            .ok_or_else(|| "its initialize reply lists no models".to_string());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_cli_that_does_not_exit_is_stopped_after_the_wait() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let started = Instant::now();
        settle(child, Duration::from_millis(300));
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    #[test]
    fn a_cli_that_exits_is_not_waited_on_further() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let started = Instant::now();
        settle(child, Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
