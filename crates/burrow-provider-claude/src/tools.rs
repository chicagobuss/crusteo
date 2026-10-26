//! Tool calls as timeline items: Paseo's `tool-call-mapper.ts` detail shapes.

use serde_json::{Value, json};

/// The `detail` of a Claude tool call, from its input and, once finished,
/// its result text.
pub fn detail(name: &str, input: &Value, output: Option<&str>) -> Value {
    let text = |key: &str| input[key].as_str().map(str::to_string);
    let mut detail = match name {
        "Bash" | "bash" | "shell" | "exec_command" => match text("command") {
            Some(command) => json!({"type": "shell", "command": command}),
            None => return unknown(input, output),
        },
        "Read" | "read" | "read_file" | "view_file" => match text("file_path") {
            Some(path) => {
                let mut detail = json!({"type": "read", "filePath": path});
                if let Some(output) = output {
                    detail["content"] = strip_gutter(output).into();
                }
                for key in ["offset", "limit"] {
                    if let Some(n) = input[key].as_u64() {
                        detail[key] = n.into();
                    }
                }
                return detail;
            }
            None => return unknown(input, output),
        },
        "Write" | "write" | "write_file" | "create_file" => match text("file_path") {
            Some(path) => {
                let mut detail = json!({"type": "write", "filePath": path});
                if let Some(content) = text("content") {
                    detail["content"] = content.into();
                }
                return detail;
            }
            None => return unknown(input, output),
        },
        "Edit" | "MultiEdit" | "edit" | "apply_patch" | "apply_diff" | "str_replace_editor" => {
            match text("file_path") {
                Some(path) => {
                    let mut detail = json!({"type": "edit", "filePath": path});
                    if let Some(old) = text("old_string") {
                        detail["oldString"] = old.into();
                    }
                    if let Some(new) = text("new_string") {
                        detail["newString"] = new.into();
                    }
                    return detail;
                }
                None => return unknown(input, output),
            }
        }
        "Grep" => match text("pattern") {
            Some(pattern) => {
                let mut detail =
                    json!({"type": "search", "query": pattern, "toolName": "grep", "numFiles": 0});
                if let Some(output) = output {
                    detail["content"] = output.into();
                }
                return detail;
            }
            None => return unknown(input, output),
        },
        "WebSearch" => match text("query") {
            Some(query) => json!({"type": "search", "query": query, "toolName": "web_search"}),
            None => return unknown(input, output),
        },
        "WebFetch" => match text("url") {
            Some(url) if output.is_none() => {
                let mut detail = json!({"type": "fetch", "url": url});
                if let Some(prompt) = text("prompt") {
                    detail["prompt"] = prompt.into();
                }
                return detail;
            }
            _ => return unknown(input, output),
        },
        "ExitPlanMode" => match text("plan") {
            Some(plan) => return json!({"type": "plan", "text": plan}),
            None => return unknown(input, output),
        },
        "Skill" => {
            let mut detail = json!({"type": "plain_text", "icon": "sparkles"});
            if let Some(skill) = text("skill").or_else(|| text("name")) {
                detail["label"] = skill.into();
            }
            if let Some(output) = output {
                detail["text"] = output.into();
            }
            return detail;
        }
        _ => return unknown(input, output),
    };
    if let Some(output) = output {
        detail["output"] = output.into();
    }
    detail
}

/// Paseo wraps an unrecognised tool's result as `{"output": <JSON or text>}`.
fn unknown(input: &Value, output: Option<&str>) -> Value {
    let output = output.map(|text| {
        json!({"output": serde_json::from_str::<Value>(text).unwrap_or_else(|_| text.into())})
    });
    json!({"type": "unknown", "input": input, "output": output})
}

/// Removes Claude's `     1→` line-number gutter from Read output.
fn strip_gutter(text: &str) -> String {
    text.lines()
        .map(|line| match line.split_once('→') {
            Some((number, rest)) if number.trim().chars().all(|c| c.is_ascii_digit()) => rest,
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A `todo` item from a TodoWrite call's input.
pub fn todo(input: &Value) -> Option<Value> {
    let todos = input["todos"].as_array()?;
    let items: Vec<Value> = todos
        .iter()
        .map(|t| {
            let status = t["status"].as_str().unwrap_or("pending");
            let mut item = json!({
                "text": t["content"].as_str().unwrap_or_default(),
                "status": status,
                "completed": status == "completed",
            });
            if let Some(form) = t["activeForm"].as_str() {
                item["activeForm"] = form.into();
            }
            if let Some(id) = t["id"].as_str() {
                item["id"] = id.into();
            }
            item
        })
        .collect();
    Some(json!({"type": "todo", "items": items}))
}

/// The text of a `tool_result` block's content: a string, or text blocks.
pub fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_match_paseo() {
        let bash = detail("Bash", &json!({"command": "ls"}), Some("a\nb"));
        assert_eq!(
            bash,
            json!({"type": "shell", "command": "ls", "output": "a\nb"})
        );
        let read = detail(
            "Read",
            &json!({"file_path": "/x"}),
            Some("     1→one\n     2→two"),
        );
        assert_eq!(read["content"], "one\ntwo");
        let edit = detail(
            "Edit",
            &json!({"file_path": "/x", "old_string": "a", "new_string": "b"}),
            None,
        );
        assert_eq!(edit["type"], "edit");
        assert_eq!(edit["newString"], "b");
        let other = detail("mcp__x__y", &json!({"q": 1}), Some("{\"ok\":true}"));
        assert_eq!(other["output"]["output"]["ok"], true);
        assert_eq!(
            detail("Bash", &json!({}), None)["type"],
            "unknown",
            "no command yet: unknown"
        );
    }
}
