//! Recording end to end: a Reef on a fake daemon, following every agent,
//! with real sinks: the jsonl files, and an exec sink that is this test
//! binary started again ([`sink_plugin`]).

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use reef::fake::{self, FakeDaemon};
use reef::{Device, ReefConfig};
use serde_json::{Value, json};
use support::WAIT;

const AGENT: &str = "agt_rec";
const AT: &str = "2026-11-01T10:00:00.000Z";

/// A scratch data dir, removed at the end of the test.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("reef-record-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write_config(&self, config: Value) {
        let path = self.0.join("record.json");
        // A new size or time each write, so Reef sees the change.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn daemon() -> FakeDaemon {
    let daemon = FakeDaemon::start("srv_rec").await.unwrap();
    daemon.add_workspace(fake::workspace(
        "wks_rec",
        "shop",
        "shop-checkout",
        &["billing"],
    ));
    daemon.upsert_agent(fake::agent(
        AGENT,
        "wks_rec",
        "Fix the build",
        AT,
        json!({}),
    ));
    // History from before recording began, which it leaves alone.
    let old = json!({ "type": "user_message", "text": "an old prompt" });
    daemon.seed_row(AGENT, "turn-old", AT, old);
    daemon
}

async fn reef(daemon: &FakeDaemon, dir: &Dir) -> reef::ReefHandle {
    let hosts = vec![fleet::HostConfig {
        id: reef::config::host_id("box"),
        name: "box".into(),
        endpoint: Some(daemon.endpoint()),
        password: None,
    }];
    let device = Device {
        name: "test phone".into(),
        token: support::TOKEN.into(),
    };
    let mut config = ReefConfig::new(hosts, "cid_reef_record", vec![device]);
    config.bind = "127.0.0.1:0".parse().unwrap();
    config.data_dir = Some(dir.0.clone());
    config.limits.record = Duration::from_millis(30);
    reef::start(config).await.unwrap()
}

/// Waits until `check` gives something.
async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Every record in a jsonl sink's folder, or in a file of lines.
fn records(path: &Path) -> Vec<Value> {
    let files: Vec<PathBuf> = if path.is_dir() {
        let mut files: Vec<PathBuf> = (std::fs::read_dir(path).unwrap())
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        files
    } else {
        vec![path.to_path_buf()]
    };
    (files.iter())
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .flat_map(|text| {
            // Only whole lines: Reef may be partway through appending the
            // last one while the test reads.
            let whole = text.rsplit_once('\n').map_or("", |(done, _)| done);
            whole
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect::<Vec<_>>()
        })
        .filter(|record: &Value| record["v"] == "fleet-event/1")
        .collect()
}

fn kinds(records: &[Value]) -> Vec<String> {
    (records.iter())
        .map(|r| r["kind"].as_str().unwrap().to_string())
        .collect()
}

/// Whether the sink has a cursor for the agent: it is following it.
fn following(dir: &Dir, sink: &str) -> bool {
    std::fs::read_to_string(dir.0.join("record").join(format!("{sink}.json")))
        .is_ok_and(|text| text.contains(AGENT))
}

/// A whole turn, a minute after the last, so the agent's `updatedAt` moves
/// as a real one's does.
fn turn(daemon: &FakeDaemon, prompt: &str, reply: &str) {
    static MINUTE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let minute = MINUTE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let at = format!("2026-11-01T11:{:02}:00.000Z", minute % 60);
    daemon.start_turn(AGENT, &at);
    let turn = daemon.current_turn(AGENT);
    daemon.stream_row(
        AGENT,
        &turn,
        &at,
        json!({ "type": "user_message", "text": prompt }),
    );
    daemon.stream_row(
        AGENT,
        &turn,
        &at,
        json!({ "type": "tool_call", "callId": "c1",
        "name": "Bash", "status": "completed", "error": null,
        "detail": { "type": "shell", "input": { "command": "cargo build" }, "output": "ok" } }),
    );
    daemon.finish_turn(AGENT, &at, reply);
}

