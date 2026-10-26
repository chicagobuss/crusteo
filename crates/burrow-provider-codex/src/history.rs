//! A Codex thread's conversation from `thread/read`, as Paseo's
//! `loadCodexThreadHistoryTimeline` replays it.

use burrow_provider::HistoryItem;
use serde_json::Value;
use time::OffsetDateTime;
use time::macros::format_description;

use crate::tools;

/// `normalizeProviderReplayTimestamp`: an ISO string as is, a number as
/// seconds (or milliseconds when large), as `toISOString()` writes it.
fn timestamp(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let text = text.trim();
        let parsed = OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339);
        return (!text.is_empty() && parsed.is_ok()).then(|| text.to_string());
    }
    let number = value.as_f64().filter(|n| n.is_finite())?;
    let millis = if number > 1_000_000_000_000.0 {
        number
    } else {
        number * 1000.0
    };
    let at = OffsetDateTime::from_unix_timestamp_nanos((millis as i128) * 1_000_000).ok()?;
    let format =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    at.format(&format).ok()
}

fn item_timestamp(item: &Value) -> Option<String> {
    ["timestamp", "createdAt", "created_at"]
        .iter()
        .find_map(|key| timestamp(&item[*key]))
}

fn turn_timestamp(turn: &Value, user_message: bool) -> Option<String> {
    let started = timestamp(&turn["startedAt"]).or_else(|| timestamp(&turn["started_at"]));
    let completed = timestamp(&turn["completedAt"]).or_else(|| timestamp(&turn["completed_at"]));
    if user_message {
        started.or(completed)
    } else {
        completed.or(started)
    }
}

/// The timeline of a `thread/read` result (`includeTurns: true`). Replayed
/// sub-agent activity is settled: only an interruption stays open.
pub fn from_thread(thread: &Value, cwd: Option<&str>) -> Vec<HistoryItem> {
    let mut out = Vec::new();
    for turn in thread["thread"]["turns"].as_array().into_iter().flatten() {
        for raw in turn["items"].as_array().into_iter().flatten() {
            let Some(mut item) = tools::thread_item(raw, true, cwd) else {
                continue;
            };
            if tools::item_type(raw).as_deref() == Some("subAgentActivity") {
                item["status"] = if raw["kind"] == "interrupted" {
                    "canceled"
                } else {
                    "completed"
                }
                .into();
            }
            let at = item_timestamp(raw)
                .or_else(|| turn_timestamp(turn, item["type"] == "user_message"));
            out.push(HistoryItem {
                timestamp: at,
                item,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_thread_reads_back_with_turn_times() {
        let thread = json!({"thread": {"turns": [{
        "id": "t1", "startedAt": 1792453888, "completedAt": 1792453891,
        "items": [
            {"type": "userMessage", "id": "u1", "clientId": null,
             "content": [{"type": "text", "text": "hi", "text_elements": []}]},
            {"type": "agentMessage", "id": "m1", "text": "ready", "phase": "final_answer"},
            {"type": "hookPrompt", "id": "h1", "fragments": []},
        ]}]}});
        let items = from_thread(&thread, None);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].item["type"], "user_message");
        assert_eq!(
            items[0].timestamp.as_deref(),
            Some("2026-10-19T23:51:28.000Z")
        );
        assert_eq!(items[1].item["text"], "ready");
        assert_eq!(
            items[1].timestamp.as_deref(),
            Some("2026-10-19T23:51:31.000Z")
        );
    }
}
