//! Agents messaging agents on other hosts, through Reef, against two real
//! burrows in-process: refused while routing is off (as it starts), then
//! routed by id, prefix or name with a receipt that names the host, and a
//! finish notice to the sender in the words of a local one. A route under
//! way outlives a Reef restart.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::router::Reach;
use burrow_daemon::{Config, Daemon, Running};
use burrow_provider::echo::EchoProvider;
use paseo_client::{Client, ConnState, Event};
use reef::{Device, ReefConfig};
use serde_json::{Value, json};
use support::*;
use tokio::time::timeout;

struct Burrow {
    name: &'static str,
    daemon: Arc<Daemon>,
    running: Running,
    client: Client,
}

async fn burrow(name: &'static str) -> Burrow {
    burrow_on(name, None).await
}

/// A Burrow that reports `hostname` as its own, when given.
async fn burrow_on(name: &'static str, hostname: Option<&str>) -> Burrow {
    let mut config = Config::ephemeral();
    if let Some(hostname) = hostname {
        config.hostname = Some(hostname.into());
    }
    let daemon = Daemon::new(config);
    daemon.agents.register(Arc::new(EchoProvider {
        chunk_delay: Duration::from_millis(2),
        slow_delay: Duration::from_secs(3),
    }));
    let running = burrow_daemon::start(daemon.clone()).await.unwrap();
    let client = Client::connect(paseo_client::Config::new(
        running.local_addr.to_string(),
        paseo_client::generate_client_id(),
    ));
    let events = client.events();
    timeout(WAIT, async {
        while !matches!(
            events.recv().await,
            Ok(Event::State(ConnState::Connected(_)))
        ) {}
    })
    .await
    .expect("connected");
    Burrow {
        name,
        daemon,
        running,
        client,
    }
}

impl Burrow {
    async fn agent(&self, cwd: &Path, title: &str) -> String {
        self.agent_with(cwd, title, json!({})).await
    }

    /// As `agent`, with the agent's own `labels`.
    async fn agent_with(&self, cwd: &Path, title: &str, labels: Value) -> String {
        let reply = (self.client)
            .request(json!({"type": "agent.create.request", "labels": labels,
                "config": {"provider": "echo", "cwd": cwd.to_string_lossy(), "title": title}}))
            .await
            .unwrap();
        reply["agent"]["id"].as_str().unwrap().to_string()
    }

