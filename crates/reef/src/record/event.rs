//! `fleet-event/1`: one record per timeline entry, the same shape whatever
//! the provider. An entry that grows (a reply streaming, a tool call
//! finishing) is sent again under the same `id`, so sinks keep the latest.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub const VERSION: &str = "fleet-event/1";

/// Tool input and output strings are cut to this many characters.
const TOOL_TEXT_MAX: usize = 4000;

/// Who a record is about, as the directory has it now.
#[derive(Debug, Clone)]
pub struct Context {
    pub host: String,
    pub agent: Value,
    pub workspace: Value,
}

impl Context {
    pub fn agent_id(&self) -> &str {
        self.agent["id"].as_str().unwrap_or_default()
    }

    pub fn labels(&self) -> impl Iterator<Item = &str> {
        (self.workspace["labels"].as_array().into_iter().flatten()).filter_map(Value::as_str)
    }
}

/// A record and where in the agent's timeline it came from.
#[derive(Debug, Clone)]
pub struct Record {
    pub seq_start: u64,
    pub seq_end: u64,
    pub kind: String,
    pub turn: Option<String>,
    /// The text a turn's summary keeps: a prompt's, a reply's, a tool's
    /// name.
    pub text: String,
    pub value: Value,
}

/// Where records say they came from.
pub fn source() -> String {
    format!("reef@{}", env!("CARGO_PKG_VERSION"))
}

