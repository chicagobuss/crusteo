//! ACP tool calls, plans and permission prompts as Paseo timeline items
//! (`mapToolSnapshotToTimeline`, `mapToolDetail`, `mapPermissionRequest` in
//! Paseo's `acp-agent.ts`).
//!
//! A tool call arrives as `tool_call` and then `tool_call_update`s that
//! carry only what changed. The adapter keeps a merged snapshot per call and
//! sends the whole item each time.

use serde_json::{Map, Value, json};

/// A tool call as last reported: every field merged over the earlier ones.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub id: String,
    pub title: String,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub content: Option<Vec<Value>>,
    pub locations: Option<Vec<Value>>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
}

/// A terminal's state, as a tool call's `terminal` content shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalView {
    pub output: String,
    /// `None` while the command runs (or when it ended by a signal).
    pub exit_code: Option<i64>,
}

/// `mergeToolSnapshot`: a field the update leaves out keeps its value; an
/// update's `null` clears `content` and `locations` (an update that names
/// them replaces them).
pub fn merge(id: &str, update: &Value, previous: Option<&Snapshot>) -> Snapshot {
    let text = |key: &str| update[key].as_str().map(str::to_string);
    let list = |key: &str, before: Option<&Vec<Value>>| match update.get(key) {
        Some(Value::Array(items)) => Some(items.clone()),
        Some(_) => None,
        None => before.cloned(),
    };
    let raw = |key: &str, before: Option<&Value>| match update.get(key) {
        Some(value) => Some(value.clone()),
        None => before.cloned(),
    };
    Snapshot {
        id: id.to_string(),
        title: text("title")
            .or_else(|| previous.map(|p| p.title.clone()))
            .unwrap_or_else(|| id.to_string()),
        kind: text("kind").or_else(|| previous.and_then(|p| p.kind.clone())),
        status: text("status").or_else(|| previous.and_then(|p| p.status.clone())),
        content: list("content", previous.and_then(|p| p.content.as_ref())),
        locations: list("locations", previous.and_then(|p| p.locations.as_ref())),
        raw_input: raw("rawInput", previous.and_then(|p| p.raw_input.as_ref())),
        raw_output: raw("rawOutput", previous.and_then(|p| p.raw_output.as_ref())),
    }
}

/// `running`, `completed` or `failed`.
pub fn status(snapshot: &Snapshot) -> &'static str {
    match snapshot.status.as_deref() {
        Some("completed") => "completed",
        Some("failed") => "failed",
        _ => "running",
    }
}

/// A call to an MCP server's tool as `mcp__<server>__<tool>`, the name
/// Claude and Codex calls have, so a client shows Paseo's tools as its own.
/// ACP has no field for it, so it is read from how each agent reports one:
/// Cursor's `rawInput`, Gemini's title `<tool> (<server> MCP Server)` and
/// OpenCode's title `<server>_<tool>`, for a server the session was given
/// (`servers`). Paseo names them all by their kind, `other`.
pub fn mcp_name(snapshot: &Snapshot, servers: &[String]) -> Option<String> {
    if snapshot.kind.as_deref().is_some_and(|kind| kind != "other") {
        return None;
    }
    let input = record(snapshot.raw_input.as_ref());
    let named = |key| string(input, &[key]).filter(|s| !s.is_empty());
    let (server, tool) = match (named("providerIdentifier"), named("toolName")) {
        (Some(server), Some(tool)) => (server, tool),
        _ => {
            let title = snapshot.title.trim();
            let gemini = (title.strip_suffix(" MCP Server)"))
                .and_then(|rest| rest.split_once(" ("))
                .map(|(tool, server)| (server.to_string(), tool.to_string()));
            let opencode = || {
                servers.iter().find_map(|server| {
                    let tool = title.strip_prefix(server.as_str())?.strip_prefix('_')?;
                    Some((server.clone(), tool.to_string()))
                })
            };
            gemini.or_else(opencode)?
        }
    };
    let plain = |s: &str| !s.is_empty() && !s.contains(char::is_whitespace);
    (plain(&server) && plain(&tool)).then(|| format!("mcp__{server}__{tool}"))
}

