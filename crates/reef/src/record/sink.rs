//! Where records go. Each sink runs on its own task and takes one batch at
//! a time, so a slow or broken one never holds up the others, the phones
//! or the daemons; Reef sends the next batch when this one is answered.
//!
//! An `exec` sink is a program Reef starts and keeps running, fed over its
//! stdin. In `acks` mode it speaks `reef-sink/1` (docs/reef/record.md): a
//! hello line, then one line per batch, each answered with an ack or a
//! retry. In `plain` mode it gets bare records, one JSON line each.
//!
//! An `http` sink is a service Reef POSTs each batch to, the same
//! protocol with the answer in the status: any `2xx` is the ack, a `429` or
//! `503` with `Retry-After` is a retry, and anything else is logged and tried
//! again after a growing pause. Nothing is dropped.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
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
/// The longest pause an `http` sink waits between tries of a batch it could
/// not deliver, when the service did not say how long.
const HTTP_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// What a service may ask for with `Retry-After`, as a program may with
/// `after_ms`: 100 ms to 10 minutes.
const RETRY_AFTER_MIN: Duration = Duration::from_millis(100);
const RETRY_AFTER_MAX: Duration = Duration::from_secs(600);

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
        SinkKind::Http { url, token_env } => {
            // Read once: the environment of a running Reef does not change.
            let token = bearer(&token_env, |variable| std::env::var(variable).ok());
            let endpoint = Endpoint { name, url, token };
            tokio::spawn(http(endpoint, rx, report));
        }
    }
    tx
}

