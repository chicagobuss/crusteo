//! Antigravity steps as timeline items, as Paseo 0.12's plugin maps them
//! (`timeline.ts`), after its schemas fill in defaults and drop unknown keys.

use serde_json::{Map, Value, json};

/// What Burrow's plugin is called in agy, which names its servers
/// `<plugin>_<server>`.
const PLUGIN_PREFIX: &str = "burrow_";

/// A `tool` or `subagent` step as a `tool_call` item with call id `id`.
pub fn tool_item(step: &Value, id: &str) -> Value {
    let name = mcp_name(step).unwrap_or_else(|| {
        let name = step["tool_name"].as_str().filter(|n| !n.is_empty());
        name.unwrap_or("Antigravity tool").to_string()
    });
    let mut item = json!({
        "type": "tool_call", "callId": id, "name": name, "detail": detail(step),
    });
    let subagents = subagents(step);
    if let Some(subagents) = &subagents {
        item["metadata"] = json!({"subagents": subagents});
    }
    let (status, error) = match step["state"].as_str() {
        Some("ACTIVE") => ("running", Value::Null),
        Some("ERROR") => {
            let message = step["tool_info"]["error"]["message"]
                .as_str()
                .unwrap_or("Antigravity tool failed");
            ("failed", message.into())
        }
        _ => ("completed", Value::Null),
    };
    item["status"] = status.into();
    item["error"] = error;
    item
}

/// An MCP call (`call_mcp_tool`) as `mcp__<server>__<tool>`, the name other
/// agents' MCP calls have, so a client shows Paseo's tools as their own.
/// (Paseo's plugin keeps `call_mcp_tool`; it gives agy no servers.)
fn mcp_name(step: &Value) -> Option<String> {
    let info = &step["tool_info"];
    if info["name"] != "call_mcp_tool" {
        return None;
    }
    let text = |key: &str| info["parameters"][key].as_str().filter(|s| !s.is_empty());
    let (server, tool) = (text("ServerName")?, text("ToolName")?);
    let server = server.strip_prefix(PLUGIN_PREFIX).unwrap_or(server);
    Some(format!("mcp__{server}__{tool}"))
}

/// `subagent_info.subagents`, each child with the wire's defaults.
fn subagents(step: &Value) -> Option<Vec<Value>> {
    let list = step["subagent_info"]["subagents"].as_array()?;
    Some(
        list.iter()
            .map(|child| {
                let text = |key: &str| child[key].as_str().unwrap_or_default();
                json!({
                    "type_name": text("type_name"),
                    "role": text("role"),
                    "initial_prompt": text("initial_prompt"),
                    "conversation_id": child["conversation_id"].as_str(),
                    "log_uri": text("log_uri"),
                    "workspace_uris": child["workspace_uris"].as_array().cloned().unwrap_or_default(),
                })
            })
            .collect(),
    )
}

fn detail(step: &Value) -> Value {
    if let Some(child) = subagents(step).and_then(|list| list.into_iter().next()) {
        let mut detail = json!({
            "type": "sub_agent", "subAgentType": child["type_name"],
            "description": child["initial_prompt"], "log": child["log_uri"],
        });
        if let Some(id) = child["conversation_id"].as_str() {
            detail["childSessionId"] = id.into();
        }
        return detail;
    }
    let info = &step["tool_info"];
    if !info.is_object() {
        let label = step["tool_name"].as_str().unwrap_or_default();
        return json!({"type": "plain_text", "label": label, "text": ""});
    }
    let parameters = info["parameters"]
        .as_object()
        .cloned()
        .unwrap_or_else(Map::new);
    let output = info["output"].as_str().unwrap_or_default();
    // `AbsolutePath || TargetFile`, then only a string counts.
    let path = ["AbsolutePath", "TargetFile"]
        .iter()
        .filter_map(|key| parameters.get(*key))
        .find(|value| truthy(value))
        .and_then(Value::as_str);
    match (info["name"].as_str().unwrap_or_default(), path) {
        ("run_command", _) if parameters.get("CommandLine").is_some_and(Value::is_string) => {
            json!({"type": "shell", "command": parameters["CommandLine"], "output": output})
        }
        ("view_file", Some(path)) => json!({"type": "read", "filePath": path, "content": output}),
        ("write_to_file", Some(path)) => json!({"type": "write", "filePath": path}),
        ("replace_file_content" | "multi_replace_file_content", Some(path)) => {
            json!({"type": "edit", "filePath": path})
        }
        // Native frames summarise writes; they carry neither contents nor a diff.
        _ => {
            let output = match info["error"].as_object() {
                Some(error) => json!({
                    "type": error.get("type").cloned().unwrap_or_default(),
                    "message": error.get("message").cloned().unwrap_or_default(),
                }),
                None => output.into(),
            };
            // An MCP call's detail is as Claude's are: the tool's own
            // arguments and its result as `{output}`.
            let (input, output) = match mcp_name(step) {
                Some(_) => {
                    let arguments = parameters.get("Arguments").cloned().unwrap_or(json!({}));
                    (arguments, json!({"output": output}))
                }
                None => (Value::Object(parameters), output),
            };
            json!({"type": "unknown", "input": input, "output": output})
        }
    }
}

