//! Creating, deleting and relabelling agents from a phone, end to end:
//! the form's choices, and each action sending exactly the daemon requests
//! crusteo sends for it, and nothing off Reef's list.

mod support;

use reef::fake::{self, FakeDaemon};
use reef::upstream::{READ_REQUESTS, WRITE_REQUESTS};
use serde_json::{Value, json};
use support::*;

/// A seeded daemon, a Reef on it, and a phone that has seen the fleet.
async fn fleet() -> (FakeDaemon, Phone) {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    let addr = reef_with(&[("example-a", &daemon)]).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "ny", "view": "needs_you"}))
        .await;
    phone.until("the fleet", |p| p.keys("ny").len() == 3).await;
    (daemon, phone)
}

/// Sends a message with a `req` and waits for its result.
async fn ask(phone: &mut Phone, message: Value) -> Value {
    let req = phone.ask(message).await;
    phone
        .until("the answer", |p| p.results.contains_key(&req))
        .await;
    phone.results[&req].clone()
}

async fn act(phone: &mut Phone, mut action: Value) -> Value {
    action["type"] = json!("action");
    ask(phone, action).await
}

/// The requests the daemon got that are not reads, by type.
fn writes(daemon: &FakeDaemon) -> Vec<String> {
    (daemon.requests().into_iter())
        .filter(|r| !READ_REQUESTS.contains(&r.as_str()))
        .collect()
}

/// Everything the daemon got was on Reef's list, or an agent's labels and
/// nothing more, and it refused nothing.
fn only_listed(daemon: &FakeDaemon) {
    for kind in daemon.requests() {
        assert!(
            READ_REQUESTS.contains(&kind.as_str())
                || WRITE_REQUESTS.contains(&kind.as_str())
                || kind == "update_agent_request",
            "{kind} is not on Reef's list"
        );
    }
    for write in daemon.writes() {
        if write["type"] == "update_agent_request" {
            assert!(reef::upstream::labels_write(&write), "{write}");
        }
    }
    assert!(daemon.refused().is_empty(), "{:?}", daemon.refused());
}

/// The label ids an agent's labels value names, read back by name from a
/// phone's colours (by id → name and colour).
fn label_names(phone: &Phone, value: &Value) -> Vec<String> {
    let text = value.as_str().unwrap_or_default();
    let list = text
        .strip_prefix("v1:")
        .unwrap_or_else(|| panic!("{value}"));
    (list.split(',').filter(|id| !id.is_empty()))
        .map(|id| {
            let label = &phone.labels[id];
            label["name"]
                .as_str()
                .unwrap_or_else(|| panic!("{id} in {}", phone.labels))
                .to_string()
        })
        .collect()
}

fn reads_of(daemon: &FakeDaemon, kind: &str) -> usize {
    daemon.requests().iter().filter(|r| *r == kind).count()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_form_gets_a_hosts_ready_providers_and_workspaces_read_once() {
    let (daemon, mut phone) = fleet().await;
    let options = ask(
        &mut phone,
        json!({"type": "create_options", "host": "example-a"}),
    )
    .await;
    assert_eq!(options["ok"], true, "{options}");
    assert_eq!(options["host"], "example-a");
    assert_eq!(options["labels"], true);
    let providers = options["providers"].as_array().unwrap();
    let ids: Vec<&str> = providers
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["claude", "codex", "opencode"]);
    let claude = &providers[0];
    assert_eq!(claude["ready"], true);
    assert_eq!(claude["mode"], "default");
    assert_eq!(claude["modes"][2], json!({"id": "plan", "label": "Plan"}));
    assert_eq!(claude["models"][0]["id"], "claude-opus-5-5");
    assert_eq!(claude["models"][0]["default"], true);
    assert_eq!(claude["models"][0]["thinking_default"], "medium");
    assert!(claude["models"][2].get("thinking").is_none());
    // Not ready: by name and why, nothing more.
    assert_eq!(
        providers[2],
        json!({"id": "opencode", "label": "OpenCode", "ready": false, "status": "unavailable"})
    );
    // Workspaces with their directories, the most recently active first.
    let workspaces = options["workspaces"].as_array().unwrap();
    assert_eq!(workspaces.len(), 4);
    assert_eq!(workspaces[0]["id"], "wks_a_1");
    let infra = workspaces.iter().find(|w| w["id"] == "wks_a_4").unwrap();
    assert_eq!(
        *infra,
        json!({"id": "wks_a_4", "name": "infra-ci", "project": "infra",
               "directory": "/home/dev/code/infra", "labels": ["infra", "billing"]})
    );

    // Asked again: the snapshot Reef keeps.
    let again = ask(
        &mut phone,
        json!({"type": "create_options", "host": "example-a"}),
    )
    .await;
    assert_eq!(again["providers"], options["providers"]);
    assert_eq!(reads_of(&daemon, "get_providers_snapshot_request"), 1);

    let nowhere = ask(
        &mut phone,
        json!({"type": "create_options", "host": "nowhere"}),
    )
    .await;
    assert_eq!(nowhere["ok"], false);
    assert_eq!(nowhere["error"]["code"], "not_found");
    assert!(writes(&daemon).is_empty());
    only_listed(&daemon);
}

