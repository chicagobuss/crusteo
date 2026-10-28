//! What a phone sees, worked out from the fleet: agent rows, the needs-you
//! list, a filtered agent list, and a conversation as grouped turns with
//! their footers. All pure, so the rules are tested here and Reef only
//! decides when to run them.
//!
//! Each view is a list of keyed parts. [`diff_rows`] and [`diff_turns`]
//! turn two versions of a view into the small delta the protocol sends.

use std::collections::{HashMap, HashSet};

use fleet::display::{ToolIcon, hidden_in_stream, tool_display};
use fleet::images::ImageFile;
use fleet::timeline::{ItemKind, TaskActivity, Timeline, Turn, finished_turns, turn_footer};
use serde_json::{Map, Value, json};

use crate::proto::{Limits, clip};

pub use crate::lists::{agent_key, agent_row, hosts, needs_you, state};

/// The part of a keyed list a window shows.
pub fn window(rows: &[(String, Value)], offset: usize, limit: usize) -> Vec<(String, Value)> {
    rows.iter().skip(offset).take(limit).cloned().collect()
}

/// What changed from `old` to `new`, as delta fields: `upsert` (rows new or
/// changed), `remove` (keys gone) and `order` (every key, only when the
/// order or membership changed). None when nothing changed.
pub fn diff_rows(old: &[(String, Value)], new: &[(String, Value)]) -> Option<Map<String, Value>> {
    let before: HashMap<&str, &Value> = old.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let after: HashSet<&str> = new.iter().map(|(k, _)| k.as_str()).collect();
    let upsert: Vec<Value> = new
        .iter()
        .filter(|(key, value)| before.get(key.as_str()) != Some(&value))
        .map(|(_, value)| value.clone())
        .collect();
    let remove: Vec<&str> = old
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| !after.contains(k))
        .collect();
    let reordered = old.len() != new.len() || old.iter().zip(new).any(|(a, b)| a.0 != b.0);
    if upsert.is_empty() && remove.is_empty() && !reordered {
        return None;
    }
    let mut delta = Map::new();
    if !upsert.is_empty() {
        delta.insert("upsert".into(), Value::Array(upsert));
    }
    if !remove.is_empty() {
        delta.insert("remove".into(), json!(remove));
    }
    if reordered {
        let order: Vec<&str> = new.iter().map(|(k, _)| k.as_str()).collect();
        delta.insert("order".into(), json!(order));
    }
    Some(delta)
}

// ---- conversations ----------------------------------------------------

/// One turn as sent: its key (its first item's id), who started it, its
/// footer once finished, how many of its first items the window leaves
/// out, and its items in order.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnView {
    pub key: String,
    pub head: Map<String, Value>,
    pub items: Vec<(String, Value)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConversationView {
    pub turns: Vec<TurnView>,
    /// Earlier turns exist, loaded or not.
    pub has_older: bool,
    /// The turn running now: when it started, if known.
    pub working: Option<Value>,
    pub asks: Vec<Value>,
}

/// For every item, the index of its turn's first item; see
/// [`fleet::timeline::turn_starts`].
fn starts(items: &[&fleet::timeline::Item]) -> Vec<usize> {
    fleet::timeline::turn_starts(items)
}

/// Where a window that shows `turns` more turns before `from` begins, as an
/// item index, taking at most `budget` visible items. A turn too big for
/// the budget on its own shows its last `budget` items.
pub fn extend_back(
    starts: &[usize],
    visible: &[bool],
    from: usize,
    turns: usize,
    budget: usize,
) -> usize {
    let mut anchor = from.min(starts.len());
    let mut taken_turns = 0;
    let mut taken_items = 0;
    while anchor > 0 && taken_turns < turns {
        let start = starts[anchor - 1];
        let count = visible[start..anchor].iter().filter(|v| **v).count();
        if taken_items + count > budget {
            if taken_items == 0 {
                let mut cut = anchor;
                let mut n = 0;
                while cut > start && n < budget {
                    cut -= 1;
                    if visible[cut] {
                        n += 1;
                    }
                }
                return cut;
            }
            break;
        }
        anchor = start;
        taken_items += count;
        taken_turns += 1;
    }
    anchor
}

