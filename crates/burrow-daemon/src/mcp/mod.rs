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
//!   can write.) A client that is not an agent, which gets the top-level
//!   tools, sends the token Burrow keeps in `<home>/mcp-token`
//!   ([`TOKEN_FILE`]), readable by the user alone; a request with no token,
//!   or one Burrow did not issue, is refused. (Before 0.2.0 a request with
//!   none was a top-level client, so any program on the host could create
//!   and prompt agents; `Config::mcp_tokenless` brings that back.)
//! - **Results** carry `structuredContent` and, for the model, a text with
//!   the counts and ids of its lists and then the JSON, as Paseo's
//!   `addModelVisibleStructuredContent` writes it.
//!
//! The handlers are [`tools`]; finish notices are [`finish`].

pub mod bridge;
pub mod finish;
pub mod tools;

use std::collections::HashMap;
use std::path::Path;
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
use tracing::{debug, info, warn};

use crate::daemon::Daemon;

/// The name agents know the server by.
pub const SERVER_NAME: &str = "paseo";

/// The file in Burrow's home with the token a top-level client sends.
pub const TOKEN_FILE: &str = "mcp-token";

/// Paseo 0.14.0's `tools/list`, for agents and for other clients.
const AGENT_CATALOGUE: &str = include_str!("paseo-tools-agent.json");
const TOP_LEVEL_CATALOGUE: &str = include_str!("paseo-tools-top-level.json");

static AGENT_TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| served(AGENT_CATALOGUE));
static TOP_LEVEL_TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| served(TOP_LEVEL_CATALOGUE));

/// The tools of `catalogue` that Burrow has handlers for, in its order,
/// with [`burrows_own`] changes.
fn served(catalogue: &str) -> Vec<Tool> {
    let listed: Value = serde_json::from_str(catalogue).expect("the embedded catalogue parses");
    (listed["tools"].as_array().into_iter().flatten())
        .filter(|tool| (tool["name"].as_str()).is_some_and(|name| tools::NAMES.contains(&name)))
        .map(|tool| burrows_own(tool.clone()))
        .filter_map(|tool| serde_json::from_value(tool).ok())
        .collect()
}

/// Where Burrow's tools do more than Paseo's, so say so: `send_agent_prompt`
/// can queue behind a running turn, and does by default for an agent whose
/// provider cannot be steered, rather than interrupting it.
pub fn burrows_own(mut tool: Value) -> Value {
    if tool["name"] == "send_agent_prompt" {
        tool["description"] = SEND_DESCRIPTION.into();
        let behavior = &mut tool["inputSchema"]["properties"]["activeTurnBehavior"];
        behavior["description"] = BEHAVIOR_DESCRIPTION.into();
        behavior["enum"] = serde_json::json!(["auto", "interrupt", "queue", "steer"]);
    }
    if tool["name"] == "list_agents" {
        tool["inputSchema"]["properties"]["workspaceLabels"] = serde_json::json!({
            "description": WORKSPACE_LABELS_DESCRIPTION,
            "type": "array",
            "items": {"type": "string", "minLength": 1},
        });
        tool["inputSchema"]["properties"]["fleet"] = serde_json::json!({
            "description": FLEET_DESCRIPTION,
            "type": "boolean",
        });
    }
    tool
}

const FLEET_DESCRIPTION: &str = "List every host's agents, through this host's Reef, each with its host and serverId; every directory unless cwd is given. Needs messaging between hosts turned on in the Reef. Any agent listed takes send_agent_prompt, get_agent_status, get_agent_activity, cancel_agent and respond_to_permission by its id, wherever it runs.";

const WORKSPACE_LABELS_DESCRIPTION: &str = "Only agents whose workspace has every one of these labels (names as the app shows them, any case). Without cwd, looks in every directory rather than the caller's. Each agent listed carries its workspaceLabels.";

const SEND_DESCRIPTION: &str = "Send a task. Agent callers get a receipt saying what happened: a new turn started, the prompt was steered into the running turn, or it is queued behind that turn (with its place in line). Completion reports include the target\u{2019}s last response by default. Paseo supplies caller identity.";
const BEHAVIOR_DESCRIPTION: &str = "What to do when the agent is mid-turn: \"auto\" joins the turn where its provider can take a message mid-turn and otherwise queues behind it; \"steer\" joins it, falling back to interrupt when unavailable; \"queue\" waits behind it; \"interrupt\" replaces it. Defaults to auto for agent callers and interrupt otherwise.";

/// An agent's activity as `get_agent_activity` shows it, one line or more
/// per timeline item: what `burrow logs` prints too.
pub fn activity_lines(items: &[&fleet::timeline::Item], cwd: &str) -> Vec<String> {
    tools::curate(items, cwd)
}

/// The catalogue a caller sees.
pub fn catalogue(agent: bool) -> &'static [Tool] {
    if agent {
        &AGENT_TOOLS
    } else {
        &TOP_LEVEL_TOOLS
    }
}

