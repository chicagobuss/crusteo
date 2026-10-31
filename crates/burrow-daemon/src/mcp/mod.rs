//! Paseo's agent tools over MCP: the `paseo` MCP server every Claude and
//! Codex agent gets (`mcp__paseo__create_agent`, `send_agent_prompt`,
//! `create_heartbeat`, …), as Paseo's daemon gives its agents.
//!
//! - **The catalogue is Paseo's own**, captured from Paseo 0.14.0's
//!   `tools/list` (`paseo-tools-*.json`, Apache-2.0, Copyright Mohamed
//!   Boudra): names, titles, descriptions and input schemas are served as
//!   they are, so agents see what they would under Paseo. Agents get the
//!   agent-scoped catalogue, other clients the top-level one. Tools Burrow
//!   cannot do (terminals, workspace scripts, voice) are left out.
//! - **Only on loopback**, on a port of its own: agents run on this host,
//!   so the server is never on the LAN or a VPN, whatever Burrow's
//!   `--listen` says. rmcp checks `Host` against loopback too.
//! - **The caller is its token.** Each agent's server entry carries a
//!   bearer token of its own, which is what says who is calling, so an
//!   agent cannot act as another. (Paseo reads a `?callerAgentId=` anyone
//!   can write.) A request without one is a top-level client; one with a
//!   token Burrow did not issue is refused.
//! - **Results** carry `structuredContent` and, for the model, a text with
//!   the counts and ids of its lists and then the JSON, as Paseo's
//!   `addModelVisibleStructuredContent` writes it.
//!
//! The handlers are [`tools`]; finish notices are [`finish`].

pub mod bridge;
pub mod finish;
pub mod tools;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use burrow_provider::{McpServer, StdioCommand};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{debug, info};

use crate::daemon::Daemon;

/// The name agents know the server by.
pub const SERVER_NAME: &str = "paseo";

/// Paseo 0.14.0's `tools/list`, for agents and for other clients.
const AGENT_CATALOGUE: &str = include_str!("paseo-tools-agent.json");
const TOP_LEVEL_CATALOGUE: &str = include_str!("paseo-tools-top-level.json");

static AGENT_TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| served(AGENT_CATALOGUE));
static TOP_LEVEL_TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| served(TOP_LEVEL_CATALOGUE));

/// The tools of `catalogue` that Burrow has handlers for, in its order.
fn served(catalogue: &str) -> Vec<Tool> {
    let listed: Value = serde_json::from_str(catalogue).expect("the embedded catalogue parses");
    (listed["tools"].as_array().into_iter().flatten())
        .filter(|tool| (tool["name"].as_str()).is_some_and(|name| tools::NAMES.contains(&name)))
        .filter_map(|tool| serde_json::from_value(tool.clone()).ok())
        .collect()
}

/// The catalogue a caller sees.
pub fn catalogue(agent: bool) -> &'static [Tool] {
    if agent {
        &AGENT_TOOLS
    } else {
        &TOP_LEVEL_TOOLS
    }
}

/// The server's address and the tokens it issued.
#[derive(Default)]
pub struct Mcp {
    url: OnceLock<String>,
    /// Token to agent, and agent to token.
    tokens: Mutex<(HashMap<String, String>, HashMap<String, String>)>,
}

impl Mcp {
    /// `http://127.0.0.1:<port>/mcp`, once it listens.
    pub fn url(&self) -> Option<&str> {
        self.url.get().map(String::as_str)
    }

    /// The agent's token, made the first time it is asked for.
    pub fn token(&self, agent_id: &str) -> String {
        let mut tokens = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(token) = tokens.1.get(agent_id) {
            return token.clone();
        }
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        tokens.0.insert(token.clone(), agent_id.to_string());
        tokens.1.insert(agent_id.to_string(), token.clone());
        token
    }

    fn agent(&self, token: &str) -> Option<String> {
        let tokens = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        tokens.0.get(token).cloned()
    }
}

/// The MCP servers an agent's CLI gets: Burrow's own, as `paseo`, once it
/// listens.
pub fn servers_for(daemon: &Daemon, agent_id: &str) -> Vec<McpServer> {
    let Some(url) = daemon.mcp.url() else {
        return Vec::new();
    };
    let token = daemon.mcp.token(agent_id);
    // For agents that take only stdio servers: this binary's bridge.
    let stdio = std::env::current_exe().ok().map(|exe| StdioCommand {
        command: exe.to_string_lossy().into_owned(),
        args: vec!["mcp-bridge".into()],
        env: vec![
            ("BURROW_MCP_URL".into(), url.to_string()),
            ("BURROW_MCP_TOKEN".into(), token.clone()),
        ],
    });
    vec![McpServer {
        name: SERVER_NAME.into(),
        url: url.to_string(),
        headers: vec![("Authorization".into(), format!("Bearer {token}"))],
        stdio,
    }]
}

/// Who is calling, as the transport layer found it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Caller {
    /// No token: a client of its own, given the top-level tools.
    TopLevel,
    Agent(String),
    /// A token Burrow did not issue.
    Refused,
}