/// Where a conversation's window begins: the item `anchor` names, else
/// (first open, or after the timeline was replaced) the start of the last
/// `turns` turns.
pub fn anchor_index(
    timeline: &Timeline,
    anchor: Option<&str>,
    turns: usize,
    limits: &Limits,
) -> usize {
    let items: Vec<_> = timeline.items().collect();
    if let Some(anchor) = anchor
        && let Some(index) = items.iter().position(|item| item.id == anchor)
    {
        return index;
    }
    let starts = starts(&items);
    let visible: Vec<bool> = items.iter().map(|i| !hidden_in_stream(&i.kind)).collect();
    extend_back(&starts, &visible, items.len(), turns, limits.items_max)
}

/// The earliest index a window ending at the timeline's end may start at
/// and hold at most `budget` shown items: a turn's start where one fits,
/// else inside the last turn.
pub fn window_floor(timeline: &Timeline, budget: usize) -> usize {
    let items: Vec<_> = timeline.items().collect();
    let starts = starts(&items);
    let visible: Vec<bool> = items.iter().map(|i| !hidden_in_stream(&i.kind)).collect();
    extend_back(&starts, &visible, items.len(), usize::MAX, budget)
}

/// The item id at `index`, to remember a window by.
pub fn item_id(timeline: &Timeline, index: usize) -> Option<String> {
    timeline.items().nth(index).map(|item| item.id.clone())
}

/// `turns` more turns before the window starting at `index`.
pub fn older_index(timeline: &Timeline, index: usize, turns: usize, limits: &Limits) -> usize {
    let items: Vec<_> = timeline.items().collect();
    let starts = starts(&items);
    let visible: Vec<bool> = items.iter().map(|i| !hidden_in_stream(&i.kind)).collect();
    extend_back(&starts, &visible, index, turns, limits.items_max)
}

/// The conversation from item `from` on, grouped by turn.
pub fn conversation(
    timeline: &Timeline,
    host: &str,
    cwd: &str,
    from: usize,
    limits: &Limits,
    titles: Titles,
) -> ConversationView {
    let items: Vec<_> = timeline.items().collect();
    let starts = starts(&items);
    let running = timeline.is_running();
    let finished: HashMap<usize, usize> = finished_turns(&items, running).into_iter().collect();
    let mut turns: Vec<TurnView> = Vec::new();
    for (index, item) in items.iter().enumerate().skip(from) {
        let start = starts[index];
        let turn_start = start.max(from);
        if turns.is_empty() || index == turn_start {
            let mut head = Map::new();
            let by = if matches!(items[start].kind, ItemKind::User { .. }) {
                "user"
            } else {
                "agent"
            };
            head.insert("by".into(), json!(by));
            if start < from {
                let clipped = (start..from)
                    .filter(|i| !hidden_in_stream(&items[*i].kind))
                    .count();
                head.insert("clipped".into(), json!(clipped));
            }
            if let Some(end) = finished.get(&start) {
                let footer = turn_footer(&items, start, *end);
                let mut value = json!({ "at": footer.ended_at });
                if let Some(secs) = footer.duration_secs {
                    value["secs"] = json!(secs);
                }
                head.insert("footer".into(), value);
            }
            turns.push(TurnView {
                key: items[start].id.clone(),
                head,
                items: Vec::new(),
            });
        }
        if hidden_in_stream(&item.kind) {
            continue;
        }
        if let Some(turn) = turns.last_mut() {
            turn.items.push((
                item.id.clone(),
                item_json(item, host, cwd, limits.text_max, titles),
            ));
        }
    }
    // A turn whose items are all hidden shows nothing.
    turns.retain(|turn| !turn.items.is_empty());
    let has_older =
        timeline.has_older || (from > 0 && (0..from).any(|i| !hidden_in_stream(&items[i].kind)));
    let working = match &timeline.turn {
        Turn::Open { started_at, .. } => Some(match started_at {
            Some(at) => json!({ "since": at }),
            None => json!({}),
        }),
        Turn::Idle => None,
    };
    let asks = timeline
        .permissions
        .iter()
        .map(|p| ask_json(&p.request, limits))
        .collect();
    ConversationView {
        turns,
        has_older,
        working,
        asks,
    }
}

