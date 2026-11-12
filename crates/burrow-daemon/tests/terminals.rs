//! Workspace terminals, end to end through crusteo's own client against a
//! real Burrow: watching a workspace's terminals, making one, attaching
//! with the screen restored, typing and seeing the output, a second
//! client's attach getting the screen so far, size ownership, capture,
//! rename and kill. The terminals run `/bin/sh`; nothing reaches a model.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::git::canonical;
use burrow_daemon::{Config, Daemon, Running, start};
use burrow_provider::echo::EchoProvider;
use paseo_client::{Client, ConnState, Event, EventReceiver};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "burrow-terminals-{name}-{}",
        burrow_daemon::ids::agent_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    canonical(&dir)
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

struct Side {
    client: Client,
    events: EventReceiver,
}

impl Side {
    async fn connect(running: &Running) -> Self {
        let client = Client::connect(paseo_client::Config::new(
            running.local_addr.to_string(),
            paseo_client::generate_client_id(),
        ));
        let events = client.events();
        timeout(WAIT, async {
            loop {
                if let Event::State(ConnState::Connected(info)) = events.recv().await.unwrap() {
                    for feature in ["workspaceTerminals", "terminal-restore-modes"] {
                        assert!(info.has(feature), "{feature}");
                    }
                    return;
                }
            }
        })
        .await
        .expect("connected");
        Self { client, events }
    }

    async fn request(&self, message: Value) -> Value {
        self.client.request(message).await.expect("request")
    }

