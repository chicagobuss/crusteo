//! A child CLI that speaks newline-delimited JSON on stdin and stdout, in a
//! process group of its own.
//!
//! Every provider's CLI (`claude` in stream-json, `codex app-server`,
//! `pi --mode rpc`, an ACP agent, `agy`) uses the same transport: one JSON
//! value per line in and out, a process group that ends with the session, and
//! a tail of stderr kept for the error that ends it. This module is that
//! transport; a provider adapter only fills in [`Config`].
//!
//! Records split on `\n` only. Codex and pi write U+2028 and U+2029 raw
//! inside JSON strings, so a reader that also breaks on those (Node's
//! `readline` does) cuts a message apart.
//!
//! A session that stops the CLI itself ([`spawn_stoppable`]) closes its stdin
//! to ask it to exit ([`Stop::Close`]) or sends `SIGINT` ([`Stop::Interrupt`]),
//! as Paseo stops an agent; either escalates to `SIGTERM` after a second and
//! `SIGKILL` two seconds after that. Otherwise letting go (dropping the input
//! sender or the output receiver) ends the CLI's process group and tree at
//! once, before its stdin closes ([`crate::group`]), so nothing it started
//! outlives the session.

use std::io::Write;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::group::{self, Grouped};

/// How much of a CLI's stderr to keep, for error messages.
pub const STDERR_TAIL: usize = 4096;
/// How long a CLI has to exit after `SIGINT` (or a closed stdin) before
/// `SIGTERM`.
const TERMINATE_AFTER: Duration = Duration::from_secs(1);
/// How long to wait for the pipes to drain once the CLI has exited.
const DRAIN: Duration = Duration::from_secs(2);

/// What the process said, in order. `Exited` comes last.
#[derive(Debug)]
pub enum Output {
    Message(Value),
    /// A line that was not JSON (logged and skipped).
    Garbage(String),
    /// The process exited: its exit code (`None` when a signal ended it), the
    /// name of that signal when there was one, and the tail of its stderr.
    Exited {
        code: Option<i32>,
        signal: Option<String>,
        stderr: String,
    },
}

/// How the session stops the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// `SIGINT`: the CLI ends its turn as `interrupted` and exits.
    Interrupt,
    /// Closes stdin: an idle CLI exits.
    Close,
}

/// What a provider's CLI needs of this transport.
#[derive(Debug, Clone, Copy)]
pub struct Config<'a> {
    /// The provider whose frames these are (`claude`), for logs.
    pub provider: &'static str,
    /// Where every frame is also recorded, as `{"dir":"in"|"out","frame":...}`.
    pub trace: Option<&'a Path>,
    /// How much of the CLI's stderr to keep, for error messages.
    pub stderr_tail: usize,
    /// How long the CLI may take to exit once its stdout ends.
    pub exit_wait: Duration,
    /// Whether the session stops the CLI itself, with [`Stop`], as
    /// antigravity's `agy` does.
    pub stops: bool,
}

impl Default for Config<'_> {
    fn default() -> Self {
        Self {
            provider: "agent",
            trace: None,
            stderr_tail: STDERR_TAIL,
            exit_wait: Duration::from_secs(2),
            stops: false,
        }
    }
}

/// The two ends of a running CLI.
#[derive(Debug)]
pub struct Process {
    pub input: mpsc::UnboundedSender<Value>,
    pub output: mpsc::UnboundedReceiver<Output>,
}

/// The ends of a CLI the session also stops itself ([`Stop`]).
#[derive(Debug)]
pub struct Stoppable {
    pub input: mpsc::UnboundedSender<Value>,
    pub stop: mpsc::UnboundedSender<Stop>,
    pub output: mpsc::UnboundedReceiver<Output>,
}

/// Starts `command` with piped stdio, in a process group of its own that ends
/// with it ([`crate::group`]).
pub fn spawn(mut command: Command, config: Config<'_>) -> std::io::Result<Process> {
    let started = start(&mut command, config, false)?;
    Ok(Process {
        input: started.input,
        output: started.output,
    })
}