    async fn until_reach(&self, reach: Reach) {
        timeout(WAIT, async {
            while self.daemon.router.reach() != reach {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{} never reached {reach:?}", self.name));
    }

    /// Calls a tool as `agent`: its structured result, or its error text.
    async fn tool(&self, agent: &str, name: &str, args: Value) -> Result<Value, String> {
        let url = self.daemon.mcp.url().expect("tools").to_string();
        let token = self.daemon.mcp.token(agent);
        let name = name.to_string();
        let reply = tokio::task::spawn_blocking(move || {
            let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                              "params": {"name": name, "arguments": args}});
            let mut response = ureq::post(&url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .header("Authorization", &format!("Bearer {token}"))
                .send(body.to_string())
                .expect("the tools answer");
            let text = response.body_mut().read_to_string().unwrap();
            let json = (text.lines())
                .find_map(|line| line.strip_prefix("data:"))
                .unwrap_or(&text)
                .trim()
                .to_string();
            serde_json::from_str::<Value>(&json).unwrap()
        })
        .await
        .unwrap();
        let result = &reply["result"];
        if result["isError"] == true {
            return Err(result["content"][0]["text"].as_str().unwrap().to_string());
        }
        Ok(result["structuredContent"].clone())
    }

    /// The finish notices `agent` has had, as their text.
    async fn notices(&self, agent: &str) -> Vec<String> {
        let page = (self.client)
            .request(
                json!({"type": "fetch_agent_timeline_request", "agentId": agent,
                            "direction": "tail", "limit": 100, "projection": "canonical"}),
            )
            .await
            .unwrap();
        (page["entries"].as_array().unwrap().iter())
            .filter(|e| e["item"]["agentMessage"]["event"].is_string())
            .map(|e| {
                e["item"]["agentMessage"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    async fn until_notices(&self, agent: &str, count: usize) -> Vec<String> {
        timeout(Duration::from_secs(30), async {
            loop {
                let notices = self.notices(agent).await;
                if notices.len() >= count {
                    return notices;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{agent} never had {count} notices"))
    }

    async fn stop(self) {
        drop(self.client);
        self.running.shutdown().await;
        self.daemon.close().unwrap();
    }
}

async fn start_reef(burrows: &[&Burrow], dir: &Path) -> (reef::ReefHandle, std::net::SocketAddr) {
    let hosts = (burrows.iter())
        .map(|b| fleet::HostConfig {
            id: reef::config::host_id(b.name),
            name: b.name.into(),
            endpoint: Some(b.running.local_addr.to_string()),
            password: None,
        })
        .collect();
    let device = Device {
        name: "test phone".into(),
        token: TOKEN.into(),
    };
    let mut config = ReefConfig::new(hosts, "cid_reef_routing", vec![device]);
    config.bind = "127.0.0.1:0".parse().unwrap();
    config.data_dir = Some(dir.to_path_buf());
    let handle = reef::start(config).await.unwrap();
    let addr = handle.addr;
    (handle, addr)
}

async fn set_routing(addr: std::net::SocketAddr, on: bool) {
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    let req = phone
        .ask(json!({"type": "action", "action": "settings", "agent_routing": on}))
        .await;
    phone
        .until("the answer", |p| p.results.contains_key(&req))
        .await;
    assert_eq!(phone.results[&req]["ok"], true, "{}", phone.results[&req]);
}

fn routes(dir: &Path) -> Value {
    std::fs::read_to_string(dir.join("routes.json"))
        .map(|text| serde_json::from_str(&text).unwrap())
        .unwrap_or(json!([]))
}

fn scratch(what: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "reef-routing-{what}-{}",
        burrow_daemon::ids::agent_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_message_agents_on_other_hosts_only_once_routing_is_on() {
    let (cwd, data) = (scratch("cwd"), scratch("data"));
    let a = burrow("example-a").await;
    let b = burrow("example-b").await;
    let sender = a.agent(&cwd, "planner").await;
    let target = b.agent(&cwd, "Builder").await;

    // No Reef yet: out of reach, and the sender is told what would help.
    let refused = a
        .tool(
            &sender,
            "send_agent_prompt",
            json!({"agentId": target, "prompt": "hello"}),
        )
        .await
        .unwrap_err();
    assert!(refused.contains("no Reef is connected"), "{refused}");

    // A Reef, which starts with routing off.
    let (reef, addr) = start_reef(&[&a, &b], &data).await;
    a.until_reach(Reach::Off).await;
    let refused = a
        .tool(
            &sender,
            "send_agent_prompt",
            json!({"agentId": target, "prompt": "hello"}),
        )
        .await
        .unwrap_err();
    assert!(
        refused.contains("off in this fleet's Reef: turn it on in crusteo's or Skiff's settings"),
        "{refused}"
    );

    // Turned on: routed by id, with the target's host on the receipt.
    set_routing(addr, true).await;
    a.until_reach(Reach::On).await;
    let sent = a
        .tool(
            &sender,
            "send_agent_prompt",
            json!({"agentId": target, "prompt": "hello there"}),
        )
        .await
        .unwrap();
    assert_eq!(sent["target"]["agentId"], target.as_str(), "{sent}");
    assert_eq!(sent["target"]["host"], "example-b", "{sent}");
    assert_eq!(sent["disposition"], "started", "{sent}");
    let guidance = sent["guidance"].as_str().unwrap();
    assert!(guidance.contains("on example-b"), "{guidance}");
    assert!(guidance.contains("You will get notified"), "{guidance}");
    // The target has it, from the sender.
    let notices = a.until_notices(&sender, 1).await;
    let notice = &notices[0];
    assert!(
        notice.contains(&format!("Agent {target} (Builder) finished.")),
        "{notice}"
    );
    assert!(notice.contains("Host: example-b"), "{notice}");
    assert!(notice.contains("hello there"), "{notice}");
    timeout(WAIT, async {
        while routes(&data) != json!([]) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the route is done once the notice is delivered");

    // By name too; a name nobody has is not found anywhere.
    let by_name = a
        .tool(
            &sender,
            "send_agent_prompt",
            json!({"agentId": "builder", "prompt": "again", "notifyOnFinish": false}),
        )
        .await
        .unwrap();
    assert_eq!(by_name["target"]["agentId"], target.as_str(), "{by_name}");
    // That turn ends before the next prompt, which would otherwise be
    // steered into it on a fast machine.
    timeout(WAIT, async {
        loop {
            let status = (a.tool(&sender, "get_agent_status", json!({"agentId": target})))
                .await
                .unwrap();
            if status["status"] == "idle" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the target is idle again");
    let missing = a
        .tool(
            &sender,
            "send_agent_prompt",
            json!({"agentId": "nobody-here", "prompt": "x"}),
        )
        .await
        .unwrap_err();
    assert!(
        missing.contains("not found on this host or any other"),
        "{missing}"
    );

    // A route under way survives Reef going away and coming back.
    let slow = a
        .tool(
            &sender,
            "send_agent_prompt",
            json!({"agentId": target, "prompt": "slow work"}),
        )
        .await
        .unwrap();
    assert_eq!(slow["disposition"], "started", "{slow}");
    // Reef records the route before it answers the sender, so a Reef
    // stopped the moment the sender has its answer still owes the notice.
    let recorded = routes(&data);
    assert_eq!(recorded.as_array().unwrap().len(), 1, "{recorded}");
    assert_eq!(recorded[0]["targetAgentId"], target.as_str(), "{recorded}");
    reef.stop();
    a.until_reach(Reach::NoReef).await;
    let (reef, addr) = start_reef(&[&a, &b], &data).await;
    let notices = a.until_notices(&sender, 2).await;
    assert!(notices[1].contains("echo: work"), "{notices:?}");
    assert!(notices[1].contains("Host: example-b"), "{notices:?}");

    // Off again: refused, and the Burrows hear it.
    set_routing(addr, false).await;
    a.until_reach(Reach::Off).await;
    let refused = a
        .tool(
            &sender,
            "send_agent_prompt",
            json!({"agentId": target, "prompt": "hello"}),
        )
        .await
        .unwrap_err();
    assert!(refused.contains("off in this fleet's Reef"), "{refused}");

    reef.stop();
    a.stop().await;
    b.stop().await;
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_that_names_a_host_goes_to_an_agent_there_and_nowhere_else() {
    let (cwd, data) = (scratch("cwd"), scratch("data"));
    let a = burrow("example-a").await;
    let b = burrow("example-b").await;
    let c = burrow_on("example-c", Some("devbox.example.com")).await;
    let sender = a.agent(&cwd, "planner").await;
    // The same name on two other hosts.
    let on_b = b.agent(&cwd, "worker").await;
    let on_c = c.agent(&cwd, "Worker").await;
    let (b_server, c_server) = (
        b.daemon.config.server_id.clone(),
        c.daemon.config.server_id.clone(),
    );
    let (reef, addr) = start_reef(&[&a, &b, &c], &data).await;
    set_routing(addr, true).await;
    a.until_reach(Reach::On).await;
    let to = |server: Option<&str>, agent: &str| {
        let mut args = json!({"agentId": agent, "prompt": "go", "notifyOnFinish": false});
        if let Some(server) = server {
            args["serverId"] = server.into();
        }
        args
    };

    // With a host named, that host's: by its server id, its name in any
    // case, or the hostname it reports in any case. Reef waits for a host's
    // agents when the one named is not among them yet, so once these are
    // through, it has both workers.
    for (server, host, id) in [
        (b_server.as_str(), "example-b", &on_b),
        ("EXAMPLE-C", "example-c", &on_c),
        ("Devbox.Example.com", "example-c", &on_c),
        (c_server.as_str(), "example-c", &on_c),
    ] {
        let sent = (a
            .tool(&sender, "send_agent_prompt", to(Some(server), "worker"))
            .await)
            .unwrap_or_else(|error| panic!("{server}: {error}"));
        assert_eq!(sent["target"]["agentId"], id.as_str(), "{server}: {sent}");
        assert_eq!(sent["target"]["host"], host, "{server}: {sent}");
    }
    // Without a host, the name is on two: which is not guessed.
    let ambiguous = (a
        .tool(&sender, "send_agent_prompt", to(None, "worker"))
        .await)
        .unwrap_err();
    assert!(
        ambiguous.contains("More than one agent in the fleet goes by \"worker\""),
        "{ambiguous}"
    );

    // The sender's own host has one of that name too: the host named still
    // decides, and nothing is delivered here.
    let local = a.agent(&cwd, "worker").await;
    let sent = (a
        .tool(&sender, "send_agent_prompt", to(Some(&b_server), "worker"))
        .await)
        .unwrap();
    assert_eq!(sent["target"]["agentId"], on_b.as_str(), "{sent}");
    assert_eq!(sent["target"]["host"], "example-b", "{sent}");
    assert_eq!(a.notices(&local).await, Vec::<String>::new());
    // Without a host it is the sender's own, as it was.
    let sent = (a
        .tool(&sender, "send_agent_prompt", to(None, "worker"))
        .await)
        .unwrap();
    assert_eq!(sent["target"]["agentId"], local.as_str(), "{sent}");
    a.until_notices(&local, 1).await;

    // A host that does not have it, or is not one: said so, and nothing is
    // delivered to the agent of that name elsewhere.
    let missing = (a
        .tool(&sender, "send_agent_prompt", to(Some(&b_server), "planner"))
        .await)
        .unwrap_err();
    assert!(
        missing.contains("No agent on") && missing.contains("goes by planner"),
        "{missing}"
    );
    let nowhere = (a
        .tool(
            &sender,
            "send_agent_prompt",
            to(Some("srv_nowhere"), "worker"),
        )
        .await)
        .unwrap_err();
    assert!(
        nowhere.contains("No host in the fleet is srv_nowhere"),
        "{nowhere}"
    );
    assert_eq!(a.notices(&sender).await, Vec::<String>::new());

    reef.stop();
    a.stop().await;
    b.stop().await;
    c.stop().await;
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_prompt_a_stop_drops_is_told_to_its_sender_as_not_delivered() {
    let (cwd, data) = (scratch("cwd"), scratch("data"));
    let a = burrow("example-a").await;
    let b = burrow("example-b").await;
    let sender = a.agent(&cwd, "planner").await;
    let target = b.agent(&cwd, "Builder").await;
    let (reef, addr) = start_reef(&[&a, &b], &data).await;
    set_routing(addr, true).await;
    a.until_reach(Reach::On).await;

    // The target is in a slow turn, so the prompt waits behind it.
    let first = (b.client)
        .request(
            json!({"type": "send_agent_message_request", "agentId": target,
                        "text": "slow first", "messageId": "m-first"}),
        )
        .await
        .unwrap();
    assert_eq!(first["disposition"], "started", "{first}");
    let queued = a
        .tool(
            &sender,
            "send_agent_prompt",
            json!({"agentId": target, "prompt": "second", "activeTurnBehavior": "queue"}),
        )
        .await
        .unwrap();
    assert_eq!(queued["disposition"], "queued", "{queued}");

    // The agent is stopped before it gets to it: that prompt never ran, and
    // its sender is told so, not only that the agent was closed.
    b.tool(&target, "kill_agent", json!({"agentId": target}))
        .await
        .unwrap();
    let notices = a.until_notices(&sender, 1).await;
    let notice = &notices[0];
    assert!(
        notice.starts_with(&format!("Agent {target} (Builder) errored.")),
        "{notice}"
    );
    assert!(
        notice.contains("Your prompt was not delivered: The queued prompt was dropped"),
        "{notice}"
    );
    assert!(!notice.contains("was closed"), "{notice}");
    timeout(WAIT, async {
        while routes(&data) != json!([]) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the route is done once the notice is delivered");

    reef.stop();
    a.stop().await;
    b.stop().await;
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&data);
}

/// `burrow ls --fleet` against `burrow`: what it lists, or its error.
async fn ls_fleet(burrow: &Burrow) -> Result<(Vec<Value>, Vec<String>), String> {
    let endpoint = burrow_daemon::client::endpoint(&burrow.running.local_addr.to_string());
    let mut client = burrow_daemon::client::Client::connect(&endpoint).await?;
    let filter = burrow_daemon::commands::LabelFilter::default();
    burrow_daemon::commands::fleet(&mut client, false, &filter).await
}

/// As `burrow`'s `fleet_tool`: one of the fleet's tools on the agent's
/// host, through the Reef that routes for `burrow`.
async fn fleet_tool(
    burrow: &Burrow,
    tool: &str,
    target: &str,
    args: Value,
) -> Result<Value, String> {
    let endpoint = burrow_daemon::client::endpoint(&burrow.running.local_addr.to_string());
    let mut client = burrow_daemon::client::Client::connect(&endpoint).await?;
    burrow_daemon::commands::fleet_tool(&mut client, tool, target, args, Duration::from_secs(15))
        .await
        .map_err(|failed| failed.message)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_see_the_fleet_and_follow_up_on_other_hosts_once_routing_is_on() {
    let (cwd, data) = (scratch("cwd"), scratch("data"));
    let a = burrow("example-a").await;
    let b = burrow("example-b").await;
    let sender = a.agent(&cwd, "planner").await;
    let target = b.agent(&cwd, "Builder").await;
    let b_server = b.daemon.config.server_id.clone();

    // No Reef: this host alone, and said why.
    let hosts = a.tool(&sender, "list_hosts", json!({})).await.unwrap();
    assert_eq!(hosts["hosts"], json!([]), "{hosts}");
    assert!(
        hosts["note"]
            .as_str()
            .unwrap()
            .contains("none is connected"),
        "{hosts}"
    );
    let refused = (a.tool(&sender, "list_agents", json!({"fleet": true})).await).unwrap_err();
    assert!(refused.contains("none is connected"), "{refused}");
    let refused = ls_fleet(&a).await.unwrap_err();
    assert!(refused.contains("none is connected"), "{refused}");

    // A Reef with routing off: still out of reach, and why.
    let (reef, addr) = start_reef(&[&a, &b], &data).await;
    a.until_reach(Reach::Off).await;
    let refused = (a
        .tool(&sender, "get_agent_status", json!({"agentId": target}))
        .await)
        .unwrap_err();
    assert!(refused.contains("off in this fleet's Reef"), "{refused}");
    let refused = (a.tool(&sender, "list_agents", json!({"fleet": true})).await).unwrap_err();
    assert!(refused.contains("off in this fleet's Reef"), "{refused}");

    set_routing(addr, true).await;
    a.until_reach(Reach::On).await;
    b.until_reach(Reach::On).await;

    // The other host, in Paseo's shape.
    let hosts = a.tool(&sender, "list_hosts", json!({})).await.unwrap();
    let listed = hosts["hosts"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{hosts}");
    assert_eq!(listed[0]["label"], "example-b", "{hosts}");
    assert_eq!(listed[0]["serverId"], b_server.as_str(), "{hosts}");
    assert_eq!(listed[0]["status"], "online", "{hosts}");
    assert_eq!(listed[0]["reachable"], true, "{hosts}");

    // Every host's agents, each with its host; the CLI's too. The list is
    // compact, so the agents are matched by their shortId, which is what it
    // gives them.
    let all = (a.tool(&sender, "list_agents", json!({"fleet": true})).await).unwrap();
    let short_of = |id: &str| id.chars().take(7).collect::<String>();
    let host_of = |agents: &Value, id: &str| {
        (agents.as_array().unwrap().iter())
            .find(|x| x["shortId"] == short_of(id))
            .map(|x| x["host"].as_str().unwrap().to_string())
    };
    assert_eq!(
        host_of(&all["agents"], &target).as_deref(),
        Some("example-b"),
        "{all}"
    );
    assert_eq!(
        host_of(&all["agents"], &sender).as_deref(),
        Some("example-a"),
        "{all}"
    );
    assert!(
        (all["hosts"].as_array().unwrap().iter()).all(|h| h["answered"] == true),
        "{all}"
    );
    let (listed, missing) = ls_fleet(&a).await.unwrap();
    assert!(missing.is_empty(), "{missing:?}");
    let shown = burrow_daemon::commands::list(
        &listed,
        &Default::default(),
        time::OffsetDateTime::now_utc(),
    );
    assert!(shown.starts_with("ID"), "{shown}");
    assert!(shown.contains("HOST"), "{shown}");
    assert!(shown.contains("example-b"), "{shown}");
    assert!(shown.contains("Builder"), "{shown}");

    // One host's, by its server id.
    let theirs = (a
        .tool(&sender, "list_agents", json!({"serverId": b_server}))
        .await)
        .unwrap();
    let shorts: Vec<&str> = (theirs["agents"].as_array().unwrap().iter())
        .map(|x| x["shortId"].as_str().unwrap())
        .collect();
    assert_eq!(shorts, [short_of(&target)], "{theirs}");
    assert_eq!(theirs["serverId"], b_server.as_str(), "{theirs}");
    // Or by its name, in any case, as a Burrow takes its own.
    let theirs = (a
        .tool(&sender, "list_agents", json!({"serverId": "EXAMPLE-B"}))
        .await)
        .unwrap();
    assert_eq!(theirs["host"], "example-b", "{theirs}");
    assert!(
        (theirs["agents"].as_array().unwrap().iter()).any(|x| x["shortId"] == short_of(&target)),
        "{theirs}"
    );

    // Follow-ups by id, with nothing said of the host.
    let status = (a
        .tool(&sender, "get_agent_status", json!({"agentId": target}))
        .await)
        .unwrap();
    assert_eq!(status["status"], "idle", "{status}");
    assert_eq!(status["host"], "example-b", "{status}");
    a.tool(
        &sender,
        "send_agent_prompt",
        json!({"agentId": target, "prompt": "count the crates"}),
    )
    .await
    .unwrap();
    a.until_notices(&sender, 1).await;
    let activity = (a
        .tool(&sender, "get_agent_activity", json!({"agentId": "builder"}))
        .await)
        .unwrap();
    let content = activity["content"].as_str().unwrap();
    assert!(content.contains("count the crates"), "{activity}");
    assert_eq!(activity["host"], "example-b", "{activity}");
    let cancel = (a
        .tool(&sender, "cancel_agent", json!({"agentId": target}))
        .await)
        .unwrap();
    assert_eq!(cancel["success"], false, "nothing was running: {cancel}");

    // What does not reach other hosts says what does.
    let refused = a
        .tool(
            &sender,
            "create_agent",
            json!({"serverId": b_server, "title": "x", "provider": "echo", "initialPrompt": "x"}),
        )
        .await
        .unwrap_err();
    assert!(refused.contains("works on this host only"), "{refused}");
    let refused = a
        .tool(
            &sender,
            "get_agent_status",
            json!({"agentId": "x", "serverId": "srv_nowhere"}),
        )
        .await
        .unwrap_err();
    assert!(refused.contains("not one of this fleet's"), "{refused}");
    let refused = (a
        .tool(
            &sender,
            "get_agent_status",
            json!({"agentId": "nobody-here"}),
        )
        .await)
        .unwrap_err();
    assert!(
        refused.contains("not found on this host or any other"),
        "{refused}"
    );

    // Off again: the list is gone with it.
    set_routing(addr, false).await;
    a.until_reach(Reach::Off).await;
    let refused = (a.tool(&sender, "list_agents", json!({"fleet": true})).await).unwrap_err();
    assert!(refused.contains("off in this fleet's Reef"), "{refused}");

    reef.stop();
    a.stop().await;
    b.stop().await;
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fleets_list_is_compact_and_verbose_gives_the_full_snapshots() {
    let (cwd, data) = (scratch("cwd"), scratch("data"));
    let a = burrow("example-a").await;
    let b = burrow("example-b").await;
    let (reef, addr) = start_reef(&[&a, &b], &data).await;
    set_routing(addr, true).await;
    a.until_reach(Reach::On).await;
    b.until_reach(Reach::On).await;
    let sender = a.agent(&cwd, "planner").await;
    let builder = b
        .agent_with(
            &cwd,
            "Builder",
            json!({
                "team": "core",
                "paseo.open-agent-tab.cid_123": "true",
                "crusteo.labels": "v1:lbl_1, lbl_2",
                "paseo.parent-agent-id": "agt_1",
            }),
        )
        .await;

    // Compact by default: what an agent needs to find and choose it, with
    // its host and its own human labels, and no other software's state.
    let all = (a.tool(&sender, "list_agents", json!({"fleet": true})).await).unwrap();
    let listed = (all["agents"].as_array().unwrap().iter())
        .find(|x| x["shortId"] == builder.chars().take(7).collect::<String>())
        .cloned()
        .unwrap();
    assert_eq!(
        listed,
        json!({"shortId": builder.chars().take(7).collect::<String>(),
               "host": "example-b", "title": "Builder",
               "status": "idle", "model": "echo", "labels": {"team": "core"},
               "updated": "now"})
    );
    // No labels of its own: the field is left out, and its host is its own.
    let planner = (all["agents"].as_array().unwrap().iter())
        .find(|x| x["shortId"] == sender.chars().take(7).collect::<String>())
        .cloned()
        .unwrap();
    assert_eq!(
        planner["host"],
        "example-a",
        "{}",
        serde_json::to_string(&all).unwrap()
    );
    assert!(planner.get("labels").is_none(), "{planner}");
    // No full-snapshot fields at all, the full id included: the shortId is
    // what addresses the agent, everywhere it is taken.
    for field in ["id", "updatedAt", "createdAt", "cwd", "serverId"] {
        assert!(!listed.as_object().unwrap().contains_key(field), "{listed}");
        assert!(
            !planner.as_object().unwrap().contains_key(field),
            "{planner}"
        );
    }
    assert!(
        (all["hosts"].as_array().unwrap().iter()).all(|h| h["answered"] == true),
        "{all}"
    );

    // verbose: true is the full snapshot, every label and timestamp in.
    let full = (a
        .tool(
            &sender,
            "list_agents",
            json!({"fleet": true, "verbose": true}),
        )
        .await)
        .unwrap();
    let snap = (full["agents"].as_array().unwrap().iter())
        .find(|x| x["id"] == builder.as_str())
        .cloned()
        .unwrap();
    assert_eq!(snap["labels"]["paseo.open-agent-tab.cid_123"], "true");
    assert_eq!(snap["labels"]["crusteo.labels"], "v1:lbl_1, lbl_2");
    assert_eq!(snap["labels"]["team"], "core");
    assert!(snap["updatedAt"].is_string(), "{snap}");
    assert!(snap["cwd"].is_string(), "{snap}");
    assert!(
        full["agents"].as_array().unwrap().len() == all["agents"].as_array().unwrap().len(),
        "the same agents, shaped differently"
    );
    // And the whole point: the same agents, fewer tokens.
    assert!(
        serde_json::to_string(&all).unwrap().len() < serde_json::to_string(&full).unwrap().len()
    );

    // A caller older than compact lists asks with no `verbose`, and still
    // reads full snapshots: hosts answer the Reef in full, and only the
    // Burrow that asked compacts, for its own agent.
    let endpoint = burrow_daemon::client::endpoint(&a.running.local_addr.to_string());
    let mut client = burrow_daemon::client::Client::connect(&endpoint)
        .await
        .unwrap();
    let reply = client
        .request(
            json!({"type": paseo_client::routing::FLEET_AGENTS_REQUEST, "args": {}}),
            Some(Duration::from_secs(20)),
        )
        .await
        .unwrap();
    let old = (reply["result"]["agents"].as_array().unwrap().iter())
        .find(|x| x["id"] == builder.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("{reply}"));
    assert!(old["updatedAt"].is_string(), "{old}");
    assert_eq!(old["host"], "example-b", "{old}");

    reef.stop();
    a.stop().await;
    b.stop().await;
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cli_reads_and_waits_on_other_hosts_by_name_once_routing_is_on() {
    let (cwd, data) = (scratch("cwd"), scratch("data"));
    let a = burrow("example-a").await;
    let b = burrow("example-b").await;
    let (reef, addr) = start_reef(&[&a, &b], &data).await;
    let builder = b.agent(&cwd, "Builder").await;

    // Routing off: the fleet cannot be asked, and the error says so and
    // what to do, not merely "no agent matches".
    set_routing(addr, false).await;
    a.until_reach(Reach::Off).await;
    let refused = fleet_tool(&a, "get_agent_activity", "builder", json!({}))
        .await
        .unwrap_err();
    assert!(refused.contains("off in this fleet's Reef"), "{refused}");
    assert!(refused.contains("give the host"), "{refused}");

    // On: a turn for it, then the wait, by a prefix of its id: the answer
    // comes shaped as a local wait's, and says where it ran.
    set_routing(addr, true).await;
    a.until_reach(Reach::On).await;
    b.until_reach(Reach::On).await;
    (b.client)
        .request(json!({"type": "send_agent_message_request",
                        "agentId": builder, "text": "hello from the test",
                        "activeTurnBehavior": "auto", "messageId": "msg-cli-test-1"}))
        .await
        .unwrap();
    let done = fleet_tool(&a, "wait_for_finish", &builder[..7], json!({}))
        .await
        .unwrap();
    assert_eq!(done["host"], "example-b", "{done}");
    assert_eq!(done["status"], "idle", "{done}");
    assert!(
        done["lastMessage"]
            .as_str()
            .unwrap_or("")
            .contains("hello from the test"),
        "the echo of the prompt: {done}"
    );

    // And the activity, read on the other host by the agent's name, any
    // case: its turn, as the agent tool shows it.
    let activity = fleet_tool(&a, "get_agent_activity", "BUILDER", json!({"limit": 10}))
        .await
        .unwrap();
    assert_eq!(activity["host"], "example-b", "{activity}");
    assert!(
        activity["content"]
            .as_str()
            .unwrap()
            .contains("hello from the test"),
        "{activity}"
    );

    // And the inspect's source: the fleet's list finds it by name, with its
    // host, and the snapshot is enough to show.
    let (listed, missing) = ls_fleet(&a).await.unwrap();
    assert!(missing.is_empty(), "{missing:?}");
    let found = burrow_daemon::commands::find(&listed, "builder").unwrap();
    assert_eq!(found["id"], builder, "{found}");
    assert_eq!(found["host"], "example-b", "{found}");
    let shown = burrow_daemon::commands::inspect(found);
    assert!(shown.contains("example-b"), "{shown}");
    // A name no host has is said so, fleet-wide.
    let absent = fleet_tool(&a, "get_agent_activity", "Nobody", json!({}))
        .await
        .unwrap_err();
    assert!(absent.contains("Nobody"), "{absent}");

    reef.stop();
    a.stop().await;
    b.stop().await;
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&data);
}
