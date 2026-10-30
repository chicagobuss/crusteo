//! `burrow import-paseo` against a made-up Paseo home: what comes over
//! shows up through the normal reads, what does not is reported, and a
//! second run changes nothing.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use burrow_daemon::paseo_import::{self, Options, Report};
use burrow_daemon::{Config, Daemon, start};
use paseo_client::{Client, ConnState, Event, EventReceiver};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);
const CWD: &str = "/home/user/code";
const ACTIVE: &str = "0b6e5d1c-2f4a-4c1e-9a43-6d0f1c2b3a41";
const ARCHIVED: &str = "5c7d9e2f-8a1b-4d3c-b6e5-7f0a1b2c3d42";
const PI: &str = "9e8d7c6b-5a49-4382-a1b0-c9d8e7f6a543";
const CODEX: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c51";
const CURSOR: &str = "2b3c4d5e-6f7a-4b8c-9d0e-1f2a3b4c5d62";
const AGY: &str = "3c4d5e6f-7a8b-4c9d-8e1f-2a3b4c5d6e73";
const OPENCODE: &str = "4d5e6f7a-8b9c-4d0e-9f2a-3b4c5d6e7f84";
const SESSION: &str = "8b32c0de-0000-4000-8000-000000000001";
const PI_SESSION: &str = "8b32c0de-0000-4000-8000-000000000002";
const THREAD: &str = "01a087d8-e44f-7c50-aa9b-4fe2043b7641";
const AGY_HANDLE: &str = r#"plugin:{"version":1,"data":{"conversationId":"9194a736"}}"#;

