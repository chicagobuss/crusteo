//! Schedules end to end, with the echo provider and Crusteo's own client,
//! in the shapes Paseo's CLI sends (`paseo schedule …`, `paseo heartbeat`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::{Config, Daemon, Running, start};
use burrow_provider::echo::EchoProvider;
use paseo_client::{Client, ConnState, Error, Event};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(15);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "burrow-schedules-{name}-{}",
        burrow_daemon::ids::agent_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Harness {
    running: Running,
    daemon: Arc<Daemon>,
    client: Client,
    cwd: PathBuf,
}

async fn harness(home: Option<&Path>, cwd: PathBuf) -> Harness {
    let mut config = Config::ephemeral();
    config.home = home.map(Path::to_path_buf);
    let daemon = Daemon::open(config).unwrap();
    daemon.agents.register(Arc::new(EchoProvider {
        chunk_delay: Duration::from_millis(2),
        slow_delay: Duration::from_secs(30),
    }));
    let running = start(daemon.clone()).await.unwrap();
    let client = Client::connect(paseo_client::Config::new(
        running.local_addr.to_string(),
        paseo_client::generate_client_id(),
    ));
    let events = client.events();
    timeout(WAIT, async {
        loop {
            if let Event::State(ConnState::Connected(_)) = events.recv().await.unwrap() {
                return;
            }
        }
    })
    .await
    .expect("connected");
    Harness {
        running,
        daemon,
        client,
        cwd,
    }
}

impl Harness {
    async fn request(&self, message: Value) -> Value {
        self.client.request(message).await.expect("request")
    }

    async fn agent(&self) -> String {
        let reply = self
            .request(json!({"type": "agent.create.request", "labels": {},
                "config": {"provider": "echo", "cwd": self.cwd.to_string_lossy()}}))
            .await;
        reply["agent"]["id"].as_str().unwrap().to_string()
    }

