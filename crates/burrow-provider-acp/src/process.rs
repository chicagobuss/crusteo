//! The agent process: newline-delimited JSON-RPC on stdin and stdout.

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// How much of stderr to keep, for error messages.
const STDERR_TAIL: usize = 4096;
/// How long to wait for the exit status once stdout has closed.
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// What the process said, in order.
#[derive(Debug)]
pub enum Output {
    Message(Value),
    /// A line that was not JSON (logged and skipped, as Paseo does).
    Garbage(String),
    /// The process exited: its code, the signal that ended it, and the
    /// tail of its stderr.
    Exited {
        code: Option<i32>,
        signal: Option<String>,
        stderr: String,
    },
}

/// The two ends of an agent process (or a test double). Dropping `output`
/// kills the process.
pub struct Transport {
    pub input: mpsc::UnboundedSender<Value>,
    pub output: mpsc::UnboundedReceiver<Output>,
}

type Trace = Option<Arc<Mutex<std::fs::File>>>;

fn record(trace: &Trace, dir: &str, frame: &Value) {
    if let Some(file) = trace
        && let Ok(mut file) = file.lock()
    {
        let _ = writeln!(file, "{}", json!({"dir": dir, "frame": frame}));
    }
}

/// Starts `command` with piped stdio. With `trace`, every frame is also
/// written there as `{"dir":"in"|"out","frame":...}`.
pub fn spawn(mut command: Command, trace: Option<&Path>) -> std::io::Result<Transport> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let trace: Trace = trace
        .and_then(|path| std::fs::File::create(path).ok())
        .map(|file| Arc::new(Mutex::new(file)));
    let (input, mut input_rx) = mpsc::unbounded_channel::<Value>();
    let (output_tx, output) = mpsc::unbounded_channel();

    let in_trace = trace.clone();
    tokio::spawn(async move {
        while let Some(message) = input_rx.recv().await {
            record(&in_trace, "in", &message);
            let mut line = message.to_string();
            debug!(target: "burrow::acp::stdin", "{line}");
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
            debug!(target: "burrow::acp::stderr", "{line}");
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

    // Owns the child: reads stdout until it closes, or kills the child once
    // nobody reads any more.
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) if line.trim().is_empty() => {}
                    Ok(Some(line)) => {
                        debug!(target: "burrow::acp::stdout", "{line}");
                        let output = match serde_json::from_str(line.trim()) {
                            Ok(value) => {
                                record(&trace, "out", &value);
                                Output::Message(value)
                            }
                            Err(_) => {
                                warn!(line, "ACP agent emitted non-JSON stdout; ignoring line");
                                Output::Garbage(line)
                            }
                        };
                        if output_tx.send(output).is_err() {
                            return; // the child is killed on drop
                        }
                    }
                    Ok(None) | Err(_) => break,
                },
                _ = output_tx.closed() => return,
            }
        }
        let status = tokio::time::timeout(EXIT_WAIT, child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        let stderr = stderr_rx.await.unwrap_or_default();
        let _ = output_tx.send(Output::Exited {
            code: status.and_then(|s| s.code()),
            signal: status.and_then(signal_name),
            stderr,
        });
    });

    Ok(Transport { input, output })
}

#[cfg(unix)]
fn signal_name(status: std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| {
        match signal {
            1 => "SIGHUP",
            2 => "SIGINT",
            6 => "SIGABRT",
            9 => "SIGKILL",
            11 => "SIGSEGV",
            13 => "SIGPIPE",
            15 => "SIGTERM",
            _ => return format!("SIG{signal}"),
        }
        .to_string()
    })
}

#[cfg(not(unix))]
fn signal_name(_: std::process::ExitStatus) -> Option<String> {
    None
}