fn scratch() -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("burrow-import-{}", burrow_daemon::ids::agent_id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn agent(id: &str, provider: &str, title: &str) -> Value {
    json!({
        "id": id, "provider": provider, "cwd": CWD, "workspaceId": "wks_0123456789abcdef",
        "createdAt": "2026-09-01T10:00:00.000Z", "updatedAt": "2026-09-02T10:00:00.000Z",
        "lastActivityAt": "2026-09-03T10:00:00.000Z",
        "lastUserMessageAt": "2026-09-03T09:59:00.000Z",
        "title": title, "labels": {"team": "core"}, "lastStatus": "idle",
        "lastModeId": "bypassPermissions",
        "config": {"modeId": "default", "model": "claude-opus-5-5", "thinkingOptionId": "high"},
        "runtimeInfo": {"provider": provider, "sessionId": SESSION, "model": "claude-opus-5-5",
                        "modeId": "bypassPermissions", "extra": {}},
        "features": [],
        "persistence": {"provider": provider, "sessionId": SESSION, "nativeHandle": SESSION,
                        "metadata": {"cwd": CWD}},
        "requiresAttention": true, "attentionReason": "finished",
        "attentionTimestamp": "2026-09-03T10:00:00.000Z",
        "internal": false, "archivedAt": null,
    })
}

/// `record` resuming `session` instead, as its provider names it.
fn resuming(mut record: Value, session: &str, mode: &str) -> Value {
    record["persistence"]["sessionId"] = session.into();
    record["persistence"]["nativeHandle"] = session.into();
    record["runtimeInfo"]["sessionId"] = session.into();
    record["lastModeId"] = mode.into();
    record
}

/// A Paseo home with an active Claude agent (with a transcript), an archived
/// Claude agent (without one), pi (with its session file), Codex, Cursor and
/// Antigravity agents, an OpenCode agent Burrow has no provider for, and a
/// corrupt record; a Claude config directory with the transcript, and pi's
/// sessions directory beside it (`pi`).
fn paseo_home(root: &Path) -> (PathBuf, PathBuf) {
    let paseo = root.join("paseo");
    let claude = root.join("claude");
    write(
        &paseo.join("projects/projects.json"),
        &json!([
            {"projectId": "prj_0123456789abcdef", "rootPath": CWD, "kind": "non_git",
             "displayName": "code", "projectKey": CWD, "customName": "Code",
             "createdAt": "2026-09-01T10:00:00.000Z", "updatedAt": "2026-09-01T10:00:00.000Z",
             "archivedAt": null},
            {"projectId": "prj_fedcba9876543210", "rootPath": "/home/user/old", "kind": "git",
             "displayName": "old", "createdAt": "2026-08-01T10:00:00.000Z",
             "updatedAt": "2026-08-01T10:00:00.000Z", "archivedAt": "2026-08-02T10:00:00.000Z"},
        ])
        .to_string(),
    );
    write(
        &paseo.join("projects/workspaces.json"),
        &json!([
            {"workspaceId": "wks_0123456789abcdef", "projectId": "prj_0123456789abcdef",
             "cwd": CWD, "kind": "directory", "displayName": "code", "title": null,
             "pinnedAt": "2026-09-01T11:00:00.000Z", "labels": ["infra"],
             "createdAt": "2026-09-01T10:00:00.000Z", "updatedAt": "2026-09-01T10:00:00.000Z",
             "archivedAt": null},
        ])
        .to_string(),
    );
    write(
        &paseo.join("projects/workspace-labels.json"),
        &json!([
            {"name": "infra", "color": "teal"},
            {"name": "Ready to ship", "color": "emerald"},
            {"name": "hex", "color": "#336699"},
        ])
        .to_string(),
    );
    let dir = paseo.join("agents/home-user-code");
    write(
        &dir.join(format!("{ACTIVE}.json")),
        &agent(ACTIVE, "claude", "Fix the build").to_string(),
    );
    let mut archived = agent(ARCHIVED, "claude", "Old work");
    archived["archivedAt"] = "2026-09-04T10:00:00.000Z".into();
    archived["persistence"]["sessionId"] = "no-such-session".into();
    write(&dir.join(format!("{ARCHIVED}.json")), &archived.to_string());
    write(
        &dir.join(format!("{PI}.json")),
        &resuming(agent(PI, "pi", "Pi agent"), PI_SESSION, "default").to_string(),
    );
    write(
        &dir.join(format!("{CODEX}.json")),
        &resuming(agent(CODEX, "codex", "Codex agent"), THREAD, "full-access").to_string(),
    );
    write(
        &dir.join(format!("{CURSOR}.json")),
        &resuming(agent(CURSOR, "cursor", "Cursor agent"), SESSION, "agent").to_string(),
    );
    let mut agy = resuming(
        agent(AGY, "antigravity", "Agy agent"),
        AGY_HANDLE,
        "default",
    );
    agy["persistence"]
        .as_object_mut()
        .unwrap()
        .remove("nativeHandle");
    write(&dir.join(format!("{AGY}.json")), &agy.to_string());
    write(
        &dir.join(format!("{OPENCODE}.json")),
        &agent(OPENCODE, "opencode", "OpenCode agent").to_string(),
    );
    let pi_session = [
        r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-09-03T09:00:00.000Z","cwd":"/home/user/code"}"#,
        r#"{"type":"message","id":"b","parentId":null,"timestamp":"2026-09-03T09:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"Count the files"}]}}"#,
        r#"{"type":"message","id":"c","parentId":"b","timestamp":"2026-09-03T09:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Three."}]}}"#,
    ]
    .join("\n");
    write(
        &root.join(format!(
            "pi/--home-user-code--/2026-09-03T09-00-00-000Z_{PI_SESSION}.jsonl"
        )),
        &pi_session,
    );
    write(&dir.join("broken.json"), "{\"id\": \"half a rec");

    let transcript = [
        json!({"type": "user", "uuid": "u1", "timestamp": "2026-09-03T09:59:00.000Z",
               "message": {"role": "user", "content": "Why does the build fail?"}}),
        json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-09-03T09:59:30.000Z",
               "message": {"content": [{"type": "text", "text": "A missing semicolon."}]}}),
    ]
    .map(|line| line.to_string())
    .join("\n");
    write(
        &claude.join(format!("projects/-home-user-code/{SESSION}.jsonl")),
        &transcript,
    );
    (paseo, claude)
}

