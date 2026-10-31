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
    let daemon = Daemon::open(Config::ephemeral()).unwrap();
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
        self.daemon.close();
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
    let url = h.url();
    let (as_agent, top_level, strange) = tokio::task::spawn_blocking(move || {
        (
            rpc(&url, Some(&token), "tools/list", json!({})),
            rpc(&url, None, "tools/list", json!({})),
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
    assert!(strange["error"].is_object(), "{strange}");
    h.stop().await;
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
    assert_eq!(sent["deliveryStatus"], "queued");
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
        .tool(None, "get_agent_status", json!({"agentId": child}))
        .await
        .unwrap();
    assert_eq!(status["snapshot"]["id"], child.as_str());

    // Another host is refused: there are none.
    let elsewhere = h
        .tool(
            token.clone(),
            "list_agents",
            json!({"serverId": "srv_somewhere"}),
        )
        .await
        .unwrap_err();
    assert!(elsewhere.contains("is not registered"), "{elsewhere}");
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
            None,
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
            None,
            "rename_workspace",
            json!({"workspaceId": workspace, "title": "Renamed"}),
        )
        .await
        .unwrap();
    assert_eq!(renamed["title"], "Renamed");
    let missing = h
        .tool(
            None,
            "create_workspace",
            json!({"isolation": "local", "path": "/no/such/dir"}),
        )
        .await
        .unwrap_err();
    assert_eq!(missing, "Directory not found: /no/such/dir");
    let archived = h
        .tool(None, "archive_workspace", json!({"workspaceId": workspace}))
        .await
        .unwrap();
    assert_eq!(archived["archivedAgentIds"], json!([]));
    let unknown = h.tool(None, "list_terminals", json!({})).await.unwrap_err();
    assert_eq!(unknown, "Paseo tool not found: list_terminals");
    h.stop().await;
}