fn icon_name(icon: ToolIcon) -> &'static str {
    match icon {
        ToolIcon::Terminal => "terminal",
        ToolIcon::Eye => "eye",
        ToolIcon::Pencil => "pencil",
        ToolIcon::Search => "search",
        ToolIcon::Bot => "bot",
        ToolIcon::Wrench => "wrench",
        ToolIcon::Brain => "brain",
        ToolIcon::Sparkles => "sparkles",
        ToolIcon::Mic => "mic",
        ToolIcon::Paseo => "paseo",
        ToolIcon::Message => "message",
        ToolIcon::Send => "send",
        ToolIcon::Finished => "finished",
        ToolIcon::Failed => "failed",
        ToolIcon::Permission => "permission",
        ToolIcon::Closed => "closed",
    }
}

fn put_text(item: &mut Value, text: &str, max: usize) {
    let (shown, cut) = clip(text, max);
    item["text"] = json!(shown);
    if cut {
        item["clipped"] = json!(true);
    }
}

/// How an image is named in `/image/<key>`: its content hash, so a phone
/// can keep it for good.
pub fn image_key(content_key: u64) -> String {
    format!("{content_key:016x}")
}

/// The key of an image a daemon keeps as a file, for `/image/<key>`: its
/// host and path, so one path on two hosts is two images.
pub fn file_key(host: &str, file: &ImageFile) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    ("kept file", host, &file.cwd, &file.path).hash(&mut hash);
    hash.finish()
}

/// The files a reply's Markdown images name, each destination with its
/// file.
pub fn reply_files(text: &str, cwd: &str) -> Vec<(String, ImageFile)> {
    let cwd = Some(cwd).filter(|cwd| !cwd.is_empty());
    let (files, sources) = fleet::images::markdown_files(text, cwd);
    (sources.into_iter())
        .map(|(source, file)| (source, files[file].clone()))
        .collect()
}

/// The title of an agent on the conversation's host, when it knows it.
pub type Titles<'a> = &'a dyn Fn(&str) -> Option<String>;

