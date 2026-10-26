//! How Paseo labels tool calls (protocol/src/tool-call-display.ts):
//! a display name, a one-line summary, an icon, and whether the row expands.

use serde_json::Value;

use crate::agent_messages::{AgentEvent, AgentMessage};
use crate::timeline::ToolCall;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolIcon {
    Terminal,
    Eye,
    Pencil,
    Search,
    Bot,
    Wrench,
    Brain,
    Sparkles,
    Mic,
    Paseo,
    /// A message from another agent.
    Message,
    /// A message to another agent.
    Send,
    Finished,
    Failed,
    Permission,
    Closed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDisplay {
    pub name: String,
    pub summary: Option<String>,
    pub icon: ToolIcon,
    /// The agent the row is about, drawn as a link to it.
    pub agent: Option<AgentLink>,
}

/// An agent a row names: who sent a message, or the agent a Paseo tool
/// call acts on.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentLink {
    pub id: String,
    /// Its title as the message or call gave it.
    pub title: Option<String>,
    /// It comes before the row's name ("planner finished") rather than
    /// after it ("From planner").
    pub lead: bool,
}

impl AgentLink {
    /// What to call it: the title the fleet knows now, else the one given,
    /// else the start of its id.
    pub fn label(&self, known: Option<String>) -> String {
        known
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| self.id.chars().take(8).collect())
    }
}

/// A pending permission request as the tool call it asks to make, so it
/// is labelled as that call would be.
pub fn permission_call(request: &Value) -> ToolCall {
    let id = request["id"].as_str().unwrap_or_default().to_string();
    ToolCall {
        identity: id.clone(),
        call_id: id,
        name: request["name"].as_str().unwrap_or_default().to_string(),
        status: "completed".into(),
        error: None,
        detail: if request["detail"].is_object() {
            request["detail"].clone()
        } else {
            serde_json::json!({"type": "unknown", "input": request["input"].clone(), "output": null})
        },
        metadata: Default::default(),
        agent_message: None,
    }
}

