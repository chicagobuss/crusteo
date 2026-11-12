//! The agent tools (MCP) end to end: what an agent's `paseo` server lists
//! is Paseo 0.14's catalogue, tokens decide who calls, and the tools do
//! what agents rely on, over real HTTP with the echo provider.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::{Config, Daemon, Running, start};
use burrow_provider::echo::EchoProvider;
use paseo_client::{Client, ConnState, Event};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(15);

struct Harness {
    running: Running,
    daemon: Arc<Daemon>,
    client: Client,
    cwd: PathBuf,
}

async fn harness() -> Harness {
    harness_with(Config::ephemeral()).await
}

async fn harness_with(config: Config) -> Harness {
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
    let cwd = std::env::temp_dir().join(format!("burrow-mcp-{}", burrow_daemon::ids::agent_id()));
    std::fs::create_dir_all(&cwd).unwrap();
    Harness {
        running,
        daemon,
        client,
        cwd,
    }
}

/// One JSON-RPC call to the tools server, as `token`'s agent or none.
fn rpc(url: &str, token: Option<&str>, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let mut request = ureq::post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    if let Some(token) = token {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let mut response = request.send(body.to_string()).expect("the server answers");
    let text = response.body_mut().read_to_string().unwrap();
    // JSON, or one SSE event.
    let json = (text.lines())
        .find_map(|line| line.strip_prefix("data:"))
        .unwrap_or(&text);
    serde_json::from_str(json.trim()).unwrap_or_else(|_| panic!("not JSON-RPC: {text}"))
}

impl Harness {
    /// The token of a client that is not an agent: what Burrow keeps in its
    /// `mcp-token` file.
    fn top(&self) -> Option<String> {
        Some(
            self.daemon
                .mcp
                .top_level_token()
                .expect("a token")
                .to_string(),
        )
    }

    fn url(&self) -> String {
        self.daemon
            .mcp
            .url()
            .expect("the tools server listens")
            .to_string()
    }

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

    /// Calls a tool off the async runtime; the result's structured content,
    /// or its error text.
    async fn tool(&self, token: Option<String>, name: &str, args: Value) -> Result<Value, String> {
        let url = self.url();
        let name = name.to_string();
        let reply = tokio::task::spawn_blocking(move || {
            rpc(
                &url,
                token.as_deref(),
                "tools/call",
                json!({"name": name, "arguments": args}),
            )
        })
        .await
        .unwrap();
        let result = &reply["result"];
        if result["isError"] == true {
            return Err(result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string());
        }
        assert!(
            result["content"][0]["text"].is_string(),
            "the model sees text too: {reply}"
        );
        Ok(result["structuredContent"].clone())
    }

    async fn timeline(&self, agent: &str) -> Vec<Value> {
        let page = self
            .request(
                json!({"type": "fetch_agent_timeline_request", "agentId": agent,
                "direction": "tail", "limit": 50, "projection": "canonical"}),
            )
            .await;
        (page["entries"].as_array().unwrap().iter())
            .map(|e| e["item"].clone())
            .collect()
    }

    async fn stop(self) {
        drop(self.client);
        self.running.shutdown().await;
        self.daemon.close().unwrap();
        let _ = std::fs::remove_dir_all(&self.cwd);
    }
}

fn paseo(catalogue: &str) -> Vec<Value> {
    let path = format!("{}/src/mcp/{catalogue}", env!("CARGO_MANIFEST_DIR"));
    let listed: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    listed["tools"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_see_paseos_own_catalogue_and_a_strange_token_sees_nothing() {
    let h = harness().await;
    let agent = h.agent().await;
    let token = h.daemon.mcp.token(&agent);
    let top = h.top().unwrap();
    let url = h.url();
    let (as_agent, top_level, strange) = tokio::task::spawn_blocking(move || {
        (
            rpc(&url, Some(&token), "tools/list", json!({})),
            rpc(&url, Some(&top), "tools/list", json!({})),
            rpc(&url, Some("made-up"), "tools/list", json!({})),
        )
    })
    .await
    .unwrap();
    for (reply, catalogue) in [
        (&as_agent, "paseo-tools-agent.json"),
        (&top_level, "paseo-tools-top-level.json"),
    ] {
        let served = reply["result"]["tools"].as_array().expect("tools");
        assert_eq!(served.len(), burrow_daemon::mcp::tools::NAMES.len());
        let paseo = paseo(catalogue);
        for tool in served {
            let theirs = (paseo.iter())
                .find(|t| t["name"] == tool["name"])
                .unwrap_or_else(|| panic!("{} is not Paseo's", tool["name"]));
            // Paseo's, but for what Burrow does more of.
            let theirs = burrow_daemon::mcp::burrows_own(theirs.clone());
            for field in ["title", "description", "inputSchema"] {
                assert_eq!(tool[field], theirs[field], "{} {field}", tool["name"]);
            }
        }
    }
    // Agents are told to notify their caller by default; other clients not.
    let notify = |reply: &Value| {
        (reply["result"]["tools"].as_array().unwrap().iter())
            .find(|t| t["name"] == "create_agent")
            .map(|t| t["inputSchema"]["properties"]["notifyOnFinish"]["default"].clone())
            .unwrap()
    };
    assert_eq!(notify(&as_agent), true);
    assert_eq!(notify(&top_level), false);
    // And they can queue behind a running turn.
    let send = (as_agent["result"]["tools"].as_array().unwrap().iter())
        .find(|t| t["name"] == "send_agent_prompt")
        .unwrap();
    let behaviors = &send["inputSchema"]["properties"]["activeTurnBehavior"]["enum"];
    assert_eq!(*behaviors, json!(["auto", "interrupt", "queue", "steer"]));
    assert!(strange["error"].is_object(), "{strange}");
    h.stop().await;
}

/// A call as `rpc` makes it, but the answer is its JSON-RPC error's message
/// (empty when there is none).
fn refusal(reply: &Value) -> String {
    reply["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_is_not_an_agent_needs_the_token_burrow_keeps() {
    let h = harness().await;
    let agent = h.agent().await;
    let (url, top, own) = (h.url(), h.top().unwrap(), h.daemon.mcp.token(&agent));
    let call = json!({"name": "list_hosts", "arguments": {}});
    let replies = tokio::task::spawn_blocking(move || {
        let ask = |token: Option<&str>, method: &str, params: &Value| {
            rpc(&url, token, method, params.clone())
        };
        [
            // No token: neither the list of tools nor a call.
            ask(None, "tools/list", &json!({})),
            ask(None, "tools/call", &call),
            // A token Burrow never issued.
            ask(Some("made-up"), "tools/call", &call),
            // The one it keeps, and an agent's own.
            ask(Some(&top), "tools/list", &json!({})),
            ask(Some(&top), "tools/call", &call),
            ask(Some(&own), "tools/call", &call),
        ]
    })
    .await
    .unwrap();
    for (reply, what) in replies[..3].iter().zip(["list", "call", "made up"]) {
        assert!(reply["result"].is_null(), "{what}: {reply}");
    }
    let none = refusal(&replies[0]);
    assert!(
        none.contains("needs a token") && none.contains("mcp-token"),
        "{none}"
    );
    assert_eq!(refusal(&replies[1]), none);
    assert_eq!(refusal(&replies[2]), "this token was not issued by Burrow");
    assert_eq!(
        replies[3]["result"]["tools"].as_array().unwrap().len(),
        burrow_daemon::mcp::tools::NAMES.len()
    );
    for reply in &replies[4..] {
        assert_ne!(reply["result"]["isError"], true, "{reply}");
        assert_eq!(
            reply["result"]["structuredContent"]["self"]["serverId"],
            h.daemon.config.server_id.as_str(),
            "{reply}"
        );
    }
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burrow_can_be_set_to_serve_a_client_with_no_token_as_it_once_did() {
    let h = harness_with(Config {
        mcp_tokenless: true,
        ..Config::ephemeral()
    })
    .await;
    let agent = h.agent().await;
    let (url, own) = (h.url(), h.daemon.mcp.token(&agent));
    let (none, wrong, agents) = tokio::task::spawn_blocking(move || {
        (
            rpc(&url, None, "tools/list", json!({})),
            rpc(&url, Some("made-up"), "tools/list", json!({})),
            rpc(&url, Some(&own), "tools/list", json!({})),
        )
    })
    .await
    .unwrap();
    let count = |reply: &Value| reply["result"]["tools"].as_array().map(Vec::len);
    // The top-level tools for a client with none; a wrong token is still refused.
    assert_eq!(count(&none), Some(burrow_daemon::mcp::tools::NAMES.len()));
    assert_eq!(refusal(&wrong), "this token was not issued by Burrow");
    assert_eq!(count(&agents), count(&none));
    let hosts = h.tool(None, "list_hosts", json!({})).await.unwrap();
    assert_eq!(
        hosts["self"]["serverId"],
        h.daemon.config.server_id.as_str()
    );
    h.stop().await;
}

fn state_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "burrow-mcp-{name}-{}",
        burrow_daemon::ids::agent_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_token_is_in_the_state_directory_for_clients_to_read_and_the_bridge_sends_it() {
    let home = state_dir("home");
    let config = || Config {
        home: Some(home.clone()),
        ..Config::ephemeral()
    };
    let h = harness_with(config()).await;
    let file = std::fs::read_to_string(home.join("mcp-token")).unwrap();
    assert_eq!(file, h.top().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.join("mcp-token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "readable by its user alone");
    }

    // `burrow mcp-bridge` given the address and nothing else reads it.
    let url = h.url();
    let server = burrow_daemon::mcp::bridge::Server::resolve(Some(url.clone()), None, Some(&home))
        .expect("an address makes a server");
    assert_eq!(server.token, file);
    let line = r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#.to_string();
    let (answered, unauthorized) = {
        let (url, token, line) = (url.clone(), server.token.clone(), line.clone());
        tokio::task::spawn_blocking(move || {
            (
                burrow_daemon::mcp::bridge::forward(&url, &token, &line),
                burrow_daemon::mcp::bridge::forward(&url, "", &line),
            )
        })
        .await
        .unwrap()
    };
    let answered: Value = serde_json::from_str(&answered[0]).unwrap();
    assert_eq!(
        answered["result"]["tools"].as_array().unwrap().len(),
        burrow_daemon::mcp::tools::NAMES.len()
    );
    // With none to send, Burrow says what it wants.
    let unauthorized: Value = serde_json::from_str(&unauthorized[0]).unwrap();
    assert!(
        refusal(&unauthorized).contains("mcp-token"),
        "{unauthorized}"
    );
    h.stop().await;

    // A Burrow started again keeps the token, so what read it keeps working.
    let again = harness_with(config()).await;
    assert_eq!(again.top().unwrap(), file);
    let reply = {
        let (url, token) = (again.url(), file.clone());
        tokio::task::spawn_blocking(move || rpc(&url, Some(&token), "tools/list", json!({})))
            .await
            .unwrap()
    };
    assert!(reply["result"]["tools"].is_array(), "{reply}");
    again.stop().await;
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_makes_a_child_in_its_workspace_and_hears_when_it_finishes() {
    let h = harness().await;
    let parent = h.agent().await;
    let token = Some(h.daemon.mcp.token(&parent));
    let made = h
        .tool(
            token.clone(),
            "create_agent",
            json!({"title": "Count the files",
            "provider": "echo/echo-1", "initialPrompt": "count them"}),
        )
        .await
        .unwrap();
    let child = made["agentId"].as_str().unwrap().to_string();
    assert_eq!(made["type"], "echo");
    assert_eq!(made["serverId"], h.daemon.config.server_id.as_str());
    let parent_snapshot = h
        .request(json!({"type": "fetch_agent_request", "agentId": parent}))
        .await;
    assert_eq!(made["workspaceId"], parent_snapshot["agent"]["workspaceId"]);
    assert!(
        made["guidance"]
            .as_str()
            .unwrap()
            .starts_with("You will get notified")
    );
    let child_snapshot = h
        .request(json!({"type": "fetch_agent_request", "agentId": child}))
        .await;
    assert_eq!(
        child_snapshot["agent"]["labels"]["paseo.parent-agent-id"],
        parent.as_str()
    );

    // The parent hears of it as an agent notification, not a prompt.
    let notice = timeout(WAIT, async {
        loop {
            let items = h.timeline(&parent).await;
            if let Some(item) = items
                .iter()
                .find(|i| i["agentMessage"]["event"] == "finished")
            {
                return item.clone();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("a finish notice");
    assert_eq!(notice["agentMessage"]["sender"]["id"], child.as_str());
    let text = notice["agentMessage"]["text"].as_str().unwrap();
    assert!(
        text.starts_with(&format!("Agent {child} (Count the files) finished.")),
        "{text}"
    );
    assert!(
        text.contains(&format!("Server: {}", h.daemon.config.server_id)),
        "{text}"
    );
    // The child's first prompt came from its parent, in Paseo's envelope,
    // which the echo provider echoes back whole.
    assert!(
        text.contains("<agent-response>\necho: <paseo-system"),
        "{text}"
    );
    assert!(text.contains(&format!(
        "kind=\"agent-message\" source-agent-id=\"{parent}\""
    )));

    // A prompt from the parent arrives from it.
    let sent = h
        .tool(
            token.clone(),
            "send_agent_prompt",
            json!({"agentId": child, "prompt": "again",
            "notifyOnFinish": false}),
        )
        .await
        .unwrap();
    assert_eq!(sent["deliveryStatus"], "delivered", "{sent}");
    assert_eq!(sent["disposition"], "started", "{sent}");
    assert_eq!(sent["target"]["agentId"], child.as_str(), "{sent}");
    assert_eq!(sent["target"]["title"], "Count the files", "{sent}");
    timeout(WAIT, async {
        loop {
            let items = h.timeline(&child).await;
            if items.iter().any(|i| {
                i["agentMessage"]["event"] == "message"
                    && i["agentMessage"]["sender"]["id"] == parent.as_str()
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the message from the parent");

    let listed = h
        .tool(token.clone(), "list_agents", json!({}))
        .await
        .unwrap();
    let ids: Vec<&str> = (listed["agents"].as_array().unwrap().iter())
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&child.as_str()) && ids.contains(&parent.as_str()));

    // Burrow's own filter: the agents of workspaces with a label.
    let workspace = parent_snapshot["agent"]["workspaceId"].as_str().unwrap();
    h.request(fleet::requests::set_label(
        workspace, "Billing", "teal", true,
    ))
    .await;
    let labelled = h
        .tool(
            token.clone(),
            "list_agents",
            json!({"workspaceLabels": ["billing"]}),
        )
        .await
        .unwrap();
    let labelled = labelled["agents"].as_array().unwrap();
    assert_eq!(labelled.len(), 2, "{labelled:?}");
    assert!(
        labelled
            .iter()
            .all(|a| a["workspaceLabels"] == json!(["Billing"]))
    );
    let none = h
        .tool(
            token.clone(),
            "list_agents",
            json!({"workspaceLabels": ["personal"]}),
        )
        .await
        .unwrap();
    assert_eq!(none["agents"], json!([]));
    let activity = h
        .tool(
            token.clone(),
            "get_agent_activity",
            json!({"agentId": child}),
        )
        .await
        .unwrap();
    let content = activity["content"].as_str().unwrap();
    assert!(content.starts_with("Showing all "), "{content}");
    assert!(
        content.contains("count them") && content.contains("again"),
        "{content}"
    );
    assert!(content.contains("echo: <paseo-system"), "{content}");
    let status = h
        .tool(h.top(), "get_agent_status", json!({"agentId": child}))
        .await
        .unwrap();
    assert_eq!(status["snapshot"]["id"], child.as_str());

    // Another host is out of reach without a Reef, and the caller is told so.
    let elsewhere = h
        .tool(
            token.clone(),
            "list_agents",
            json!({"serverId": "srv_somewhere"}),
        )
        .await
        .unwrap_err();
    assert!(
        elsewhere.contains("Host srv_somewhere is not this host"),
        "{elsewhere}"
    );
    assert!(elsewhere.contains("none is connected"), "{elsewhere}");
    h.stop().await;
}

/// The echo provider as one that cannot be steered, under its own id.
struct Unsteerable(EchoProvider);

impl burrow_provider::Provider for Unsteerable {
    fn info(&self) -> burrow_provider::ProviderInfo {
        let mut info = self.0.info();
        info.id = "unsteerable".into();
        info.capabilities.steer = false;
        info
    }

    fn start(
        &self,
        spec: burrow_provider::SessionSpec,
    ) -> Result<burrow_provider::Session, burrow_provider::ProviderError> {
        self.0.start(spec)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_for_a_busy_unsteerable_agent_waits_and_its_notice_is_its_own() {
    let h = harness().await;
    h.daemon.agents.register(Arc::new(Unsteerable(EchoProvider {
        chunk_delay: Duration::from_millis(2),
        slow_delay: Duration::from_millis(1500),
    })));
    let sender = h.agent().await;
    let token = Some(h.daemon.mcp.token(&sender));
    let busy = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "unsteerable", "cwd": h.cwd.to_string_lossy()}}))
        .await["agent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let started = h
        .request(
            json!({"type": "send_agent_message_request", "agentId": busy,
            "text": "slow first", "messageId": "m-first"}),
        )
        .await;
    assert_eq!(started["disposition"], "started", "{started}");

    // Not cut off: it waits behind the turn, and the receipt says so.
    let sent = h
        .tool(
            token.clone(),
            "send_agent_prompt",
            json!({"agentId": busy, "prompt": "second"}),
        )
        .await
        .unwrap();
    assert_eq!(sent["deliveryStatus"], "queued", "{sent}");
    assert_eq!(sent["disposition"], "queued", "{sent}");
    assert_eq!(sent["queuePosition"], 1, "{sent}");
    assert_eq!(sent["target"]["agentId"], busy.as_str(), "{sent}");
    let guidance = sent["guidance"].as_str().unwrap();
    assert!(
        guidance.contains("waits behind it (next in line)"),
        "{guidance}"
    );
    assert!(guidance.contains("You will get notified"), "{guidance}");

    // The notice is of the turn the message started, not the one before.
    let notice = timeout(WAIT, async {
        loop {
            let items = h.timeline(&sender).await;
            if let Some(item) = items
                .iter()
                .find(|i| i["agentMessage"]["event"] == "finished")
            {
                return item.clone();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("a finish notice");
    let text = notice["agentMessage"]["text"].as_str().unwrap();
    assert!(text.contains("second\n</paseo-system>"), "{text}");
    assert!(!text.contains("echo: first"), "{text}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeats_belong_to_their_agent_and_workspaces_are_made_and_renamed() {
    let h = harness().await;
    let (one, other) = (h.agent().await, h.agent().await);
    let (one_token, other_token) = (
        Some(h.daemon.mcp.token(&one)),
        Some(h.daemon.mcp.token(&other)),
    );
    let beat = h
        .tool(
            one_token.clone(),
            "create_heartbeat",
            json!({"prompt": "check in",
            "cron": "*/15 * * * *", "timezone": "America/Chicago", "name": "watch"}),
        )
        .await
        .unwrap();
    let id = beat["id"].as_str().unwrap().to_string();
    assert_eq!(beat["target"], json!({"type": "agent", "agentId": one}));
    // Registering it again refreshes it: still one.
    let again = h
        .tool(
            one_token.clone(),
            "create_heartbeat",
            json!({"prompt": "check in, again",
            "cron": "*/30 * * * *", "name": "watch"}),
        )
        .await
        .unwrap();
    assert_eq!(again["id"], id.as_str());
    assert_eq!(again["cadence"]["timezone"], "America/Chicago");
    assert_eq!(h.daemon.schedules.list().len(), 1);
    let theirs = h
        .tool(other_token, "delete_heartbeat", json!({"id": id}))
        .await
        .unwrap_err();
    assert!(theirs.contains("does not belong"), "{theirs}");
    let listed = h
        .tool(one_token.clone(), "list_schedules", json!({}))
        .await
        .unwrap();
    assert_eq!(
        listed["schedules"],
        json!([]),
        "heartbeats are not schedules to these tools"
    );
    let schedule = h
        .tool(
            one_token.clone(),
            "create_schedule",
            json!({"prompt": "nightly", "cron": "0 3 * * *"}),
        )
        .await
        .unwrap();
    assert_eq!(
        schedule["target"]["config"]["provider"], "echo",
        "the caller's own provider"
    );
    h.tool(one_token.clone(), "delete_heartbeat", json!({"id": id}))
        .await
        .unwrap();

    let made = h
        .tool(
            h.top(),
            "create_workspace",
            json!({"isolation": "local",
            "path": h.cwd.to_string_lossy(), "title": "Scratch"}),
        )
        .await
        .unwrap();
    assert_eq!(made["isolation"], "local");
    assert_eq!(made["title"], "Scratch");
    let workspace = made["workspaceId"].as_str().unwrap().to_string();
    let renamed = h
        .tool(
            h.top(),
            "rename_workspace",
            json!({"workspaceId": workspace, "title": "Renamed"}),
        )
        .await
        .unwrap();
    assert_eq!(renamed["title"], "Renamed");
    let missing = h
        .tool(
            h.top(),
            "create_workspace",
            json!({"isolation": "local", "path": "/no/such/dir"}),
        )
        .await
        .unwrap_err();
    assert_eq!(missing, "Directory not found: /no/such/dir");
    let archived = h
        .tool(
            h.top(),
            "archive_workspace",
            json!({"workspaceId": workspace}),
        )
        .await
        .unwrap();
    assert_eq!(archived["archivedAgentIds"], json!([]));
    let unknown = h
        .tool(h.top(), "list_terminals", json!({}))
        .await
        .unwrap_err();
    assert_eq!(unknown, "Paseo tool not found: list_terminals");
    h.stop().await;
}
