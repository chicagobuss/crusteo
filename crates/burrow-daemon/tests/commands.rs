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
    assert!(commands::list(&agents, false, now).contains(&id[..8]));

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
    assert!(!commands::list(&agents, false, now).contains(&id[..8]));
    assert!(commands::list(&agents, true, now).contains("archived"));

    client.close().await;
    let _ = std::fs::remove_dir_all(&cwd);
}