pub fn tool_display(tool: &ToolCall, cwd: &str) -> ToolDisplay {
    let detail = &tool.detail;
    let lower = tool.name.to_lowercase();
    if let Some(message) = &tool.agent_message {
        return agent_message_display(message);
    }
    if lower == "thinking" {
        return ToolDisplay {
            name: "Thinking".into(),
            summary: None,
            icon: ToolIcon::Brain,
            agent: None,
        };
    }
    if let Some(leaf) = paseo_leaf(&tool.name) {
        return paseo_tool_display(&leaf, detail);
    }
    let text = |key: &str| {
        detail[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let (name, summary, icon) = match tool.detail_type() {
        "shell" => ("Shell".to_string(), text("command"), ToolIcon::Terminal),
        "read" => (
            "Read".into(),
            text("filePath").map(|p| strip_cwd(&p, cwd)),
            ToolIcon::Eye,
        ),
        "edit" => (
            "Edit".into(),
            text("filePath").map(|p| strip_cwd(&p, cwd)),
            ToolIcon::Pencil,
        ),
        "write" => (
            "Write".into(),
            text("filePath").map(|p| strip_cwd(&p, cwd)),
            ToolIcon::Pencil,
        ),
        "search" => ("Search".into(), text("query"), ToolIcon::Search),
        "fetch" => ("Fetch".into(), text("url"), ToolIcon::Search),
        "worktree_setup" => (
            "Worktree setup".into(),
            text("branchName"),
            ToolIcon::Terminal,
        ),
        "sub_agent" => (
            text("subAgentType").unwrap_or_else(|| "Task".into()),
            text("description"),
            ToolIcon::Bot,
        ),
        "plain_text" => (
            humanize(&tool.name),
            text("label"),
            match detail["icon"].as_str() {
                Some("square_terminal") => ToolIcon::Terminal,
                Some("eye") => ToolIcon::Eye,
                Some("pencil") => ToolIcon::Pencil,
                Some("search") => ToolIcon::Search,
                Some("bot") => ToolIcon::Bot,
                Some("sparkles") => ToolIcon::Sparkles,
                Some("brain") => ToolIcon::Brain,
                Some("mic_vocal") => ToolIcon::Mic,
                _ => ToolIcon::Wrench,
            },
        ),
        "plan" => ("Plan".into(), None, ToolIcon::Brain),
        _ if lower == "task" => (
            "Task".into(),
            tool.metadata
                .get("subAgentActivity")
                .and_then(Value::as_str)
                .map(str::to_string),
            ToolIcon::Bot,
        ),
        _ => (humanize(&tool.name), None, ToolIcon::Wrench),
    };
    let icon = match lower.as_str() {
        "speak" => ToolIcon::Mic,
        _ => icon,
    };
    let name = if lower == "terminal" {
        "Terminal".into()
    } else {
        name
    };
    ToolDisplay {
        name,
        summary,
        icon,
        agent: None,
    }
}

/// A message from another agent leads with who sent it ("From planner"),
/// then its first line; a notice reads as a status line ("worker
/// finished"), then the first line of the agent's last reply. The whole
/// text is the row's detail.
fn agent_message_display(message: &AgentMessage) -> ToolDisplay {
    let (name, icon) = match message.event {
        AgentEvent::Message => ("From", ToolIcon::Message),
        AgentEvent::Finished => ("finished", ToolIcon::Finished),
        AgentEvent::Errored => ("failed", ToolIcon::Failed),
        AgentEvent::PermissionRequired => ("needs permission", ToolIcon::Permission),
        AgentEvent::Closed => ("was closed", ToolIcon::Closed),
    };
    let summary = if message.is_notice() {
        message.response().and_then(first_line)
    } else {
        first_line(&message.text)
    };
    ToolDisplay {
        name: name.into(),
        summary,
        icon,
        agent: Some(AgentLink {
            id: message.sender_id.clone(),
            title: message.sender_title.clone(),
            lead: message.is_notice(),
        }),
    }
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// The tool's name within Paseo's own tools (`mcp__paseo__list_agents`,
/// `paseo.list_agents`), as protocol/tool-name-normalization.ts reads it.
fn paseo_leaf(name: &str) -> Option<String> {
    let lower = name.trim().to_lowercase();
    let paseo = |server: &str| server == "paseo" || server.starts_with("paseo_");
    // Speaking is the voice tool's, wherever it is served from.
    if lower.rsplit(|c: char| !c.is_ascii_alphanumeric()).next() == Some("speak") {
        return None;
    }
    if lower.contains("__") {
        let segments: Vec<&str> = lower.split("__").filter(|s| !s.is_empty()).collect();
        return (segments.len() >= 3 && segments[0] == "mcp" && paseo(segments[1]))
            .then(|| segments[2..].join("__"));
    }
    let (server, leaf) = lower.split_once('.')?;
    paseo(server).then(|| leaf.to_string())
}

/// What a Paseo tool call's row shows after its label.
enum Context {
    None,
    /// The first of these input fields that holds something.
    Field(&'static [&'static str]),
    /// The agent named by `agentId`.
    Agent,
    /// The field when given, else the agent.
    FieldOrAgent(&'static str),
    Workspace,
    Viewport,
}

/// Paseo's labels for its own tools (protocol/paseo-tool-presentation/
/// catalog.ts): the action first, then what it acts on.
#[rustfmt::skip]
const PASEO_TOOLS: &[(&str, &str, Context)] = &[
    ("create_agent", "Create agent", Context::Field(&["title", "initialPrompt"])),
    ("send_agent_prompt", "Send message", Context::Field(&["prompt"])),
    ("get_agent_status", "Check agent", Context::Agent),
    ("get_agent_activity", "Read agent activity", Context::Agent),
    ("list_agents", "List agents", Context::None),
    ("update_agent", "Update agent", Context::FieldOrAgent("name")),
    ("set_agent_mode", "Set agent mode", Context::Field(&["modeId"])),
    ("cancel_agent", "Cancel agent", Context::Agent),
    ("archive_agent", "Archive agent", Context::Agent),
    ("kill_agent", "Kill agent", Context::Agent),
    ("list_pending_permissions", "List permissions", Context::None),
    ("respond_to_permission", "Respond to permission", Context::Agent),
    ("create_workspace", "Create workspace", Context::Workspace),
    ("list_workspaces", "List workspaces", Context::None),
    ("rename_workspace", "Rename workspace", Context::Field(&["title"])),
    ("archive_workspace", "Archive workspace", Context::Field(&["workspaceId"])),
    ("list_workspace_scripts", "List scripts", Context::None),
    ("start_workspace_script", "Start script", Context::Field(&["scriptName"])),
    ("stop_workspace_script", "Stop script", Context::Field(&["scriptName"])),
    ("create_terminal", "Create terminal", Context::Field(&["name", "command"])),
    ("list_terminals", "List terminals", Context::None),
    ("capture_terminal", "Read terminal", Context::Field(&["terminalId"])),
    ("send_terminal_keys", "Send keys", Context::Field(&["keys"])),
    ("kill_terminal", "Kill terminal", Context::Field(&["terminalId"])),
    ("create_schedule", "Create schedule", Context::Field(&["name", "cron"])),
    ("create_heartbeat", "Create heartbeat", Context::Field(&["name", "cron"])),
    ("update_schedule", "Update schedule", Context::Field(&["name", "id"])),
    ("list_schedules", "List schedules", Context::None),
    ("inspect_schedule", "Inspect schedule", Context::Field(&["id"])),
    ("schedule_logs", "Read schedule logs", Context::Field(&["id"])),
    ("run_schedule_once", "Run schedule", Context::Field(&["id"])),
    ("pause_schedule", "Pause schedule", Context::Field(&["id"])),
    ("resume_schedule", "Resume schedule", Context::Field(&["id"])),
    ("delete_schedule", "Delete schedule", Context::Field(&["id"])),
    ("delete_heartbeat", "Delete heartbeat", Context::Field(&["id"])),
    ("list_providers", "List providers", Context::None),
    ("list_models", "List models", Context::Field(&["provider"])),
    ("list_profiles", "List profiles", Context::None),
    ("inspect_provider", "Inspect provider", Context::Field(&["provider"])),
    ("browser_list_tabs", "List browser tabs", Context::None),
    ("browser_new_tab", "Open browser tab", Context::Field(&["url"])),
    ("browser_close_tab", "Close browser tab", Context::Field(&["browserId"])),
    ("browser_navigate", "Navigate", Context::Field(&["url"])),
    ("browser_back", "Go back", Context::None),
    ("browser_forward", "Go forward", Context::None),
    ("browser_reload", "Reload page", Context::None),
    ("browser_snapshot", "Snapshot page", Context::None),
    ("browser_screenshot", "Take screenshot", Context::None),
    ("browser_click", "Click element", Context::Field(&["ref"])),
    ("browser_hover", "Hover element", Context::Field(&["ref"])),
    ("browser_drag", "Drag element", Context::Field(&["sourceRef"])),
    ("browser_fill", "Fill field", Context::Field(&["ref"])),
    ("browser_select", "Select option", Context::Field(&["value", "ref"])),
    ("browser_type", "Type text", Context::Field(&["text"])),
    ("browser_keypress", "Press key", Context::Field(&["key"])),
    ("browser_upload", "Upload files", Context::Field(&["filePaths"])),
    ("browser_wait", "Wait for page", Context::Field(&["text", "url"])),
    ("browser_scroll", "Scroll page", Context::None),
    ("browser_resize", "Resize browser", Context::Viewport),
    ("browser_logs", "Read browser logs", Context::None),
    ("browser_evaluate", "Run JavaScript", Context::Field(&["function"])),
];

/// A Paseo tool call, as Paseo labels it. A message to another agent
/// names it as a message from one names its sender ("To worker", then the
/// prompt's first line), and calls on an agent link to it.
fn paseo_tool_display(leaf: &str, detail: &Value) -> ToolDisplay {
    let input = if detail["type"] == "unknown" {
        &detail["input"]
    } else {
        &Value::Null
    };
    let agent_id = input["agentId"].as_str().filter(|id| !id.is_empty());
    let link = |id: &str, title: Option<&str>| AgentLink {
        id: id.to_string(),
        title: title.map(str::to_string),
        lead: false,
    };
    let entry = PASEO_TOOLS.iter().find(|(name, ..)| *name == leaf);
    let Some((_, label, context)) = entry else {
        return ToolDisplay {
            name: humanize(leaf),
            summary: None,
            icon: ToolIcon::Paseo,
            agent: None,
        };
    };
    let field = |keys: &[&str]| keys.iter().find_map(|key| context_text(&input[*key]));
    let (name, summary, icon, agent) = match (leaf, context) {
        ("send_agent_prompt", _) if agent_id.is_some() => (
            "To".to_string(),
            field(&["prompt"]),
            ToolIcon::Send,
            agent_id.map(|id| link(id, None)),
        ),
        // Once made, the new agent can be opened.
        ("create_agent", _) => {
            let made = detail["output"]["agentId"]
                .as_str()
                .filter(|id| !id.is_empty());
            match made {
                Some(id) => (
                    label.to_string(),
                    None,
                    ToolIcon::Paseo,
                    Some(link(id, input["title"].as_str())),
                ),
                None => (
                    label.to_string(),
                    field(&["title", "initialPrompt"]),
                    ToolIcon::Paseo,
                    None,
                ),
            }
        }
        (_, Context::None) => (label.to_string(), None, ToolIcon::Paseo, None),
        (_, Context::Field(keys)) => (label.to_string(), field(keys), ToolIcon::Paseo, None),
        (_, Context::FieldOrAgent(key)) if field(&[*key]).is_some() => {
            (label.to_string(), field(&[*key]), ToolIcon::Paseo, None)
        }
        (_, Context::Agent | Context::FieldOrAgent(_)) => (
            label.to_string(),
            None,
            ToolIcon::Paseo,
            agent_id.map(|id| link(id, None)),
        ),
        (_, Context::Workspace) => (
            label.to_string(),
            field(&["title", "branchName", "branch"]).or_else(|| {
                input["path"]
                    .as_str()
                    .and_then(|p| p.rsplit(['/', '\\']).find(|s| !s.is_empty()))
                    .map(str::to_string)
            }),
            ToolIcon::Paseo,
            None,
        ),
        (_, Context::Viewport) => (
            label.to_string(),
            match (input["width"].as_u64(), input["height"].as_u64()) {
                (Some(w), Some(h)) => Some(format!("{w}×{h}")),
                _ => None,
            },
            ToolIcon::Paseo,
            None,
        ),
    };
    ToolDisplay {
        name,
        summary,
        icon,
        agent,
    }
}

/// A context field as one line: text, a number, or a list's first item
/// and how many more.
fn context_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => first_line(text),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) => {
            let head = first_line(items.first()?.as_str()?)?;
            Some(match items.len() {
                1 => head,
                n => format!("{head} +{}", n - 1),
            })
        }
        _ => None,
    }
}

