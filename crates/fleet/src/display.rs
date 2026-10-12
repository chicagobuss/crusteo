//! How Paseo labels tool calls (protocol/src/tool-call-display.ts):
//! a display name, a one-line summary, an icon, and whether the row expands.

use serde_json::Value;

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
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDisplay {
    pub name: String,
    pub summary: Option<String>,
    pub icon: ToolIcon,
}

pub fn tool_display(tool: &ToolCall, cwd: &str) -> ToolDisplay {
    let detail = &tool.detail;
    let lower = tool.name.to_lowercase();
    if lower == "thinking" {
        return ToolDisplay {
            name: "Thinking".into(),
            summary: None,
            icon: ToolIcon::Brain,
        };
    }
    if lower.starts_with("mcp__paseo__") || lower.starts_with("paseo.") {
        let leaf = tool.name.rsplit(['.', '_']).next().unwrap_or(&tool.name);
        return ToolDisplay {
            name: leaf.to_string(),
            summary: None,
            icon: ToolIcon::Paseo,
        };
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
        }
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
