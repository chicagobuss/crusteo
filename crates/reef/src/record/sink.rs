//! Where records go. Each sink runs on its own task and takes one batch at
//! a time, so a slow or broken one never holds up the others, the phones
//! or the daemons; Reef sends the next batch when this one is answered.
//!
//! An `exec` sink is a program Reef starts and keeps running, fed over its
//! stdin. In `acks` mode it speaks `reef-sink/1` (docs/reef/record.md): a
//! hello line, then one line per batch, each answered with an ack or a
//! retry. In `plain` mode it gets bare records, one JSON line each.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use super::config::{Mode, SinkKind};

pub const PROTOCOL: &str = "reef-sink/1";

/// How long a program has to answer a batch.
const ANSWER_WITHIN: Duration = Duration::from_secs(120);
/// How long a program has to finish once its stdin closes.
const EXIT_WITHIN: Duration = Duration::from_secs(10);

pub struct Batch {
    pub id: u64,
    pub records: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done,
    /// Not taken; send it again after this long.
    Retry(Duration, String),
}

/// Starts a sink's worker. It ends when the returned sender is dropped.
pub fn spawn(
    name: &str,
    kind: &SinkKind,
    data_dir: PathBuf,
    report: impl Fn(u64, Outcome) + Send + 'static,
) -> mpsc::Sender<Batch> {
    let (tx, rx) = mpsc::channel(1);
    let name = name.to_string();
    match kind.clone() {
        SinkKind::Jsonl { dir } => {
            tokio::spawn(jsonl(data_dir.join(dir), rx, report));
        }
        SinkKind::Exec {
            command,
            mode,
            env,
            settings,
        } => {
            let program = Program {
                name,
                command,
                mode,
                env,
                settings,
            };
            tokio::spawn(exec(program, rx, report));
        }
    }
    tx
}

async fn jsonl(dir: PathBuf, mut rx: mpsc::Receiver<Batch>, report: impl Fn(u64, Outcome)) {
    while let Some(batch) = rx.recv().await {
        let dir = dir.clone();
        let written = tokio::task::spawn_blocking(move || append(&dir, &batch.records)).await;
        report(
            batch.id,
            match written {
                Ok(Ok(())) => Outcome::Done,
                Ok(Err(err)) => Outcome::Retry(Duration::from_secs(10), err.to_string()),
                Err(err) => Outcome::Retry(Duration::from_secs(10), err.to_string()),
            },
        );
    }
}

/// Appends to today's file (UTC), one line per record, synced to disk.
fn append(dir: &std::path::Path, records: &[Value]) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let today = &fleet::clock::now_iso()[..10];
    let path = dir.join(format!("{today}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut lines = Vec::new();
    for record in records {
        serde_json::to_writer(&mut lines, record)?;
        lines.push(b'\n');
    }
    file.write_all(&lines)?;
    file.sync_data()
}

struct Program {
    name: String,
    command: Vec<String>,
    mode: Mode,
    env: std::collections::BTreeMap<String, String>,
    settings: Value,
}

struct Running {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

async fn exec(program: Program, mut rx: mpsc::Receiver<Batch>, report: impl Fn(u64, Outcome)) {
    let mut running: Option<Running> = None;
    let mut backoff = Duration::from_secs(1);
    while let Some(batch) = rx.recv().await {
        if running.is_none() {
            match start(&program).await {
                Ok(started) => running = Some(started),
                Err(err) => {
                    report(batch.id, Outcome::Retry(backoff, err));
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
            }
        }
        let process = running.as_mut().expect("started above");
        match send(process, &batch, program.mode).await {
            Ok(outcome) => {
                backoff = Duration::from_secs(1);
                report(batch.id, outcome);
            }
            // Gone or wedged: the next batch starts it again.
            Err(err) => {
                if let Some(mut gone) = running.take() {
                    let _ = gone.child.start_kill();
                }
                report(batch.id, Outcome::Retry(backoff, err));
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
    // The sink was removed or changed: let the program finish what it has.
    if let Some(Running {
        mut child, stdin, ..
    }) = running
    {
        drop(stdin);
        if tokio::time::timeout(EXIT_WITHIN, child.wait())
            .await
            .is_err()
        {
            let _ = child.start_kill();
        }
    }
}

async fn start(program: &Program) -> Result<Running, String> {
    let (bin, args) = program.command.split_first().ok_or("an empty command")?;
    let mut child = Command::new(bin)
        .args(args)
        .envs(&program.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("{bin}: {err}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?).lines();
    // What it says on stderr goes to Reef's log, under its name.
    if let Some(stderr) = child.stderr.take() {
        let name = program.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("reef: sink {name}: {line}");
            }
        });
    }
    if program.mode == Mode::Acks {
        let hello = json!({ "type": "hello", "protocol": PROTOCOL, "sink": program.name,
            "reef": env!("CARGO_PKG_VERSION"), "settings": program.settings });
        write_line(&mut stdin, &hello).await?;
    }
    Ok(Running {
        child,
        stdin,
        stdout,
    })
}

async fn send(process: &mut Running, batch: &Batch, mode: Mode) -> Result<Outcome, String> {
    if mode == Mode::Plain {
        let mut lines = Vec::new();
        for record in &batch.records {
            serde_json::to_writer(&mut lines, record).map_err(|err| err.to_string())?;
            lines.push(b'\n');
        }
        process
            .stdin
            .write_all(&lines)
            .await
            .map_err(|err| err.to_string())?;
        process.stdin.flush().await.map_err(|err| err.to_string())?;
        return Ok(Outcome::Done);
    }
    let line = json!({ "type": "batch", "id": batch.id, "records": batch.records });
    write_line(&mut process.stdin, &line).await?;
    tokio::time::timeout(ANSWER_WITHIN, answer(&mut process.stdout, batch.id))
        .await
        .map_err(|_| format!("no answer to batch {} within {ANSWER_WITHIN:?}", batch.id))?
}

/// Reads lines until the answer to batch `id`; other lines are ignored.
async fn answer(stdout: &mut Lines<BufReader<ChildStdout>>, id: u64) -> Result<Outcome, String> {
    loop {
        let line = (stdout.next_line().await).map_err(|err| err.to_string())?;
        let Some(line) = line else {
            return Err("the program exited".into());
        };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message["id"].as_u64() != Some(id) {
            continue;
        }
        match message["type"].as_str() {
            Some("ack") => return Ok(Outcome::Done),
            Some("retry") => {
                let after = message["after_ms"]
                    .as_u64()
                    .unwrap_or(5000)
                    .clamp(100, 600_000);
                let error = message["error"]
                    .as_str()
                    .unwrap_or("asked to retry")
                    .to_string();
                return Ok(Outcome::Retry(Duration::from_millis(after), error));
            }
            _ => continue,
        }
    }
}

async fn write_line(stdin: &mut ChildStdin, message: &Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(message).map_err(|err| err.to_string())?;
    line.push(b'\n');
    stdin
        .write_all(&line)
        .await
        .map_err(|err| err.to_string())?;
    stdin.flush().await.map_err(|err| err.to_string())
}
