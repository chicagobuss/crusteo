//! Burrow's agent commands against a running daemon with the echo provider:
//! list, find, send and wait for the reply, rename, label, archive.

use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::client::Client;
use burrow_daemon::commands;
use burrow_daemon::{Config, Daemon, start};
use burrow_provider::echo::EchoProvider;
use serde_json::json;

#[tokio::test]
async fn the_commands_drive_an_agent_end_to_end() {
    let cwd = std::env::temp_dir().join(format!("burrow-commands-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let daemon = Daemon::new(Config::ephemeral());
    daemon.agents.register(Arc::new(EchoProvider {
        chunk_delay: Duration::from_millis(2),
        slow_delay: Duration::from_secs(30),
    }));
    let running = start(daemon).await.expect("start");
    let mut client = Client::connect(&running.local_addr.to_string())
        .await
        .expect("connect");

    let created = client
        .request(
            json!({"type": "agent.create.request", "labels": {},
                   "config": {"provider": "echo", "cwd": cwd.to_string_lossy()}}),
            Some(Duration::from_secs(10)),
        )
        .await
        .expect("create");
    let id = created["agent"]["id"].as_str().expect("an id").to_string();

    // Found by a prefix of its id, and listed.
    let agents = commands::agents(&mut client).await.unwrap();
    let agent = commands::find(&agents, &id[..6]).unwrap().clone();
    assert_eq!(agent["id"], id.as_str());
    let now = time::OffsetDateTime::now_utc();
    assert!(commands::list(&agents, &Default::default(), now).contains(&id[..8]));

    // A prompt, and the reply once the turn ends.
    let reply = commands::send(&mut client, &agent, "hello there", None, "auto", Some(None))
        .await
        .expect("a reply");
    assert!(
        !reply.trim().is_empty(),
        "the echo agent answered: {reply:?}"
    );

    // Renamed, then found by its new name; labelled.
    commands::update(
        &mut client,
        &agent,
        Some("Echo helper"),
        &Default::default(),
    )
    .await
    .expect("rename");
    let labels = commands::parse_labels(&["team=core".into()]).unwrap();
    commands::update(&mut client, &agent, None, &labels)
        .await
        .expect("label");
    let agents = commands::agents(&mut client).await.unwrap();
    let renamed = commands::find(&agents, "echo helper").unwrap().clone();
    assert_eq!(renamed["id"], id.as_str());
    assert_eq!(renamed["labels"]["team"], "core");
    assert!(commands::inspect(&renamed).contains("team=core"));

    // `ls --label`: its workspace's label by name, or its own key=value.
    let workspace = renamed["workspaceId"].as_str().expect("a workspace");
    client
        .request(
            fleet::requests::set_label(workspace, "Billing", "teal", true),
            Some(Duration::from_secs(10)),
        )
        .await
        .expect("a workspace label");
    let workspaces = commands::workspace_labels(&mut client).await.unwrap();
    assert_eq!(workspaces[workspace], ["Billing"]);
    let keeps = |given: &[&str]| {
        let given: Vec<String> = given.iter().map(|s| s.to_string()).collect();
        commands::LabelFilter::parse(&given)
            .unwrap()
            .keeps(&renamed, &workspaces)
    };
    assert!(keeps(&["billing"]));
    assert!(keeps(&["billing", "team=core"]));
    assert!(!keeps(&["personal"]));
    assert!(!keeps(&["billing", "team=web"]));

    // `burrow run`: a new agent on a task, waited for; then `wait` and `logs`.
    let run = commands::Run {
        prompt: "count to three".into(),
        provider: "echo".into(),
        title: Some("Runner".into()),
        cwd: cwd.to_string_lossy().into_owned(),
        labels: commands::parse_pairs("--label", &["role=helper".into()]).unwrap(),
        ..commands::Run::default()
    };
    let said = commands::run(&mut client, &run, Some(None))
        .await
        .expect("a run");
    assert!(said.contains("count to three"), "the echo reply: {said:?}");
    let agents = commands::agents(&mut client).await.unwrap();
    let runner = commands::find(&agents, "runner").unwrap().clone();
    assert_eq!(runner["labels"]["role"], "helper");
    let again = commands::wait(&mut client, &runner, Some(Duration::from_secs(10)))
        .await
        .expect("an idle agent's wait ends at once");
    assert!(again.contains("count to three"), "{again:?}");
    let logs = commands::logs(&mut client, &runner, None).await.unwrap();
    assert!(logs.contains("count to three"), "{logs}");
    let last = commands::logs(&mut client, &runner, Some(1)).await.unwrap();
    assert!(last.lines().count() <= logs.lines().count(), "{last}");
    // Started without waiting: says how to follow it.
    let started = commands::run(&mut client, &run, None).await.unwrap();
    assert!(started.starts_with("Started Runner"), "{started}");
    assert!(started.contains("burrow wait"), "{started}");

    // Archived: gone from the list, there with -a.
    commands::archive(&mut client, &renamed)
        .await
        .expect("archive");
    let agents = commands::agents(&mut client).await.unwrap();
    let now = time::OffsetDateTime::now_utc();
    assert!(!commands::list(&agents, &Default::default(), now).contains(&id[..8]));
    assert!(
        commands::list(
            &agents,
            &commands::ListOpts {
                all: true,
                ..Default::default()
            },
            now
        )
        .contains("archived")
    );

    client.close().await;
    let _ = std::fs::remove_dir_all(&cwd);
}

/// `burrow ls` with the real binary: --json and --json --brief list the
/// same agents as the text rows for each filter, and the hidden agents
/// stay out of the JSON while the hidden-count footer stays text-only.
// Multi-threaded: the binary calls block a worker while the daemon, in
// this process, answers them from another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_json_lists_the_same_agents_as_the_text_for_each_filter() {
    let cwd = std::env::temp_dir().join(format!("burrow-ls-json-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let daemon = Daemon::new(Config::ephemeral());
    daemon.agents.register(Arc::new(EchoProvider {
        chunk_delay: Duration::from_millis(2),
        slow_delay: Duration::from_secs(30),
    }));
    let running = start(daemon).await.expect("start");
    let host = format!("127.0.0.1:{}", running.local_addr.port());
    let mut client = Client::connect(&running.local_addr.to_string())
        .await
        .expect("connect");

    let keeper = client
        .request(
            json!({"type": "agent.create.request", "labels": {},
                   "config": {"provider": "echo", "title": "Keeper",
                              "cwd": cwd.to_string_lossy()}}),
            Some(Duration::from_secs(10)),
        )
        .await
        .expect("create");
    let keeper_id = keeper["agent"]["id"].as_str().unwrap().to_string();
    let quiet = client
        .request(
            json!({"type": "agent.create.request", "labels": {},
                   "config": {"provider": "echo", "title": "Quiet",
                              "cwd": cwd.to_string_lossy()}}),
            Some(Duration::from_secs(10)),
        )
        .await
        .expect("create");
    let quiet_id = quiet["agent"]["id"].as_str().unwrap().to_string();
    let quiet = commands::find(&commands::agents(&mut client).await.unwrap(), &quiet_id)
        .unwrap()
        .clone();
    commands::archive(&mut client, &quiet)
        .await
        .expect("archive");

    // The binary, this host only, the PASEO environment out of it.
    let ls = |args: &[&str]| -> String {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_burrow"))
            .arg("ls")
            .args(["--host", &host])
            .args(args)
            .env_remove("PASEO_HOST")
            .output()
            .expect("run burrow ls");
        assert!(
            out.status.success(),
            "ls {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // The text's rows, in order, by their id; the JSON's agents, in order.
    let text_ids = |text: &str| -> Vec<String> {
        (text.lines())
            .skip(1)
            .filter_map(|line| line.split_whitespace().next())
            .filter(|t| t.len() == 8 && t.chars().all(|c| c.is_ascii_hexdigit()))
            .map(|t| t[..7].to_string())
            .collect()
    };
    let json_ids = |out: &str, brief: bool| -> Vec<String> {
        let list: Vec<serde_json::Value> = serde_json::from_str(out).unwrap();
        (list.iter())
            .map(|a| {
                let key = if brief { "shortId" } else { "id" };
                // The text's id, cut to the seven characters both the full
                // id and the shortId share.
                a[key].as_str().unwrap()[..7].to_string()
            })
            .collect()
    };

    // Default: the archived one hidden, the footer says so; the JSON lists
    // the same one row, and nothing else.
    let text = ls(&[]);
    let keeper7 = keeper_id[..7].to_string();
    assert_eq!(text_ids(&text), vec![keeper7.clone()]);
    assert!(
        text.contains("1 hidden (archived); -a shows them"),
        "{text}"
    );
    assert_eq!(json_ids(&ls(&["--json"]), false), vec![keeper7.clone()]);
    assert_eq!(json_ids(&ls(&["--json", "--brief"]), true), vec![keeper7]);

    // --state: the archived one, and the same one in the JSON; no footer,
    // as a state filter shows what it names.
    let text = ls(&["--state", "archived"]);
    let quiet7 = quiet_id[..7].to_string();
    assert_eq!(text_ids(&text), vec![quiet7.clone()]);
    assert!(!text.contains("hidden"), "{text}");
    assert_eq!(
        json_ids(&ls(&["--state", "archived", "--json"]), false),
        vec![quiet7]
    );

    // A state that names nothing: nothing, in both.
    let nothing: &[&[&str]] = &[
        &["--state", "error"],
        &["--state", "error", "--json"],
        &["--state", "error", "--json", "--brief"],
    ];
    for args in nothing {
        let out = ls(args);
        if args.contains(&"--json") {
            let brief = args.contains(&"--brief");
            assert!(json_ids(&out, brief).is_empty(), "{out}");
        } else {
            assert!(text_ids(&out).is_empty(), "{out}");
        }
    }

    // -a: both agents, in both, in the same order.
    let text_a = text_ids(&ls(&["-a"]));
    let json_a = json_ids(&ls(&["-a", "--json"]), false);
    let brief_a = json_ids(&ls(&["-a", "--json", "--brief"]), true);
    assert_eq!(text_a.len(), 2, "text: {}", ls(&["-a"]));
    assert_eq!(text_a, json_a);
    assert_eq!(text_a, brief_a);
    for id in [&keeper_id[..7], &quiet_id[..7]] {
        assert!(text_a.iter().any(|x| x == id), "{text_a:?}");
    }
    assert!(!ls(&["-a"]).contains("hidden"), "-a shows them all");

    client.close().await;
    let _ = std::fs::remove_dir_all(&cwd);
}
