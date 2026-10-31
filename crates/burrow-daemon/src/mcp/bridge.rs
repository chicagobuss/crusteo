//! `burrow mcp-bridge`: the agent tools on stdio, for agents that take only
//! stdio MCP servers (every ACP agent must). Each JSON-RPC line read is
//! posted to Burrow's loopback server with the agent's token, and the
//! answer written back as a line. Requests run side by side, so a long
//! tool call holds up no other; a notification gets no answer.
//!
//! Without a server (no `BURROW_MCP_URL` and `BURROW_MCP_TOKEN`) it is an
//! MCP server with no tools, so an agent that loads it from a config of its
//! own, as Antigravity does, runs as before outside Burrow.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// Burrow's server and the agent's token.
#[derive(Debug, Clone)]
pub struct Server {
    pub url: String,
    pub token: String,
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
fn forward(url: &str, token: &str, line: &str) -> Vec<String> {
    let id = serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|message| message.get("id").cloned());
    let sent = ureq::post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", &format!("Bearer {token}"))
        .send(line.to_string());
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