/// One timeline item, compact. Tool calls and thoughts carry only their
/// one-line form; `detail` fetches the rest.
pub fn item_json(
    item: &fleet::timeline::Item,
    host: &str,
    cwd: &str,
    text_max: usize,
    titles: Titles,
) -> Value {
    let mut out = json!({ "id": item.id, "at": item.timestamp });
    match &item.kind {
        ItemKind::User {
            text,
            images,
            image_files,
            ..
        } => {
            out["kind"] = json!("user");
            put_text(&mut out, text, text_max);
            // Each image is fetched on its own, from `/image/<key>`: those
            // sent from here while Reef holds them, else the files the
            // daemon keeps them in, as crusteo shows a prompt's images.
            let keys: Vec<String> = if images.is_empty() {
                (image_files.iter())
                    .map(|file| image_key(file_key(host, file)))
                    .collect()
            } else {
                images.iter().map(|i| image_key(i.content_key())).collect()
            };
            if !keys.is_empty() {
                out["images"] = json!(keys.len());
                out["image_keys"] = json!(keys);
            }
        }
        ItemKind::Assistant { text, .. } => {
            out["kind"] = json!("assistant");
            put_text(&mut out, text, text_max);
            // The key for each Markdown image a file on the host holds.
            let sources: Map<String, Value> = (reply_files(text, cwd).into_iter())
                .map(|(source, file)| (source, json!(image_key(file_key(host, &file)))))
                .collect();
            if !sources.is_empty() {
                out["image_sources"] = Value::Object(sources);
            }
        }
        ItemKind::Thought { text, loading } => {
            out["kind"] = json!("thought");
            out["chars"] = json!(text.chars().count());
            if *loading {
                out["loading"] = json!(true);
            }
            out["detail"] = json!(!text.trim().is_empty());
        }
        ItemKind::Tool(tool) => {
            let shown = tool_display(tool, cwd);
            out["kind"] = json!("tool");
            out["name"] = json!(shown.name);
            if let Some(summary) = shown.summary {
                out["summary"] = json!(clip(&summary, 200).0);
            }
            out["status"] = json!(tool.status);
            out["icon"] = json!(icon_name(shown.icon));
            out["detail"] = json!(fleet::display::has_details(tool));
            // Who sent it, or whom the call is about: a link when the host
            // knows the agent.
            if let Some(link) = shown.agent {
                let known = titles(&link.id);
                out["agent"] = json!({
                    "id": link.id,
                    "title": clip(&link.label(known.clone()), 200).0,
                    "lead": link.lead,
                    "known": known.is_some(),
                });
            }
        }
        ItemKind::Todo {
            items, activity, ..
        } => {
            out["kind"] = json!("todo");
            out["label"] = json!(match activity {
                TaskActivity::Created(1) => "Created 1 task".to_string(),
                TaskActivity::Created(n) => format!("Created {n} tasks"),
                TaskActivity::Added(task) => format!("Added {task}"),
                TaskActivity::Started(task) => format!("Started {task}"),
                TaskActivity::Completed(task) => format!("Completed {task}"),
            });
            out["done"] = json!(items.iter().filter(|t| t.status == "completed").count());
            out["total"] = json!(items.len());
        }
        ItemKind::Notification { level, message } => {
            out["kind"] = json!("notice");
            out["level"] = json!(level);
            put_text(&mut out, message, text_max);
        }
        ItemKind::Compaction {
            loading,
            trigger,
            pre_tokens,
        } => {
            out["kind"] = json!("compaction");
            out["label"] = json!(if *loading {
                "Compacting...".to_string()
            } else {
                match (trigger.as_deref(), pre_tokens) {
                    (Some("auto"), _) => "Context automatically compacted".into(),
                    (Some("manual"), _) => "Context manually compacted".into(),
                    (_, Some(tokens)) => format!("Context compacted ({}K tokens)", tokens / 1000),
                    _ => "Context compacted".into(),
                }
            });
        }
    }
    out
}

