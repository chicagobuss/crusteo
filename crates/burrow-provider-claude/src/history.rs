//! Claude Code's own transcripts: `<config>/projects/<cwd>/<session>.jsonl`.
//!
//! Burrow keeps durable timelines of its own, so this is only needed for
//! a session Burrow did not record (one resumed from elsewhere). The
//! mapping follows Paseo's history replay: sidechain, meta and compact-summary
//! lines are skipped; user text becomes `user_message`, assistant blocks
//! become text, reasoning and tool calls, and a `tool_result` completes the
//! call it answers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use burrow_provider::HistoryItem;
use serde_json::{Value, json};

use crate::tools;

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".claude")
}

/// Claude Code's project directory name for a working directory.
pub fn encode(cwd: &str) -> String {
    let encoded: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if encoded.chars().count() <= 200 {
        return encoded;
    }
    let mut hash: i32 = 0;
    for unit in cwd.encode_utf16() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(i32::from(unit as i16) & 0xffff);
    }
    let head: String = encoded.chars().take(200).collect();
    format!("{head}-{}", base36(u64::from(hash.unsigned_abs())))
}

fn base36(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Where the transcript is: the encoded working directory first, then any
/// project directory that has the session.
pub fn locate(config: &Path, cwd: &str, session_id: &str) -> Option<PathBuf> {
    let file = format!("{session_id}.jsonl");
    let real = std::fs::canonicalize(cwd)
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| cwd.to_string());
    for dir in [encode(&real), encode(cwd)] {
        let path = config.join("projects").join(dir).join(&file);
        if path.is_file() {
            return Some(path);
        }
    }
    std::fs::read_dir(config.join("projects"))
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(&file))
        .find(|path| path.is_file())
}

pub fn read(config: &Path, cwd: &str, session_id: &str) -> Vec<HistoryItem> {
    match locate(config, cwd, session_id).and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(text) => parse(&text),
        None => Vec::new(),
    }
}

fn noise(text: &str) -> bool {
    let text = text.trim();
    text.is_empty()
        || text.starts_with("[Request interrupted")
        || text == "No response requested."
        || text.starts_with("<local-command-stdout>")
        || text.starts_with("<command-name>")
}

/// Turns transcript lines into timeline items, in order.
pub fn parse(text: &str) -> Vec<HistoryItem> {
    let mut items: Vec<HistoryItem> = Vec::new();
    // Tool calls waiting for their result: id → (index in items, name, input).
    let mut open: HashMap<String, (usize, String, Value)> = HashMap::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry["isSidechain"] == true || entry["isCompactSummary"] == true {
            continue;
        }
        let timestamp = entry["timestamp"].as_str().map(str::to_string);
        let push = |items: &mut Vec<HistoryItem>, item: Value| {
            items.push(HistoryItem {
                timestamp: timestamp.clone(),
                item,
            });
        };
        let uuid = entry["uuid"].as_str().unwrap_or_default().to_string();
        match entry["type"].as_str() {
            Some("user") => {
                let content = &entry["message"]["content"];
                let mut texts = Vec::new();
                match content {
                    Value::String(text) => texts.push(text.clone()),
                    Value::Array(blocks) => {
                        for block in blocks {
                            match block["type"].as_str() {
                                Some("text") => {
                                    texts.push(block["text"].as_str().unwrap_or_default().into())
                                }
                                Some("tool_result") => {
                                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                                    if let Some((at, name, input)) = open.remove(id) {
                                        let output = tools::result_text(&block["content"]);
                                        let failed = block["is_error"].as_bool().unwrap_or(false);
                                        let (status, detail, error) = if failed {
                                            (
                                                "failed",
                                                tools::detail(&name, &input, None),
                                                json!({"message": output}),
                                            )
                                        } else {
                                            (
                                                "completed",
                                                tools::detail(&name, &input, Some(&output)),
                                                Value::Null,
                                            )
                                        };
                                        items[at].item = json!({"type": "tool_call",
                                            "callId": id, "name": name, "detail": detail,
                                            "status": status, "error": error});
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if entry["isMeta"] == true {
                    continue;
                }
                let text = texts.join("\n");
                if !noise(&text) {
                    push(
                        &mut items,
                        json!({"type": "user_message", "text": text, "messageId": uuid}),
                    );
                }
            }
            Some("assistant") => {
                for block in entry["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let text = block["text"].as_str().unwrap_or_default();
                            if !text.is_empty() {
                                push(
                                    &mut items,
                                    json!({"type": "assistant_message", "text": text,
                                           "messageId": uuid}),
                                );
                            }
                        }
                        Some("thinking") => {
                            if let Some(text) = block["thinking"].as_str().filter(|t| !t.is_empty())
                            {
                                push(&mut items, json!({"type": "reasoning", "text": text}));
                            }
                        }
                        Some("tool_use") => {
                            let id = block["id"].as_str().unwrap_or_default().to_string();
                            let name = block["name"].as_str().unwrap_or_default().to_string();
                            let input = block["input"].clone();
                            push(
                                &mut items,
                                json!({"type": "tool_call", "callId": id, "name": name,
                                       "detail": tools::detail(&name, &input, None),
                                       "status": "running", "error": null}),
                            );
                            if name == "TodoWrite"
                                && let Some(todo) = tools::todo(&input)
                            {
                                push(&mut items, todo);
                            }
                            let at = items
                                .iter()
                                .rposition(|i| i.item["callId"] == id.as_str())
                                .unwrap_or(items.len() - 1);
                            open.insert(id, (at, name, input));
                        }
                        _ => {}
                    }
                }
            }
            Some("system") if entry["subtype"] == "compact_boundary" => {
                push(
                    &mut items,
                    json!({"type": "compaction", "status": "completed"}),
                );
            }
            _ => {}
        }
    }
    // Calls the transcript never finished were cut off.
    for (_, (at, _, _)) in open {
        items[at].item["status"] = "canceled".into();
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_working_directories_like_claude_code() {
        assert_eq!(encode("/home/me/my.project"), "-home-me-my-project");
        let long = format!("/{}", "a".repeat(300));
        let encoded = encode(&long);
        assert!(encoded.len() > 200 && encoded.starts_with("-aaa"));
    }

    #[test]
    fn parses_a_transcript() {
        let text = [
            r#"{"type":"user","uuid":"u1","timestamp":"t1","message":{"role":"user","content":"list files"}}"#,
            r#"{"type":"assistant","uuid":"a1","timestamp":"t2","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","uuid":"u2","timestamp":"t3","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a\nb","is_error":false}]}}"#,
            r#"{"type":"assistant","uuid":"a2","timestamp":"t4","message":{"content":[{"type":"text","text":"Two files."}]}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"content":"ignored"}}"#,
            r#"{"type":"user","uuid":"u3","message":{"content":"[Request interrupted by user]"}}"#,
        ]
        .join("\n");
        let items = parse(&text);
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| i.item["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["user_message", "tool_call", "assistant_message"]);
        assert_eq!(items[1].item["status"], "completed");
        assert_eq!(items[1].item["detail"]["output"], "a\nb");
        assert_eq!(items[0].timestamp.as_deref(), Some("t1"));
    }
}
