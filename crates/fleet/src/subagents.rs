//! Provider subagents: the sidechains an agent's provider runs inside it,
//! such as Claude Code's Task and Explore agents. The daemon describes them
//! with `agent.provider_subagents.*` messages and serves each one's
//! timeline in the same rows as an agent's, so a [`crate::timeline::Timeline`]
//! can hold it once the payloads are reshaped here.

use serde_json::{Value, json};

use crate::Bucket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Completed,
    Failed,
    Canceled,
}

impl Status {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }
}

/// One provider subagent, as its descriptor last described it.
#[derive(Debug, Clone, PartialEq)]
pub struct Subagent {
    pub id: String,
    pub parent_agent_id: String,
    /// The subagent that started this one, when it is nested.
    pub parent_subagent_id: Option<String>,
    pub provider: String,
    /// The kind of subagent, e.g. "Explore".
    pub title: Option<String>,
    /// Its task.
    pub description: Option<String>,
    pub status: Status,
    pub created_at: String,
    pub updated_at: String,
    /// Compact context the provider chose to show.
    pub subtitle: Option<String>,
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

impl Subagent {
    pub fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            id: value["id"].as_str()?.to_string(),
            parent_agent_id: value["parentAgentId"].as_str()?.to_string(),
            parent_subagent_id: text(&value["parentSubagentId"]),
            provider: value["provider"].as_str().unwrap_or_default().to_string(),
            title: text(&value["title"]),
            description: text(&value["description"]),
            status: value["status"]
                .as_str()
                .and_then(Status::parse)
                .unwrap_or(Status::Running),
            created_at: value["createdAt"].as_str().unwrap_or_default().to_string(),
            updated_at: value["updatedAt"].as_str().unwrap_or_default().to_string(),
            subtitle: text(&value["subtitle"]),
        })
    }

    /// Applies an upsert. The daemon's upserts are sticky: a field left out
    /// keeps its value.
    pub fn merge(&mut self, value: &Value) {
        let object = value.as_object();
        let has = |key: &str| object.is_some_and(|object| object.contains_key(key));
        if has("parentSubagentId") {
            self.parent_subagent_id = text(&value["parentSubagentId"]);
        }
        if let Some(provider) = value["provider"].as_str() {
            self.provider = provider.to_string();
        }
        if has("title") {
            self.title = text(&value["title"]);
        }
        if has("description") {
            self.description = text(&value["description"]);
        }
        if let Some(status) = value["status"].as_str().and_then(Status::parse) {
            self.status = status;
        }
        if let Some(created) = value["createdAt"].as_str() {
            self.created_at = created.to_string();
        }
        if let Some(updated) = value["updatedAt"].as_str() {
            self.updated_at = updated.to_string();
        }
        if has("subtitle") {
            self.subtitle = text(&value["subtitle"]);
        }
    }

    /// What it is doing: its task, else its kind.
    pub fn label(&self) -> String {
        self.description
            .clone()
            .or_else(|| self.title.clone())
            .filter(|label| !label.eq_ignore_ascii_case("new agent"))
            .unwrap_or_else(|| "Subagent".to_string())
    }

    /// The muted text beside the label: the provider's context, else its
    /// kind when the label is its task.
    pub fn detail(&self) -> Option<String> {
        self.subtitle
            .clone()
            .or_else(|| self.description.as_ref().and(self.title.clone()))
    }

    pub fn bucket(&self) -> Bucket {
        match self.status {
            Status::Running => Bucket::Running,
            Status::Failed => Bucket::Failed,
            Status::Completed | Status::Canceled => Bucket::Done,
        }
    }

    pub fn running(&self) -> bool {
        self.status == Status::Running
    }
}

/// Upserts `value` into a parent's subagents, kept oldest first. Returns
/// the subagent's status before, if it was known.
pub fn upsert(list: &mut Vec<Subagent>, value: &Value) -> Option<Option<Status>> {
    let id = value["id"].as_str()?;
    if let Some(existing) = list.iter_mut().find(|sub| sub.id == id) {
        let before = existing.status;
        existing.merge(value);
        return Some(Some(before));
    }
    let subagent = Subagent::from_value(value)?;
    let at = list.partition_point(|other| {
        (other.created_at.as_str(), other.id.as_str())
            <= (subagent.created_at.as_str(), subagent.id.as_str())
    });
    list.insert(at, subagent);
    Some(None)
}

/// A list response's subagents, oldest first.
pub fn list(payload: &Value) -> Vec<Subagent> {
    let mut list: Vec<Subagent> = payload["subagents"]
        .as_array()
        .map(|subagents| subagents.iter().filter_map(Subagent::from_value).collect())
        .unwrap_or_default();
    list.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    list
}