/// The bearer token an `http` sink sends: none for a sink with no
/// `token_env`, else the variable's value, or why there is none.
fn bearer(
    token_env: &Option<String>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, String> {
    let Some(variable) = token_env else {
        return Ok(None);
    };
    match lookup(variable) {
        Some(token) if !token.trim().is_empty() => Ok(Some(token.trim().to_string())),
        _ => Err(format!("the environment variable {variable} is not set")),
    }
}

/// Where an `http` sink posts, and the token it posts with.
struct Endpoint {
    name: String,
    url: String,
    /// The bearer token, none for a sink with no `token_env`, or why there
    /// is none when it was asked for.
    token: Result<Option<String>, String>,
}

/// What a service said to a batch.
#[derive(Debug, PartialEq)]
enum Answer {
    Taken,
    /// Not taken: try again after this long.
    Later(Duration, String),
}

async fn http(endpoint: Endpoint, mut rx: mpsc::Receiver<Batch>, report: impl Fn(u64, Outcome)) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(ANSWER_WITHIN))
        // The answer is the status. A redirect is not followed, so the token
        // never goes to a place the settings did not name.
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent(format!("reef/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let endpoint = Arc::new(endpoint);
    let mut backoff = Duration::from_secs(1);
    while let Some(batch) = rx.recv().await {
        let id = batch.id;
        let sent = match &endpoint.token {
            Err(why) => Err(why.clone()),
            Ok(_) => {
                let (agent, endpoint) = (agent.clone(), endpoint.clone());
                tokio::task::spawn_blocking(move || post(&agent, &endpoint, &batch))
                    .await
                    .unwrap_or_else(|err| Err(err.to_string()))
            }
        };
        let outcome = match sent {
            Ok(Answer::Taken) => {
                backoff = Duration::from_secs(1);
                Outcome::Done
            }
            Ok(Answer::Later(after, why)) => {
                backoff = Duration::from_secs(1);
                Outcome::Retry(after, why)
            }
            Err(why) => {
                let after = backoff;
                backoff = (backoff * 2).min(HTTP_BACKOFF_MAX);
                Outcome::Retry(after, why)
            }
        };
        report(id, outcome);
    }
}

/// Posts one batch. An `Err` is a batch the service did not answer in
/// the protocol's terms (nothing listening, a status that is not an answer):
/// tried again after a growing pause.
fn post(agent: &ureq::Agent, endpoint: &Endpoint, batch: &Batch) -> Result<Answer, String> {
    let body = json!({ "protocol": PROTOCOL, "sink": endpoint.name, "id": batch.id,
        "records": batch.records })
    .to_string();
    let mut request = (agent.post(&endpoint.url))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json");
    if let Ok(Some(token)) = &endpoint.token {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let mut response = request.send(body).map_err(|err| err.to_string())?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(Answer::Taken);
    }
    let after = (response.headers().get("retry-after"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds).clamp(RETRY_AFTER_MIN, RETRY_AFTER_MAX));
    if matches!(status, 429 | 503)
        && let Some(after) = after
    {
        return Ok(Answer::Later(
            after,
            format!("the service answered {status}"),
        ));
    }
    // What it said, a line of it, to tell a misconfiguration from an outage.
    let mut said = Vec::new();
    {
        use std::io::Read;
        let _ = (response.body_mut().as_reader())
            .take(200)
            .read_to_end(&mut said);
    }
    let said = String::from_utf8_lossy(&said);
    let said = said.split_whitespace().collect::<Vec<_>>().join(" ");
    Err(if said.is_empty() {
        format!("the service answered {status}")
    } else {
        format!("the service answered {status}: {said}")
    })
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    use super::*;

    /// What a test server was sent: the request line and headers (names in
    /// lower case), and the JSON body.
    type Seen = Arc<Mutex<Vec<(Vec<String>, Value)>>>;

    /// What a test server answers: a status, its extra headers, and a body.
    type Script = Vec<(u16, Vec<(&'static str, &'static str)>, &'static str)>;

    /// A local HTTP server on its own thread that answers each request with
    /// the next of `script` (the last one again once it runs out).
    fn serve(script: Script) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/feed", listener.local_addr().unwrap());
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (turn, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                // The head, and where the body starts.
                let (head, body_at) = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                        let length = (head.lines())
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break (head, end + 4);
                        }
                    }
                    if n == 0 {
                        break (String::new(), request.len());
                    }
                };
                let body = serde_json::from_slice(&request[body_at..]).unwrap_or(Value::Null);
                log.lock()
                    .unwrap()
                    .push((head.lines().map(str::to_string).collect(), body));
                let (status, headers, text) = script[turn.min(script.len() - 1)].clone();
                let mut answer = format!("HTTP/1.1 {status} Answer\r\n");
                for (name, value) in headers {
                    answer.push_str(&format!("{name}: {value}\r\n"));
                }
                answer.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                ));
                let _ = stream.write_all(answer.as_bytes());
            }
        });
        (url, seen)
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into()
    }

    fn endpoint(url: &str, token: Option<&str>) -> Endpoint {
        Endpoint {
            name: "search".into(),
            url: url.into(),
            token: Ok(token.map(str::to_string)),
        }
    }

    fn batch(id: u64) -> Batch {
        Batch {
            id,
            records: vec![json!({ "id": "box/a/e/1", "kind": "prompt" })],
        }
    }

    #[test]
    fn a_batch_is_posted_as_reef_sink_1_and_a_2xx_is_the_ack() {
        let (url, seen) = serve(vec![(200, vec![], "{}"), (204, vec![], "")]);
        let sent = post(&agent(), &endpoint(&url, Some("s3cret")), &batch(7));
        assert_eq!(sent, Ok(Answer::Taken));
        assert_eq!(
            post(&agent(), &endpoint(&url, None), &batch(8)),
            Ok(Answer::Taken)
        );
        let seen = seen.lock().unwrap();
        let (head, body) = &seen[0];
        assert_eq!(head[0], "post /feed http/1.1");
        assert!(
            head.contains(&"authorization: bearer s3cret".to_string()),
            "{head:?}"
        );
        assert!(
            head.contains(&"content-type: application/json".to_string()),
            "{head:?}"
        );
        assert_eq!(
            *body,
            json!({ "protocol": "reef-sink/1", "sink": "search", "id": 7,
                    "records": [{ "id": "box/a/e/1", "kind": "prompt" }] })
        );
        // No token_env: no Authorization header at all.
        assert!(
            !seen[1]
                .0
                .iter()
                .any(|line| line.starts_with("authorization"))
        );
        assert_eq!(seen[1].1["id"], 8);
    }

    #[test]
    fn a_429_or_503_with_retry_after_is_a_retry_that_long() {
        let after = |seconds: &'static str| vec![("Retry-After", seconds)];
        let (url, seen) = serve(vec![
            (429, after("7"), ""),
            (503, after("0"), ""),
            (503, after("99999"), "down for the night"),
            (429, after(" 3 "), ""),
        ]);
        let wants = |answer: Result<Answer, String>, seconds: f64| match answer {
            Ok(Answer::Later(after, why)) => {
                assert_eq!(after.as_secs_f64(), seconds, "{why}");
                assert!(why.contains("429") || why.contains("503"), "{why}");
            }
            other => panic!("{other:?}"),
        };
        let go = || post(&agent(), &endpoint(&url, None), &batch(1));
        wants(go(), 7.0);
        // What a service may ask for is 100 ms to 10 minutes.
        wants(go(), 0.1);
        wants(go(), 600.0);
        wants(go(), 3.0);
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn anything_else_is_an_error_to_log_and_try_again_with_a_growing_pause() {
        let (url, seen) = serve(vec![
            // No Retry-After, or one that is a date: nothing to go by.
            (429, vec![], ""),
            (
                503,
                vec![("Retry-After", "Wed, 21 Oct 2026 07:28:00 GMT")],
                "",
            ),
            (500, vec![], "boom"),
            (401, vec![], "unknown token\n  for this service"),
            (413, vec![], ""),
            // A redirect is not followed.
            (302, vec![("Location", "http://127.0.0.1:9/elsewhere")], ""),
        ]);
        let go = || post(&agent(), &endpoint(&url, Some("t")), &batch(1)).unwrap_err();
        assert_eq!(go(), "the service answered 429");
        assert_eq!(go(), "the service answered 503");
        assert_eq!(go(), "the service answered 500: boom");
        assert_eq!(
            go(),
            "the service answered 401: unknown token for this service"
        );
        assert_eq!(go(), "the service answered 413");
        assert_eq!(go(), "the service answered 302");
        assert_eq!(seen.lock().unwrap().len(), 6, "a redirect is not followed");
        // Nothing listening is an error too.
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let gone = format!("http://{}/feed", closed.local_addr().unwrap());
        drop(closed);
        assert!(post(&agent(), &endpoint(&gone, None), &batch(1)).is_err());
    }

    #[test]
    fn the_token_comes_from_the_variable_or_the_sink_waits_for_it() {
        let env = |name: &str| (name == "FEED_TOKEN").then(|| "  abc123\n".to_string());
        assert_eq!(bearer(&None, env), Ok(None));
        assert_eq!(
            bearer(&Some("FEED_TOKEN".into()), env),
            Ok(Some("abc123".into()))
        );
        let missing = bearer(&Some("OTHER_TOKEN".into()), env).unwrap_err();
        assert!(missing.contains("OTHER_TOKEN"), "{missing}");
        let blank = bearer(&Some("BLANK".into()), |_| Some("  ".into())).unwrap_err();
        assert!(blank.contains("BLANK"), "{blank}");
    }

    /// Runs the worker over `script`, a batch at a time, and gives what it
    /// reported for each.
    async fn worker(kind: SinkKind, batches: u64) -> Vec<(u64, Outcome)> {
        let (reported, mut outcomes) = mpsc::unbounded_channel();
        let tx = spawn("search", &kind, PathBuf::new(), move |id, outcome| {
            let _ = reported.send((id, outcome));
        });
        let mut got = Vec::new();
        for id in 1..=batches {
            tx.send(batch(id)).await.unwrap();
            got.push(outcomes.recv().await.unwrap());
        }
        got
    }

    #[tokio::test]
    async fn the_pause_grows_with_each_failure_and_starts_over_after_an_ack() {
        let (url, seen) = serve(vec![
            (500, vec![], ""),
            (500, vec![], ""),
            (500, vec![], ""),
            (200, vec![], ""),
            (503, vec![], ""),
            (429, vec![("Retry-After", "2")], ""),
            (500, vec![], ""),
        ]);
        let kind = SinkKind::Http {
            url,
            token_env: None,
        };
        let got = worker(kind, 7).await;
        let pauses: Vec<Option<u64>> = got
            .iter()
            .map(|(_, outcome)| match outcome {
                Outcome::Retry(after, _) => Some(after.as_secs()),
                Outcome::Done => None,
            })
            .collect();
        // 1 s, 2 s, 4 s; an ack; then from 1 s again, and again after a
        // service's own Retry-After (its 2 s, not ours).
        assert_eq!(
            pauses,
            [Some(1), Some(2), Some(4), None, Some(1), Some(2), Some(1)]
        );
        assert_eq!(
            got.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(seen.lock().unwrap().len(), 7);
    }

    #[tokio::test]
    async fn a_missing_token_sends_nothing_and_says_why() {
        let (url, seen) = serve(vec![(200, vec![], "")]);
        let kind = SinkKind::Http {
            url,
            token_env: Some("REEF_TEST_A_VARIABLE_NOBODY_SETS".into()),
        };
        let got = worker(kind, 2).await;
        for (_, outcome) in &got {
            match outcome {
                Outcome::Retry(_, why) => {
                    assert!(why.contains("REEF_TEST_A_VARIABLE_NOBODY_SETS"), "{why}");
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(seen.lock().unwrap().is_empty(), "records stay with Reef");
    }
}