    /// The schedule, once `done` says its runs are as wanted.
    async fn until(&self, id: &str, done: impl Fn(&Value) -> bool) -> Value {
        let mut last = Value::Null;
        let found = timeout(WAIT, async {
            loop {
                last = self
                    .request(json!({"type": "schedule/inspect", "scheduleId": id}))
                    .await["schedule"]
                    .clone();
                if done(&last) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        assert!(found.is_ok(), "schedule never got there: {last:#}");
        last
    }

    async fn stop(self) {
        drop(self.client);
        self.running.shutdown().await;
        self.daemon.close();
    }
}

fn finished(schedule: &Value) -> bool {
    (schedule["runs"].as_array())
        .is_some_and(|runs| !runs.is_empty() && runs.iter().all(|run| run["status"] != "running"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heartbeat_reaches_its_agent_as_a_system_notice() {
    let h = harness(None, scratch("cwd")).await;
    let agent = h.agent().await;
    // `paseo heartbeat create`: a cron on the calling agent.
    let created = h
        .request(
            json!({"type": "schedule/create", "prompt": "  Check the build.  ",
            "name": "watch", "cadence": {"type": "cron", "expression": "*/5 * * * *",
            "timezone": "America/Chicago"}, "target": {"type": "self", "agentId": agent},
            "runOnCreate": true}),
        )
        .await;
    let schedule = &created["schedule"];
    let id = schedule["id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 8, "Paseo's ids: {id}");
    assert_eq!(
        schedule["target"],
        json!({"type": "agent", "agentId": agent})
    );
    assert_eq!(schedule["prompt"], "Check the build.");
    assert_eq!(schedule["status"], "active");
    assert!(schedule.get("runs").is_none(), "a summary");

    let ran = h.until(&id, finished).await;
    let run = &ran["runs"][0];
    assert_eq!(run["status"], "succeeded", "{ran:#}");
    assert_eq!(run["agentId"], agent.as_str());
    let output = run["output"].as_str().unwrap();
    assert!(
        output.contains(&format!("Schedule \"watch\" fired (id={id}")),
        "{output}"
    );
    assert!(output.contains("Check the build."), "{output}");
    // The next slot is a five-minute mark after now.
    let next = ran["nextRunAt"].as_str().unwrap();
    let minute: u32 = next[14..16].parse().unwrap();
    assert!(
        next.ends_with(":00.000Z") && minute.is_multiple_of(5),
        "{next}"
    );

    // In its timeline it is a heartbeat, not a prompt from the user.
    let page = h
        .request(
            json!({"type": "fetch_agent_timeline_request", "agentId": agent,
            "direction": "tail", "limit": 20, "projection": "canonical"}),
        )
        .await;
    let items: Vec<&Value> = (page["entries"].as_array().unwrap().iter())
        .map(|e| &e["item"])
        .collect();
    let beat = items
        .iter()
        .find(|i| i["name"] == "heartbeat")
        .expect("a heartbeat row");
    assert!(
        beat["callId"]
            .as_str()
            .unwrap()
            .starts_with("paseo-system:")
    );
    assert_eq!(beat["detail"]["icon"], "sparkles");
    assert!(
        beat["detail"]["text"]
            .as_str()
            .unwrap()
            .ends_with("Check the build.")
    );
    assert!(!items.iter().any(|i| i["type"] == "user_message"));
    let titled = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    assert!(
        titled["agent"]["title"].is_null(),
        "a heartbeat does not name the agent"
    );

    // Archiving the agent ends its heartbeat.
    h.request(json!({"type": "archive_agent_request", "agentId": agent}))
        .await;
    let ended = h.until(&id, |s| s["status"] == "completed").await;
    assert!(ended["nextRunAt"].is_null());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_agent_run_gets_its_own_workspace_which_is_archived_after() {
    let h = harness(None, scratch("cwd")).await;
    let created = h
        .request(
            json!({"type": "schedule/create", "prompt": "Summarise the day",
            "cadence": {"type": "cron", "expression": "0 18 * * *"},
            "target": {"type": "new-agent", "config": {"provider": "echo",
                "cwd": h.cwd.to_string_lossy(), "title": "Daily summary"}}}),
        )
        .await;
    let id = created["schedule"]["id"].as_str().unwrap().to_string();
    assert!(
        created["schedule"]["nextRunAt"]
            .as_str()
            .unwrap()
            .contains("T18:00:00.000Z"),
        "a cron waits for its slot unless told to run on create"
    );
    // `paseo schedule run-once`: answered when the run is over.
    let ran = h
        .request(json!({"type": "schedule/run-once", "scheduleId": id}))
        .await;
    let run = &ran["schedule"]["runs"][0];
    assert_eq!(run["status"], "succeeded", "{ran:#}");
    assert_eq!(run["output"], "echo: Summarise the day");
    let agent = run["agentId"].as_str().unwrap();
    let workspace = run["workspaceId"].as_str().unwrap();
    // A manual run leaves the cadence alone.
    assert_eq!(
        ran["schedule"]["nextRunAt"],
        created["schedule"]["nextRunAt"]
    );
    let made = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent}))
        .await;
    assert_eq!(made["agent"]["title"], "Daily summary");
    assert_eq!(made["agent"]["labels"]["paseo.schedule-id"], id.as_str());
    assert_eq!(made["agent"]["labels"]["paseo.schedule-run"], run["id"]);
    assert!(
        !made["agent"]["archivedAt"].is_null(),
        "archived with its workspace"
    );
    let archived = h
        .daemon
        .read(|store| store.archived_workspaces.contains_key(workspace));
    assert!(archived);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schedules_are_listed_paused_changed_and_deleted_and_survive_a_restart() {
    let home = scratch("home");
    let h = harness(Some(&home), scratch("cwd")).await;
    let agent = h.agent().await;
    let created = h
        .request(json!({"type": "schedule/create", "prompt": "Nightly",
            "cadence": {"type": "cron", "expression": "0 2 * * *", "timezone": "Europe/Paris"},
            "target": {"type": "agent", "agentId": agent}, "maxRuns": 3,
            "expiresAt": "2099-01-01T00:00:00.000Z"}))
        .await;
    let id = created["schedule"]["id"].as_str().unwrap().to_string();
    let listed = h.request(json!({"type": "schedule/list"})).await;
    assert_eq!(listed["schedules"].as_array().unwrap().len(), 1);
    assert_eq!(listed["schedules"][0]["maxRuns"], 3);

    let paused = h
        .request(json!({"type": "schedule/pause", "scheduleId": id}))
        .await;
    assert_eq!(paused["schedule"]["status"], "paused");
    assert!(paused["schedule"]["nextRunAt"].is_null());
    assert!(!paused["schedule"]["pausedAt"].is_null());
    let resumed = h
        .request(json!({"type": "schedule/resume", "scheduleId": id}))
        .await;
    assert_eq!(resumed["schedule"]["status"], "active");

    // A cron without a zone keeps the one it had; null clears a limit.
    let updated = h
        .request(
            json!({"type": "schedule/update", "scheduleId": id, "name": "Overnight",
            "cadence": {"type": "cron", "expression": "30 3 * * *"}, "maxRuns": null}),
        )
        .await;
    let schedule = &updated["schedule"];
    assert_eq!(schedule["name"], "Overnight");
    assert_eq!(schedule["cadence"]["timezone"], "Europe/Paris");
    assert!(schedule["maxRuns"].is_null());
    assert_eq!(schedule["expiresAt"], "2099-01-01T00:00:00.000Z");
    let refused = h
        .client
        .request(json!({"type": "schedule/update", "scheduleId": id,
            "newAgentConfig": {"model": "x"}}))
        .await;
    assert!(
        matches!(&refused, Err(Error::Rpc { code: Some(code), .. }) if code == "schedule_request_failed"),
        "{refused:?}"
    );
    let bad = h
        .client
        .request(json!({"type": "schedule/create", "prompt": "x",
            "cadence": {"type": "cron", "expression": "* * *"},
            "target": {"type": "agent", "agentId": agent}}))
        .await;
    assert!(
        matches!(&bad, Err(Error::Rpc { message, .. }) if message == "Cron expressions must have 5 fields"),
        "{bad:?}"
    );
    let logs = h
        .request(json!({"type": "schedule/logs", "scheduleId": id}))
        .await;
    assert_eq!(logs["runs"], json!([]));
    assert!(home.join("schedules").join(format!("{id}.json")).is_file());
    h.stop().await;

    // Back after a restart, as it was.
    let h = harness(Some(&home), scratch("cwd")).await;
    let back = h
        .request(json!({"type": "schedule/inspect", "scheduleId": id}))
        .await;
    assert_eq!(back["schedule"]["name"], "Overnight");
    assert_eq!(back["schedule"]["status"], "active");
    let deleted = h
        .request(json!({"type": "schedule/delete", "scheduleId": id}))
        .await;
    assert_eq!(deleted["scheduleId"], id.as_str());
    let missing = h
        .client
        .request(json!({"type": "schedule/inspect", "scheduleId": id}))
        .await;
    assert!(
        matches!(&missing, Err(Error::Rpc { message, .. }) if message == &format!("Schedule not found: {id}")),
        "{missing:?}"
    );
    assert!(!home.join("schedules").join(format!("{id}.json")).exists());
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_schedule_whose_agent_is_gone_completes_on_its_next_run() {
    let h = harness(None, scratch("cwd")).await;
    let created = h
        .request(json!({"type": "schedule/create", "prompt": "Ping",
            "cadence": {"type": "every", "everyMs": 60_000},
            "target": {"type": "agent", "agentId": "00000000-0000-4000-8000-000000000000"}}))
        .await;
    let id = created["schedule"]["id"].as_str().unwrap().to_string();
    // `every` runs at once by default.
    let done = h.until(&id, |s| s["status"] == "completed").await;
    assert_eq!(done["runs"][0]["status"], "failed");
    assert_eq!(
        done["runs"][0]["error"],
        "Agent 00000000-0000-4000-8000-000000000000 no longer exists"
    );
    h.stop().await;
}
