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