/// Starts `command` as [`spawn`] does, with a [`Stop`] channel: the session
/// interrupts it as Paseo does, rather than only letting it go.
pub fn spawn_stoppable(mut command: Command, config: Config<'_>) -> std::io::Result<Stoppable> {
    let started = start(&mut command, config, true)?;
    Ok(Stoppable {
        input: started.input,
        stop: started.stop.expect("stop channel"),
        output: started.output,
    })
}

/// The JSONL trace of a process's frames, when its provider was asked for one.
#[derive(Clone, Default)]
pub struct Trace(Option<Arc<Mutex<std::fs::File>>>);

impl Trace {
    /// Records to `path`, truncating it; nothing when there is no path.
    pub fn create(path: Option<&Path>) -> Self {
        Self(
            path.and_then(|path| std::fs::File::create(path).ok())
                .map(|file| Arc::new(Mutex::new(file))),
        )
    }

    /// One frame, as `{"dir":"in"|"out","frame":...}`.
    pub fn record(&self, dir: &str, frame: &Value) {
        if let Some(file) = &self.0
            && let Ok(mut file) = file.lock()
        {
            let _ = writeln!(file, "{}", json!({"dir": dir, "frame": frame}));
        }
    }
}

struct Started {
    input: mpsc::UnboundedSender<Value>,
    stop: Option<mpsc::UnboundedSender<Stop>>,
    output: mpsc::UnboundedReceiver<Output>,
}

fn start(command: &mut Command, config: Config<'_>, stops: bool) -> std::io::Result<Started> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = group::spawn(command)?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let trace = Trace::create(config.trace);
    let provider = config.provider;
    let (input, input_rx) = mpsc::unbounded_channel();
    let (stop, stop_rx) = mpsc::unbounded_channel();
    let (output_tx, output) = mpsc::unbounded_channel();
    let (done_tx, done_rx) = oneshot::channel();

    let stderr_task = tokio::spawn(tail(stderr, config.stderr_tail, provider));

    let stdout_trace = trace.clone();
    let stdout_tx = output_tx.clone();
    let stdout_task = tokio::spawn(async move {
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
            debug!(target: "burrow::provider::stdout", provider, "{line}");
            let output = match serde_json::from_str(line) {
                Ok(frame) => {
                    stdout_trace.record("out", &frame);
                    Output::Message(frame)
                }
                Err(_) => {
                    warn!(
                        target: "burrow::provider::stdout",
                        provider,
                        line,
                        "non-JSON line from the agent process"
                    );
                    Output::Garbage(line.to_string())
                }
            };
            if stdout_tx.send(output).is_err() {
                break;
            }
        }
        let _ = done_tx.send(());
    });

    let exit_wait = config.exit_wait;
    let owned = output_tx.clone();
    tokio::spawn(async move {
        let status = own(
            &mut child,
            Ends {
                stdin,
                input: input_rx,
                stops: stops.then_some(stop_rx),
                done: done_rx,
            },
            &owned,
            &trace,
            exit_wait,
        )
        .await;
        // Messages first: wait for stdout to drain (ending the group closes
        // pipes a tool inherited).
        let _ = tokio::time::timeout(DRAIN, stdout_task).await;
        let stderr = tokio::time::timeout(DRAIN, stderr_task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let _ = output_tx.send(Output::Exited {
            code: status.and_then(|status| status.code()),
            signal: status.and_then(signal_name),
            stderr,
        });
    });

    Ok(Started {
        input,
        stop: stops.then_some(stop),
        output,
    })
}

/// Reads the CLI's stderr into the tail that ends the session's error.
async fn tail(stderr: ChildStderr, limit: usize, provider: &'static str) -> String {
    let mut reader = BufReader::new(stderr);
    let mut buffer = Vec::new();
    let mut tail = String::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim_end_matches(['\n', '\r']);
        debug!(target: "burrow::provider::stderr", provider, "{line}");
        tail.push_str(line);
        tail.push('\n');
        keep_tail(&mut tail, limit);
    }
    tail
}

/// Trims `tail` to its last `limit` bytes, on a UTF-8 char boundary.
fn keep_tail(tail: &mut String, limit: usize) {
    if tail.len() > limit {
        let cut = tail.len() - limit;
        let cut = (cut..tail.len())
            .find(|i| tail.is_char_boundary(*i))
            .unwrap_or(tail.len());
        tail.drain(..cut);
    }
}