#[tokio::test]
async fn a_turn_is_recorded_scrubbed_and_summed_up() {
    let daemon = daemon().await;
    let dir = Dir::new("jsonl");
    dir.write_config(json!({ "follow_all": true,
        "sinks": [{ "name": "log", "type": "jsonl", "dir": "log" }] }));
    let _reef = reef(&daemon, &dir).await;
    eventually("the sink to follow the agent", || {
        following(&dir, "log").then_some(())
    })
    .await;

    turn(
        &daemon,
        "Fix it, the key is AKIAABCDEFGHIJKLMNOP",
        "Done: the build is green.",
    );
    let log = dir.0.join("log");
    let got = eventually("the turn's records", || {
        let got = records(&log);
        got.iter()
            .any(|r| r["kind"] == "turn_finished")
            .then_some(got)
    })
    .await;

    // From now on: the history before recording began is not in it.
    assert_eq!(
        kinds(&got),
        ["prompt", "tool_call", "reply", "turn_finished"]
    );
    let (prompt, call, finished) = (&got[0], &got[1], &got[3]);
    assert_eq!(prompt["text"], "Fix it, the key is [AWS-KEY]");
    assert_eq!(prompt["host"], "box");
    assert_eq!(prompt["agent"]["title"], "Fix the build");
    assert_eq!(prompt["workspace"]["labels"], json!(["billing"]));
    assert!(
        prompt["id"]
            .as_str()
            .unwrap()
            .starts_with("box/agt_rec/epoch-srv_rec/")
    );
    assert_eq!(prompt["key"].as_str().unwrap().len(), 32);
    assert!(
        prompt["redaction"]
            .as_str()
            .unwrap()
            .starts_with("builtin/1")
    );
    // Reef's own fields aren't scrubbed: `reef@…` is no email address.
    assert!(prompt["source"].as_str().unwrap().starts_with("reef@0."));
    // Tool output is left out unless asked for.
    assert_eq!(call["tool"]["input"]["command"], "cargo build");
    assert!(call["tool"].get("output").is_none());
    assert_eq!(finished["reply"], "Done: the build is green.");
    assert_eq!(finished["tools"], json!({ "Bash": 1 }));
    assert_eq!(finished["whole"], true);
    // Every record has its searchable text in one field, scrubbed as the rest
    // is, and with no tool output in it, as none is in the record.
    assert_eq!(prompt["body"], "Fix it, the key is [AWS-KEY]");
    assert_eq!(call["body"], "Bash\ncargo build");
    assert_eq!(got[2]["body"], "Done: the build is green.");
    assert_eq!(
        finished["body"],
        "Fix it, the key is [AWS-KEY]\n\nDone: the build is green."
    );

    // A sink added while Reef runs starts from then, and the first keeps
    // going without repeats.
    dir.write_config(json!({ "follow_all": true, "sinks": [
        { "name": "log", "type": "jsonl", "dir": "log" },
        { "name": "late", "type": "jsonl", "dir": "late", "kinds": ["turn_finished"] } ] }));
    eventually("the new sink to follow", || {
        following(&dir, "late").then_some(())
    })
    .await;
    turn(&daemon, "And the docs?", "Docs updated.");
    let late = eventually("the second turn in the new sink", || {
        let late = records(&dir.0.join("late"));
        (!late.is_empty()).then_some(late)
    })
    .await;
    assert_eq!(kinds(&late), ["turn_finished"]);
    assert_eq!(late[0]["reply"], "Docs updated.");
    let all = eventually("the second turn in the first sink", || {
        let all = records(&log);
        (all.len() == 8).then_some(all)
    })
    .await;
    assert_eq!(all.iter().filter(|r| r["kind"] == "prompt").count(), 2);
}