/// A host's tools changing under it (a CLI upgraded) reach the form at the
/// next ask, not after the ten minutes Reef otherwise keeps them.
#[tokio::test(flavor = "multi_thread")]
async fn the_form_reads_the_providers_again_once_the_host_says_they_changed() {
    let (daemon, mut phone) = fleet().await;
    let ask_options = json!({"type": "create_options", "host": "example-a"});
    let before = ask(&mut phone, ask_options.clone()).await;
    let codex = |options: &Value| -> Vec<String> {
        let providers = options["providers"].as_array().unwrap();
        let codex = providers.iter().find(|p| p["id"] == "codex").unwrap();
        (codex["models"].as_array().unwrap().iter())
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(codex(&before), ["gpt-5-codex"]);

    let mut entries = fake::providers();
    entries[1]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "gpt-6-codex", "label": "GPT-6 Codex"}));
    daemon.update_providers(entries);
    // Once Reef has taken the push, the next ask reads them again.
    let mut after = before.clone();
    for _ in 0..50 {
        after = ask(&mut phone, ask_options.clone()).await;
        if codex(&after).len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(codex(&after), ["gpt-5-codex", "gpt-6-codex"]);
    assert_eq!(reads_of(&daemon, "get_providers_snapshot_request"), 2);
    only_listed(&daemon);
}