/// The dedupe key: the same prompt or reply recorded by Reef and by a hook
/// gets the same key. Times are left out (the hook's never match Burrow's);
/// a sink pairs records whose keys match and whose times are close.
/// SHA-256 cut to 128 bits, so a hook in any language computes it too.
pub fn key(agent: &str, kind: &str, text: &str) -> String {
    let hash = Sha256::digest(format!("{agent}\n{kind}\n{}", normalize(text)));
    hash[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// What both sides hash: no `<USER_REQUEST>` wrapper (Antigravity's), no
/// quote marks a hook puts before each line, no `\r`, no surrounding space.
pub fn normalize(text: &str) -> String {
    let text = text
        .replace('\r', "")
        .replace("<USER_REQUEST>", "")
        .replace("</USER_REQUEST>", "");
    let lines: Vec<&str> = (text.lines())
        .map(|line| {
            line.strip_prefix("> ")
                .or(line.strip_prefix('>'))
                .unwrap_or(line)
        })
        .collect();
    lines.join("\n").trim().to_string()
}

pub fn from_entry(context: &Context, epoch: &str, entry: &Value) -> Option<Record> {
    let seq_start = entry["seqStart"].as_u64()?;
    let seq_end = entry["seqEnd"].as_u64().unwrap_or(seq_start);
    let turn = entry["turnId"].as_str().map(str::to_string);
    let raw = &entry["item"];
    // A message from another agent, unwrapped as the apps show it.
    let projected =
        (raw["type"] == "user_message").then(|| fleet::agent_messages::project_user_message(raw));
    let item = projected.flatten().unwrap_or_else(|| raw.clone());
    let text_of = |field: &str| item[field].as_str().unwrap_or_default().replace('\r', "");
    let mut fields = Map::new();
    let (kind, text) = match item["type"].as_str()? {
        "user_message" => ("prompt", text_of("text")),
        "assistant_message" => ("reply", text_of("text")),
        "reasoning" => ("thought", text_of("text")),
        "tool_call" => {
            let detail = &item["detail"];
            let input = match &detail["input"] {
                Value::Null => without(detail, "output"),
                input => input.clone(),
            };
            let name = item["name"].as_str().unwrap_or_default().to_string();
            let mut tool = json!({ "name": name, "status": item["status"],
                "input": clip(input), "output": clip(detail["output"].clone()) });
            if !item["error"].is_null() {
                tool["error"] = item["error"].clone();
            }
            // The key tells calls apart by what they were asked to do.
            let asked = serde_json::to_string(&tool["input"]).unwrap_or_default();
            fields.insert("tool".into(), tool);
            ("tool_call", format!("{name} {asked}"))
        }
        "todo" => {
            fields.insert("todo".into(), item["items"].clone());
            ("todo", String::new())
        }
        "error" => {
            fields.insert("level".into(), json!("error"));
            ("error", text_of("message"))
        }
        "notification" => {
            fields.insert("level".into(), item["level"].clone());
            ("notice", text_of("message"))
        }
        "compaction" => {
            fields.insert("status".into(), item["status"].clone());
            ("compaction", String::new())
        }
        _ => return None,
    };
    let mut value = base(context, &format!("{epoch}/{seq_start}"), kind, &turn);
    value["time"] = entry["timestamp"].clone();
    value["key"] = json!(key(context.agent_id(), kind, &text));
    if kind != "tool_call" && !text.is_empty() {
        value["text"] = json!(text);
    }
    value.as_object_mut().expect("an object").extend(fields);
    let summary = match kind {
        "tool_call" => item["name"].as_str().unwrap_or_default().to_string(),
        _ => text,
    };
    Some(Record {
        seq_start,
        seq_end,
        kind: kind.into(),
        turn,
        text: summary,
        value,
    })
}

/// The fields every record has; `at` is what follows `host/agent/` in its id.
pub fn base(context: &Context, at: &str, kind: &str, turn: &Option<String>) -> Value {
    json!({
        "v": VERSION,
        "id": format!("{}/{}/{at}", context.host, context.agent_id()),
        "source": source(),
        "host": context.host,
        "agent": context.agent,
        "workspace": context.workspace,
        "turn": turn,
        "kind": kind,
    })
}

fn without(value: &Value, field: &str) -> Value {
    let mut value = value.clone();
    if let Some(fields) = value.as_object_mut() {
        fields.remove(field);
    }
    value
}

/// Long strings cut short, at a character boundary.
fn clip(mut value: Value) -> Value {
    fn walk(value: &mut Value) {
        match value {
            Value::String(text) if text.chars().count() > TOOL_TEXT_MAX => {
                let cut: String = text.chars().take(TOOL_TEXT_MAX).collect();
                *text = format!("{cut}…");
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            Value::Object(fields) => fields.values_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut value);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            host: "box".into(),
            agent: json!({ "id": "a1", "title": "Worker", "provider": "claude" }),
            workspace: json!({ "id": "w1", "labels": ["shop"] }),
        }
    }

    fn entry(seq: u64, item: Value) -> Value {
        json!({ "seqStart": seq, "seqEnd": seq + 2, "turnId": "t1",
                "timestamp": "2026-11-01T10:00:00Z", "item": item })
    }

    #[test]
    fn a_prompt_and_a_tool_call_as_records() {
        let prompt = entry(
            4,
            json!({ "type": "user_message", "text": "Fix the build\r\n" }),
        );
        let prompt = from_entry(&context(), "e1", &prompt).unwrap();
        assert_eq!(prompt.value["id"], "box/a1/e1/4");
        assert_eq!(prompt.value["kind"], "prompt");
        assert_eq!(prompt.value["text"], "Fix the build\n");
        assert_eq!(prompt.value["time"], "2026-11-01T10:00:00Z");
        assert_eq!((prompt.seq_start, prompt.seq_end), (4, 6));

        let call = entry(
            7,
            json!({ "type": "tool_call", "name": "Bash", "status": "completed",
            "error": null, "detail": { "type": "shell", "input": { "command": "cargo build" },
            "output": "x".repeat(5000) } }),
        );
        let call = from_entry(&context(), "e1", &call).unwrap();
        assert_eq!(call.value["tool"]["input"]["command"], "cargo build");
        assert_eq!(
            call.value["tool"]["output"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            4001
        );
        assert!(call.value.get("text").is_none() && call.text == "Bash");
        // Kinds Reef doesn't know are left out.
        assert!(from_entry(&context(), "e1", &entry(9, json!({ "type": "x" }))).is_none());
    }

    #[test]
    fn the_key_is_what_a_hook_would_compute_for_the_same_prompt() {
        let reef = key("a1", "prompt", "<USER_REQUEST>\nFix it\n</USER_REQUEST>");
        let hook = key("a1", "prompt", "> Fix it  ");
        assert_eq!(reef, hook);
        assert_eq!(reef.len(), 32);
        assert_ne!(reef, key("a2", "prompt", "Fix it"));
        assert_ne!(reef, key("a1", "reply", "Fix it"));
    }
}
