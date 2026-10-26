//! The `agy` child process: newline-delimited JSON on stdin and stdout.
//!
//! It runs in its own process group, as Paseo starts it, so an interrupt
//! reaches the commands its tools run. Stopping follows Paseo: `SIGINT`
//! (an interrupt) or closing stdin (a close), then `SIGTERM` after a second
//! and `SIGKILL` after two, and `SIGKILL` to the group once the CLI has
//! exited, because a tool can outlive it.

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, warn};

/// How much of stderr to keep, for error messages (Paseo keeps 8 KiB).
const STDERR_TAIL: usize = 8192;
const TERMINATE_AFTER: Duration = Duration::from_secs(1);
const KILL_AFTER: Duration = Duration::from_secs(2);
/// How long to wait for the pipes to drain once the CLI has exited.
const DRAIN: Duration = Duration::from_secs(2);

/// What the process said, in order. `Exited` comes last.
#[derive(Debug)]
pub enum Output {
    Message(Value),
    /// A line that was not JSON.
    Garbage(String),
    /// The process exited: its exit code (`None` for a signal) and the tail
    /// of its stderr.
    Exited {
        status: Option<i32>,
        stderr: String,
    },
}

/// How to stop the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// `SIGINT`: the CLI ends its turn as `interrupted` and exits.
    Interrupt,
    /// Closes stdin: an idle CLI exits.
    Close,
}

/// The ends of a CLI process (or a test double). Dropping it closes the
/// process as [`Stop::Close`] does.
pub struct Transport {
    pub input: mpsc::UnboundedSender<Value>,
    pub stop: mpsc::UnboundedSender<Stop>,
    pub output: mpsc::UnboundedReceiver<Output>,
}

/// Starts `command` with piped stdio in its own process group. With
/// `trace`, every frame is also appended there as `{"dir","frame"}` JSONL.
pub fn spawn(mut command: Command, trace: Option<&Path>) -> std::io::Result<Transport> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let trace = trace
        .and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        })
        .map(|file| Arc::new(Mutex::new(file)));
    let (input, input_rx) = mpsc::unbounded_channel();
    let (stop, stop_rx) = mpsc::unbounded_channel();
    let (output_tx, output) = mpsc::unbounded_channel();

    let stderr_task = tokio::spawn(async move {
        let mut tail = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            debug!(target: "burrow::agy::stderr", "{line}");
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
        tail
    });

    let stdout_tx = output_tx.clone();
    let stdout_trace = trace.clone();
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            debug!(target: "burrow::agy::stdout", "{line}");
            let output = match serde_json::from_str::<Value>(&line) {
                Ok(frame) => {
                    record(&stdout_trace, "out", &frame);
                    Output::Message(frame)
                }
                Err(_) => {
                    warn!(line, "non-JSON line from agy");
                    Output::Garbage(line)
                }
            };
            if stdout_tx.send(output).is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let status = own(child, stdin, input_rx, stop_rx, trace).await;
        // Messages first: wait for stdout to drain (killing the group above
        // closes pipes a tool inherited).
        let _ = tokio::time::timeout(DRAIN, stdout_task).await;
        let stderr = tokio::time::timeout(DRAIN, stderr_task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let _ = output_tx.send(Output::Exited { status, stderr });
    });

    Ok(Transport {
        input,
        stop,
        output,
    })
}

type Trace = Option<Arc<Mutex<std::fs::File>>>;

fn record(trace: &Trace, dir: &str, frame: &Value) {
    if let Some(file) = trace
        && let Ok(mut file) = file.lock()
    {
        let _ = writeln!(file, "{}", json!({"dir": dir, "frame": frame}));
    }
}