/// The ends of the session the owner task drives.
struct Ends {
    stdin: ChildStdin,
    input: mpsc::UnboundedReceiver<Value>,
    stops: Option<mpsc::UnboundedReceiver<Stop>>,
    /// Resolves once the CLI's stdout has closed.
    done: oneshot::Receiver<()>,
}

/// Writes the session's frames, carries out its stops, and waits for the
/// CLI to exit. Returns how it exited.
async fn own(
    child: &mut Grouped,
    ends: Ends,
    output_tx: &mpsc::UnboundedSender<Output>,
    trace: &Trace,
    exit_wait: Duration,
) -> Option<ExitStatus> {
    let Ends {
        stdin,
        mut input,
        mut stops,
        mut done,
    } = ends;
    let mut stdin = Some(stdin);
    let mut input_open = true;
    // Whether the session stops the CLI itself ([`Stop`]).
    let stoppable = stops.is_some();
    let mut stops_open = stoppable;
    // When the escalation started, once a stop was asked for.
    let mut stopping: Option<Instant> = None;
    let mut terminated = false;
    let status = loop {
        let escalate = stopping.map(|at| {
            if terminated {
                at + group::KILL_AFTER
            } else {
                at + TERMINATE_AFTER
            }
        });
        let mut stop = None;
        tokio::select! {
            exited = child.wait() => break exited.ok(),
            _ = &mut done => {
                // Nobody listens any more: stop the CLI with what it started.
                if output_tx.is_closed() {
                    break None;
                }
                // Otherwise the CLI has said all it will say: give it
                // `exit_wait` to exit before it is killed.
                break match tokio::time::timeout(exit_wait, child.wait()).await {
                    Ok(Ok(status)) => Some(status),
                    _ => {
                        let _ = child.kill().await;
                        None
                    }
                };
            }
            frame = input.recv(), if input_open => match frame {
                Some(frame) => {
                    if let Some(pipe) = stdin.as_mut() {
                        trace.record("in", &frame);
                        let mut line = frame.to_string();
                        line.push('\n');
                        if pipe.write_all(line.as_bytes()).await.is_err()
                            || pipe.flush().await.is_err()
                        {
                            stdin = None;
                        }
                    }
                }
                // Every sender is gone: the session let go. One that stops the
                // CLI itself gets a close, as a stop does. Otherwise the tree
                // ends first and stdin closes after: a CLI that exits on EOF
                // would leave its children to init, out of the tree's reach.
                // The loop still waits for the exit, for its code.
                None => {
                    input_open = false;
                    if stoppable {
                        stop = Some(Stop::Close);
                    } else {
                        child.end();
                        stdin = None;
                    }
                }
            },
            asked = recv_stop(&mut stops), if stops_open => match asked {
                Some(asked) => stop = Some(asked),
                // The transport was dropped: close, as Paseo's close does.
                None => {
                    stops_open = false;
                    stop = Some(Stop::Close);
                }
            },
            _ = output_tx.closed() => break None,
            _ = sleep_until(escalate), if escalate.is_some() => {
                if terminated {
                    signal(child, "KILL").await;
                    stopping = None;
                } else {
                    signal(child, "TERM").await;
                    terminated = true;
                }
            }
        }
        match stop {
            Some(Stop::Interrupt) => signal(child, "INT").await,
            Some(Stop::Close) => stdin = None,
            None => continue,
        }
        if stopping.is_none() && !terminated {
            stopping = Some(Instant::now());
        }
    };
    // What the CLI left behind goes too.
    child.end();
    status
}