#[tokio::test]
async fn a_sink_that_was_down_gets_everything_once_it_is_back() {
    let daemon = daemon().await;
    let dir = Dir::new("exec");
    let out = dir.0.join("plugin-out.jsonl");
    let plain = dir.0.join("plain-out.jsonl");
    let down = dir.0.join("plugin-out.jsonl.down");
    std::fs::write(&down, "").unwrap();
    let exe = std::env::current_exe().unwrap();
    let command = json!([exe, "sink_plugin", "--exact", "--nocapture", "-q"]);
    dir.write_config(json!({ "follow_all": true, "sinks": [
        { "name": "plug", "type": "exec", "command": command,
          "env": { "REEF_TEST_SINK_OUT": out }, "kinds": ["prompt", "reply", "turn_finished"] },
        { "name": "plain", "type": "exec", "mode": "plain", "command": command,
          "env": { "REEF_TEST_SINK_OUT": plain }, "kinds": ["turn_finished"] } ] }));
    let _reef = reef(&daemon, &dir).await;
    eventually("both sinks to follow", || {
        (following(&dir, "plug") && following(&dir, "plain")).then_some(())
    })
    .await;

    turn(&daemon, "First", "One.");
    turn(&daemon, "Second", "Two.");
    // The plain sink has them; the one that is down refuses them.
    eventually("the plain sink's turns", || {
        (records(&plain).len() == 2).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(records(&out).is_empty());
    let hello = std::fs::read_to_string(&out).unwrap();
    assert!(hello.contains("reef-sink/1"), "{hello}");

    std::fs::remove_file(&down).unwrap();
    let got = eventually("the turns once the sink is back", || {
        let got = records(&out);
        (got.len() == 6).then_some(got)
    })
    .await;
    assert_eq!(
        kinds(&got),
        [
            "prompt",
            "reply",
            "turn_finished",
            "prompt",
            "reply",
            "turn_finished"
        ]
    );
    assert_eq!(got[5]["reply"], "Two.");
}

#[tokio::test]
async fn a_restarted_reef_takes_up_where_it_stopped() {
    let daemon = daemon().await;
    let dir = Dir::new("restart");
    dir.write_config(json!({ "follow_all": true,
        "sinks": [{ "name": "log", "type": "jsonl", "dir": "log", "kinds": ["prompt"] }] }));
    let first = reef(&daemon, &dir).await;
    eventually("the sink to follow", || {
        following(&dir, "log").then_some(())
    })
    .await;
    turn(&daemon, "Before the restart", "ok");
    let log = dir.0.join("log");
    eventually("the first prompt", || {
        (records(&log).len() == 1).then_some(())
    })
    .await;
    // Its cursor saved, and Reef gone.
    tokio::time::sleep(Duration::from_millis(200)).await;
    first.stop();
    daemon.drop_connections();

    // A turn while no Reef was watching.
    turn(&daemon, "While Reef was away", "ok");

    let _second = reef(&daemon, &dir).await;
    let got = eventually("the missed prompt", || {
        let got = records(&log);
        (got.len() == 2).then_some(got)
    })
    .await;
    assert_eq!(got[0]["text"], "Before the restart");
    assert_eq!(got[1]["text"], "While Reef was away");

    // The daemon lost its history: a gap record, then the tail again.
    daemon.new_epoch("epoch-after");
    turn(&daemon, "After the new epoch", "ok");
    dir.write_config(json!({ "follow_all": true,
        "sinks": [{ "name": "log", "type": "jsonl", "dir": "log", "kinds": ["prompt", "gap"] }] }));
    let got = eventually("the gap", || {
        let got = records(&log);
        got.iter().any(|r| r["kind"] == "gap").then_some(got)
    })
    .await;
    let gap = got.iter().position(|r| r["kind"] == "gap").unwrap();
    assert!(
        got[gap + 1..]
            .iter()
            .any(|r| r["text"] == "After the new epoch"),
        "{got:#?}"
    );
}

#[tokio::test]
async fn tool_output_is_in_the_body_only_when_it_is_kept() {
    let daemon = daemon().await;
    let dir = Dir::new("output");
    dir.write_config(json!({ "follow_all": true, "redact": { "drop": [] },
        "sinks": [{ "name": "log", "type": "jsonl", "dir": "log", "kinds": ["tool_call"] }] }));
    let _reef = reef(&daemon, &dir).await;
    eventually("the sink to follow", || {
        following(&dir, "log").then_some(())
    })
    .await;
    turn(&daemon, "Build it", "Built.");
    let got = eventually("the tool call", || {
        let got = records(&dir.0.join("log"));
        (!got.is_empty()).then_some(got)
    })
    .await;
    assert_eq!(got[0]["tool"]["output"], "ok");
    assert_eq!(got[0]["body"], "Bash\ncargo build\nok");
}

/// `n` turns of history on the daemon before any Reef looks: prompt `p<i>`
/// answered by reply `r<i>`.
fn history(daemon: &FakeDaemon, agent: &str, n: u64) {
    for i in 0..n {
        let at = format!("2026-10-31T09:{:02}:00.000Z", i % 60);
        let turn = format!("turn-h{i}");
        let prompt = json!({ "type": "user_message", "text": format!("p{i}") });
        daemon.seed_row(agent, &turn, &at, prompt);
        let reply = json!({ "type": "assistant_message", "text": format!("r{i}"),
                            "messageId": format!("m{i}") });
        daemon.seed_row(agent, &turn, &at, reply);
    }
}

#[tokio::test]
async fn a_sink_that_starts_from_the_start_gets_the_history_and_one_that_does_not_gets_none() {
    let daemon = daemon().await;
    // 300 turns: 600 rows, which Reef reads three pages at a time.
    history(&daemon, AGENT, 300);
    let dir = Dir::new("start");
    dir.write_config(json!({ "follow_all": true, "sinks": [
        { "name": "log", "type": "jsonl", "dir": "log" },
        { "name": "search", "type": "jsonl", "dir": "search", "from": "start",
          "kinds": ["prompt", "reply"] } ] }));
    let _reef = reef(&daemon, &dir).await;
    let got = eventually("the whole history", || {
        let got = records(&dir.0.join("search"));
        (got.len() == 601).then_some(got)
    })
    .await;
    // Oldest first, each once: the old prompt the fixture seeded, then p0, r0…
    let said: Vec<&str> = got.iter().map(|r| r["text"].as_str().unwrap()).collect();
    let mut want = vec!["an old prompt".to_string()];
    for i in 0..300 {
        want.extend([format!("p{i}"), format!("r{i}")]);
    }
    assert_eq!(said, want);
    assert!(got.iter().all(|r| r["body"] == r["text"]));
    // A turn now goes to it and to the log.
    turn(&daemon, "After", "Later.");
    let log = eventually("the log's turn", || {
        let log = records(&dir.0.join("log"));
        log.iter()
            .any(|r| r["kind"] == "turn_finished")
            .then_some(log)
    })
    .await;
    // The log began at the end: nothing of the history, summaries included.
    assert_eq!(
        kinds(&log),
        ["prompt", "tool_call", "reply", "turn_finished"]
    );
    let more = eventually("the new turn in the search sink", || {
        let more = records(&dir.0.join("search"));
        (more.len() == 603).then_some(more)
    })
    .await;
    assert_eq!(more[601]["text"], "After");
    assert_eq!(more[602]["text"], "Later.");

    // A sink added later with `from: start` gets the whole of it as well,
    // and the first two do not hear it again.
    dir.write_config(json!({ "follow_all": true, "sinks": [
        { "name": "log", "type": "jsonl", "dir": "log" },
        { "name": "search", "type": "jsonl", "dir": "search", "from": "start",
          "kinds": ["prompt", "reply"] },
        { "name": "late", "type": "jsonl", "dir": "late", "from": "start",
          "kinds": ["prompt"] } ] }));
    let late = eventually("the late sink's history", || {
        let late = records(&dir.0.join("late"));
        (late.len() == 302).then_some(late)
    })
    .await;
    assert_eq!(late[0]["text"], "an old prompt");
    assert_eq!(late[301]["text"], "After");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(records(&dir.0.join("search")).len(), 603);
    assert_eq!(records(&dir.0.join("log")).len(), 4);
}

#[tokio::test]
async fn a_deleted_agent_is_forgotten_and_an_archived_one_is_not() {
    let daemon = daemon().await;
    daemon.upsert_agent(fake::agent("agt_two", "wks_rec", "Second", AT, json!({})));
    let dir = Dir::new("forget");
    dir.write_config(json!({ "follow_all": true, "sinks": [
        { "name": "log", "type": "jsonl", "dir": "log" },
        // Not asking for forget records: it gets them all the same.
        { "name": "prompts", "type": "jsonl", "dir": "prompts", "kinds": ["prompt"] } ] }));
    let _reef = reef(&daemon, &dir).await;
    eventually("both sinks to follow both agents", || {
        let saved = |sink: &str| {
            std::fs::read_to_string(dir.0.join("record").join(format!("{sink}.json")))
                .is_ok_and(|text| text.contains(AGENT) && text.contains("agt_two"))
        };
        (saved("log") && saved("prompts")).then_some(())
    })
    .await;

    // Archived: it leaves the daemon's list, which says the same of this as
    // of a deletion; Reef asks the daemon, which still has it, and says nothing.
    daemon.archive_agent("agt_two");
    eventually("Reef to ask about the agent that left", || {
        (daemon.requests().iter().any(|r| r == "fetch_agent_request")).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(records(&dir.0.join("log")).is_empty());
    assert!(
        std::fs::read_to_string(dir.0.join("record").join("log.json"))
            .unwrap()
            .contains("agt_two"),
        "the archived agent is still followed"
    );

    // Deleted: each sink is told, whatever it asked for, and drops its cursor.
    daemon.delete_agent(AGENT);
    for sink in ["log", "prompts"] {
        let got = eventually("the forget record", || {
            let got = records(&dir.0.join(sink));
            (!got.is_empty()).then_some(got)
        })
        .await;
        assert_eq!(kinds(&got), ["forget"], "{sink}");
        assert_eq!(got[0]["id"], "box/agt_rec/forget");
        assert_eq!(got[0]["host"], "box");
        assert_eq!(got[0]["agent"], json!({ "id": AGENT }));
        assert_eq!(got[0]["v"], "fleet-event/1");
        assert_eq!(got[0]["body"], "");
        assert!(got[0]["time"].is_string());
    }
    eventually("the cursor to go", || {
        (!following(&dir, "log")).then_some(())
    })
    .await;
    // Only that agent's.
    let saved = std::fs::read_to_string(dir.0.join("record").join("log.json")).unwrap();
    assert!(saved.contains("agt_two"), "{saved}");
}

#[tokio::test]
async fn a_deletion_while_the_host_was_down_or_reef_was_away_is_caught_up() {
    let daemon = daemon().await;
    daemon.upsert_agent(fake::agent("agt_two", "wks_rec", "Second", AT, json!({})));
    daemon.upsert_agent(fake::agent("agt_three", "wks_rec", "Third", AT, json!({})));
    let dir = Dir::new("catchup");
    dir.write_config(json!({ "follow_all": true,
        "sinks": [{ "name": "log", "type": "jsonl", "dir": "log" }] }));
    let first = reef(&daemon, &dir).await;
    eventually("every agent to be followed", || {
        std::fs::read_to_string(dir.0.join("record").join("log.json"))
            .is_ok_and(|text| text.contains("agt_two") && text.contains("agt_three"))
            .then_some(())
    })
    .await;

    // The host goes, and an agent with it, with nobody told.
    daemon.drop_connections();
    daemon.delete_agent("agt_two");
    let got = eventually("the forget record once Reef is back on the host", || {
        let got = records(&dir.0.join("log"));
        (!got.is_empty()).then_some(got)
    })
    .await;
    assert_eq!(kinds(&got), ["forget"]);
    assert_eq!(got[0]["agent"]["id"], "agt_two");

    // An agent archived, then deleted: a daemon says nothing of that. Reef
    // gone meanwhile finds out when it starts again.
    daemon.archive_agent("agt_three");
    eventually("Reef to ask about it", || {
        let asked = daemon.requests();
        (asked.iter().filter(|r| *r == "fetch_agent_request").count() >= 2).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    first.stop();
    daemon.drop_connections();
    daemon.delete_agent("agt_three");
    daemon.delete_agent(AGENT);
    let _second = reef(&daemon, &dir).await;
    let got = eventually("both forgets", || {
        let got = records(&dir.0.join("log"));
        (got.len() == 3).then_some(got)
    })
    .await;
    let mut gone: Vec<&str> = got
        .iter()
        .map(|r| r["agent"]["id"].as_str().unwrap())
        .collect();
    gone.sort();
    assert_eq!(gone, ["agt_rec", "agt_three", "agt_two"]);
    assert!(kinds(&got).iter().all(|kind| *kind == "forget"));
    eventually("every cursor to go", || {
        std::fs::read_to_string(dir.0.join("record").join("log.json"))
            .is_ok_and(|text| text.trim() == "[]")
            .then_some(())
    })
    .await;
}

/// What a feed service was sent: each POST's headers (names in lower case)
/// and body.
type Posts = std::sync::Arc<std::sync::Mutex<Vec<(Vec<String>, Value)>>>;

/// A feed service for a test: an HTTP server that keeps each POST it gets,
/// and answers the next of `script` (200 when it has run out).
struct Feed {
    url: String,
    posts: Posts,
}

impl Feed {
    async fn start(script: &[(u16, &[(&str, &str)])]) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/feed", listener.local_addr().unwrap());
        let posts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = posts.clone();
        let mut script: std::collections::VecDeque<(u16, Vec<(String, String)>)> = (script.iter())
            .map(|(status, headers)| {
                let headers = headers.iter().map(|(n, v)| (n.to_string(), v.to_string()));
                (*status, headers.collect())
            })
            .collect();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head, body_at) = loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
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
                kept.lock()
                    .unwrap()
                    .push((head.lines().map(str::to_string).collect(), body));
                let (status, headers) = script.pop_front().unwrap_or((200, Vec::new()));
                let mut answer = format!("HTTP/1.1 {status} Answer\r\n");
                for (name, value) in headers {
                    answer.push_str(&format!("{name}: {value}\r\n"));
                }
                answer.push_str("Content-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = stream.write_all(answer.as_bytes()).await;
            }
        });
        Self { url, posts }
    }

    /// The POSTs so far: headers, and body.
    fn posts(&self) -> Vec<(Vec<String>, Value)> {
        self.posts.lock().unwrap().clone()
    }

    /// Every record the service was sent, in order.
    fn records(&self) -> Vec<Value> {
        (self.posts().into_iter())
            .flat_map(|(_, body)| body["records"].as_array().cloned().unwrap_or_default())
            .collect()
    }
}

#[tokio::test]
async fn an_http_sink_posts_each_batch_with_its_token_and_keeps_what_is_not_taken() {
    let daemon = daemon().await;
    // Not taken three times, in three ways, and then taken.
    let feed = Feed::start(&[
        (503, &[("Retry-After", "0")]),
        (429, &[("Retry-After", "0")]),
        (500, &[]),
    ])
    .await;
    let dir = Dir::new("http");
    // cargo sets this one for a test, which is all a token has to be.
    dir.write_config(json!({ "follow_all": true, "sinks": [{
        "name": "search", "type": "http", "url": feed.url, "token_env": "CARGO_PKG_NAME",
        "from": "start", "kinds": ["prompt", "reply", "turn_finished"] }] }));
    let _reef = reef(&daemon, &dir).await;
    eventually("the history to be taken", || {
        (feed.posts().len() >= 4 && following(&dir, "search")).then_some(())
    })
    .await;

    // The same records each time, in a batch of a new id, until it was taken.
    let posts = feed.posts();
    assert_eq!(posts.len(), 4, "{posts:#?}");
    let ids: Vec<u64> = posts
        .iter()
        .map(|(_, b)| b["id"].as_u64().unwrap())
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    for (head, body) in &posts {
        assert_eq!(head[0], "post /feed http/1.1");
        assert!(
            head.contains(&"authorization: bearer reef".to_string()),
            "{head:?}"
        );
        assert!(head.contains(&"content-type: application/json".to_string()));
        assert_eq!(body["protocol"], "reef-sink/1");
        assert_eq!(body["sink"], "search");
        assert_eq!(body["records"], posts[0].1["records"]);
    }
    let first = posts[0].1["records"].as_array().unwrap();
    assert_eq!(kinds(first), ["prompt", "turn_finished"]);
    assert_eq!(first[0]["text"], "an old prompt");
    assert_eq!(first[0]["body"], "an old prompt");

    // Taken, so the next turn is a batch of its own; and a deletion reaches it.
    turn(&daemon, "Next", "Right.");
    eventually("the turn", || {
        (feed.records().iter().any(|r| r["text"] == "Right.")).then_some(())
    })
    .await;
    let records = feed.records();
    assert_eq!(
        records
            .iter()
            .filter(|r| r["text"] == "an old prompt")
            .count(),
        4
    );
    assert_eq!(records.iter().filter(|r| r["text"] == "Right.").count(), 1);
    daemon.delete_agent(AGENT);
    let forget = eventually("the forget record", || {
        feed.records().into_iter().find(|r| r["kind"] == "forget")
    })
    .await;
    assert_eq!(forget["id"], "box/agt_rec/forget");
    assert_eq!(forget["agent"], json!({ "id": AGENT }));
    eventually("the cursor to go", || {
        (!following(&dir, "search")).then_some(())
    })
    .await;
}

/// Following every agent of a 5,000-agent fleet: how long until every
/// agent is followed, and a turn still recorded promptly after. Slow, so
/// run on purpose: `cargo test -p reef --test record -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_big_fleet_is_followed_and_recorded() {
    let daemon = daemon().await;
    fake::seed_many(&daemon, "s", 5000);
    let dir = Dir::new("big");
    dir.write_config(json!({ "follow_all": true,
        "sinks": [{ "name": "log", "type": "jsonl", "dir": "log", "kinds": ["turn_finished"] }] }));
    let started = std::time::Instant::now();
    let _reef = reef(&daemon, &dir).await;
    let cursors = dir.0.join("record").join("log.json");
    let followed = eventually("every agent followed", || {
        let saved: Vec<Value> = serde_json::from_slice(&std::fs::read(&cursors).ok()?).ok()?;
        (saved.len() > 5000).then_some(saved.len())
    })
    .await;
    eprintln!("{followed} agents followed in {:?}", started.elapsed());
    let asked = std::time::Instant::now();
    turn(&daemon, "On a big fleet", "Still quick.");
    eventually("the turn", || {
        (records(&dir.0.join("log")).len() == 1).then_some(())
    })
    .await;
    eprintln!("a turn recorded {:?} after it ended", asked.elapsed());
}

/// The same fleet read from the start by a sink: every agent's seeded history
/// is read, a few agents at a time, and a turn after that is recorded
/// promptly. Slow, so run on purpose, with the one above.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_big_fleet_is_read_from_the_start() {
    let daemon = daemon().await;
    fake::seed_many(&daemon, "s", 5000);
    let dir = Dir::new("big-start");
    dir.write_config(json!({ "follow_all": true, "sinks": [
        { "name": "history", "type": "jsonl", "dir": "history", "from": "start",
          "kinds": ["prompt"] },
        { "name": "log", "type": "jsonl", "dir": "log", "kinds": ["turn_finished"] } ] }));
    let started = std::time::Instant::now();
    let _reef = reef(&daemon, &dir).await;
    let lines = |sink: &str| {
        let files = std::fs::read_dir(dir.0.join(sink)).into_iter().flatten();
        (files.flatten())
            .filter_map(|file| std::fs::read(file.path()).ok())
            .map(|bytes| bytes.iter().filter(|b| **b == b'\n').count())
            .sum::<usize>()
    };
    // A prompt each, and the fixture's old one. Longer than a test waits:
    // two requests an agent, eight at a time.
    let mut read = 0;
    while read < 5001 {
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "only {read} prompts read after {:?}",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        read = lines("history");
        if started.elapsed().as_secs().is_multiple_of(5) {
            eprintln!("{read} prompts read after {:?}", started.elapsed());
        }
    }
    eprintln!("{read} prompts read in {:?}", started.elapsed());
    let asked = std::time::Instant::now();
    turn(&daemon, "On a big fleet", "Still quick.");
    eventually("the turn in the log", || (lines("log") == 1).then_some(())).await;
    eprintln!("a turn recorded {:?} after it ended", asked.elapsed());
    // Nothing the log sink has was history.
    assert_eq!(records(&dir.0.join("log")).len(), 1);
}