/// Writes input, carries out stops, and waits for the exit. Returns the
/// exit code.
async fn own(
    mut child: Child,
    stdin: tokio::process::ChildStdin,
    mut input: mpsc::UnboundedReceiver<Value>,
    mut stops: mpsc::UnboundedReceiver<Stop>,
    trace: Trace,
) -> Option<i32> {
    let pid = child.id();
    let mut stdin = Some(stdin);
    let mut input_open = true;
    let mut stops_open = true;
    // When the escalation started, once a stop was asked for.
    let mut stopping: Option<Instant> = None;
    let mut terminated = false;
    let status = loop {
        let escalate = stopping.map(|at| {
            if terminated {
                at + KILL_AFTER
            } else {
                at + TERMINATE_AFTER
            }
        });
        let mut stop = None;
        tokio::select! {
            status = child.wait() => break status.ok().and_then(|s| s.code()),
            frame = input.recv(), if input_open => match frame {
                Some(frame) => {
                    if let Some(pipe) = stdin.as_mut() {
                        record(&trace, "in", &frame);
                        let mut line = frame.to_string();
                        line.push('\n');
                        if pipe.write_all(line.as_bytes()).await.is_err()
                            || pipe.flush().await.is_err()
                        {
                            stdin = None;
                        }
                    }
                }
                None => input_open = false,
            },
            asked = stops.recv(), if stops_open => match asked {
                Some(asked) => stop = Some(asked),
                // The transport was dropped: close, as Paseo's close does.
                None => {
                    stops_open = false;
                    stop = Some(Stop::Close);
                }
            },
            _ = sleep_until(escalate), if escalate.is_some() => {
                if terminated {
                    signal(pid, "KILL", &mut child).await;
                    stopping = None;
                } else {
                    signal(pid, "TERM", &mut child).await;
                    terminated = true;
                }
            }
        }
        match stop {
            Some(Stop::Interrupt) => signal(pid, "INT", &mut child).await,
            Some(Stop::Close) => stdin = None,
            None => continue,
        }
        if stopping.is_none() && !terminated {
            stopping = Some(Instant::now());
        }
    };
    signal(pid, "KILL", &mut child).await;
    status
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Signals the CLI's process group.
#[cfg(unix)]
async fn signal(pid: Option<u32>, name: &str, _child: &mut Child) {
    let Some(pid) = pid else { return };
    let _ = Command::new("kill")
        .args(["-s", name, "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// Windows has no signals to send; every stop kills the CLI.
#[cfg(not(unix))]
async fn signal(_pid: Option<u32>, _name: &str, child: &mut Child) {
    let _ = child.start_kill();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    async fn exited(transport: &mut Transport) -> (Vec<Value>, Option<i32>) {
        let mut frames = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), transport.output.recv()).await {
                Ok(Some(Output::Message(frame))) => frames.push(frame),
                Ok(Some(Output::Exited { status, .. })) => return (frames, status),
                Ok(Some(Output::Garbage(_))) => {}
                other => panic!("no exit: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_close_ends_stdin_and_the_cli_exits() {
        let mut transport = spawn(Command::new("cat"), None).unwrap();
        transport.input.send(json!({"event": "user"})).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        transport.stop.send(Stop::Close).unwrap();
        let (frames, status) = exited(&mut transport).await;
        assert_eq!(frames, vec![json!({"event": "user"})]);
        assert_eq!(status, Some(0));
    }

    #[tokio::test]
    async fn an_interrupt_escalates_to_kill() {
        let mut command = Command::new("sh");
        command.args(["-c", "trap '' INT TERM; echo '{\"ready\":true}'; sleep 30"]);
        let mut transport = spawn(command, None).unwrap();
        let started = Instant::now();
        assert!(matches!(
            transport.output.recv().await,
            Some(Output::Message(frame)) if frame["ready"] == true
        ));
        transport.stop.send(Stop::Interrupt).unwrap();
        let (_, status) = exited(&mut transport).await;
        assert_eq!(status, None, "killed by a signal");
        let took = started.elapsed();
        assert!(
            took >= KILL_AFTER && took < Duration::from_secs(8),
            "{took:?}"
        );
    }
}