/// Every file under `dir` with its contents.
fn contents(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(contents(&path));
        } else {
            files.insert(path.clone(), fs::read(&path).unwrap());
        }
    }
    files
}

fn options(paseo: &Path, home: &Path, claude: &Path, dry_run: bool) -> Options {
    Options {
        from: paseo.into(),
        home: home.into(),
        claude_dir: claude.into(),
        pi_sessions: claude.parent().unwrap().join("pi"),
        // No Codex here: its threads are read at the agent's first prompt.
        codex: None,
        dry_run,
    }
}

fn import(paseo: &Path, home: &Path, claude: &Path, dry_run: bool) -> Report {
    paseo_import::run(&options(paseo, home, claude, dry_run)).expect("import")
}

async fn connected(events: &EventReceiver) {
    timeout(WAIT, async {
        loop {
            if let Event::State(ConnState::Connected(_)) = events.recv().await.unwrap() {
                return;
            }
        }
    })
    .await
    .expect("connected in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imports_every_shared_providers_agents_once() {
    let root = scratch();
    let (paseo, claude) = paseo_home(&root);
    let home = root.join("burrow");
    let before = contents(&paseo);

    let dry = import(&paseo, &home, &claude, true);
    assert_eq!((dry.agents, dry.projects, dry.workspaces), (6, 1, 1));
    assert!(!home.exists(), "a dry run writes nothing");

    let report = import(&paseo, &home, &claude, false);
    let text = report.lines.join("\n");
    assert_eq!(
        (report.agents, report.projects, report.workspaces),
        (6, 1, 1)
    );
    assert_eq!(
        report.lines, dry.lines,
        "the dry run said what would happen"
    );
    assert!(
        text.contains(&format!(
            "skip: agent {OPENCODE}: provider \"opencode\" (Burrow has no such provider)"
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "agent {PI} \"Pi agent\": 2 timeline items, resumes Pi session {PI_SESSION}"
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "warning: agent {CODEX}: Codex is not here to read thread {THREAD}"
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("warning: agent {CURSOR}: Cursor replays nothing")),
        "{text}"
    );
    assert!(
        text.contains("skip: agents/home-user-code/broken.json"),
        "{text}"
    );
    assert!(text.contains("skip: project prj_fedcba9876543210 /home/user/old: archived"));
    assert!(
        text.contains(&format!(
            "warning: agent {ARCHIVED}: no transcript for Claude session no-such-session"
        )),
        "{text}"
    );
    assert!(text.contains(&format!(
        "agent {ACTIVE} \"Fix the build\": 2 timeline items"
    )));
    assert_eq!(report.labels, 2);
    assert!(text.contains("label \"Ready to ship\""), "{text}");
    assert!(
        text.contains("skip: label #3 in projects/workspace-labels.json"),
        "{text}"
    );
    assert_eq!(report.skipped, 4);

    // Burrow serves it like any agent of its own after a restart.
    let mut config = Config::ephemeral();
    config.home = Some(home.clone());
    let daemon = Daemon::open(config).expect("open home");
    let running = start(daemon.clone()).await.expect("start");
    let client = Client::connect(paseo_client::Config::new(
        running.local_addr.to_string(),
        paseo_client::generate_client_id(),
    ));
    connected(&client.events()).await;
    let agents = client
        .request(json!({"type": "fetch_agents_request", "scope": "active"}))
        .await
        .expect("agents");
    let entries = agents["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 5, "every active agent: {agents}");
    let entry = |id: &str| {
        entries
            .iter()
            .find(|e| e["agent"]["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {agents}"))
    };
    let agent = &entry(ACTIVE)["agent"];
    assert_eq!(agent["id"], ACTIVE);
    assert_eq!(agent["title"], "Fix the build");
    assert_eq!(agent["labels"], json!({"team": "core"}));
    assert_eq!(agent["status"], "closed");
    assert_eq!(agent["workspaceId"], "wks_0123456789abcdef");
    assert_eq!(agent["currentModeId"], "bypassPermissions");
    assert_eq!(agent["model"], "claude-opus-5-5");
    assert_eq!(agent["thinkingOptionId"], "high");
    assert_eq!(agent["persistence"]["sessionId"], SESSION);
    assert_eq!(agent["createdAt"], "2026-09-01T10:00:00.000Z");
    assert_eq!(agent["updatedAt"], "2026-09-03T10:00:00.000Z");
    assert_eq!(agent["requiresAttention"], true);
    assert_eq!(agent["attentionReason"], "finished");
    assert_eq!(entry(ACTIVE)["project"]["projectName"], "Code");
    // The others resume their own provider's session, from Paseo's handle.
    for (id, provider, session, mode) in [
        (PI, "pi", PI_SESSION, "default"),
        (CODEX, "codex", THREAD, "full-access"),
        (CURSOR, "cursor", SESSION, "agent"),
        (AGY, "antigravity", AGY_HANDLE, "default"),
    ] {
        let agent = &entry(id)["agent"];
        assert_eq!(agent["provider"], provider, "{agent}");
        assert_eq!(agent["persistence"]["sessionId"], session, "{agent}");
        assert_eq!(agent["currentModeId"], mode, "{agent}");
        assert_eq!(agent["status"], "closed");
    }

    let page = client
        .request(
            json!({"type": "fetch_agent_timeline_request", "agentId": ACTIVE,
                        "direction": "tail", "limit": 40}),
        )
        .await
        .expect("timeline");
    let items: Vec<&Value> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["item"])
        .collect();
    assert_eq!(items.len(), 2, "{page}");
    assert_eq!(items[0]["type"], "user_message");
    assert_eq!(items[0]["text"], "Why does the build fail?");
    assert_eq!(items[1]["text"], "A missing semicolon.");

    let archived = client
        .request(json!({"type": "fetch_agent_request", "agentId": ARCHIVED}))
        .await
        .expect("archived agent");
    assert_eq!(archived["agent"]["archivedAt"], "2026-09-04T10:00:00.000Z");
    assert_eq!(archived["agent"]["title"], "Old work");
    let pi = client
        .request(
            json!({"type": "fetch_agent_timeline_request", "agentId": PI,
                        "direction": "tail", "limit": 40}),
        )
        .await
        .expect("pi timeline");
    let texts: Vec<&Value> = pi["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["item"]["text"])
        .collect();
    assert_eq!(texts, [&json!("Count the files"), &json!("Three.")], "{pi}");
    let opencode = client
        .request(json!({"type": "fetch_agent_request", "agentId": OPENCODE}))
        .await
        .expect("opencode agent");
    assert!(opencode["agent"].is_null());

    let workspaces = client
        .request(json!({"type": "fetch_workspaces_request"}))
        .await
        .expect("workspaces");
    let workspace = &workspaces["entries"][0];
    assert_eq!(workspace["id"], "wks_0123456789abcdef");
    assert_eq!(workspace["projectId"], "prj_0123456789abcdef");
    assert_eq!(workspace["labels"], json!(["infra"]));
    assert_eq!(workspace["pinnedAt"], "2026-09-01T11:00:00.000Z");
    // The catalog, so the assignment keeps its colour.
    let labels = client
        .request(json!({"type": "workspace.label.list.request"}))
        .await
        .expect("labels");
    assert_eq!(
        labels["labels"],
        json!([{"name": "infra", "color": "teal"}, {"name": "Ready to ship", "color": "emerald"}])
    );

    // Changes made in Burrow since then.
    daemon.apply(|store| {
        let mut agent = store.agents.get(ACTIVE).cloned().unwrap();
        agent.title = Some("Renamed in burrow".into());
        ((), store.upsert_agent(agent))
    });
    client
        .request(
            json!({"type": "workspace.label.update.request", "name": "infra",
                        "color": "red"}),
        )
        .await
        .expect("recolour");
    drop(client);
    running.shutdown().await;
    daemon.close();
    drop(daemon);

    let saved = fs::read(home.join(format!("agents/{ACTIVE}.json"))).unwrap();
    let timeline = fs::read(home.join(format!("timelines/{ACTIVE}.jsonl"))).unwrap();
    let again = import(&paseo, &home, &claude, false);
    assert_eq!(
        (again.agents, again.projects, again.workspaces),
        (0, 0, 0),
        "{:?}",
        again.lines
    );
    assert_eq!(again.labels, 0);
    assert_eq!(again.present, 10, "{:?}", again.lines);
    let catalog = burrow_daemon::persist::read(&home).unwrap().labels;
    assert_eq!(
        serde_json::to_value(&catalog).unwrap(),
        json!([{"name": "infra", "color": "red"}, {"name": "Ready to ship", "color": "emerald"}]),
        "Burrow's colour wins"
    );
    assert_eq!(
        fs::read(home.join(format!("agents/{ACTIVE}.json"))).unwrap(),
        saved,
        "the second run leaves the agent as Burrow has it"
    );
    assert_eq!(
        fs::read(home.join(format!("timelines/{ACTIVE}.jsonl"))).unwrap(),
        timeline
    );
    assert!(String::from_utf8_lossy(&saved).contains("Renamed in burrow"));

    assert_eq!(contents(&paseo), before, "the Paseo home is never written");
    fs::remove_dir_all(&root).unwrap();
}

/// A new Burrow takes Paseo's server id, so clients see the same host; one
/// that has its own keeps it.
#[test]
fn a_new_burrow_takes_paseos_server_id() {
    let root = scratch();
    let (paseo, claude) = paseo_home(&root);
    write(&paseo.join("server-id"), "srv_paseo_host\n");
    let fresh = root.join("fresh");
    import(&paseo, &fresh, &claude, true);
    assert!(
        !fresh.join("server-id").exists(),
        "a dry run writes nothing"
    );
    let report = import(&paseo, &fresh, &claude, false);
    assert!(report.lines.iter().any(|l| l.contains("srv_paseo_host")));
    assert_eq!(
        fs::read_to_string(fresh.join("server-id")).unwrap(),
        "srv_paseo_host"
    );
    let own = root.join("own");
    write(&own.join("server-id"), "srv_burrows_own");
    import(&paseo, &own, &claude, false);
    assert_eq!(
        fs::read_to_string(own.join("server-id")).unwrap(),
        "srv_burrows_own"
    );
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn refuses_a_home_inside_the_paseo_home() {
    let root = scratch();
    let (paseo, claude) = paseo_home(&root);
    let result = paseo_import::run(&options(&paseo, &paseo.join("burrow"), &claude, false));
    assert!(result.is_err());
    assert!(!paseo.join("burrow").exists());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_agent_outside_any_workspace_gets_one_for_its_cwd() {
    let root = scratch();
    let (paseo, claude) = paseo_home(&root);
    let mut record = agent(ACTIVE, "claude", "Loose");
    record.as_object_mut().unwrap().remove("workspaceId");
    write(
        &paseo.join(format!("agents/home-user-code/{ACTIVE}.json")),
        &record.to_string(),
    );
    fs::remove_file(paseo.join("projects/workspaces.json")).unwrap();
    let home = root.join("burrow");
    let report = import(&paseo, &home, &claude, false);
    assert_eq!(report.workspaces, 0);
    let loaded = burrow_daemon::persist::read(&home).unwrap();
    let imported = loaded
        .agents
        .iter()
        .find(|a| a.snapshot.id == ACTIVE)
        .unwrap();
    let workspace = loaded
        .workspaces
        .iter()
        .find(|w| Some(&w.workspace_id) == imported.snapshot.workspace_id.as_ref())
        .expect("a workspace for its cwd");
    assert_eq!(workspace.cwd, CWD);
    // Paseo's project for that root is the one it joins.
    assert_eq!(workspace.project_id, "prj_0123456789abcdef");
    fs::remove_dir_all(&root).unwrap();
}
