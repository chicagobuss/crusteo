//! Terminals the agent asks the client to run (`terminal/create` and the
//! calls after it), as Paseo's ACP session runs them: a sibling process
//! whose stdout and stderr are kept, up to the agent's byte limit.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, oneshot};

use crate::tools::TerminalView;

/// How long an exited command's output may take to drain.
const READER_GRACE: Duration = Duration::from_secs(1);

/// What a terminal's tasks report to the session.
#[derive(Debug)]
pub enum Event {
    Output {
        id: String,
        text: String,
    },
    Exited {
        id: String,
        code: Option<i32>,
        signal: Option<String>,
    },
}

/// The answer to `terminal/wait_for_exit`.
#[derive(Debug, Clone, PartialEq)]
pub enum Wait {
    Exited(Value),
    /// Answered when it exits.
    Later,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<String>,
}

impl Exit {
    pub fn to_json(&self) -> Value {
        json!({"exitCode": self.code, "signal": self.signal})
    }
}

struct Entry {
    output: String,
    truncated: bool,
    limit: Option<usize>,
    exit: Option<Exit>,
    kill: Option<oneshot::Sender<()>>,
    /// `terminal/wait_for_exit` requests to answer when it exits.
    waiters: Vec<Value>,
}

#[derive(Default)]
pub struct Terminals {
    entries: HashMap<String, Entry>,
}

/// `command` and `args` as Paseo runs them: a command line with spaces and
/// no arguments goes through a shell.
fn invocation(command: &str, args: &[String]) -> (String, Vec<String>, bool) {
    if !args.is_empty() || !command.trim().contains(char::is_whitespace) {
        return (command.to_string(), args.to_vec(), false);
    }
    if cfg!(windows) {
        ("cmd.exe".into(), vec!["/c".into(), command.into()], true)
    } else {
        ("bash".into(), vec!["-c".into(), command.into()], true)
    }
}

impl Terminals {
    /// Whether any terminal has not exited yet.
    pub fn running(&self) -> bool {
        self.entries.values().any(|entry| entry.exit.is_none())
    }