/// A timeline get response, reshaped as an agent timeline page for
/// [`crate::timeline::Timeline::apply_page`].
pub fn page(payload: &Value) -> Value {
    let rows: Vec<Value> = payload["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let seq = row["seq"].as_u64().unwrap_or(0);
                    let mut entry = row.clone();
                    entry["seqEnd"] = json!(row["seqEnd"].as_u64().unwrap_or(seq));
                    entry["seqStart"] = json!(row["seqStart"].as_u64().unwrap_or(seq));
                    entry["provider"] = payload["provider"].clone();
                    entry
                })
                .collect()
        })
        .unwrap_or_default();
    let bound = |pick: fn(&Value) -> Option<u64>, first: bool| {
        let seqs = rows.iter().filter_map(pick);
        if first { seqs.min() } else { seqs.max() }
    };
    let epoch = payload["epoch"].clone();
    let start = payload["startCursor"]["seq"]
        .as_u64()
        .or_else(|| bound(|row| row["seqStart"].as_u64(), true));
    let end = payload["endCursor"]["seq"]
        .as_u64()
        .or_else(|| bound(|row| row["seqEnd"].as_u64(), false));
    let mut page = json!({
        "direction": payload["direction"],
        "reset": payload["reset"],
        "epoch": epoch,
        "entries": rows,
        "window": payload["window"],
        "hasOlder": payload["hasOlder"],
        "hasNewer": payload["hasNewer"],
        "error": payload["error"],
    });
    if let Some(start) = start {
        page["startCursor"] = json!({ "epoch": epoch, "seq": start });
    }
    if let Some(end) = end {
        page["endCursor"] = json!({ "epoch": epoch, "seq": end });
    }
    page
}

/// A `timeline` update, reshaped as an `agent_stream` payload for
/// [`crate::timeline::Timeline::apply_live`].
pub fn live(payload: &Value) -> Value {
    json!({
        "event": {
            "type": "timeline",
            "item": payload["item"],
            "provider": payload["provider"],
        },
        "timestamp": payload["timestamp"],
        "seq": payload["seq"],
        "epoch": payload["epoch"],
    })
}

/// The stream event that brings a subagent's turn in line with `status`.
pub fn status_event(status: Status, timestamp: &str) -> Value {
    let event = match status {
        Status::Running => json!({ "type": "turn_started" }),
        Status::Completed => json!({ "type": "turn_completed" }),
        Status::Failed => json!({ "type": "turn_failed", "error": "Subagent failed" }),
        Status::Canceled => json!({ "type": "turn_canceled" }),
    };
    json!({ "event": event, "timestamp": timestamp })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(id: &str, created: &str) -> Value {
        json!({
            "id": id, "parentAgentId": "agt", "provider": "claude",
            "title": "Explore", "description": "Find auth code", "status": "running",
            "createdAt": created, "updatedAt": created, "toolCallId": id,
        })
    }

    #[test]
    fn upserts_keep_creation_order_and_omitted_fields() {
        let mut list = Vec::new();
        assert_eq!(upsert(&mut list, &descriptor("b", "2")), Some(None));
        assert_eq!(upsert(&mut list, &descriptor("a", "1")), Some(None));
        assert_eq!(
            list.iter().map(|sub| sub.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        let before = upsert(&mut list, &json!({ "id": "a", "status": "completed" }));
        assert_eq!(before, Some(Some(Status::Running)));
        assert_eq!(list[0].status, Status::Completed);
        assert_eq!(list[0].description.as_deref(), Some("Find auth code"));
        upsert(&mut list, &json!({ "id": "a", "description": null }));
        assert_eq!(list[0].label(), "Explore");
    }

    #[test]
    fn labels_prefer_the_task_and_skip_placeholders() {
        let mut sub = Subagent::from_value(&descriptor("a", "1")).unwrap();
        assert_eq!(sub.label(), "Find auth code");
        assert_eq!(sub.detail().as_deref(), Some("Explore"));
        sub.description = Some("New agent".into());
        sub.title = None;
        assert_eq!(sub.label(), "Subagent");
    }

    #[test]
    fn pages_become_agent_timeline_pages() {
        let page = page(&json!({
            "direction": "tail", "epoch": "e1", "provider": "claude", "reset": false,
            "window": { "minSeq": 1, "maxSeq": 3, "nextSeq": 4 },
            "hasOlder": true, "hasNewer": false, "error": null,
            "rows": [
                { "item": { "type": "assistant_message", "text": "hi" }, "timestamp": "t", "seq": 2 },
                { "item": { "type": "assistant_message", "text": "yo" }, "timestamp": "t", "seq": 3, "seqStart": 3, "seqEnd": 3 },
            ],
        }));
        assert_eq!(page["entries"][0]["seqEnd"], 2);
        assert_eq!(page["startCursor"], json!({ "epoch": "e1", "seq": 2 }));
        assert_eq!(page["endCursor"], json!({ "epoch": "e1", "seq": 3 }));
        let mut timeline = crate::timeline::Timeline::default();
        timeline.apply_page(&page);
        assert_eq!(timeline.len(), 1, "consecutive assistant text merges");
        assert!(timeline.has_older);
        assert_eq!(timeline.cursor.as_ref().map(|c| c.end_seq), Some(3));
    }

    #[test]
    fn live_rows_follow_the_page_they_extend() {
        let mut timeline = crate::timeline::Timeline::default();
        timeline.apply_page(&page(&json!({
            "direction": "tail", "epoch": "e1", "reset": false, "rows": [
                { "item": { "type": "user_message", "text": "go" }, "timestamp": "t", "seq": 1 },
            ],
            "window": { "minSeq": 1, "maxSeq": 1, "nextSeq": 2 },
        })));
        let follow = timeline.apply_live(&live(&json!({
            "kind": "timeline", "parentAgentId": "agt", "subagentId": "a", "provider": "claude",
            "item": { "type": "assistant_message", "text": "done" },
            "timestamp": "t", "seq": 2, "epoch": "e1",
        })));
        assert_eq!(follow, crate::timeline::Follow::Nothing);
        assert_eq!(timeline.len(), 2);
        timeline.apply_live(&status_event(Status::Running, "t"));
        assert!(timeline.is_running());
        timeline.apply_live(&status_event(Status::Completed, "t"));
        assert!(!timeline.is_running());
    }
}