/// A pending permission or question, enough to show and, later, answer.
pub fn ask_json(request: &Value, limits: &Limits) -> Value {
    let kind = request["kind"].as_str().unwrap_or("tool");
    let mut ask = json!({
        "id": request["id"].as_str().unwrap_or_default(),
        "kind": kind,
        "title": request["title"].as_str().or(request["name"].as_str()).unwrap_or("Permission Required"),
    });
    if let Some(name) = request["name"].as_str() {
        ask["name"] = json!(name);
    }
    if let Some(description) = request["description"].as_str() {
        ask["description"] = json!(clip(description, limits.text_max).0);
    }
    // The buttons: the provider's own, else Deny and Accept as crusteo
    // and the Paseo app offer them. A question has its form instead.
    let actions: Vec<Value> = match request["actions"].as_array() {
        Some(actions) if !actions.is_empty() => actions
            .iter()
            .map(|a| json!({ "id": a["id"], "label": a["label"], "behavior": a["behavior"] }))
            .collect(),
        _ if kind == "question" => Vec::new(),
        _ => vec![
            json!({ "id": "reject", "label": "Deny", "behavior": "deny" }),
            json!({
                "id": "accept",
                "label": if kind == "plan" { "Implement" } else { "Accept" },
                "behavior": "allow",
            }),
        ],
    };
    if !actions.is_empty() {
        ask["actions"] = Value::Array(actions);
    }
    if kind == "plan" {
        let plan = request["metadata"]["planText"]
            .as_str()
            .or(request["input"]["plan"].as_str())
            .unwrap_or_default();
        ask["plan"] = json!(clip(plan, limits.text_max).0);
    }
    if fleet::questions::is_question(request)
        && let Some(parsed) = fleet::questions::parse(&request["input"])
    {
        let questions: Vec<Value> = parsed
            .iter()
            .map(|q| {
                json!({
                    "header": q.header,
                    "question": q.question,
                    "options": q.options.iter().map(|o| json!({ "label": o.label, "description": o.description })).collect::<Vec<_>>(),
                    "multi": q.multi_select,
                    "other": q.shows_text(),
                })
            })
            .collect();
        ask["questions"] = Value::Array(questions);
        ask["dismiss"] = json!(fleet::questions::dismiss_label(&parsed));
    } else if let Some(detail) = request.get("detail").filter(|d| d.is_object()) {
        // What the tool would do, in a line: "Shell · cargo test".
        let tool = fleet::timeline::ToolCall {
            identity: String::new(),
            call_id: String::new(),
            name: request["name"].as_str().unwrap_or_default().to_string(),
            status: "running".into(),
            error: None,
            detail: detail.clone(),
            metadata: Map::new(),
            agent_message: None,
        };
        let shown = tool_display(&tool, "");
        if let Some(summary) = shown.summary {
            ask["summary"] = json!(clip(&summary, 400).0);
        }
    }
    ask
}

/// The expanded body of a tool call or thought, each section cut to the
/// budget.
pub fn detail(timeline: &Timeline, item: &str, limits: &Limits) -> Option<Value> {
    let item = timeline.items().find(|i| i.id == item)?;
    let sections: Vec<(Option<&str>, String)> = match &item.kind {
        ItemKind::Tool(tool) => fleet::display::detail_sections(tool),
        ItemKind::Thought { text, .. } => vec![(None, text.clone())],
        _ => return None,
    };
    let sections: Vec<Value> = sections
        .iter()
        .map(|(title, body)| {
            let (text, cut) = clip(body, limits.detail_max);
            let mut section = json!({ "text": text });
            if let Some(title) = title {
                section["title"] = json!(title);
            }
            if cut {
                section["clipped"] = json!(true);
            }
            section
        })
        .collect();
    Some(json!({ "item": item.id, "sections": sections }))
}

/// The whole turn as a new one is sent.
pub fn turn_json(turn: &TurnView) -> Value {
    let mut out = turn.head.clone();
    out.insert("key".into(), json!(turn.key));
    let items: Vec<Value> = turn.items.iter().map(|(_, v)| v.clone()).collect();
    out.insert("items".into(), Value::Array(items));
    Value::Object(out)
}

/// What changed in one turn: head fields that changed (null when gone),
/// items new or changed (text that only grew goes as `append`), and the
/// item order when it changed. None when nothing did.
pub fn diff_turn(old: &TurnView, new: &TurnView) -> Option<Value> {
    let mut patch = Map::new();
    for (key, value) in &new.head {
        if old.head.get(key) != Some(value) {
            patch.insert(key.clone(), value.clone());
        }
    }
    for key in old.head.keys() {
        if !new.head.contains_key(key) {
            patch.insert(key.clone(), Value::Null);
        }
    }
    let before: HashMap<&str, &Value> = old.items.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let mut items = Vec::new();
    for (id, value) in &new.items {
        match before.get(id.as_str()) {
            Some(previous) if *previous == value => {}
            Some(previous) => items.push(append_or_whole(previous, value)),
            None => items.push(value.clone()),
        }
    }
    if !items.is_empty() {
        patch.insert("items".into(), Value::Array(items));
    }
    let reordered = old.items.len() != new.items.len()
        || old.items.iter().zip(&new.items).any(|(a, b)| a.0 != b.0);
    if reordered {
        let order: Vec<&str> = new.items.iter().map(|(k, _)| k.as_str()).collect();
        patch.insert("order".into(), json!(order));
    }
    if patch.is_empty() {
        return None;
    }
    patch.insert("key".into(), json!(new.key));
    Some(Value::Object(patch))
}