#[derive(Clone)]
struct Handler {
    daemon: Arc<Daemon>,
}

fn caller(context: &RequestContext<RoleServer>) -> Caller {
    (context.extensions.get::<http::request::Parts>())
        .and_then(|parts| parts.extensions.get::<Caller>().cloned())
        .unwrap_or(Caller::TopLevel)
}

fn refused() -> ErrorData {
    ErrorData::invalid_request("this token was not issued by Burrow", None)
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        // Paseo's server names itself so.
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agent-mcp", "2.0.0"))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let agent = match caller(&context) {
            Caller::Refused => return Err(refused()),
            Caller::Agent(_) => true,
            Caller::TopLevel => false,
        };
        let listed = serde_json::json!({"tools": catalogue(agent)});
        serde_json::from_value(listed).map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let caller = match caller(&context) {
            Caller::Refused => return Err(refused()),
            Caller::Agent(id) => Some(id),
            Caller::TopLevel => None,
        };
        let name = request.name.to_string();
        if !catalogue(caller.is_some()).iter().any(|t| t.name == name) {
            return Ok(result(Err(format!("Paseo tool not found: {name}"))).into());
        }
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        debug!(tool = name, caller = ?caller, "mcp tool call");
        let outcome = tools::call(&self.daemon, caller.as_deref(), &name, arguments).await;
        Ok(result(outcome).into())
    }
}

/// A tool's outcome as MCP returns it: an error as its text, a result as
/// structured content with Paseo's model-visible text.
fn result(outcome: Result<Value, String>) -> CallToolResult {
    match outcome {
        Ok(value) => {
            let mut result = CallToolResult::success(vec![ContentBlock::text(model_text(&value))]);
            result.structured_content = Some(value);
            result
        }
        Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]),
    }
}

/// Paseo's `formatStructuredContentForModel`: `<key>_count=N` (and
/// `<key>_ids=…` when every item has an id) for each list, then the JSON.
fn model_text(value: &Value) -> String {
    let json = serde_json::to_string_pretty(value).unwrap_or_default();
    let Value::Object(fields) = value else {
        return json;
    };
    let mut summary = Vec::new();
    for (key, items) in fields {
        let Some(items) = items.as_array() else {
            continue;
        };
        summary.push(format!("{key}_count={}", items.len()));
        let ids: Vec<&str> = (items.iter())
            .filter_map(|item| item.get("id").and_then(Value::as_str))
            .filter(|id| !id.is_empty())
            .collect();
        if !ids.is_empty() && ids.len() == items.len() {
            summary.push(format!("{key}_ids={}", ids.join(",")));
        }
    }
    if summary.is_empty() {
        json
    } else {
        format!("{}\n\n{json}", summary.join("\n"))
    }
}

/// Serves the tools on a loopback port of its own until shutdown. Its
/// address is [`Mcp::url`] from then on.
pub async fn serve(
    daemon: Arc<Daemon>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let _ = daemon.mcp.url.set(format!("http://{address}/mcp"));
    info!(%address, "agent tools (MCP) on http://{address}/mcp");
    let mut config = StreamableHttpServerConfig::default();
    // One request, one answer: tool calls carry no session.
    config.legacy_session_mode = false;
    config.json_response = true;
    let factory = {
        let daemon = daemon.clone();
        move || {
            Ok(Handler {
                daemon: daemon.clone(),
            })
        }
    };
    let service =
        StreamableHttpService::new(factory, Arc::new(NeverSessionManager::default()), config);
    Ok(tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = shutdown.changed() => return,
            };
            let Ok((stream, _)) = accepted else {
                continue;
            };
            let (daemon, service) = (daemon.clone(), service.clone());
            tokio::spawn(async move {
                let handle = hyper::service::service_fn(move |mut request| {
                    let caller = match bearer(request.headers()) {
                        None => Caller::TopLevel,
                        Some(token) => match daemon.mcp.agent(token) {
                            Some(agent) => Caller::Agent(agent),
                            None => Caller::Refused,
                        },
                    };
                    request.extensions_mut().insert(caller);
                    let mut service = service.clone();
                    async move { tower_service::Service::call(&mut service, request).await }
                });
                let io = hyper_util::rt::TokioIo::new(stream);
                if let Err(err) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, handle)
                    .await
                {
                    debug!(%err, "mcp connection ended");
                }
            });
        }
    }))
}

fn bearer(headers: &http::HeaderMap) -> Option<&str> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_sees_counts_and_ids_then_the_json() {
        let value = serde_json::json!({"workspaces": [{"id": "a"}, {"id": "b"}], "total": 2});
        let text = model_text(&value);
        assert!(
            text.starts_with("workspaces_count=2\nworkspaces_ids=a,b\n\n{"),
            "{text}"
        );
        assert_eq!(
            model_text(&serde_json::json!({"success": true})),
            "{\n  \"success\": true\n}"
        );
    }
}