/// The `tool_call` timeline item for a snapshot. `servers` are the
/// session's MCP servers ([`mcp_name`]); `terminal` resolves a client
/// terminal id to what it has shown.
pub fn item(
    snapshot: &Snapshot,
    servers: &[String],
    terminal: &dyn Fn(&str) -> Option<TerminalView>,
) -> Value {
    let status = status(snapshot);
    let mut metadata = Map::new();
    if let Some(kind) = &snapshot.kind {
        metadata.insert("kind".into(), kind.clone().into());
    }
    metadata.insert("title".into(), snapshot.title.clone().into());
    let error = if status == "failed" {
        json!({"message": error_message(snapshot.raw_output.as_ref())})
    } else {
        Value::Null
    };
    json!({
        "type": "tool_call",
        "callId": snapshot.id,
        "name": name(snapshot, servers),
        "detail": call_detail(snapshot, servers, terminal),
        "metadata": metadata,
        "status": status,
        "error": error,
    })
}

/// A call's name: an MCP tool's own, else its kind or title, as Paseo's.
fn name(snapshot: &Snapshot, servers: &[String]) -> String {
    mcp_name(snapshot, servers)
        .or_else(|| snapshot.kind.clone())
        .unwrap_or_else(|| snapshot.title.clone())
}

/// A call's detail: an MCP call's as Claude's are (the tool's arguments and
/// `{output}`), so a client shows it as it shows theirs; else [`detail`].
fn call_detail(
    snapshot: &Snapshot,
    servers: &[String],
    terminal: &dyn Fn(&str) -> Option<TerminalView>,
) -> Value {
    if mcp_name(snapshot, servers).is_none() {
        return detail(snapshot, terminal);
    }
    let input = record(snapshot.raw_input.as_ref());
    // Cursor wraps the arguments in `args`.
    let arguments = (input.filter(|input| input.contains_key("providerIdentifier")))
        .and_then(|input| input.get("args"))
        .or(snapshot.raw_input.as_ref())
        .cloned()
        .unwrap_or(json!({}));
    let output = content_text(snapshot.content.as_deref())
        .map(|text| serde_json::from_str(&text).unwrap_or(Value::String(text)))
        .or_else(|| snapshot.raw_output.clone())
        .map(|output| json!({"output": output}));
    json!({"type": "unknown", "input": arguments, "output": output})
}

/// Inserts `value` under `key` when it is there (JavaScript's `undefined`
/// fields are not sent).
fn put(detail: &mut Value, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        detail[key] = value;
    }
}

