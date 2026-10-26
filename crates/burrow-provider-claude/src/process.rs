//! A child process that speaks newline-delimited JSON on stdin and stdout.

use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// How much of stderr to keep, for error messages.
const STDERR_TAIL: usize = 4096;

/// What the process said, in order.
#[derive(Debug)]
pub enum Output {
    Message(Value),
    /// A line that was not JSON (logged; the CLI prints these rarely).
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
    child: Child,
}

impl JsonProcess {
    pub fn spawn(mut command: Command) -> std::io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (input, mut input_rx) = mpsc::unbounded_channel::<Value>();
        let (output_tx, output) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            while let Some(message) = input_rx.recv().await {
                let mut line = message.to_string();
                debug!(target: "burrow::claude::stdin", "{line}");
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
                debug!(target: "burrow::claude::stderr", "{line}");
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
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => {}
                    Ok(Some(line)) => {
                        debug!(target: "burrow::claude::stdout", "{line}");
                        let output = match serde_json::from_str(&line) {
                            Ok(value) => Output::Message(value),
                            Err(_) => {
                                warn!(line, "non-JSON line from the agent process");
                                Output::Garbage(line)
                            }
                        };
                        if stdout_tx.send(output).is_err() {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => break,
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

    /// The channels and the child, to be owned separately. The child is
    /// killed when dropped.
    pub fn into_parts(self) -> Parts {
        Parts {
            input: self.input,
            output: self.output,
            child: self.child,
        }
    }
}

pub struct Parts {
    pub input: mpsc::UnboundedSender<Value>,
    pub output: mpsc::UnboundedReceiver<Output>,
    pub child: Child,
}
