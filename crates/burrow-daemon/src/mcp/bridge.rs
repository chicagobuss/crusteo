//! `burrow mcp-bridge`: the agent tools on stdio, for agents that take only
//! stdio MCP servers (every ACP agent must). Each JSON-RPC line read is
//! posted to Burrow's loopback server with the agent's token, and the
//! answer written back as a line. Requests run side by side, so a long
//! tool call holds up no other; a notification gets no answer.
//!
//! Without a server (no `BURROW_MCP_URL`) it is an MCP server with no
//! tools, so an agent that loads it from a config of its own, as
//! Antigravity does, runs as before outside Burrow. The server is never
//! looked for: a bridge started by hand is given its address.
//!
//! The token is the agent's (`BURROW_MCP_TOKEN`). A bridge given an address
//! and no token sends the one Burrow keeps in `<home>/mcp-token`
//! ([`super::TOKEN_FILE`]), that of a client that is not an agent, which
//! Burrow no longer serves without one.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// Burrow's server and the token to send it.
#[derive(Debug, Clone)]
pub struct Server {
    pub url: String,
    /// Empty: none to send, and Burrow says what it wants instead.
    pub token: String,
}

impl Server {
    /// The server the bridge is asked to forward to: `url`, with `token` if
    /// there is one, else the one in `home`'s [`super::TOKEN_FILE`], else
    /// none. `None` without a `url`: that is the server with no tools.
    pub fn resolve(
        url: Option<String>,
        token: Option<String>,
        home: Option<&Path>,
    ) -> Option<Self> {
        let url = url.filter(|url| !url.trim().is_empty())?;
        let token = (token.filter(|token| !token.trim().is_empty()))
            .or_else(|| read_token(home?))
            .unwrap_or_default();
        Some(Self { url, token })
    }
}

/// The token in `home`'s token file, if there is one.
fn read_token(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join(super::TOKEN_FILE)).ok()?;
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Forwards stdin to `server`, or answers it with no tools, until stdin
/// ends.
pub fn run(server: Option<Server>) -> std::io::Result<()> {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let mut workers = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let (server, out) = (server.clone(), out.clone());
        workers.push(std::thread::spawn(move || {
            let replies = match &server {
                Some(server) => forward(&server.url, &server.token, &line),
                None => no_tools(&line).into_iter().collect(),
            };
            let mut out = out.lock().unwrap_or_else(|p| p.into_inner());
            for reply in replies {
                let _ = writeln!(out, "{reply}");
            }
            let _ = out.flush();
        }));
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

/// The lines to write back for one message.
pub fn forward(url: &str, token: &str, line: &str) -> Vec<String> {
    let id = serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|message| message.get("id").cloned());
    let mut request = ureq::post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    if !token.is_empty() {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let sent = request.send(line.to_string());
    let body = sent.and_then(|mut response| response.body_mut().read_to_string());
    match body {
        Ok(body) => replies(&body),
        // A request is told why it got no answer; a notification has
        // no one to tell.
        Err(error) => id
            .map(|id| {
                let message = format!("Burrow's agent tools are unreachable: {error}");
                json!({"jsonrpc": "2.0", "id": id,
                       "error": {"code": -32603, "message": message}})
                .to_string()
            })
            .into_iter()
            .collect(),
    }
}

/// The answer of a server with no tools, named as Burrow's is.
fn no_tools(line: &str) -> Option<String> {
    let message: Value = serde_json::from_str(line).ok()?;
    let id = message.get("id")?.clone();
    let mut reply = match message["method"].as_str().unwrap_or_default() {
        "initialize" => json!({"result": {
            "protocolVersion": message["params"]["protocolVersion"]
                .as_str()
                .unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "agent-mcp", "version": "2.0.0"},
        }}),
        "tools/list" => json!({"result": {"tools": []}}),
        "ping" => json!({"result": {}}),
        method => json!({"error": {"code": -32601,
            "message": format!("{method} is not offered outside Burrow")}}),
    };
    reply["jsonrpc"] = "2.0".into();
    reply["id"] = id;
    Some(reply.to_string())
}

