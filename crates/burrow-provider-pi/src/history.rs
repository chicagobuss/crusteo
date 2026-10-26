//! pi's own session files:
//! `<sessions>/--<cwd>--/<timestamp>_<session id>.jsonl`.
//!
//! Burrow keeps durable timelines of its own, so this is only needed for
//! a session it did not record. The mapping follows Paseo's
//! `pi/history-mapper.ts`, which maps the messages pi replays: user text,
//! assistant text, reasoning and tool calls, tool results completing their
//! calls, `!` shell runs and extension messages. The conversation is the
//! branch that ends at the file's last entry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use burrow_provider::HistoryItem;
use serde_json::{Value, json};

use crate::tools::{self, Call};

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if path == "~" => home(),
        None => PathBuf::from(path),
    }
}

/// `$PI_CODING_AGENT_SESSION_DIR`, else `$PI_CODING_AGENT_DIR/sessions`,
/// else `~/.pi/agent/sessions`.
pub fn sessions_dir(env: &[(String, String)]) -> PathBuf {
    let var = |name: &str| {
        env.iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(name).ok())
            .filter(|v| !v.trim().is_empty())
    };
    if let Some(dir) = var("PI_CODING_AGENT_SESSION_DIR") {
        return expand(dir.trim());
    }
    let agent = var("PI_CODING_AGENT_DIR").map_or_else(
        || home().join(".pi").join("agent"),
        |dir| expand(dir.trim()),
    );
    agent.join("sessions")
}

/// pi's directory name for a working directory.
pub fn encode(cwd: &str) -> String {
    let trimmed = cwd.trim_start_matches(['/', '\\']);
    let body: String = trimmed
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{body}--")
}

/// Where the session file is: under the working directory's folder first,
/// then directly in the sessions folder (a custom one is flat), then in any
/// folder.
pub fn locate(sessions: &Path, cwd: &str, session_id: &str) -> Option<PathBuf> {
    let suffix = format!("_{session_id}.jsonl");
    let find = |dir: &Path| -> Option<PathBuf> {
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(&suffix))
            })
    };
    find(&sessions.join(encode(cwd)))
        .or_else(|| find(sessions))
        .or_else(|| {
            std::fs::read_dir(sessions)
                .ok()?
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .find_map(|entry| find(&entry.path()))
        })
}

pub fn read(sessions: &Path, cwd: &str, session_id: &str) -> Vec<HistoryItem> {
    match locate(sessions, cwd, session_id).and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(text) => parse(&text),
        None => Vec::new(),
    }
}

/// The entries on the branch that ends at the last entry, oldest first.
fn branch(text: &str) -> Vec<Value> {
    let entries: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] != "session" && entry["id"].is_string())
        .collect();
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| e["id"].as_str().map(|id| (id, e)))
        .collect();
    let mut path = Vec::new();
    let mut next = entries.last();
    while let Some(entry) = next {
        path.push(entry.clone());
        if path.len() > entries.len() {
            break; // a cycle; files are not supposed to have one
        }
        next = entry["parentId"]
            .as_str()
            .and_then(|id| by_id.get(id).copied());
    }
    path.reverse();
    path
}