#[tokio::test(flavor = "multi_thread")]
async fn creating_sends_one_create_request_carrying_the_agents_labels() {
    let (daemon, mut phone) = fleet().await;
    let answer = act(
        &mut phone,
        json!({"action": "create", "host": "example-a", "provider": "claude",
               "model": "claude-sonnet-5", "mode": "plan", "workspace": "wks_a_3",
               "labels": ["docs", "Billing"], "text": "Write the changelog.",
               "message_id": "msg-new"}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    let agent = answer["agent"].as_str().unwrap().to_string();
    assert_eq!(answer["host"], "example-a");
    assert_eq!(answer["key"], format!("example-a/{agent}"));
    assert!(answer.get("labels_error").is_none(), "{answer}");
    assert_eq!(writes(&daemon), ["agent.create.request"]);
    let sent = &daemon.writes()[0];
    let labels = sent["labels"][fleet::labels::KEY].clone();
    assert_eq!(
        *sent,
        json!({"type": "agent.create.request", "workspaceId": "wks_a_3",
               "config": {"provider": "claude", "cwd": "/home/dev/code/docs",
                          "model": "claude-sonnet-5", "modeId": "plan", "thinkingOptionId": "low"},
               "labels": {fleet::labels::KEY: labels}, "initialPrompt": "Write the changelog.",
               "clientMessageId": "msg-new", "requestId": sent["requestId"]})
    );
    // A label the catalog has is that label, matched as crusteo matches
    // names; a new one is made, in the colour its name picks.
    phone
        .until("the new label's colour", |p| {
            p.labels.as_object().is_some_and(|l| l.len() == 4)
        })
        .await;
    assert_eq!(label_names(&phone, &labels), ["docs", "billing"]);
    let docs = (phone.labels.as_object().unwrap().values())
        .find(|l| l["name"] == "docs")
        .unwrap();
    assert_eq!(docs["color"], reef::lists::label_color("docs", None));
    // The workspace keeps its own.
    assert_eq!(daemon.workspace("wks_a_3").unwrap()["labels"], json!([]));

    // The phone opens it at once: Reef has it, with its first message.
    phone
        .send(
            json!({"type": "subscribe", "id": "c", "view": "conversation",
                     "host": "example-a", "agent": agent}),
        )
        .await;
    phone
        .until("the new conversation", |p| {
            p.views.get("c").is_some_and(|v| {
                v.fields["loading"] == false
                    && v.parts.iter().any(|t| {
                        (t["items"].as_array().into_iter().flatten())
                            .any(|i| i["text"] == "Write the changelog.")
                    })
            })
        })
        .await;
    phone
        .until("its labels", |p| {
            p.view("c").fields["agent"]["labels"] == json!(["docs", "billing"])
        })
        .await;
    assert!(phone.errors.is_empty(), "{:?}", phone.errors);
    only_listed(&daemon);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_typed_directory_is_its_workspace_or_left_to_the_daemon() {
    let (daemon, mut phone) = fleet().await;
    // A workspace's own directory: created in that workspace, with the
    // provider's defaults.
    let known = act(
        &mut phone,
        json!({"action": "create", "host": "example-a", "provider": "codex",
               "cwd": "/home/dev/code/infra", "text": "Bump the runners."}),
    )
    .await;
    assert_eq!(known["ok"], true, "{known}");
    let sent = &daemon.writes()[0];
    assert_eq!(sent["workspaceId"], "wks_a_4");
    assert_eq!(
        sent["config"],
        json!({"provider": "codex", "cwd": "/home/dev/code/infra", "model": "gpt-5-codex",
               "modeId": "auto", "thinkingOptionId": "medium"})
    );
    // A message id is made when the phone sends none.
    assert!(
        sent["clientMessageId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );

    // Any other: the daemon finds or makes the workspace.
    let other = act(
        &mut phone,
        json!({"action": "create", "host": "example-a", "provider": "claude",
               "cwd": "/home/dev/code/tools", "text": ""}),
    )
    .await;
    assert_eq!(other["ok"], true, "{other}");
    let sent = &daemon.writes()[1];
    assert!(sent.get("workspaceId").is_none(), "{sent}");
    assert_eq!(sent["config"]["cwd"], "/home/dev/code/tools");
    // No prompt: none sent.
    assert!(sent.get("initialPrompt").is_none(), "{sent}");
    assert_eq!(writes(&daemon).len(), 2);
    only_listed(&daemon);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_create_the_reef_cannot_serve_sends_nothing() {
    let (daemon, mut phone) = fleet().await;
    let create = |extra: Value| {
        let mut message = json!({"action": "create", "host": "example-a", "provider": "claude",
                                 "workspace": "wks_a_1", "text": "Hello."});
        for (k, v) in extra.as_object().unwrap() {
            message[k] = v.clone();
        }
        message
    };
    for (extra, code) in [
        (json!({"host": "nowhere"}), "not_found"),
        (json!({"provider": "gemini"}), "not_found"),
        (json!({"provider": "opencode"}), "bad_request"),
        (json!({"model": "gpt-2"}), "bad_request"),
        (json!({"mode": "yolo"}), "bad_request"),
        (
            json!({"model": "claude-haiku-4-5", "thinking": "high"}),
            "bad_request",
        ),
        (json!({"workspace": "wks_gone"}), "not_found"),
        (json!({"workspace": null}), "bad_request"),
        (json!({"labels": "billing"}), "bad_request"),
        (json!({"labels": ["   "]}), "bad_request"),
    ] {
        let answer = act(&mut phone, create(extra.clone())).await;
        assert_eq!(answer["ok"], false, "{extra}: {answer}");
        assert_eq!(answer["error"]["code"], code, "{extra}: {answer}");
        assert!(answer["error"]["message"].is_string(), "{answer}");
    }
    let unready = act(&mut phone, create(json!({"provider": "opencode"}))).await;
    assert!(
        unready["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not ready"),
        "{unready}"
    );
    assert!(writes(&daemon).is_empty(), "{:?}", writes(&daemon));
    only_listed(&daemon);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_sends_one_delete_request_and_the_agent_goes() {
    let (daemon, mut phone) = fleet().await;
    let answer = act(
        &mut phone,
        json!({"action": "delete", "host": "example-a", "agent": "agt_a_failed"}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(writes(&daemon), ["delete_agent_request"]);
    assert_eq!(
        daemon.writes()[0],
        json!({"type": "delete_agent_request", "agentId": "agt_a_failed",
               "requestId": daemon.writes()[0]["requestId"]})
    );
    phone
        .until("the agent to go", |p| {
            !p.keys("ny").contains(&"example-a/agt_a_failed".to_string())
        })
        .await;
    let again = act(
        &mut phone,
        json!({"action": "delete", "host": "example-a", "agent": "agt_a_failed"}),
    )
    .await;
    assert_eq!(again["error"]["code"], "not_found");
    let elsewhere = act(
        &mut phone,
        json!({"action": "delete", "host": "nowhere", "agent": "agt_a_run"}),
    )
    .await;
    assert_eq!(elsewhere["error"]["code"], "not_found");
    assert_eq!(writes(&daemon).len(), 1);
    only_listed(&daemon);
}

#[tokio::test(flavor = "multi_thread")]
async fn relabelling_sets_the_agents_own_labels_and_leaves_its_workspace_alone() {
    let (daemon, mut phone) = fleet().await;
    phone
        .send(json!({"type": "subscribe", "id": "all", "view": "agents"}))
        .await;
    phone
        .until("the agents", |p| p.keys("all").len() == 5)
        .await;
    // The orchestrator shares its workspace, and so its billing label,
    // with another agent; it alone should have infra and a new one.
    let answer = act(
        &mut phone,
        json!({"action": "relabel", "host": "example-a", "agent": "agt_a_orch",
               "labels": ["infra", " Ops   team "]}),
    )
    .await;
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(writes(&daemon), ["update_agent_request"]);
    let sent = &daemon.writes()[0];
    let labels = sent["labels"][fleet::labels::KEY].clone();
    assert_eq!(
        *sent,
        json!({"type": "update_agent_request", "agentId": "agt_a_orch",
               "labels": {fleet::labels::KEY: labels}, "requestId": sent["requestId"]})
    );
    phone
        .until("the new label's colour", |p| {
            p.labels.as_object().is_some_and(|l| l.len() == 4)
        })
        .await;
    assert_eq!(label_names(&phone, &labels), ["infra", "Ops team"]);
    assert_eq!(
        daemon.workspace("wks_a_1").unwrap()["labels"],
        json!(["billing"])
    );
    assert_eq!(
        daemon.agent("agt_a_orch").unwrap()["labels"][fleet::labels::KEY],
        labels
    );
    // Its row shows them; its neighbour still has the workspace's.
    phone
        .until("the new labels on its row", |p| {
            let rows = &p.view("all").parts;
            let row = |key: &str| {
                rows.iter()
                    .find(|r| r["key"] == key)
                    .map(|r| r["labels"].clone())
            };
            row("example-a/agt_a_orch") == Some(json!(["infra", "Ops team"]))
                && row("example-a/agt_a_asks") == Some(json!(["billing"]))
        })
        .await;

    // The same set again, in other words and order: nothing to do.
    let same = act(
        &mut phone,
        json!({"action": "relabel", "host": "example-a", "agent": "agt_a_orch",
               "labels": ["ops team", "INFRA"]}),
    )
    .await;
    assert_eq!(same["ok"], true, "{same}");
    assert_eq!(writes(&daemon).len(), 1);

    // None at all is a choice too: no labels, not the workspace's.
    let none = act(
        &mut phone,
        json!({"action": "relabel", "host": "example-a", "agent": "agt_a_asks", "labels": []}),
    )
    .await;
    assert_eq!(none["ok"], true, "{none}");
    assert_eq!(
        daemon.writes()[1]["labels"],
        json!({fleet::labels::KEY: "v1:"})
    );
    phone
        .until("no labels on its row", |p| {
            (p.view("all").parts.iter())
                .any(|r| r["key"] == "example-a/agt_a_asks" && r.get("labels").is_none())
        })
        .await;

    for (bad, code) in [
        (json!({"agent": "agt_a_asks"}), "bad_request"),
        (json!({"agent": "agt_gone", "labels": ["x"]}), "not_found"),
    ] {
        let mut action = json!({"action": "relabel", "host": "example-a"});
        for (k, v) in bad.as_object().unwrap() {
            action[k] = v.clone();
        }
        let answer = act(&mut phone, action).await;
        assert_eq!(answer["error"]["code"], code, "{bad}: {answer}");
    }
    assert_eq!(writes(&daemon).len(), 2);
    only_listed(&daemon);
}
