//! Paseo's 60 ms coalescing of streamed timeline items
//! (`agent-stream-coalescer.ts`).
//!
//! Only `assistant_message`, `reasoning` and `tool_call` items are buffered.
//! The first item after a quiet period (60 ms since the last flush) goes out
//! at once; later ones wait for a single trailing 60 ms timer. A tool call
//! reaching a terminal status flushes at once, and so does any other event
//! (the caller flushes before it), which keeps ordering. On flush, adjacent
//! text entries with the same type, turn and `messageId` are concatenated;
//! each resulting entry becomes one row with its own seq. Empty text is
//! dropped.

use std::time::Duration;

use serde_json::{Map, Value};
use tokio::time::Instant;

pub const WINDOW: Duration = Duration::from_millis(60);

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub turn: Option<String>,
    pub item: Value,
}

#[derive(Debug, Default)]
pub struct Coalescer {
    entries: Vec<Entry>,
    last_flush: Option<Instant>,
    deadline: Option<Instant>,
}

pub fn coalescable(item: &Value) -> bool {
    matches!(
        item["type"].as_str(),
        Some("assistant_message" | "reasoning" | "tool_call")
    )
}

fn is_text(item: &Value) -> bool {
    matches!(
        item["type"].as_str(),
        Some("assistant_message" | "reasoning")
    )
}

impl Coalescer {
    /// When the trailing timer fires, if one is armed.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Buffers a coalescable item. Returns true when the caller should flush now.
    pub fn push(&mut self, turn: Option<String>, item: Value, now: Instant) -> bool {
        if is_text(&item) && item["text"].as_str().is_some_and(str::is_empty) {
            return false;
        }
        let terminal = item["type"] == "tool_call"
            && matches!(
                item["status"].as_str(),
                Some("completed" | "failed" | "canceled")
            );
        let existing = (item["type"] == "tool_call")
            .then(|| {
                self.entries.iter_mut().find(|entry| {
                    entry.item["type"] == "tool_call"
                        && entry.item["callId"] == item["callId"]
                        && entry.turn == turn
                })
            })
            .flatten();
        match existing {
            Some(entry) => merge_tool_call(&mut entry.item, &item),
            None => self.entries.push(Entry { turn, item }),
        }
        if terminal {
            return true;
        }
        if self.deadline.is_some() {
            return false;
        }
        if self
            .last_flush
            .is_none_or(|last| now.duration_since(last) >= WINDOW)
        {
            return true;
        }
        self.deadline = Some(now + WINDOW);
        false
    }

    /// Takes everything buffered, with adjacent text chunks joined.
    pub fn take(&mut self, now: Instant) -> Vec<Entry> {
        self.last_flush = Some(now);
        self.deadline = None;
        let mut out: Vec<Entry> = Vec::new();
        for entry in self.entries.drain(..) {
            if let Some(last) = out.last_mut()
                && is_text(&entry.item)
                && last.item["type"] == entry.item["type"]
                && last.turn == entry.turn
                && last.item.get("messageId") == entry.item.get("messageId")
            {
                let joined = format!(
                    "{}{}",
                    last.item["text"].as_str().unwrap_or_default(),
                    entry.item["text"].as_str().unwrap_or_default()
                );
                last.item["text"] = joined.into();
                continue;
            }
            out.push(entry);
        }
        out
    }
}

/// `mergeToolCallItems`: later fields win, an `unknown` detail never replaces
/// a known one, metadata merges, and a completed or canceled call has no error.
pub fn merge_tool_call(into: &mut Value, update: &Value) {
    let (Some(target), Some(source)) = (into.as_object_mut(), update.as_object()) else {
        return;
    };
    for (key, value) in source {
        match key.as_str() {
            "detail" => {
                let known = target.get("detail").is_some_and(|d| d["type"] != "unknown");
                if value["type"] != "unknown" || !known {
                    target.insert(key.clone(), value.clone());
                }
            }
            "metadata" => {
                let mut merged = target
                    .get("metadata")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_else(Map::new);
                if let Some(extra) = value.as_object() {
                    merged.extend(extra.clone());
                }
                target.insert(key.clone(), Value::Object(merged));
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
    if matches!(
        target.get("status").and_then(Value::as_str),
        Some("completed" | "canceled")
    ) {
        target.insert("error".into(), Value::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(t: &str) -> Value {
        json!({"type": "assistant_message", "text": t, "messageId": "m1"})
    }

    #[test]
    fn the_first_chunk_leaves_at_once_then_chunks_join() {
        let start = Instant::now();
        let mut c = Coalescer::default();
        assert!(c.push(Some("t".into()), text("a"), start), "leading edge");
        let first = c.take(start);
        assert_eq!(first.len(), 1);

        let soon = start + Duration::from_millis(10);
        assert!(!c.push(Some("t".into()), text("b"), soon));
        assert!(!c.push(Some("t".into()), text("c"), soon));
        assert!(
            !c.push(Some("t".into()), text(""), soon),
            "empty text dropped"
        );
        assert_eq!(c.deadline(), Some(soon + WINDOW));
        let joined = c.take(soon + WINDOW);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].item["text"], "bc");
    }

    #[test]
    fn tool_updates_merge_and_a_terminal_status_flushes() {
        let start = Instant::now();
        let mut c = Coalescer::default();
        c.take(start);
        let soon = start + Duration::from_millis(5);
        let running = json!({"type": "tool_call", "callId": "c1", "name": "Bash",
            "detail": {"type": "shell", "command": "ls"}, "status": "running", "error": null});
        let unknown = json!({"type": "tool_call", "callId": "c1", "name": "Bash",
            "detail": {"type": "unknown", "input": {}}, "status": "running", "error": null});
        assert!(!c.push(None, running, soon));
        assert!(!c.push(None, unknown, soon));
        let done = json!({"type": "tool_call", "callId": "c1", "status": "completed",
            "error": {"message": "stale"}});
        assert!(c.push(None, done, soon));
        let rows = c.take(soon);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].item["detail"]["type"], "shell");
        assert_eq!(rows[0].item["status"], "completed");
        assert!(rows[0].item["error"].is_null());
    }
}
