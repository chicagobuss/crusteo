//! Workspace labels end to end through `paseo-client`: the catalog with its
//! colours, assignments, the rest of Paseo's label requests, the pushes, a
//! sync cursor catching up, and the catalog and assignments across a
//! restart. Assignments go in the exact shape crusteo and Reef send
//! (`fleet::requests::set_label`), and `fleet`'s directory, which crusteo's
//! lanes and boards read, follows the pushes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::{Config, Daemon, Running, start};
use burrow_provider::echo::EchoProvider;
use fleet::Directory;
use paseo_client::{Client, ConnState, Error, Event, EventReceiver};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

struct Harness {
    running: Running,
    daemon: Arc<Daemon>,
    client: Client,
    events: EventReceiver,
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "burrow-labels-{name}-{}",
        burrow_daemon::ids::agent_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn harness(home: &Path, server_id: &str) -> Harness {
    let mut config = Config::ephemeral();
    config.home = Some(home.to_path_buf());
    config.server_id = server_id.into();
    let daemon = Daemon::open(config).unwrap();
    daemon.agents.register(Arc::new(EchoProvider::default()));
    let running = start(daemon.clone()).await.unwrap();
    let client = Client::connect(paseo_client::Config::new(
        running.local_addr.to_string(),
        paseo_client::generate_client_id(),
    ));
    let events = client.events();
    timeout(WAIT, async {
        loop {
            if let Event::State(ConnState::Connected(info)) = events.recv().await.unwrap() {
                assert!(info.has("workspaceLabels"));
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
        events,
    }
}

impl Harness {
    async fn request(&self, message: Value) -> Value {
        self.client.request(message).await.expect("request")
    }

    async fn refused(&self, message: Value) -> (Option<String>, String) {
        match self.client.request(message).await {
            Err(Error::Rpc { code, message }) => (code, message),
            other => panic!("expected an rpc_error, got {other:?}"),
        }
    }

    /// The next push of type `kind`, with what came before it.
    async fn push(&self, kind: &str) -> (Value, Vec<Value>) {
        let mut seen = Vec::new();
        let found = timeout(WAIT, async {
            loop {
                if let Event::Message(message) = self.events.recv().await.unwrap() {
                    if message["type"] == kind {
                        return message;
                    }
                    seen.push(message);
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {kind} within {WAIT:?}"));
        (found, seen)
    }

    /// A workspace, made the way a new agent makes one.
    async fn workspace(&self, cwd: &Path) -> String {
        let reply = self
            .request(json!({"type": "agent.create.request", "labels": {},
                "config": {"provider": "echo", "cwd": cwd.to_string_lossy()}}))
            .await;
        reply["agent"]["workspaceId"].as_str().unwrap().to_string()
    }

    async fn stop(self) {
        drop(self.client);
        self.running.shutdown().await;
        self.daemon.close();
    }
}

fn assign(workspace: &str, name: &str, color: &str, assigned: bool) -> Value {
    fleet::requests::set_label(workspace, name, color, assigned)
}

/// Pushes until the update that gives workspace `id` these labels (`null`
/// for none), applied to `directory`. Agent status changes re-describe
/// workspaces too.
async fn follow(h: &Harness, directory: &mut Directory, id: &str, labels: Value) -> Value {
    loop {
        let (update, before) = h.push("workspace_update").await;
        for push in before.iter().chain([&update]) {
            directory.apply_push(push);
        }
        let workspace = &update["payload"]["workspace"];
        if workspace["id"] == id && workspace["labels"] == labels {
            return update;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn labels_are_made_by_assigning_them_and_follow_every_change() {
    let home = scratch("home");
    let server_id = burrow_daemon::ids::server_id();
    let h = harness(&home, &server_id).await;
    let (shop, docs) = (scratch("shop"), scratch("docs"));
    let a = h.workspace(&shop).await;
    let b = h.workspace(&docs).await;

    // Crusteo's bootstrap: the workspaces, then the catalog, both subscribed.
    let mut directory = Directory::default();
    let workspaces = h
        .request(fleet::requests::fetch_workspaces(
            fleet::requests::DirectoryRead {
                sync: true,
                background: false,
            },
            None,
        ))
        .await;
    directory.apply_workspaces_reply(&workspaces, true);
    assert!(
        workspaces["entries"][0].get("labels").is_none(),
        "no labels: no field, as in Paseo"
    );
    let listed = h.request(fleet::requests::list_labels()).await;
    assert_eq!(listed["labels"], json!([]));
    assert_eq!(listed["sync"]["mode"], "snapshot");
    assert!(listed["subscriptionId"].is_string());
    directory.apply_labels_reply(&listed);

    // A new label: made with the colour asked for, its name tidied.
    let made = h.request(assign(&a, "  Ops   team ", "teal", true)).await;
    assert_eq!(made["label"], json!({"name": "Ops team", "color": "teal"}));
    assert_eq!(made["workspaceLabels"], json!(["Ops team"]));
    let (update, _) = h.push("workspace.label.update").await;
    let payload = &update["payload"];
    assert_eq!(payload["kind"], "upsert");
    assert_eq!(payload["label"], made["label"]);
    assert_eq!(payload["subscriptionId"], listed["subscriptionId"]);
    assert_eq!(payload["generation"], listed["sync"]["generation"]);
    assert_eq!(payload["seq"], 1);
    assert!(payload.get("previousName").is_none());
    directory.apply_push(&update);
    follow(&h, &mut directory, &a, json!(["Ops team"])).await;
    assert_eq!(directory.label("ops team").unwrap().color, "teal");
    assert_eq!(directory.workspaces[&a].labels, ["Ops team"]);

    // The catalog's name and colour win over what a request says.
    let again = h.request(assign(&b, "OPS TEAM", "red", true)).await;
    assert_eq!(again["label"], json!({"name": "Ops team", "color": "teal"}));
    assert_eq!(again["workspaceLabels"], json!(["Ops team"]));
    follow(&h, &mut directory, &b, json!(["Ops team"])).await;
    // Assigned twice, or taken off where it is not: nothing changes.
    let same = h.request(assign(&a, "ops team", "teal", true)).await;
    assert_eq!(same["workspaceLabels"], json!(["Ops team"]));
    let absent = h.request(assign(&a, "nowhere", "sky", false)).await;
    assert_eq!(absent["label"], json!({"name": "nowhere", "color": "sky"}));
    assert_eq!(absent["workspaceLabels"], json!(["Ops team"]));

    // Off one workspace: the catalog keeps it.
    h.request(assign(&b, "Ops team", "teal", false)).await;
    let off = follow(&h, &mut directory, &b, Value::Null).await;
    assert!(off["payload"]["workspace"].get("labels").is_none());
    h.request(assign(&a, "infra", "blue", true)).await;
    follow(&h, &mut directory, &a, json!(["Ops team", "infra"])).await;
    let catalog = h.request(fleet::requests::list_labels()).await;
    assert_eq!(
        catalog["labels"],
        json!([{"name": "Ops team", "color": "teal"}, {"name": "infra", "color": "blue"}])
    );
    let cursor = json!({"generation": catalog["sync"]["generation"],
                        "afterSeq": catalog["sync"]["headSeq"]});

    // A rename follows into every workspace; a taken name is refused.
    let renamed = h
        .request(
            json!({"type": "workspace.label.update.request", "name": "INFRA",
                        "newName": "Infra CI", "color": "amber"}),
        )
        .await;
    assert_eq!(
        renamed["label"],
        json!({"name": "Infra CI", "color": "amber"})
    );
    assert_eq!(renamed["affectedWorkspaceCount"], 1);
    let (update, _) = h.push("workspace.label.update").await;
    assert_eq!(update["payload"]["previousName"], "infra");
    directory.apply_push(&update);
    follow(&h, &mut directory, &a, json!(["Ops team", "Infra CI"])).await;
    assert_eq!(directory.workspaces[&a].labels, ["Ops team", "Infra CI"]);
    let (code, message) = h
        .refused(
            json!({"type": "workspace.label.update.request", "name": "Infra CI",
                        "newName": "ops TEAM"}),
        )
        .await;
    assert_eq!(code.as_deref(), Some("label_name_taken"));
    assert_eq!(message, "A label with that name already exists");
    // A recolour is the catalog's alone.
    let recolored = h
        .request(
            json!({"type": "workspace.label.update.request", "name": "ops team",
                        "color": "pink"}),
        )
        .await;
    assert_eq!(recolored["affectedWorkspaceCount"], 0);
    let (update, before) = h.push("workspace.label.update").await;
    assert!(!before.iter().any(|m| m["type"] == "workspace_update"));
    assert_eq!(update["payload"]["label"]["color"], "pink");
    directory.apply_push(&update);

    // A cursor from before the rename catches up with only the changes.
    let caught = h
        .request(json!({"type": "workspace.label.list.request", "sync": cursor}))
        .await;
    assert_eq!(caught["sync"]["mode"], "changes");
    assert_eq!(
        caught["labels"],
        json!([{"name": "Infra CI", "color": "amber"}, {"name": "Ops team", "color": "pink"}])
    );
    assert_eq!(caught["sync"]["removals"][0]["name"], "infra");

    // Deleting asks first how many workspaces it would change.
    let inspected = h
        .request(json!({"type": "workspace.label.delete.inspect.request", "name": "infra ci"}))
        .await;
    assert_eq!(inspected["affectedWorkspaceCount"], 1);
    let deleted = h
        .request(json!({"type": "workspace.label.delete.request", "name": "infra ci"}))
        .await;
    assert_eq!(deleted["affectedWorkspaceCount"], 1);
    follow(&h, &mut directory, &a, json!(["Ops team"])).await;
    let (update, _) = h.push("workspace.label.update").await;
    assert_eq!(update["payload"]["kind"], "remove");
    assert_eq!(update["payload"]["name"], "Infra CI");
    directory.apply_push(&update);
    assert!(directory.label("infra ci").is_none());
    assert_eq!(directory.workspaces[&a].labels, ["Ops team"]);
    let gone = h
        .request(json!({"type": "workspace.label.delete.request", "name": "infra ci"}))
        .await;
    assert_eq!(gone["affectedWorkspaceCount"], 0);

    // Paseo's refusals.
    let (code, message) = h.refused(assign("wks_nowhere", "x", "red", true)).await;
    assert_eq!(code.as_deref(), Some("workspace_not_found"));
    assert_eq!(message, "Workspace not found");
    let (code, _) = h.refused(assign(&a, "   ", "red", true)).await;
    assert_eq!(code.as_deref(), Some("label_name_empty"));
    let (code, _) = h
        .refused(
            json!({"type": "workspace.label.update.request", "name": "missing",
                        "color": "red"}),
        )
        .await;
    assert_eq!(code.as_deref(), Some("label_not_found"));
    // A colour outside Paseo's ten does not validate.
    let (code, _) = h.refused(assign(&a, "hex", "#ff0000", true)).await;
    assert_eq!(code.as_deref(), Some("unknown_schema"));
    h.stop().await;

    // The catalog and the assignments are both on disk.
    let saved: Value =
        serde_json::from_slice(&std::fs::read(home.join("workspace-labels.json")).unwrap())
            .unwrap();
    assert_eq!(saved, json!([{"name": "Ops team", "color": "pink"}]));
    let h = harness(&home, &server_id).await;
    let listed = h.request(fleet::requests::list_labels()).await;
    assert_eq!(listed["labels"], saved);
    assert_ne!(
        listed["sync"]["generation"], catalog["sync"]["generation"],
        "a new process, a new generation"
    );
    let workspaces = h.request(json!({"type": "fetch_workspaces_request"})).await;
    let labelled: Vec<(Value, Value)> = workspaces["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| (w["id"].clone(), w["labels"].clone()))
        .collect();
    assert!(
        labelled.contains(&(json!(a), json!(["Ops team"]))),
        "{labelled:?}"
    );
    assert!(labelled.contains(&(json!(b), Value::Null)), "{labelled:?}");
    h.stop().await;
    for dir in [home, shop, docs] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_socket_with_a_label_subscription_hears_a_change() {
    let home = scratch("home");
    let cwd = scratch("cwd");
    let h = harness(&home, &burrow_daemon::ids::server_id()).await;
    let workspace = h.workspace(&cwd).await;
    let other = Client::connect(paseo_client::Config::new(
        h.running.local_addr.to_string(),
        paseo_client::generate_client_id(),
    ));
    let events = other.events();
    let listed = other
        .request(fleet::requests::list_labels())
        .await
        .expect("labels");
    // A client-chosen subscription id is refused, as everywhere.
    let chosen = other
        .request(json!({"type": "workspace.label.list.request",
                        "subscribe": {"subscriptionId": "mine"}}))
        .await;
    assert!(
        matches!(&chosen, Err(Error::Rpc { code: Some(code), .. }) if code == "handler_error"),
        "{chosen:?}"
    );

    h.request(assign(&workspace, "release", "emerald", true))
        .await;
    let pushed = timeout(WAIT, async {
        loop {
            if let Event::Message(message) = events.recv().await.unwrap()
                && message["type"] == "workspace.label.update"
            {
                return message;
            }
        }
    })
    .await
    .expect("the other socket's push");
    assert_eq!(
        pushed["payload"]["subscriptionId"],
        listed["subscriptionId"]
    );
    assert_eq!(pushed["payload"]["label"]["name"], "release");
    drop(other);
    h.stop().await;
    for dir in [home, cwd] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}