fn record(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// The first non-blank string among `keys`.
fn string(record: Option<&Map<String, Value>>, keys: &[&str]) -> Option<String> {
    let record = record?;
    keys.iter()
        .filter_map(|key| record.get(*key).and_then(Value::as_str))
        .find(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn number(record: Option<&Map<String, Value>>, keys: &[&str]) -> Option<Value> {
    let record = record?;
    keys.iter()
        .filter_map(|key| record.get(*key))
        .find(|v| v.is_number())
        .cloned()
}

/// The text of an ACP content block (`contentBlockToText`).
pub fn block_text(block: &Value) -> String {
    match block["type"].as_str() {
        Some("text") => block["text"].as_str().unwrap_or_default().to_string(),
        Some("resource_link") => block["title"]
            .as_str()
            .or(block["uri"].as_str())
            .unwrap_or_default()
            .to_string(),
        Some("resource") => match block["resource"]["text"].as_str() {
            Some(text) => text.to_string(),
            None => format!(
                "[resource:{}]",
                block["resource"]["mimeType"].as_str().unwrap_or("binary")
            ),
        },
        Some("image") => "[image]".into(),
        Some("audio") => "[audio]".into(),
        _ => String::new(),
    }
}

/// The text of a tool call's `content` entries, one per line.
pub fn content_text(content: Option<&[Value]>) -> Option<String> {
    let parts: Vec<String> = content?
        .iter()
        .filter(|item| item["type"] == "content")
        .map(|item| block_text(&item["content"]))
        .filter(|text| !text.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn diff(content: Option<&[Value]>) -> Option<&Value> {
    content?.iter().find(|item| item["type"] == "diff")
}

fn terminal_view(
    content: Option<&[Value]>,
    terminal: &dyn Fn(&str) -> Option<TerminalView>,
) -> Option<TerminalView> {
    let id = content?.iter().find(|item| item["type"] == "terminal")?["terminalId"].as_str()?;
    terminal(id)
}

fn stringify(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// `readErrorMessage`.
fn error_message(raw_output: Option<&Value>) -> String {
    if let Some(Value::String(text)) = raw_output {
        return text.clone();
    }
    string(record(raw_output), &["message", "error"]).unwrap_or_else(|| "Tool call failed".into())
}

/// `buildShellCommand`: `command` with its `args`.
fn shell_command(input: Option<&Map<String, Value>>) -> Option<String> {
    let command = string(input, &["command"])?;
    let args: Vec<&str> = input?
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    Some(if args.is_empty() {
        command
    } else {
        format!("{command} {}", args.join(" "))
    })
}

/// A tool call's `detail`, by its ACP `kind`.
pub fn detail(snapshot: &Snapshot, terminal: &dyn Fn(&str) -> Option<TerminalView>) -> Value {
    let content = snapshot.content.as_deref();
    let first_location = snapshot
        .locations
        .as_ref()
        .and_then(|l| l.first())
        .and_then(|l| l["path"].as_str())
        .map(str::to_string);
    let text = content_text(content);
    let input = record(snapshot.raw_input.as_ref());
    let output = record(snapshot.raw_output.as_ref());
    let path = || {
        first_location
            .clone()
            .or_else(|| string(input, &["path", "filePath", "file"]))
            .unwrap_or_else(|| snapshot.title.clone())
    };
    match snapshot.kind.as_deref() {
        Some("read") => {
            let mut detail = json!({"type": "read", "filePath": path()});
            put(
                &mut detail,
                "content",
                text.or_else(|| string(output, &["content", "text"]))
                    .map(Value::from),
            );
            put(&mut detail, "offset", number(input, &["offset", "line"]));
            put(&mut detail, "limit", number(input, &["limit"]));
            detail
        }
        Some(kind @ ("edit" | "delete")) => {
            let diff = diff(content);
            let mut detail = json!({"type": "edit", "filePath": path()});
            put(
                &mut detail,
                "oldString",
                diff.and_then(|d| d["oldText"].as_str())
                    .map(str::to_string)
                    .or_else(|| string(input, &["oldText", "oldString"]))
                    .map(Value::from),
            );
            let new = if kind == "delete" {
                Some(String::new())
            } else {
                diff.and_then(|d| d["newText"].as_str())
                    .map(str::to_string)
                    .or_else(|| string(input, &["newText", "newString"]))
            };
            put(&mut detail, "newString", new.map(Value::from));
            put(&mut detail, "unifiedDiff", text.map(Value::from));
            detail
        }
        Some("search") => {
            let mut detail = json!({
                "type": "search",
                "query": string(input, &["query", "pattern"]).unwrap_or_else(|| snapshot.title.clone()),
                "toolName": "search",
            });
            put(
                &mut detail,
                "content",
                text.or_else(|| string(output, &["content", "text"]))
                    .map(Value::from),
            );
            put(
                &mut detail,
                "filePaths",
                snapshot.locations.as_ref().map(|locations| {
                    locations
                        .iter()
                        .map(|l| l["path"].clone())
                        .collect::<Vec<_>>()
                        .into()
                }),
            );
            detail
        }
        Some("execute") => {
            let view = terminal_view(content, terminal);
            let command = shell_command(input)
                .or_else(|| string(input, &["command"]))
                .unwrap_or_else(|| snapshot.title.clone());
            let mut detail = json!({"type": "shell", "command": command});
            put(&mut detail, "cwd", string(input, &["cwd"]).map(Value::from));
            put(
                &mut detail,
                "output",
                view.as_ref()
                    .map(|v| v.output.clone())
                    .or(text)
                    .or_else(|| string(output, &["output", "text"]))
                    .map(Value::from),
            );
            put(
                &mut detail,
                "exitCode",
                view.and_then(|v| v.exit_code)
                    .map(Value::from)
                    .or_else(|| number(output, &["exitCode"])),
            );
            detail
        }
        Some("fetch") => {
            let mut detail = json!({
                "type": "fetch",
                "url": string(input, &["url"]).unwrap_or_else(|| snapshot.title.clone()),
            });
            put(
                &mut detail,
                "prompt",
                string(input, &["prompt"]).map(Value::from),
            );
            put(
                &mut detail,
                "result",
                text.or_else(|| string(output, &["result", "text", "content"]))
                    .map(Value::from),
            );
            put(&mut detail, "code", number(output, &["status", "code"]));
            detail
        }
        Some(kind @ ("think" | "switch_mode")) => {
            let (icon, fallback) = if kind == "think" {
                ("brain", snapshot.raw_output.as_ref())
            } else {
                ("sparkles", snapshot.raw_input.as_ref())
            };
            let mut detail = json!({"type": "plain_text", "label": snapshot.title, "icon": icon});
            put(
                &mut detail,
                "text",
                text.or_else(|| stringify(fallback)).map(Value::from),
            );
            detail
        }
        _ => {
            if let Some(view) = terminal_view(content, terminal) {
                return json!({"type": "shell", "command": snapshot.title,
                              "output": view.output, "exitCode": view.exit_code});
            }
            if let Some(text) = text {
                return json!({"type": "plain_text", "label": snapshot.title,
                              "text": text, "icon": "wrench"});
            }
            json!({
                "type": "unknown",
                "input": snapshot.raw_input.clone().unwrap_or(Value::Null),
                "output": snapshot.raw_output.clone().unwrap_or(Value::Null),
            })
        }
    }
}

/// A `todo` item for an ACP `plan` update.
pub fn plan(update: &Value) -> Value {
    let items: Vec<Value> = update["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| {
            json!({"text": entry["content"].as_str().unwrap_or_default(),
                   "completed": entry["status"] == "completed"})
        })
        .collect();
    json!({"type": "todo", "items": items})
}

fn allows(option: &Value) -> bool {
    option["kind"]
        .as_str()
        .is_some_and(|k| k.starts_with("allow"))
}

/// A prompt that offers two choices of the same allow kind is a chooser
/// (pick one of several), not a yes/no.
pub fn is_chooser(options: &[Value]) -> bool {
    let mut seen = Vec::new();
    for option in options.iter().filter(|o| allows(o)) {
        let kind = option["kind"].as_str().unwrap_or_default();
        if seen.contains(&kind) {
            return true;
        }
        seen.push(kind);
    }
    false
}

/// The `AgentPermissionRequest` for a `session/request_permission`, with
/// Paseo's id (`request_id`), named as [`item`] names the call.
pub fn permission_request(
    provider: &str,
    request_id: &str,
    params: &Value,
    snapshot: &Snapshot,
    servers: &[String],
) -> Value {
    let options = params["options"].as_array().cloned().unwrap_or_default();
    let kind = if snapshot.kind.as_deref() == Some("switch_mode") {
        "mode"
    } else {
        "tool"
    };
    let title = params["toolCall"]["title"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| snapshot.title.clone());
    let chooser_text = if is_chooser(&options) {
        params["toolCall"]["content"]
            .as_array()
            .and_then(|content| content_text(Some(content)))
    } else {
        None
    };
    let detail = match chooser_text {
        Some(text) => json!({"type": "plain_text", "label": title, "text": text, "icon": "wrench"}),
        None => call_detail(snapshot, servers, &|_| None),
    };
    let actions: Vec<Value> = options
        .iter()
        .map(|option| {
            json!({"id": option["optionId"], "label": option["name"],
                   "behavior": if allows(option) { "allow" } else { "deny" }})
        })
        .collect();
    json!({
        "id": request_id,
        "provider": provider,
        "name": name(snapshot, servers),
        "kind": kind,
        "title": title,
        "detail": detail,
        "actions": actions,
        "metadata": {
            "toolCallId": params["toolCall"]["toolCallId"],
            "rawRequest": params,
            "options": options,
        },
    })
}

/// The option a client's answer picks (`selectPermissionOption`): the one
/// named by `selectedActionId` if its behaviour matches, else the first
/// `allow_once`/`allow_always` (or `reject_once`/`reject_always`). `None`
/// answers the agent with `cancelled`.
pub fn select_option<'a>(options: &'a [Value], response: &Value) -> Option<&'a Value> {
    let behavior = if response["behavior"] == "allow" {
        "allow"
    } else {
        "deny"
    };
    if let Some(selected) = response["selectedActionId"].as_str() {
        let option = options.iter().find(|o| o["optionId"] == selected)?;
        let option_behavior = if allows(option) { "allow" } else { "deny" };
        return (option_behavior == behavior).then_some(option);
    }
    let order: [&str; 2] = if behavior == "allow" {
        ["allow_once", "allow_always"]
    } else {
        ["reject_once", "reject_always"]
    };
    order
        .iter()
        .find_map(|kind| options.iter().find(|o| o["kind"] == *kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<TerminalView> {
        None
    }

    #[test]
    fn updates_merge_into_the_snapshot() {
        let first = merge(
            "c1",
            &json!({"title": "Run ls", "kind": "execute", "status": "pending",
                    "rawInput": {"command": "ls", "args": ["-la"]}}),
            None,
        );
        let next = merge(
            "c1",
            &json!({"status": "completed", "content": [
                {"type": "content", "content": {"type": "text", "text": "a\nb"}}]}),
            Some(&first),
        );
        assert_eq!(next.title, "Run ls");
        assert_eq!(next.kind.as_deref(), Some("execute"));
        let item = item(&next, &[], &none);
        assert_eq!(item["name"], "execute");
        assert_eq!(item["status"], "completed");
        assert_eq!(
            item["detail"],
            json!({"type": "shell", "command": "ls -la", "output": "a\nb"})
        );
        assert_eq!(
            item["metadata"],
            json!({"kind": "execute", "title": "Run ls"})
        );
    }

    #[test]
    fn details_follow_the_kind() {
        let edit = merge(
            "e",
            &json!({"title": "Edit", "kind": "edit", "locations": [{"path": "/x/a.rs"}],
                    "content": [{"type": "diff", "path": "/x/a.rs", "oldText": "a", "newText": "b"}]}),
            None,
        );
        assert_eq!(
            detail(&edit, &none),
            json!({"type": "edit", "filePath": "/x/a.rs", "oldString": "a", "newString": "b"})
        );
        let read = merge(
            "r",
            &json!({"title": "Read a.rs", "kind": "read", "rawInput": {"path": "/x/a.rs", "line": 3}}),
            None,
        );
        assert_eq!(
            detail(&read, &none),
            json!({"type": "read", "filePath": "/x/a.rs", "offset": 3})
        );
        let failed = merge(
            "f",
            &json!({"title": "Mystery", "status": "failed", "rawOutput": {"error": "boom"}}),
            None,
        );
        let item = item(&failed, &[], &none);
        assert_eq!(item["name"], "Mystery");
        assert_eq!(item["error"], json!({"message": "boom"}));
        assert_eq!(item["detail"]["type"], "unknown");
        let shell = merge(
            "t",
            &json!({"title": "Terminal", "kind": "execute", "rawInput": {"command": "make"},
                    "content": [{"type": "terminal", "terminalId": "term-1"}]}),
            None,
        );
        let view = |id: &str| {
            (id == "term-1").then(|| TerminalView {
                output: "ok\n".into(),
                exit_code: Some(0),
            })
        };
        assert_eq!(
            detail(&shell, &view),
            json!({"type": "shell", "command": "make", "output": "ok\n", "exitCode": 0})
        );
    }

    #[test]
    fn permissions_map_options_to_actions() {
        let params = json!({"sessionId": "s", "toolCall": {"toolCallId": "c1", "title": "`rm x`",
        "kind": "execute", "rawInput": {"command": "rm x"}},
        "options": [
            {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "allow-always", "name": "Allow always", "kind": "allow_always"},
            {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"},
        ]});
        let snapshot = merge("c1", &params["toolCall"], None);
        let request = permission_request("cursor", "p1", &params, &snapshot, &[]);
        assert_eq!(request["kind"], "tool");
        assert_eq!(request["name"], "execute");
        assert_eq!(request["actions"][2]["behavior"], "deny");
        assert_eq!(request["detail"]["command"], "rm x");
        let options = params["options"].as_array().unwrap();
        let allow = select_option(options, &json!({"behavior": "allow"})).unwrap();
        assert_eq!(allow["optionId"], "allow-once");
        let picked = select_option(
            options,
            &json!({"behavior": "allow", "selectedActionId": "allow-always"}),
        )
        .unwrap();
        assert_eq!(picked["optionId"], "allow-always");
        assert!(
            select_option(
                options,
                &json!({"behavior": "allow", "selectedActionId": "reject-once"})
            )
            .is_none(),
            "an action of the other behaviour is refused"
        );
        assert!(!is_chooser(options));
    }

    #[test]
    fn mcp_calls_are_named_by_server_and_tool_however_the_agent_reports_them() {
        let servers = ["paseo".to_string()];
        let named =
            |update: Value| item(&merge("m", &update, None), &servers, &none)["name"].clone();
        // Cursor: the call is pending as `MCP: tool`, then named in rawInput.
        let pending = merge(
            "m",
            &json!({"kind": "other", "title": "MCP: tool", "rawInput": {}}),
            None,
        );
        let cursor = merge(
            "m",
            &json!({"title": "paseo: list_agents", "rawInput": {"args": {"serverId": "srv_1"},
                    "providerIdentifier": "paseo", "toolName": "list_agents"}}),
            Some(&pending),
        );
        let cursor_item = item(&cursor, &servers, &none);
        assert_eq!(cursor_item["name"], "mcp__paseo__list_agents");
        assert_eq!(
            cursor_item["detail"],
            json!({"type": "unknown", "input": {"serverId": "srv_1"}, "output": null})
        );
        // A result as text, as OpenCode and Gemini send it, is the output.
        let done = merge(
            "m",
            &json!({"status": "completed", "content": [{"type": "content",
                    "content": {"type": "text", "text": "agents_count=0\n\n{}"}}]}),
            Some(&cursor),
        );
        assert_eq!(
            item(&done, &servers, &none)["detail"]["output"],
            json!({"output": "agents_count=0\n\n{}"})
        );
        let params = json!({"toolCall": {"toolCallId": "m", "title": "paseo-list_agents: list_agents"},
                            "options": []});
        let request = permission_request("acp", "p", &params, &cursor, &servers);
        assert_eq!(request["name"], "mcp__paseo__list_agents");
        // Gemini and OpenCode name it in the title.
        let gemini = named(json!({"kind": "other", "title": "list_hosts (paseo MCP Server)"}));
        assert_eq!(gemini, "mcp__paseo__list_hosts");
        let opencode = named(json!({"kind": "other", "title": "paseo_list_hosts"}));
        assert_eq!(opencode, "mcp__paseo__list_hosts");
        // Anything else keeps Paseo's name.
        assert_eq!(
            named(json!({"kind": "other", "title": "github_list_issues"})),
            "other"
        );
        assert_eq!(
            named(json!({"kind": "execute", "title": "paseo_list_hosts"})),
            "execute"
        );
        let spaced = named(json!({"kind": "other", "title": "Read the (big) MCP Server)"}));
        assert_eq!(spaced, "other");
    }
}
