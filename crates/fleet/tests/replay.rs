//! Replays a captured Claude session: the live stream must reduce to the same
//! rows as the daemon's own projected tail page.

use fleet::timeline::{ItemKind, Timeline};
use serde_json::Value;

fn frames() -> Vec<Value> {
    include_str!("fixtures/claude-turns.jsonl")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|f| f["dir"] == "in")
        .map(|f| f["frame"]["message"].clone())
        .filter(|m| m.is_object())
        .collect()
}

fn summary(tl: &Timeline) -> Vec<String> {
    tl.items()
        .map(|item| match &item.kind {
            ItemKind::User { text, .. } => format!("user:{text}"),
            ItemKind::Assistant { text, .. } => format!("assistant:{text}"),
            ItemKind::Thought { text, .. } => format!("thought:{text}"),
            ItemKind::Tool(tool) => format!(
                "tool:{}:{}:{}",
                tool.call_id,
                tool.status,
                tool.detail["type"].as_str().unwrap_or("")
            ),
            ItemKind::Todo { items, .. } => format!("todo:{}", items.len()),
            ItemKind::Notification { message, .. } => format!("note:{message}"),
            ItemKind::Compaction { .. } => "compaction".into(),
        })
        .collect()
}

#[test]
fn live_stream_matches_projected_page() {
    let frames = frames();
    let pages: Vec<&Value> = frames
        .iter()
        .filter(|m| m["type"] == "fetch_agent_timeline_response")
        .collect();
    let empty_tail = pages[0];
    let full_tail = pages[1];
    let agent = empty_tail["payload"]["agentId"]
        .as_str()
        .unwrap()
        .to_string();

    let mut live = Timeline::default();
    live.apply_page(&empty_tail["payload"]);
    for frame in &frames {
        if frame["type"] == "agent_stream" && frame["payload"]["agentId"] == agent.as_str() {
            live.apply_live(&frame["payload"]);
        }
        if frame == full_tail {
            break;
        }
    }

    let mut fetched = Timeline::default();
    fetched.apply_page(&full_tail["payload"]);

    let live_rows = summary(&live);
    let fetched_rows = summary(&fetched);
    assert!(!fetched_rows.is_empty());
    assert_eq!(live_rows, fetched_rows);
}

#[test]
fn before_page_prepends() {
    let frames = frames();
    let pages: Vec<&Value> = frames
        .iter()
        .filter(|m| m["type"] == "fetch_agent_timeline_response")
        .collect();
    let mut full = Timeline::default();
    full.apply_page(&pages[1]["payload"]);
    // Rebuild from an `after`-only window: start from seq 12+ by trimming.
    let before = &pages[3]["payload"];
    assert_eq!(before["direction"], "before");
    let mut partial = Timeline::default();
    let mut tail = pages[1]["payload"].clone();
    let entries: Vec<Value> = tail["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["seqStart"].as_u64().unwrap() >= 12)
        .cloned()
        .collect();
    let first = entries[0]["seqStart"].as_u64().unwrap();
    tail["entries"] = Value::Array(entries);
    tail["startCursor"]["seq"] = first.into();
    tail["hasOlder"] = true.into();
    partial.apply_page(&tail);
    let count = partial.len();
    partial.apply_page(before);
    assert!(partial.len() > count, "before page should prepend rows");
}