/// JavaScript truthiness, for `a || b` over JSON values.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(state: &str, info: Value) -> Value {
        json!({"step_index": 2, "state": state, "step_type": "tool",
               "tool_name": info["name"], "tool_info": info})
    }

    #[test]
    fn details_match_paseo() {
        let shell = tool_item(
            &step(
                "DONE",
                json!({"name": "run_command", "parameters": {"CommandLine": "ls"}, "output": "a"}),
            ),
            "t:2",
        );
        assert_eq!(
            shell,
            json!({"type": "tool_call", "callId": "t:2", "name": "run_command",
                   "detail": {"type": "shell", "command": "ls", "output": "a"},
                   "status": "completed", "error": null})
        );
        let read = tool_item(
            &step(
                "ACTIVE",
                json!({"name": "view_file", "parameters": {"AbsolutePath": "/w/a.txt"}}),
            ),
            "t:3",
        );
        assert_eq!(
            read["detail"],
            json!({"type": "read", "filePath": "/w/a.txt", "content": ""})
        );
        assert_eq!(read["status"], "running");
        let write = tool_item(
            &step(
                "DONE",
                json!({"name": "write_to_file", "parameters": {"TargetFile": "/w/b"}}),
            ),
            "t:4",
        );
        assert_eq!(
            write["detail"],
            json!({"type": "write", "filePath": "/w/b"})
        );
        let edit = tool_item(
            &step(
                "DONE",
                json!({"name": "multi_replace_file_content", "parameters": {"AbsolutePath": "", "TargetFile": "/w/c"}}),
            ),
            "t:5",
        );
        assert_eq!(edit["detail"], json!({"type": "edit", "filePath": "/w/c"}));
        let other = tool_item(
            &step(
                "DONE",
                json!({"name": "grep_search", "parameters": {"Query": "x"}, "output": "2 hits"}),
            ),
            "t:6",
        );
        assert_eq!(
            other["detail"],
            json!({"type": "unknown", "input": {"Query": "x"}, "output": "2 hits"})
        );
        let bare = tool_item(
            &json!({"state": "DONE", "step_type": "tool", "tool_name": "wait"}),
            "t:7",
        );
        assert_eq!(
            bare["detail"],
            json!({"type": "plain_text", "label": "wait", "text": ""})
        );
        let unnamed = tool_item(&json!({"state": "DONE", "step_type": "tool"}), "t:8");
        assert_eq!(unnamed["name"], "Antigravity tool");
    }

    #[test]
    fn an_mcp_call_is_named_by_its_server_and_tool() {
        let call = |server: &str| {
            step(
                "DONE",
                json!({"name": "call_mcp_tool", "output": "agents_count=0",
                       "parameters": {"Arguments": {"serverId": "srv_1"}, "ServerName": server,
                                      "ToolName": "list_agents"}}),
            )
        };
        let ours = tool_item(&call("burrow_paseo"), "t:4");
        assert_eq!(ours["name"], "mcp__paseo__list_agents");
        assert_eq!(
            ours["detail"],
            json!({"type": "unknown", "input": {"serverId": "srv_1"},
                   "output": {"output": "agents_count=0"}})
        );
        assert_eq!(
            tool_item(&call("github"), "t:5")["name"],
            "mcp__github__list_agents"
        );
        let unnamed = step("DONE", json!({"name": "call_mcp_tool", "parameters": {}}));
        assert_eq!(tool_item(&unnamed, "t:6")["name"], "call_mcp_tool");
    }

    #[test]
    fn a_denied_tool_fails_with_its_message() {
        let denied = tool_item(
            &step(
                "ERROR",
                json!({"name": "run_command", "parameters": {"CommandLine": "rm x"},
                       "error": {"type": "TOOL_ERROR", "message": "denied", "extra": 1}}),
            ),
            "t:9",
        );
        assert_eq!(denied["status"], "failed");
        assert_eq!(denied["error"], "denied");
        assert_eq!(denied["detail"]["type"], "shell");
        let unknown = tool_item(
            &step(
                "ERROR",
                json!({"name": "send_message", "error": {"type": "TOOL_ERROR", "message": "no", "extra": 1}}),
            ),
            "t:10",
        );
        assert_eq!(
            unknown["detail"]["output"],
            json!({"type": "TOOL_ERROR", "message": "no"})
        );
        let silent = tool_item(&step("ERROR", json!({"name": "wait"})), "t:11");
        assert_eq!(silent["error"], "Antigravity tool failed");
    }

    #[test]
    fn a_subagent_carries_its_child_conversation() {
        let item = tool_item(
            &json!({"state": "DONE", "step_type": "subagent", "tool_name": "invoke_subagent",
                "subagent_info": {"subagents": [{"type_name": "counter", "initial_prompt": "2+2?",
                    "conversation_id": "child-1", "log_uri": "file:///home/user/log.jsonl"}]}}),
            "t:4",
        );
        assert_eq!(
            item["detail"],
            json!({"type": "sub_agent", "subAgentType": "counter", "description": "2+2?",
                   "childSessionId": "child-1", "log": "file:///home/user/log.jsonl"})
        );
        assert_eq!(
            item["metadata"]["subagents"][0],
            json!({"type_name": "counter", "role": "", "initial_prompt": "2+2?",
                   "conversation_id": "child-1", "log_uri": "file:///home/user/log.jsonl",
                   "workspace_uris": []})
        );
    }
}