/// The next stop the session sent, or nothing when it stops with no channel
/// of its own.
async fn recv_stop(stops: &mut Option<mpsc::UnboundedReceiver<Stop>>) -> Option<Stop> {
    match stops {
        Some(stops) => stops.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Signals the CLI's process group, or kills it where there are no signals to
/// send.
async fn signal(child: &mut Grouped, name: &str) {
    #[cfg(unix)]
    {
        let Some(pid) = child.id() else { return };
        let _ = Command::new("kill")
            .args(["-s", name, "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        let _ = child.start_kill();
    }
}

/// The signal that ended `status`, as its name.
#[cfg(unix)]
fn signal_name(status: ExitStatus) -> Option<String> {
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

/// The signal that ended `status`, as its name.
#[cfg(not(unix))]
fn signal_name(_: ExitStatus) -> Option<String> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(10);

    fn config() -> Config<'static> {
        Config {
            provider: "test",
            ..Config::default()
        }
    }

    /// Everything the process said, until it exited.
    async fn collect(output: &mut mpsc::UnboundedReceiver<Output>) -> (Vec<Value>, Option<i32>) {
        let mut frames = Vec::new();
        loop {
            match timeout(WAIT, output.recv()).await.expect("no exit") {
                Some(Output::Message(frame)) => frames.push(frame),
                Some(Output::Exited { code, .. }) => return (frames, code),
                Some(Output::Garbage(_)) => {}
                None => panic!("the process said nothing more"),
            }
        }
    }

    #[tokio::test]
    async fn frames_round_trip_one_line_each() {
        let Process { input, mut output } = spawn(Command::new("cat"), config()).unwrap();
        let sent = [
            json!({"id": 1, "method": "initialize"}),
            json!({"id": 2, "result": {"ok": true}}),
        ];
        for frame in &sent {
            input.send(frame.clone()).unwrap();
        }
        let mut frames = Vec::new();
        while frames.len() < sent.len() {
            match timeout(WAIT, output.recv()).await.expect("no frame") {
                Some(Output::Message(frame)) => frames.push(frame),
                other => panic!("expected a frame, got {other:?}"),
            }
        }
        assert_eq!(frames, sent);
        // Letting go ends it (by SIGTERM, before its stdin closes), and the
        // exit still comes through.
        drop(input);
        let (rest, _) = collect(&mut output).await;
        assert!(rest.is_empty());
    }

    /// A JSON string may hold U+2028 and U+2029 raw; only LF ends a record.
    #[tokio::test]
    async fn line_separators_inside_a_string_do_not_split_a_frame() {
        // E2 80 A8 is U+2028, E2 80 A9 U+2029.
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"printf '{"text":"one\342\200\250two\342\200\251three"}\n'"#,
        ]);
        let Process {
            input: _input,
            mut output,
        } = spawn(command, config()).unwrap();
        let (frames, code) = collect(&mut output).await;
        assert_eq!(frames, vec![json!({"text": "one\u{2028}two\u{2029}three"})]);
        assert_eq!(code, Some(0));
    }

    #[tokio::test]
    async fn a_line_that_is_not_json_is_garbage() {
        let mut command = Command::new("sh");
        command.args(["-c", r#"echo not json; echo '{"ok":true}'"#]);
        let Process {
            input: _input,
            mut output,
        } = spawn(command, config()).unwrap();
        let mut said = Vec::new();
        loop {
            match timeout(WAIT, output.recv()).await.expect("no exit") {
                Some(Output::Message(frame)) => said.push(frame),
                Some(Output::Garbage(line)) => said.push(json!(line)),
                Some(Output::Exited { .. }) => break,
                None => panic!("the process said nothing more"),
            }
        }
        assert_eq!(said, vec![json!("not json"), json!({"ok": true})]);
    }

    #[tokio::test]
    async fn the_exit_carries_the_code_and_the_stderr_tail() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'boom\\n' >&2; exit 3"]);
        let Process {
            input: _input,
            mut output,
        } = spawn(command, config()).unwrap();
        loop {
            match timeout(WAIT, output.recv()).await.expect("no exit") {
                Some(Output::Exited { code, stderr, .. }) => {
                    assert_eq!(code, Some(3));
                    assert_eq!(stderr.trim(), "boom");
                    break;
                }
                Some(_) => {}
                None => panic!("the process said nothing more"),
            }
        }
    }

    #[test]
    fn a_tail_cuts_on_a_char_boundary() {
        let mut tail = String::new();
        keep_tail(&mut tail, 4);
        // Six two-byte characters: a cut into the middle of one moves on to
        // the next boundary.
        tail.push_str("αααααα");
        keep_tail(&mut tail, 5);
        assert_eq!(tail, "αα");
        keep_tail(&mut tail, 4096);
        assert_eq!(tail, "αα", "a short tail is left alone");
    }

    #[tokio::test]
    async fn a_stderr_tail_is_kept_whole_and_valid_utf8() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'αααααααα\\n' >&2"]);
        let process = spawn(
            command,
            Config {
                stderr_tail: 5,
                ..config()
            },
        )
        .unwrap();
        let Process {
            input: _input,
            mut output,
        } = process;
        loop {
            match timeout(WAIT, output.recv()).await.expect("no exit") {
                Some(Output::Exited { stderr, .. }) => {
                    assert!(stderr.len() <= 5, "{stderr:?}");
                    assert_eq!(stderr, "αα\n");
                    break;
                }
                Some(_) => {}
                None => panic!("the process said nothing more"),
            }
        }
    }

    #[tokio::test]
    async fn a_trace_records_both_directions() {
        let path =
            std::env::temp_dir().join(format!("burrow-process-trace-{}", std::process::id()));
        let process = spawn(
            Command::new("cat"),
            Config {
                trace: Some(&path),
                ..config()
            },
        )
        .unwrap();
        let Process { input, mut output } = process;
        input.send(json!({"hello": "world"})).unwrap();
        drop(input);
        collect(&mut output).await;
        let trace = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = trace
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            lines,
            vec![
                json!({"dir": "in", "frame": {"hello": "world"}}),
                json!({"dir": "out", "frame": {"hello": "world"}}),
            ]
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn a_close_ends_stdin_and_the_cli_exits() {
        let Stoppable {
            input,
            stop,
            mut output,
        } = spawn_stoppable(
            Command::new("cat"),
            Config {
                stops: true,
                ..config()
            },
        )
        .unwrap();
        input.send(json!({"event": "user"})).unwrap();
        timeout(WAIT, tokio::time::sleep(Duration::from_millis(100)))
            .await
            .unwrap();
        stop.send(Stop::Close).unwrap();
        let (frames, code) = collect(&mut output).await;
        assert_eq!(frames, vec![json!({"event": "user"})]);
        assert_eq!(code, Some(0));
    }

    /// A CLI that exits on stdin EOF must not leave a child it started
    /// outside its group behind: the tree ends before stdin closes.
    #[cfg(unix)]
    #[tokio::test]
    async fn letting_go_ends_the_tree_before_stdin_closes() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"perl -MPOSIX -e 'POSIX::setsid(); sleep 30' & echo "{\"child\":$!}"; read line; exit 0"#,
        ]);
        let Process { input, mut output } = spawn(command, config()).unwrap();
        let child = match timeout(WAIT, output.recv()).await.expect("no frame") {
            Some(Output::Message(frame)) => frame["child"].as_u64().expect("the child's pid"),
            other => panic!("expected the child's pid, got {other:?}"),
        };
        // Its own session: no longer in the CLI's process group.
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(input);
        collect(&mut output).await;
        let alive = || {
            std::process::Command::new("kill")
                .args(["-0", &child.to_string()])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let survived = alive();
        if survived {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &child.to_string()])
                .status();
        }
        assert!(!survived, "the CLI's child outlived it");
    }

    #[tokio::test]
    async fn an_interrupt_escalates_to_kill() {
        let mut command = Command::new("sh");
        command.args(["-c", "trap '' INT TERM; echo '{\"ready\":true}'; sleep 30"]);
        let Stoppable {
            input: _input,
            stop,
            mut output,
        } = spawn_stoppable(
            command,
            Config {
                stops: true,
                ..config()
            },
        )
        .unwrap();
        let started = Instant::now();
        assert!(matches!(
            timeout(WAIT, output.recv()).await.expect("no frame"),
            Some(Output::Message(frame)) if frame["ready"] == true
        ));
        stop.send(Stop::Interrupt).unwrap();
        let (_, code) = collect(&mut output).await;
        assert_eq!(code, None, "killed by a signal");
        let took = started.elapsed();
        assert!(
            took >= group::KILL_AFTER && took < Duration::from_secs(8),
            "{took:?}"
        );
    }
}