/// A new token: 256 random bits as hex.
fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// The server's address and the tokens it issued.
#[derive(Default)]
pub struct Mcp {
    url: OnceLock<String>,
    /// Token to agent, and agent to token.
    tokens: Mutex<(HashMap<String, String>, HashMap<String, String>)>,
    /// What a client that is not an agent sends; made when the server starts.
    top_level: OnceLock<String>,
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
        let token = new_token();
        tokens.0.insert(token.clone(), agent_id.to_string());
        tokens.1.insert(agent_id.to_string(), token.clone());
        token
    }

    /// The token a client that is not an agent sends, the one in
    /// [`TOKEN_FILE`], once the server has started.
    pub fn top_level_token(&self) -> Option<&str> {
        self.top_level.get().map(String::as_str)
    }

    fn agent(&self, token: &str) -> Option<String> {
        let tokens = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        tokens.0.get(token).cloned()
    }

    /// Whether `token` is [`Self::top_level_token`], compared in constant
    /// time.
    fn is_top_level(&self, token: &str) -> bool {
        self.top_level_token().is_some_and(|own| same(own, token))
    }
}

/// Whether two tokens are equal, in time that does not depend on where they
/// differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |different, (x, y)| different | (x ^ y))
            == 0
}

/// A token that is the whole of a token file, as Burrow writes one: long
/// enough to guess nothing, and plain enough to sit in a header.
fn valid_token(text: &str) -> bool {
    text.len() >= 32
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The top-level token of the Burrow with `home`: the one in its
/// [`TOKEN_FILE`] if that holds a good one, kept across restarts so a script
/// that read it keeps working, else a new one written there. The file is
/// readable by its user alone (on Unix; Windows keeps the profile's own
/// permissions). Without a home, or if the file cannot be written, the token
/// is only in memory: no client can have it, which is as safe as it gets.
pub fn load_top_level_token(home: Option<&Path>) -> String {
    let Some(path) = home.map(|home| home.join(TOKEN_FILE)) else {
        return new_token();
    };
    if let Ok(text) = std::fs::read_to_string(&path) {
        let text = text.trim();
        if valid_token(text) {
            // As Burrow wrote it, whatever has happened to the file since.
            if let Err(err) = restrict(&path) {
                warn!(path = %path.display(), %err, "cannot make the MCP token file private");
            }
            return text.to_string();
        }
    }
    let token = new_token();
    match write_private(&path, &token) {
        Ok(()) => info!(path = %path.display(), "wrote the MCP token for top-level clients"),
        Err(err) => warn!(
            path = %path.display(), %err,
            "cannot write the MCP token file; no client but agents can use the agent tools"
        ),
    }
    token
}

/// Writes `text` to `path` readable by the user alone from the first byte,
/// by writing a file beside it and renaming that over it.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("new");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    restrict(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

/// Mode 0600 on Unix; nothing on Windows.
fn restrict(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
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
    /// A client of its own, given the top-level tools: one that sent the
    /// token in [`TOKEN_FILE`], or any at all with `mcp_tokenless`.
    TopLevel,
    Agent(String),
    /// No token, and a Burrow that wants one.
    NoToken,
    /// A token Burrow did not issue.
    Refused,
}

#[derive(Clone)]
struct Handler {
    daemon: Arc<Daemon>,
}

/// Who a request is from. One the transport did not look at is nobody.
fn caller(context: &RequestContext<RoleServer>) -> Caller {
    (context.extensions.get::<http::request::Parts>())
        .and_then(|parts| parts.extensions.get::<Caller>().cloned())
        .unwrap_or(Caller::Refused)
}

/// Why a request is turned away: `None` for a caller that may use the tools.
fn turned_away(caller: &Caller) -> Option<ErrorData> {
    match caller {
        Caller::TopLevel | Caller::Agent(_) => None,
        Caller::NoToken => Some(ErrorData::invalid_request(
            format!(
                "this server needs a token: send the one Burrow keeps in {TOKEN_FILE} in its \
                 state directory as \"Authorization: Bearer <token>\""
            ),
            None,
        )),
        Caller::Refused => Some(ErrorData::invalid_request(
            "this token was not issued by Burrow",
            None,
        )),
    }
}

/// Who a request is from, given the `Authorization` header it came with.
fn identify(daemon: &Daemon, headers: &http::HeaderMap) -> Caller {
    match bearer(headers) {
        None if daemon.config.mcp_tokenless => Caller::TopLevel,
        None => Caller::NoToken,
        Some(token) => match daemon.mcp.agent(token) {
            Some(agent) => Caller::Agent(agent),
            None if daemon.mcp.is_top_level(token) => Caller::TopLevel,
            None => Caller::Refused,
        },
    }
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
        let caller = caller(&context);
        if let Some(error) = turned_away(&caller) {
            return Err(error);
        }
        let listed = serde_json::json!({"tools": catalogue(matches!(caller, Caller::Agent(_)))});
        serde_json::from_value(listed).map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let caller = caller(&context);
        if let Some(error) = turned_away(&caller) {
            return Err(error);
        }
        let caller = match caller {
            Caller::Agent(id) => Some(id),
            _ => None,
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
    // Before anything can connect: a request is told apart by it.
    let _ = (daemon.mcp.top_level).set(load_top_level_token(daemon.config.home.as_deref()));
    let _ = daemon.mcp.url.set(format!("http://{address}/mcp"));
    info!(%address, "agent tools (MCP) on http://{address}/mcp");
    if daemon.config.mcp_tokenless {
        warn!("agent tools (MCP) serve a client with no token: --mcp-tokenless is on");
    }
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
                    let caller = identify(&daemon, request.headers());
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

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("burrow-mcp-{name}-{}", crate::ids::agent_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_top_level_token_is_written_private_and_kept_across_restarts() {
        let home = scratch("token");
        let path = home.join(TOKEN_FILE);
        let first = load_top_level_token(Some(&home));
        assert!(valid_token(&first) && first.len() == 64, "{first}");
        // Just the token: nothing for a script to trim, nothing to leak into
        // a header.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        assert!(!home.join("mcp-token.new").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            // A file made readable since is made private again, token kept.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(load_top_level_token(Some(&home)), first);
            assert_eq!(mode(&path), 0o600);
        }
        // A script's trailing newline does no harm.
        std::fs::write(&path, format!("{first}\n")).unwrap();
        assert_eq!(load_top_level_token(Some(&home)), first);
        // Anything else is replaced: too short, or not a token.
        for bad in [
            "",
            "short",
            "has a space in it and is long enough to be a token ok",
        ] {
            std::fs::write(&path, bad).unwrap();
            let made = load_top_level_token(Some(&home));
            assert!(valid_token(&made) && made != first, "{bad:?}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), made);
        }
        // Without a home, one in memory that no one else can have; and a home
        // that cannot be written still gives a working token.
        let a = load_top_level_token(None);
        assert!(valid_token(&a) && a != load_top_level_token(None));
        let file = home.join("not-a-directory");
        std::fs::write(&file, "x").unwrap();
        assert!(valid_token(&load_top_level_token(Some(&file))));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn tokens_compare_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd") && !same("abc", "ab") && !same("", "a"));
        assert!(valid_token(&new_token()));
        assert!(!valid_token(&"a".repeat(31)) && valid_token(&"a".repeat(32)));
        assert!(!valid_token(&format!("{}\r\nx: y", "a".repeat(32))));
    }

    fn headers(authorization: Option<&str>) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        if let Some(value) = authorization {
            headers.insert(http::header::AUTHORIZATION, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn a_caller_is_its_token_and_one_with_none_needs_the_setting_to_be_a_client() {
        let strict = Daemon::new(crate::Config::ephemeral());
        let open = Daemon::new(crate::Config {
            mcp_tokenless: true,
            ..crate::Config::ephemeral()
        });
        for daemon in [&strict, &open] {
            let _ = daemon.mcp.top_level.set("t".repeat(64));
        }
        let agent = strict.mcp.token("agt_1");
        let top = "t".repeat(64);
        let who =
            |daemon: &Daemon, header: Option<String>| identify(daemon, &headers(header.as_deref()));
        // An agent is its own token, and the client's is the file's.
        assert_eq!(
            who(&strict, Some(format!("Bearer {agent}"))),
            Caller::Agent("agt_1".into())
        );
        assert_eq!(
            who(&strict, Some(format!("Bearer {top}"))),
            Caller::TopLevel
        );
        // Neither: no token at all, or one that no one was given, or one that
        // only looks like the file's.
        assert_eq!(who(&strict, None), Caller::NoToken);
        assert_eq!(who(&strict, Some("Bearer made-up".into())), Caller::Refused);
        assert_eq!(
            who(&strict, Some(format!("Bearer {}x", &top[..63]))),
            Caller::Refused
        );
        assert_eq!(who(&strict, Some(format!("Basic {top}"))), Caller::NoToken);
        // Set to take a client with none, it is one; a wrong token still is not.
        assert_eq!(who(&open, None), Caller::TopLevel);
        assert_eq!(who(&open, Some("Bearer made-up".into())), Caller::Refused);
        assert_eq!(who(&open, Some(format!("Bearer {top}"))), Caller::TopLevel);
        // Who may use the tools, and what the others are told.
        assert!(turned_away(&Caller::TopLevel).is_none());
        assert!(turned_away(&Caller::Agent("a".into())).is_none());
        let none = turned_away(&Caller::NoToken).unwrap().message.to_string();
        assert!(
            none.contains("mcp-token") && none.contains("Bearer"),
            "{none}"
        );
        assert!(
            turned_away(&Caller::Refused)
                .unwrap()
                .message
                .contains("not issued")
        );
    }

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