/// The messages in a body: JSON, or the `data:` lines of an event stream.
fn replies(body: &str) -> Vec<String> {
    let body = body.trim();
    if body.is_empty() {
        return Vec::new();
    }
    if body.starts_with('{') || body.starts_with('[') {
        return vec![body.to_string()];
    }
    (body.lines())
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| !data.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_come_as_json_or_from_an_event_stream() {
        assert_eq!(replies("{\"id\":1}\n"), ["{\"id\":1}"]);
        assert_eq!(
            replies("event: message\ndata: {\"id\":2}\n\n"),
            ["{\"id\":2}"]
        );
        assert!(replies("").is_empty(), "a notification's empty answer");
    }

    #[test]
    fn without_a_server_it_has_no_tools() {
        let answer =
            |line: &str| -> Value { serde_json::from_str(&no_tools(line).unwrap()).unwrap() };
        let init = answer(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["capabilities"], json!({"tools": {}}));
        let listed = answer(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#);
        assert_eq!(
            (listed["id"].clone(), listed["result"].clone()),
            (json!("a"), json!({"tools": []}))
        );
        let call = answer(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_agents"}}"#,
        );
        assert_eq!(call["error"]["code"], -32601);
        assert_eq!(
            no_tools(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            None
        );
    }

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("burrow-bridge-{}", crate::ids::agent_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_server_is_the_address_it_is_given_with_the_agents_token_or_the_files() {
        let dir = scratch();
        std::fs::write(dir.join(super::super::TOKEN_FILE), "file-token\n").unwrap();
        let (home, nowhere) = (dir.as_path(), dir.join("nowhere"));
        let url = || Some("http://127.0.0.1:1/mcp".to_string());
        let token = |url: Option<String>, token: Option<&str>, home: Option<&Path>| {
            Server::resolve(url, token.map(str::to_string), home).map(|server| server.token)
        };
        // An address is what makes a server; without one, no tools.
        assert_eq!(token(None, Some("t"), Some(home)), None);
        assert_eq!(token(Some("  ".into()), None, Some(home)), None);
        // An agent's token is its own; a bridge with none sends the file's.
        assert_eq!(
            token(url(), Some("agent"), Some(home)).as_deref(),
            Some("agent")
        );
        assert_eq!(
            token(url(), None, Some(home)).as_deref(),
            Some("file-token")
        );
        assert_eq!(
            token(url(), Some(" "), Some(home)).as_deref(),
            Some("file-token")
        );
        // None to send: Burrow says what it wants.
        assert_eq!(token(url(), None, None).as_deref(), Some(""));
        assert_eq!(token(url(), None, Some(&nowhere)).as_deref(), Some(""));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_token_goes_as_a_bearer_and_no_token_sends_no_authorization() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let heads = Arc::new(Mutex::new(Vec::new()));
        let kept = heads.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // The whole request, so the answer is not met with a reset.
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                let head = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                        let length = (head.lines())
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break head;
                        }
                    }
                    if n == 0 {
                        break String::new();
                    }
                };
                kept.lock().unwrap().push(head);
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(answer.as_bytes());
            }
        });
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        assert_eq!(
            forward(&url, "s3cret", line),
            [r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#]
        );
        assert_eq!(forward(&url, "", line).len(), 1);
        let heads = heads.lock().unwrap();
        assert!(
            heads[0].contains("authorization: bearer s3cret"),
            "{}",
            heads[0]
        );
        assert!(!heads[1].contains("authorization"), "{}", heads[1]);
    }

    #[test]
    fn an_unreachable_server_answers_a_request_and_not_a_notification() {
        let url = "http://127.0.0.1:9/mcp";
        let answer = forward(
            url,
            "t",
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#,
        );
        let answer: Value = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(answer["id"], 7);
        assert!(
            answer["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unreachable")
        );
        assert!(
            forward(
                url,
                "t",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            )
            .is_empty()
        );
    }
}