    /// Pushed session messages until one matches.
    async fn until_message(&self, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
        timeout(WAIT, async {
            loop {
                if let Event::Message(message) = self.events.recv().await.unwrap()
                    && matches(&message)
                {
                    return message;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {what} within {WAIT:?}"))
    }

    /// Binary frames on `slot`, the payloads joined, until they hold
    /// `needle`; the opcodes seen too.
    async fn until_output(&self, slot: u8, needle: &str) -> (String, Vec<u8>) {
        let mut seen = Vec::new();
        let mut opcodes = Vec::new();
        timeout(WAIT, async {
            loop {
                if let Event::Binary(frame) = self.events.recv().await.unwrap()
                    && frame.get(1) == Some(&slot)
                {
                    opcodes.push(frame[0]);
                    seen.extend_from_slice(&frame[2..]);
                    if String::from_utf8_lossy(&seen).contains(needle) {
                        return;
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "no {needle:?} on slot {slot}; saw {:?}",
                String::from_utf8_lossy(&seen)
            )
        });
        (String::from_utf8_lossy(&seen).into_owned(), opcodes)
    }

    fn input(&self, terminal: &str, data: &str) {
        self.client
            .send(json!({"type": "terminal_input", "terminalId": terminal,
                                "message": {"type": "input", "data": data}}));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminals_run_in_a_workspace_and_clients_attach_to_them() {
    let home = scratch("home");
    let folder = scratch("folder");
    let mut config = Config::ephemeral();
    config.home = Some(home.clone());
    let daemon: Arc<Daemon> = Daemon::open(config).unwrap();
    daemon.agents.register(Arc::new(EchoProvider::default()));
    let running = start(daemon.clone()).await.unwrap();
    let a = Side::connect(&running).await;

    let project = a
        .request(json!({"type": "project.add.request", "cwd": text(&folder)}))
        .await;
    let made = a
        .request(json!({"type": "workspace.create.request",
            "source": {"kind": "directory", "path": text(&folder),
                       "projectId": project["project"]["projectId"]}}))
        .await;
    let workspace = made["workspace"]["id"].as_str().unwrap().to_string();

    // Watching: empty at first, then each change pushed whole.
    let watched = a
        .request(
            json!({"type": "subscribe_terminals_request", "cwd": text(&folder),
                        "workspaceId": workspace}),
        )
        .await;
    assert_eq!(watched["terminals"], json!([]), "{watched}");
    assert_eq!(watched["workspaceId"], workspace.as_str());
    let watch_id = watched["subscriptionId"].as_str().unwrap().to_string();

    // Refusals in Paseo's words.
    let relative = a
        .request(
            json!({"type": "create_terminal_request", "cwd": "relative/path",
                        "workspaceId": workspace}),
        )
        .await;
    assert_eq!(relative["error"], "cwd must be absolute path");
    let unknown = a
        .request(
            json!({"type": "create_terminal_request", "cwd": text(&folder),
                        "workspaceId": "ws_nowhere"}),
        )
        .await;
    assert_eq!(
        unknown["error"],
        "Workspace ws_nowhere is not active or does not exist"
    );

    let created = a
        .request(
            json!({"type": "create_terminal_request", "cwd": text(&folder),
                        "workspaceId": workspace, "command": "/bin/sh",
                        "size": {"rows": 24, "cols": 80}}),
        )
        .await;
    assert!(created["error"].is_null(), "{created}");
    let terminal = created["terminal"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["terminal"]["name"], "Terminal 1");
    assert_eq!(created["terminal"]["workspaceId"], workspace.as_str());
    let pushed = a
        .until_message("the new terminal's push", |m| {
            m["type"] == "terminals_changed"
                && m["payload"]["terminals"][0]["id"] == terminal.as_str()
        })
        .await;
    assert_eq!(pushed["payload"]["subscriptionId"], watch_id.as_str());
    assert_eq!(pushed["payload"]["workspaceId"], workspace.as_str());

    // Attach as crusteo does: claim the size, then subscribe with a restore.
    a.client
        .send(json!({"type": "terminal_input", "terminalId": terminal,
        "message": {"type": "resize", "rows": 24, "cols": 80, "intent": "claim"}}));
    let attached = a
        .request(
            json!({"type": "subscribe_terminal_request", "terminalId": terminal,
            "restore": {"mode": "visible-snapshot", "scrollbackLines": 200,
                        "size": {"rows": 24, "cols": 80}}}),
        )
        .await;
    assert!(attached["error"].is_null(), "{attached}");
    let slot = attached["slot"].as_u64().unwrap() as u8;
    assert!(attached["subscriptionId"].is_string());

    a.input(&terminal, "echo hello-$((40+2))\n");
    let (output, opcodes) = a.until_output(slot, "hello-42").await;
    assert_eq!(opcodes[0], 0x05, "the screen comes first: {opcodes:?}");
    assert!(opcodes[1..].iter().all(|op| *op == 0x01), "{opcodes:?}");
    assert!(output.contains("echo hello-"), "{output:?}");

    // What it shows, as text.
    let captured = a
        .request(
            json!({"type": "capture_terminal_request", "terminalId": terminal,
                        "stripAnsi": true}),
        )
        .await;
    let lines: Vec<&str> = (captured["lines"].as_array().unwrap().iter())
        .filter_map(Value::as_str)
        .collect();
    assert!(lines.contains(&"hello-42"), "{captured}");
    assert!(captured["totalLines"].as_u64().unwrap() >= 24);

    // A second client attaching gets the screen so far, in one frame. Its
    // size update does not count: the first client holds the size.
    let b = Side::connect(&running).await;
    b.client
        .send(json!({"type": "terminal_input", "terminalId": terminal,
        "message": {"type": "resize", "rows": 10, "cols": 40, "intent": "update"}}));
    let second = b
        .request(
            json!({"type": "subscribe_terminal_request", "terminalId": terminal,
            "restore": {"mode": "visible-snapshot"}}),
        )
        .await;
    let b_slot = second["slot"].as_u64().unwrap() as u8;
    let (restored, opcodes) = b.until_output(b_slot, "hello-42").await;
    assert_eq!(opcodes, [0x05], "{restored:?}");
    // And it sees what anyone types from then on.
    a.input(&terminal, "echo second-$((1+1))\n");
    b.until_output(b_slot, "second-2").await;
    let size = a
        .request(json!({"type": "capture_terminal_request", "terminalId": terminal}))
        .await;
    assert!(size["totalLines"].as_u64().unwrap() >= 24, "still 24 rows");

    // Renamed: the title shows in the list.
    let renamed = a
        .request(
            json!({"type": "terminal.rename.request", "terminalId": terminal,
                        "title": "  builds  "}),
        )
        .await;
    assert_eq!(renamed["success"], true, "{renamed}");
    let listed = b
        .request(json!({"type": "list_terminals_request", "workspaceId": workspace}))
        .await;
    assert_eq!(listed["terminals"][0]["title"], "builds", "{listed}");
    assert_eq!(listed["terminals"][0]["cwd"], text(&folder));
    let empty = a
        .request(
            json!({"type": "terminal.rename.request", "terminalId": terminal,
                        "title": " "}),
        )
        .await;
    assert_eq!(empty["error"], "Title is required");

    // Released by id: no more output on that slot.
    let released = b
        .request(json!({"type": "subscription.release.request",
                        "subscriptionId": second["subscriptionId"]}))
        .await;
    assert_eq!(released["subscriptionId"], second["subscriptionId"]);

    // Killed: the stream ends, and the list empties.
    let killed = a
        .request(json!({"type": "kill_terminal_request", "terminalId": terminal}))
        .await;
    assert_eq!(killed["success"], true);
    let exit = a
        .until_message("the stream's end", |m| m["type"] == "terminal_stream_exit")
        .await;
    assert_eq!(exit["payload"]["terminalId"], terminal.as_str());
    assert_eq!(
        exit["payload"]["subscriptionId"],
        attached["subscriptionId"]
    );
    a.until_message("the emptied list", |m| {
        m["type"] == "terminals_changed" && m["payload"]["terminals"] == json!([])
    })
    .await;
    assert_eq!(daemon.terminals.count(), 0);

    drop((a, b));
    running.shutdown().await;
    daemon.close().unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&folder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_whose_program_exits_leaves_the_list() {
    let home = scratch("exit-home");
    let folder = scratch("exit-folder");
    let mut config = Config::ephemeral();
    config.home = Some(home.clone());
    let daemon = Daemon::open(config).unwrap();
    let running = start(daemon.clone()).await.unwrap();
    let a = Side::connect(&running).await;
    a.request(json!({"type": "project.add.request", "cwd": text(&folder)}))
        .await;
    let made = a
        .request(json!({"type": "workspace.create.request",
            "source": {"kind": "directory", "path": text(&folder)}}))
        .await;
    let workspace = made["workspace"]["id"].as_str().unwrap().to_string();
    // Without a workspace id, the folder's workspace is found by its path.
    let created = a
        .request(
            json!({"type": "create_terminal_request", "cwd": text(&folder),
                        "command": "/bin/sh", "args": ["-c", "echo done; exit 3"]}),
        )
        .await;
    assert_eq!(
        created["terminal"]["workspaceId"],
        workspace.as_str(),
        "{created}"
    );
    timeout(WAIT, async {
        while daemon.terminals.count() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the terminal ends with its program");
    let listed = a
        .request(json!({"type": "list_terminals_request", "cwd": text(&folder)}))
        .await;
    assert_eq!(listed["terminals"], json!([]));
    drop(a);
    running.shutdown().await;
    daemon.close().unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&folder);
}