/// The text blocks of a user or custom message, joined.
pub fn user_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// An extension message as Paseo shows it: a finished synthetic tool row.
pub fn custom_message(message: &Value, text: &str, call_id: &str) -> Value {
    let custom_type = message["customType"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("custom-message");
    let mut metadata = json!({"synthetic": true, "customType": custom_type});
    if let Some(details) = message.get("details") {
        metadata["details"] = details.clone();
    }
    json!({"type": "tool_call", "callId": call_id, "name": custom_type, "status": "completed",
           "detail": {"type": "plain_text", "text": text}, "metadata": metadata, "error": null})
}

/// Turns session entries into timeline items, in order.
pub fn parse(text: &str) -> Vec<HistoryItem> {
    let mut items: Vec<HistoryItem> = Vec::new();
    // Tool calls waiting for their result: id to (index in items, call).
    let mut open: HashMap<String, (usize, Call)> = HashMap::new();
    let (mut assistants, mut customs) = (0, 0);
    for entry in branch(text) {
        let timestamp = entry["timestamp"].as_str().map(str::to_string);
        let push = |items: &mut Vec<HistoryItem>, item: Value| {
            items.push(HistoryItem {
                timestamp: timestamp.clone(),
                item,
            })
        };
        let custom;
        let message = match entry["type"].as_str() {
            Some("message") => &entry["message"],
            Some("custom_message") => {
                custom = json!({"role": "custom", "content": entry["content"],
                    "customType": entry["customType"], "display": entry["display"]});
                &custom
            }
            Some("compaction") => {
                push(
                    &mut items,
                    json!({"type": "compaction", "status": "completed"}),
                );
                continue;
            }
            _ => continue,
        };
        match message["role"].as_str() {
            Some("user") => {
                let text = user_text(&message["content"]);
                if !text.is_empty() {
                    let mut item = json!({"type": "user_message", "text": text});
                    if let Some(id) = entry["id"].as_str() {
                        item["messageId"] = id.into();
                    }
                    push(&mut items, item);
                }
            }
            Some("custom") => {
                let text = user_text(&message["content"]);
                if message["display"] != false && !text.is_empty() {
                    customs += 1;
                    push(
                        &mut items,
                        custom_message(message, &text, &format!("pi-custom-{customs}")),
                    );
                }
            }
            Some("assistant") => {
                assistants += 1;
                let message_id = message["responseId"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map_or_else(
                        || format!("pi-history-assistant-{assistants}"),
                        str::to_string,
                    );
                for block in message["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            if let Some(text) = block["text"].as_str().filter(|t| !t.is_empty()) {
                                push(
                                    &mut items,
                                    json!({"type": "assistant_message", "text": text,
                                            "messageId": message_id}),
                                );
                            }
                        }
                        Some("thinking") => {
                            if let Some(text) = block["thinking"].as_str().filter(|t| !t.is_empty())
                            {
                                push(&mut items, json!({"type": "reasoning", "text": text}));
                            }
                        }
                        Some("toolCall") => {
                            let id = block["id"].as_str().unwrap_or_default().to_string();
                            let call = Call {
                                name: block["name"].as_str().unwrap_or_default().to_string(),
                                args: block["arguments"].clone(),
                            };
                            push(
                                &mut items,
                                tools::item(&id, &call, "running", &Value::Null, Value::Null),
                            );
                            open.insert(id, (items.len() - 1, call));
                        }
                        _ => {}
                    }
                }
            }
            Some("toolResult") => {
                let id = message["toolCallId"].as_str().unwrap_or_default();
                let (at, call) = match open.remove(id) {
                    Some((at, call)) => (Some(at), call),
                    None => (
                        None,
                        Call {
                            name: message["toolName"].as_str().unwrap_or_default().to_string(),
                            args: Value::Null,
                        },
                    ),
                };
                let mut raw = json!({"content": message["content"]});
                if let Some(details) = message.get("details") {
                    raw["details"] = details.clone();
                }
                let result = tools::result(&raw);
                let item = if message["isError"] == true {
                    let error = tools::result_text(&result)
                        .unwrap_or_else(|| "Tool call failed".to_string());
                    tools::item(id, &call, "failed", &result, error.into())
                } else {
                    tools::item(id, &call, "completed", &result, Value::Null)
                };
                // The result completes the call in place.
                match at {
                    Some(at) => items[at].item = item,
                    None => push(&mut items, item),
                }
            }
            Some("bashExecution") => {
                let detail = json!({"type": "shell", "command": message["command"],
                    "output": message["output"], "exitCode": message["exitCode"]});
                let status = if message["cancelled"] == true {
                    "canceled"
                } else {
                    "completed"
                };
                push(
                    &mut items,
                    json!({"type": "tool_call",
                    "callId": format!("pi-bash-{}", message["timestamp"]), "name": "bash",
                    "status": status, "detail": detail, "error": null}),
                );
            }
            _ => {}
        }
    }
    // Calls the session never finished were cut off.
    for (_, (at, _)) in open {
        items[at].item["status"] = "canceled".into();
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_working_directories_like_pi() {
        assert_eq!(encode("/home/user/my.project"), "--home-user-my.project--");
        assert_eq!(encode("/tmp"), "--tmp--");
    }

    #[test]
    fn parses_the_branch_ending_at_the_last_entry() {
        let text = [
            r#"{"type":"session","version":3,"id":"s1","timestamp":"t0","cwd":"/w"}"#,
            r#"{"type":"message","id":"a","parentId":null,"timestamp":"t1","message":{"role":"system","content":""}}"#,
            r#"{"type":"message","id":"b","parentId":"a","timestamp":"t2","message":{"role":"user","content":[{"type":"text","text":"list files"}]}}"#,
            r#"{"type":"message","id":"c","parentId":"b","timestamp":"t3","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"ls"}}],"responseId":"r1"}}"#,
            r#"{"type":"message","id":"d","parentId":"c","timestamp":"t4","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"a\nb"}],"isError":false}}"#,
            r#"{"type":"message","id":"x","parentId":"b","timestamp":"t5","message":{"role":"assistant","content":[{"type":"text","text":"abandoned"}]}}"#,
            r#"{"type":"message","id":"e","parentId":"d","timestamp":"t6","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"Two files."}]}}"#,
        ]
        .join("\n");
        let items = parse(&text);
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| i.item["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "user_message",
                "tool_call",
                "reasoning",
                "assistant_message"
            ]
        );
        assert_eq!(items[0].item["messageId"], "b");
        assert_eq!(items[1].item["status"], "completed");
        assert_eq!(items[1].item["detail"]["output"], "a\nb");
        assert_eq!(items[3].item["messageId"], "pi-history-assistant-2");
        assert_eq!(items[3].timestamp.as_deref(), Some("t6"));
    }
}