/// A streamed message grows at its end: send only what was added.
fn append_or_whole(old: &Value, new: &Value) -> Value {
    let (Some(old_text), Some(new_text)) = (old["text"].as_str(), new["text"].as_str()) else {
        return new.clone();
    };
    let suffix = match new_text.strip_prefix(old_text) {
        Some(suffix) if !suffix.is_empty() => suffix,
        _ => return new.clone(),
    };
    let same_otherwise = |a: &Value, b: &Value| {
        let strip = |v: &Value| {
            let mut v = v.clone();
            if let Some(o) = v.as_object_mut() {
                o.remove("text");
                o.remove("at");
            }
            v
        };
        strip(a) == strip(b)
    };
    if !same_otherwise(old, new) {
        return new.clone();
    }
    json!({ "id": new["id"], "append": suffix, "at": new["at"] })
}

/// The delta fields for a conversation's turns: `turns` (patches, or whole
/// new turns), `remove` and `order`, as [`diff_rows`] has them.
pub fn diff_turns(old: &[TurnView], new: &[TurnView]) -> Map<String, Value> {
    let before: HashMap<&str, &TurnView> = old.iter().map(|t| (t.key.as_str(), t)).collect();
    let after: HashSet<&str> = new.iter().map(|t| t.key.as_str()).collect();
    let mut patches = Vec::new();
    for turn in new {
        match before.get(turn.key.as_str()) {
            Some(previous) => patches.extend(diff_turn(previous, turn)),
            None => patches.push(turn_json(turn)),
        }
    }
    let mut delta = Map::new();
    if !patches.is_empty() {
        delta.insert("turns".into(), Value::Array(patches));
    }
    let remove: Vec<&str> = old
        .iter()
        .map(|t| t.key.as_str())
        .filter(|k| !after.contains(k))
        .collect();
    if !remove.is_empty() {
        delta.insert("remove".into(), json!(remove));
    }
    if old.len() != new.len() || old.iter().zip(new).any(|(a, b)| a.key != b.key) {
        let order: Vec<&str> = new.iter().map(|t| t.key.as_str()).collect();
        delta.insert("order".into(), json!(order));
    }
    delta
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn row_diffs_carry_only_what_changed() {
        let a = |n: u64| ("a".to_string(), json!({"key": "a", "n": n}));
        let b = ("b".to_string(), json!({"key": "b"}));
        assert_eq!(diff_rows(&[a(1), b.clone()], &[a(1), b.clone()]), None);
        let delta = diff_rows(&[a(1), b.clone()], &[a(2), b.clone()]).unwrap();
        assert_eq!(delta["upsert"], json!([{"key": "a", "n": 2}]));
        assert!(!delta.contains_key("order"));
        let delta = diff_rows(&[a(1), b.clone()], &[b.clone(), a(1)]).unwrap();
        assert_eq!(delta["order"], json!(["b", "a"]));
        assert!(!delta.contains_key("upsert"));
        let delta = diff_rows(&[a(1), b.clone()], &[b]).unwrap();
        assert_eq!(delta["remove"], json!(["a"]));
    }

    #[test]
    fn a_window_takes_whole_turns_within_the_budget() {
        // Turns: [0..2], [3..3], [4..9].
        let starts = [0, 0, 0, 3, 4, 4, 4, 4, 4, 4];
        let visible = [true; 10];
        assert_eq!(extend_back(&starts, &visible, 10, 1, 60), 4);
        assert_eq!(extend_back(&starts, &visible, 10, 2, 60), 3);
        assert_eq!(extend_back(&starts, &visible, 10, 9, 60), 0);
        // The budget stops before a turn that does not fit...
        assert_eq!(extend_back(&starts, &visible, 10, 3, 8), 3);
        // ...and a lone turn larger than it shows its end.
        assert_eq!(extend_back(&starts, &visible, 10, 1, 4), 6);
        // Going further back from a cut turn first shows the rest of it.
        assert_eq!(extend_back(&starts, &visible, 6, 1, 60), 4);
    }

    #[test]
    fn agent_rows_name_the_agent_and_whether_it_opens() {
        let mut timeline = Timeline::default();
        let received = |id: &str, event: &str, sender: Value| {
            json!({"type": "tool_call", "callId": format!("paseo-agent-message:{id}"),
                "name": "agent_message", "status": "completed", "error": null,
                "detail": {"type": "plain_text", "text": "Agent p1 (x) finished.", "icon": "bot"},
                "agentMessage": {"event": event, "sender": sender, "text": "Re-check refunds\ntoo"}})
        };
        let entry = |seq: u64, item: Value| json!({"item": item, "turnId": format!("t{seq}"), "timestamp": "t", "seqStart": seq, "seqEnd": seq});
        timeline.apply_page(&json!({"direction": "tail", "epoch": "e", "window": {"maxSeq": 3},
            "startCursor": {"seq": 1}, "endCursor": {"seq": 3}, "entries": [
            entry(1, received("m1", "message", json!({"id": "p1", "title": "old title"}))),
            entry(2, received("m2", "closed", json!({"id": "gone", "title": "Gone agent"}))),
            entry(3, json!({"type": "tool_call", "callId": "c3", "name": "mcp__paseo__send_agent_prompt",
                "status": "completed", "error": null,
                "detail": {"type": "unknown", "input": {"agentId": "p1", "prompt": "Thanks"}, "output": null}})),
        ]}));
        let titles = |id: &str| (id == "p1").then(|| "planner".to_string());
        let view = conversation(&timeline, "example-a", "", 0, &Limits::default(), &titles);
        let items: Vec<&Value> = view
            .turns
            .iter()
            .flat_map(|turn| turn.items.iter().map(|(_, item)| item))
            .collect();
        assert_eq!(items[0]["name"], "From");
        assert_eq!(items[0]["summary"], "Re-check refunds");
        assert_eq!(
            items[0]["agent"],
            json!({"id": "p1", "title": "planner", "lead": false, "known": true})
        );
        assert_eq!(items[1]["name"], "was closed");
        assert_eq!(
            items[1]["agent"],
            json!({"id": "gone", "title": "Gone agent", "lead": true, "known": false})
        );
        assert_eq!(
            (&items[2]["name"], &items[2]["agent"]["title"]),
            (&json!("To"), &json!("planner"))
        );
    }

    #[test]
    fn grown_text_goes_as_an_append() {
        let old = TurnView {
            key: "t".into(),
            head: Map::new(),
            items: vec![(
                "m".into(),
                json!({"id": "m", "kind": "assistant", "text": "Hello", "at": "1"}),
            )],
        };
        let mut new = old.clone();
        new.items[0].1 = json!({"id": "m", "kind": "assistant", "text": "Hello world", "at": "2"});
        new.head.insert("footer".into(), json!({"at": "2"}));
        let patch = diff_turn(&old, &new).unwrap();
        assert_eq!(
            patch["items"],
            json!([{"id": "m", "append": " world", "at": "2"}])
        );
        assert_eq!(patch["footer"], json!({"at": "2"}));
        assert!(patch.get("order").is_none());
        assert_eq!(diff_turn(&new, &new), None);
    }
}
