//! A `pi --mode rpc` child: newline-delimited JSON on stdin and stdout.
//!
//! Records split only on LF, as pi's RPC docs require (a JSON string may
//! hold U+2028). Dropping the input sender closes stdin, which is pi's
//! orderly shutdown; a pi that has not exited shortly after its output is
//! no longer read is killed. pi runs in a process group of its own, which
//! ends with it ([`burrow_provider::group`]).

use std::process::Stdio;
use std::time::Duration;

use burrow_provider::group;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// How much of stderr to keep, for error messages.
const STDERR_TAIL: usize = 8192;
/// How long pi may take to exit once its stdin is closed or stdout ends.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// What the process said, in order.
#[derive(Debug)]
pub enum Output {
    Message(Value),
    /// A line that was not JSON (logged; pi keeps stdout for records).
    Garbage(String),
    /// The process exited: its exit code, if it had one, and the tail of
    /// its stderr.
    Exited {
        status: Option<i32>,
        stderr: String,
    },
}

pub struct JsonProcess {
    pub input: mpsc::UnboundedSender<Value>,
    pub output: mpsc::UnboundedReceiver<Output>,
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

        tokio::spawn(async move {
            while let Some(message) = input_rx.recv().await {
                let mut line = message.to_string();
                debug!(target: "burrow::pi::stdin", "{line}");
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            // Dropping stdin asks pi to shut down.
        });

        let (stderr_tx, stderr_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut tail = String::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(target: "burrow::pi::stderr", "{line}");
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

        // The reader owns the child: it ends with stdout, then reaps it.
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buffer = Vec::new();
            let mut listening = true;
            loop {
                buffer.clear();
                let read = tokio::select! {
                    read = reader.read_until(b'\n', &mut buffer) => read,
                    // Nobody listens any more: stop, and stop pi with what it
                    // started.
                    _ = output_tx.closed() => {
                        listening = false;
                        child.end();
                        break;
                    }
                };
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buffer);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.trim().is_empty() {
                    continue;
                }
                debug!(target: "burrow::pi::stdout", "{line}");
                let output = match serde_json::from_str(line) {
                    Ok(value) => Output::Message(value),
                    Err(_) => {
                        warn!(line, "non-JSON line from pi");
                        Output::Garbage(line.to_string())
                    }
                };
                if output_tx.send(output).is_err() {
                    listening = false;
                    child.end();
                    break;
                }
            }
            let status = match tokio::time::timeout(EXIT_GRACE, child.wait()).await {
                Ok(Ok(status)) => status.code(),
                _ => {
                    let _ = child.kill().await;
                    None
                }
            };
            if listening {
                let stderr = stderr_rx.await.unwrap_or_default();
                let _ = output_tx.send(Output::Exited { status, stderr });
            }
        });

        Ok(Self { input, output })
    }
}
