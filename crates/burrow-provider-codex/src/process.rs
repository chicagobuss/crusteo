//! `codex app-server`: a child process that speaks newline-delimited
//! JSON-RPC on stdin and stdout.
//!
//! Lines are split on `\n` only. Codex writes U+2028 and U+2029 raw inside
//! JSON strings, so a reader that also breaks on those (Node's `readline`)
//! cuts messages apart; Paseo replaced its reader for that reason.

use std::process::Stdio;

use burrow_provider::group::{self, Grouped};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// How much of stderr to keep, for error messages.
const STDERR_TAIL: usize = 4096;

/// What the process said, in order.
#[derive(Debug)]
pub enum Output {
    Message(Value),
    /// A line that was not JSON (logged).
    Garbage(String),
    /// The process exited; the tail of its stderr.
    Exited {
        status: Option<i32>,
        stderr: String,
    },
}

pub struct JsonProcess {
    pub input: mpsc::UnboundedSender<Value>,
    pub output: mpsc::UnboundedReceiver<Output>,
    /// Its process group ends when it is dropped.
    pub child: Grouped,
}

impl JsonProcess {
    pub fn spawn(mut command: Command) -> std::io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = group::spawn(&mut command)?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (input, mut input_rx) = mpsc::unbounded_channel::<Value>();
        let (output_tx, output) = mpsc::unbounded_channel();

        // Dropping every sender of `input` closes stdin, which app-server
        // takes as the end of the session.
        tokio::spawn(async move {
            while let Some(message) = input_rx.recv().await {
                let mut line = message.to_string();
                debug!(target: "burrow::codex::stdin", "{line}");
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        let (stderr_tx, stderr_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut tail = String::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(target: "burrow::codex::stderr", "{line}");
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > STDERR_TAIL {
                    let cut = tail.len() - STDERR_TAIL;
                    let cut = (cut..tail.len())
                        .find(|i| tail.is_char_boundary(*i))
                        .unwrap_or(0);
                    tail.drain(..cut);
                }
            }
            let _ = stderr_tx.send(tail);
        });

        let stdout_tx = output_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(b'\n', &mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buffer);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.trim().is_empty() {
                    continue;
                }
                debug!(target: "burrow::codex::stdout", "{line}");
                let output = match serde_json::from_str(line) {
                    Ok(value) => Output::Message(value),
                    Err(_) => {
                        warn!(line, "non-JSON line from codex app-server");
                        Output::Garbage(line.to_string())
                    }
                };
                if stdout_tx.send(output).is_err() {
                    break;
                }
            }
            let stderr = stderr_rx.await.unwrap_or_default();
            let _ = stdout_tx.send(Output::Exited {
                status: None,
                stderr,
            });
        });

        Ok(Self {
            input,
            output,
            child,
        })
    }
}