/// Not a test of its own: the sink program the exec test starts, which is
/// this binary again. It writes what it gets to `REEF_TEST_SINK_OUT` and
/// refuses batches while a `.down` file sits beside that.
#[test]
fn sink_plugin() {
    use std::io::{BufRead, Write};
    let Ok(out) = std::env::var("REEF_TEST_SINK_OUT") else {
        return;
    };
    let down = format!("{out}.down");
    let mut stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let mut file = (std::fs::OpenOptions::new().create(true).append(true))
            .open(&out)
            .unwrap();
        if message["v"] == "fleet-event/1" {
            // Plain mode: a bare record.
            writeln!(file, "{message}").unwrap();
            continue;
        }
        match message["type"].as_str() {
            Some("hello") => writeln!(file, "{}", json!({ "hello": message["protocol"] })).unwrap(),
            Some("batch") => {
                let id = &message["id"];
                let answer = if Path::new(&down).exists() {
                    json!({ "type": "retry", "id": id, "after_ms": 100, "error": "down for the test" })
                } else {
                    for record in message["records"].as_array().unwrap() {
                        writeln!(file, "{record}").unwrap();
                    }
                    json!({ "type": "ack", "id": id })
                };
                writeln!(stdout, "{answer}").unwrap();
                stdout.flush().unwrap();
            }
            _ => {}
        }
    }
}