/// Whether a conversation leaves this item out: a plan waiting for
/// approval shows as its permission card instead.
pub fn hidden_in_stream(kind: &crate::timeline::ItemKind) -> bool {
    match kind {
        crate::timeline::ItemKind::Tool(tool) => {
            let name = tool.name.as_str();
            name == "ExitPlanMode" || (name == "plan_approval" && tool.running())
        }
        _ => false,
    }
}

/// `web_fetch` → `Web fetch`; names with `:`, `.`, `/` or `__` stay verbatim.
pub fn humanize(name: &str) -> String {
    if name.contains(':') || name.contains('.') || name.contains('/') || name.contains("__") {
        return name.to_string();
    }
    let spaced: String = name
        .split(['_', '-', '.'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => name.to_string(),
    }
}

fn strip_cwd(path: &str, cwd: &str) -> String {
    let cwd = cwd.trim_end_matches('/');
    if !cwd.is_empty() {
        if path == cwd {
            return ".".into();
        }
        if let Some(rest) = path.strip_prefix(cwd).and_then(|r| r.strip_prefix('/')) {
            return rest.to_string();
        }
    }
    path.to_string()
}

/// Whether the row has anything to expand.
pub fn has_details(tool: &ToolCall) -> bool {
    if tool.error.is_some() {
        return true;
    }
    let d = &tool.detail;
    let present = |key: &str| match &d[key] {
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Null => false,
        _ => true,
    };
    match tool.detail_type() {
        "shell" => true,
        "read" | "write" => present("filePath") || present("content"),
        "edit" => present("filePath") || present("unifiedDiff") || present("newString"),
        "search" => {
            present("query") || present("content") || present("filePaths") || present("webResults")
        }
        "fetch" => present("url") || present("result"),
        "worktree_setup" | "sub_agent" => present("log") || present("description"),
        "plain_text" => present("text"),
        "plan" => present("text"),
        _ => {
            let meaningful = |v: &Value| match v {
                Value::Null => false,
                Value::Object(o) => !o.is_empty(),
                Value::Array(a) => !a.is_empty(),
                Value::String(s) => !s.is_empty(),
                _ => true,
            };
            meaningful(&d["input"]) || meaningful(&d["output"])
        }
    }
}

/// The expanded body as sections of (title, monospace text). The UI draws
/// them; kinds that need richer rendering (edit diffs) get their own lines.
pub fn detail_sections(tool: &ToolCall) -> Vec<(Option<&'static str>, String)> {
    let d = &tool.detail;
    let s = |key: &str| d[key].as_str().unwrap_or_default().to_string();
    let mut out: Vec<(Option<&'static str>, String)> = Vec::new();
    match tool.detail_type() {
        "shell" => {
            let mut body = format!("$ {}", s("command"));
            let output = s("output");
            if !output.is_empty() {
                body.push_str("\n\n");
                body.push_str(output.trim_end());
            }
            out.push((None, body));
        }
        "read" => {
            let start = d["offset"].as_u64().unwrap_or(1).max(1);
            let content = s("content");
            let numbered: Vec<String> = content
                .lines()
                .enumerate()
                .map(|(i, line)| format!("{:>5}  {line}", start + i as u64))
                .collect();
            out.push((None, numbered.join("\n")));
        }
        "write" => out.push((None, s("content"))),
        "edit" => out.push((None, edit_diff(d))),
        "search" => {
            for (key, title) in [
                ("content", None),
                ("filePaths", None),
                ("annotations", None),
            ] {
                match &d[key] {
                    Value::String(text) if !text.is_empty() => out.push((title, text.clone())),
                    Value::Array(items) if !items.is_empty() => out.push((
                        title,
                        items
                            .iter()
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_string)
                                    .unwrap_or_else(|| v.to_string())
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )),
                    _ => {}
                }
            }
            if let Some(results) = d["webResults"].as_array() {
                let text = results
                    .iter()
                    .map(|r| {
                        format!(
                            "{}\n{}",
                            r["title"].as_str().unwrap_or(""),
                            r["url"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if !text.is_empty() {
                    out.push((None, text));
                }
            }
        }
        "fetch" => {
            let mut body = s("url");
            let result = s("result");
            if !result.is_empty() {
                body.push_str("\n\n");
                body.push_str(&result);
            }
            out.push((None, body));
        }
        "worktree_setup" => {
            let log = s("log");
            out.push((
                None,
                if log.is_empty() {
                    format!(
                        "Preparing worktree {} at {}",
                        s("branchName"),
                        s("worktreePath")
                    )
                } else {
                    log
                },
            ));
        }
        "sub_agent" => {
            let log = s("log");
            out.push((
                None,
                if log.is_empty() {
                    format!("{}: {}", s("subAgentType"), s("description"))
                } else {
                    log
                },
            ));
        }
        "plain_text" | "plan" => out.push((None, s("text"))),
        _ => {
            for (key, title) in [("input", "Input"), ("output", "Output")] {
                match &d[key] {
                    Value::Null => {}
                    Value::Object(o) if o.is_empty() => {}
                    Value::String(text) => out.push((Some(title), text.clone())),
                    other => out.push((
                        Some(title),
                        serde_json::to_string_pretty(other).unwrap_or_default(),
                    )),
                }
            }
        }
    }
    if tool.failed()
        && let Some(error) = tool.error_text()
    {
        out.push((Some("Error"), error));
    }
    out.retain(|(_, body)| !body.trim().is_empty());
    out
}

/// A unified-diff-ish rendering of an edit: the provided diff with headers
/// dropped, else `-old` / `+new` lines.
pub fn edit_diff(detail: &Value) -> String {
    if let Some(diff) = detail["unifiedDiff"].as_str().filter(|d| !d.is_empty()) {
        return diff
            .lines()
            .filter(|line| {
                !(line.starts_with("diff --git")
                    || line.starts_with("index ")
                    || line.starts_with("--- ")
                    || line.starts_with("+++ "))
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let old = detail["oldString"].as_str().unwrap_or_default();
    let new = detail["newString"].as_str().unwrap_or_default();
    let mut out = Vec::new();
    for line in old.lines() {
        out.push(format!("-{line}"));
    }
    for line in new.lines() {
        out.push(format!("+{line}"));
    }
    out.join("\n")
}

/// `Copied 3 lines` style relative time for footers: "5s", "3m", "2h".
pub fn short_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, detail: Value) -> ToolCall {
        ToolCall {
            identity: "x".into(),
            call_id: "x".into(),
            name: name.into(),
            status: "completed".into(),
            error: None,
            detail,
            metadata: Default::default(),
            agent_message: None,
        }
    }

    #[test]
    fn agent_messages_name_their_sender() {
        let mut received = tool(
            "agent_message",
            json!({"type": "plain_text", "text": "Review it\nthen merge", "icon": "bot"}),
        );
        received.agent_message = Some(Box::new(AgentMessage {
            event: AgentEvent::Message,
            sender_id: "0f3a9c2e-77aa".into(),
            sender_title: None,
            text: "\nReview it\nthen merge".into(),
        }));
        let shown = tool_display(&received, "");
        assert_eq!(shown.name, "From");
        assert_eq!(shown.summary.as_deref(), Some("Review it"));
        assert_eq!(shown.icon, ToolIcon::Message);
        let link = shown.agent.unwrap();
        assert!(!link.lead);
        assert_eq!(link.label(None), "0f3a9c2e");
        assert_eq!(link.label(Some("planner".into())), "planner");
        assert!(has_details(&received));

        received.agent_message = Some(Box::new(AgentMessage {
            event: AgentEvent::Finished,
            sender_id: "w1".into(),
            sender_title: Some("worker".into()),
            text: "Agent w1 (worker) finished.\n\n<agent-response>\nAll green.\n</agent-response>"
                .into(),
        }));
        let shown = tool_display(&received, "");
        assert_eq!(
            (shown.name.as_str(), shown.summary.as_deref()),
            ("finished", Some("All green."))
        );
        assert_eq!(shown.agent.unwrap().label(None), "worker");
    }

    #[test]
    fn paseo_tools_read_as_paseo_labels_them() {
        let call = |name: &str, input: Value| {
            tool(
                name,
                json!({"type": "unknown", "input": input, "output": null}),
            )
        };
        let sent = tool_display(
            &call(
                "mcp__paseo__send_agent_prompt",
                json!({"agentId": "w1", "prompt": "Take the second one.\nThanks"}),
            ),
            "",
        );
        assert_eq!(sent.name, "To");
        assert_eq!(sent.summary.as_deref(), Some("Take the second one."));
        assert_eq!(sent.agent.unwrap().id, "w1");
        let status = tool_display(
            &call("paseo.get_agent_status", json!({"agentId": "w1"})),
            "",
        );
        assert_eq!(status.name, "Check agent");
        assert_eq!(status.agent.unwrap().id, "w1");
        let listed = tool_display(&call("mcp__paseo__list_agents", json!({})), "");
        assert_eq!((listed.name.as_str(), listed.agent), ("List agents", None));
        let terminal = tool_display(
            &call(
                "mcp__paseo__create_terminal",
                json!({"command": "cargo test"}),
            ),
            "",
        );
        assert_eq!(terminal.summary.as_deref(), Some("cargo test"));
        assert_eq!(
            tool_display(&call("mcp__paseo__brand_new_tool", json!({})), "").name,
            "Brand new tool"
        );
        // Not Paseo's.
        assert_eq!(paseo_leaf("mcp__github__create_issue"), None);
        assert_eq!(paseo_leaf("paseo.speak"), None);
    }

    #[test]
    fn names_and_summaries() {
        let read = tool(
            "Read",
            json!({"type": "read", "filePath": "/repo/src/a.rs"}),
        );
        let shown = tool_display(&read, "/repo");
        assert_eq!(shown.name, "Read");
        assert_eq!(shown.summary.as_deref(), Some("src/a.rs"));
        assert_eq!(humanize("web_fetch"), "Web fetch");
        assert_eq!(
            humanize("mcp__github__create_issue"),
            "mcp__github__create_issue"
        );
        let mcp = tool(
            "mcp__linear__list",
            json!({"type": "unknown", "input": {"q": 1}, "output": null}),
        );
        assert!(has_details(&mcp));
        assert_eq!(detail_sections(&mcp)[0].0, Some("Input"));
    }
}