    /// Starts the terminal `params` asks for. Returns its id.
    pub fn create(
        &mut self,
        params: &Value,
        default_cwd: &str,
        env: &[(String, String)],
        events: mpsc::UnboundedSender<Event>,
    ) -> std::io::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let args: Vec<String> = params["args"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let (program, args, shell) =
            invocation(params["command"].as_str().unwrap_or_default(), &args);
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(params["cwd"].as_str().unwrap_or(default_cwd))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        for pair in params["env"].as_array().into_iter().flatten() {
            if let (Some(name), Some(value)) = (pair["name"].as_str(), pair["value"].as_str()) {
                command.env(name, value);
            }
        }
        if shell {
            // Shell startup files should not rewrite the environment.
            command.env_remove("BASH_ENV");
        }
        let mut child = command.spawn()?;
        let mut readers = Vec::new();
        for stream in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let events = events.clone();
            let id = id.clone();
            readers.push(tokio::spawn(async move {
                let mut stream = stream;
                let mut buffer = vec![0u8; 8192];
                while let Ok(n) = stream.read(&mut buffer).await {
                    if n == 0 {
                        break;
                    }
                    let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                    if events
                        .send(Event::Output {
                            id: id.clone(),
                            text,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }
        let (kill, killed) = oneshot::channel::<()>();
        let exit_id = id.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                _ = killed => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            // Report the exit after the last output, unless something the
            // command left behind still holds its pipes open.
            let _ = tokio::time::timeout(READER_GRACE, async {
                for reader in readers {
                    let _ = reader.await;
                }
            })
            .await;
            let status = status.ok();
            let _ = events.send(Event::Exited {
                id: exit_id,
                code: status.and_then(|s| s.code()),
                signal: status.and_then(signal_name),
            });
        });
        self.entries.insert(
            id.clone(),
            Entry {
                output: String::new(),
                truncated: false,
                limit: params["outputByteLimit"].as_u64().map(|n| n as usize),
                exit: None,
                kill: Some(kill),
                waiters: Vec::new(),
            },
        );
        Ok(id)
    }

    /// Applies a terminal event. Returns the `wait_for_exit` requests it
    /// answers, with the answer.
    pub fn on_event(&mut self, event: Event) -> Vec<(Value, Value)> {
        match event {
            Event::Output { id, text } => {
                if let Some(entry) = self.entries.get_mut(&id) {
                    entry.output.push_str(&text);
                    if let Some(limit) = entry.limit.filter(|l| *l > 0) {
                        while entry.output.len() > limit {
                            // Drop from the front, one character at a time.
                            let first = entry.output.chars().next().map_or(1, char::len_utf8);
                            entry.output.drain(..first);
                            entry.truncated = true;
                        }
                    }
                }
                Vec::new()
            }
            Event::Exited { id, code, signal } => {
                let Some(entry) = self.entries.get_mut(&id) else {
                    return Vec::new();
                };
                let exit = Exit { code, signal };
                entry.kill = None;
                entry.exit = Some(exit.clone());
                entry
                    .waiters
                    .drain(..)
                    .map(|waiter| (waiter, exit.to_json()))
                    .collect()
            }
        }
    }

    /// `terminal/output`.
    pub fn output(&self, id: &str) -> Option<Value> {
        let entry = self.entries.get(id)?;
        let mut answer = json!({"output": entry.output, "truncated": entry.truncated});
        if let Some(exit) = &entry.exit {
            answer["exitStatus"] = exit.to_json();
        }
        Some(answer)
    }

    /// `terminal/wait_for_exit`: the answer now if it has exited, else the
    /// request is answered when it does.
    pub fn wait(&mut self, id: &str, request: Value) -> Wait {
        let Some(entry) = self.entries.get_mut(id) else {
            return Wait::Unknown;
        };
        match &entry.exit {
            Some(exit) => Wait::Exited(exit.to_json()),
            None => {
                entry.waiters.push(request);
                Wait::Later
            }
        }
    }

    /// `terminal/kill`: stops it, keeping its output. False if unknown.
    pub fn kill(&mut self, id: &str) -> bool {
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        if let Some(kill) = entry.kill.take() {
            let _ = kill.send(());
        }
        true
    }

    /// `terminal/release`: stops it and forgets it. False if unknown.
    pub fn release(&mut self, id: &str) -> bool {
        let known = self.kill(id);
        self.entries.remove(id);
        known
    }

    /// What a tool call's `terminal` content shows.
    pub fn view(&self, id: &str) -> Option<TerminalView> {
        let entry = self.entries.get(id)?;
        Some(TerminalView {
            output: entry.output.clone(),
            exit_code: entry.exit.as_ref().and_then(|e| e.code).map(i64::from),
        })
    }

    /// Stops every terminal (the session is closing).
    pub fn kill_all(&mut self) {
        for entry in self.entries.values_mut() {
            if let Some(kill) = entry.kill.take() {
                let _ = kill.send(());
            }
        }
        self.entries.clear();
    }
}

#[cfg(unix)]
fn signal_name(status: std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| match signal {
        9 => "SIGKILL".to_string(),
        15 => "SIGTERM".to_string(),
        2 => "SIGINT".to_string(),
        other => format!("SIG{other}"),
    })
}

#[cfg(not(unix))]
fn signal_name(_: std::process::ExitStatus) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_go_through_a_shell() {
        assert_eq!(invocation("ls", &[]), ("ls".into(), vec![], false));
        assert_eq!(
            invocation("git", &["status".into()]),
            ("git".into(), vec!["status".into()], false)
        );
        let (_, args, shell) = invocation("echo hi > x", &[]);
        assert!(shell);
        assert_eq!(args.last().unwrap(), "echo hi > x");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_terminal_runs_and_reports_its_exit() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut terminals = Terminals::default();
        let id = terminals
            .create(
                &json!({"command": "printf 'hello world'; exit 3", "outputByteLimit": 5}),
                "/",
                &[],
                tx,
            )
            .unwrap();
        assert!(terminals.running(), "running until it exits");
        assert_eq!(terminals.wait(&id, json!(7)), Wait::Later);
        let answered = loop {
            let event = rx.recv().await.unwrap();
            let answered = terminals.on_event(event);
            if !answered.is_empty() {
                break answered;
            }
        };
        assert_eq!(answered[0].0, json!(7));
        assert_eq!(answered[0].1["exitCode"], 3);
        assert!(!terminals.running());
        // The exit is reported after the output, cut to the byte limit.
        let output = terminals.output(&id).unwrap();
        assert_eq!(output["exitStatus"]["exitCode"], 3);
        assert_eq!(output["output"], "world");
        assert_eq!(output["truncated"], true);
        assert!(terminals.release(&id));
        assert!(terminals.output(&id).is_none());
    }
}
