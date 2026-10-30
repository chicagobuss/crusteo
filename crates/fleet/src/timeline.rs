//! One agent's conversation, reduced from fetched pages and live
//! `agent_stream` frames into display rows.
//!
//! This ports the Paseo app's reducer (docs/protocol/agent-timeline.md §2.6,
//! §2.7) in its "simplified" form: a committed `tail`, a streaming `head`,
//! a seq gate on live events, and replace/prepend for fetched pages. The
//! daemon folds deltas into the same rows it serves on fetch, so hydrating a
//! page and streaming the deltas end in the same picture.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use crate::agent_messages::AgentMessage;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub identity: String,
    pub call_id: String,
    pub name: String,
    pub status: String,
    pub error: Option<Value>,
    pub detail: Value,
    pub metadata: Map<String, Value>,
    /// Set when the call stands for a message from another agent, or a
    /// notice about one, with who sent it.
    pub agent_message: Option<Box<AgentMessage>>,
}

impl ToolCall {
    pub fn detail_type(&self) -> &str {
        self.detail["type"].as_str().unwrap_or("unknown")
    }

    pub fn running(&self) -> bool {
        self.status == "running"
    }

    pub fn failed(&self) -> bool {
        self.status == "failed"
    }

    /// Error text the way the app shows it: a string, else `error.content`,
    /// else `error.message`, else pretty JSON.
    pub fn error_text(&self) -> Option<String> {
        let error = self.error.as_ref()?;
        if let Some(text) = error.as_str() {
            return Some(text.to_string());
        }
        if let Some(text) = error["content"].as_str() {
            return Some(text.to_string());
        }
        if let Some(text) = error["message"].as_str() {
            return Some(text.to_string());
        }
        serde_json::to_string_pretty(error).ok()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TodoEntry {
    pub text: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TaskActivity {
    Created(usize),
    Added(String),
    Started(String),
    Completed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ItemKind {
    User {
        text: String,
        images: Vec<crate::prompt::ImageAttachment>,
        /// Images the daemon kept as files, read on demand; see
        /// [`crate::images`].
        image_files: Vec<crate::images::ImageFile>,
        seq: Option<u64>,
        client_message_id: Option<String>,
        message_id: Option<String>,
    },
    Assistant {
        text: String,
        message_id: Option<String>,
    },
    Thought {
        text: String,
        loading: bool,
    },
    /// Boxed: a call is several times the size of any other item.
    Tool(Box<ToolCall>),
    Todo {
        provider: String,
        items: Vec<TodoEntry>,
        activity: TaskActivity,
    },
    Notification {
        level: String,
        message: String,
    },
    Compaction {
        loading: bool,
        trigger: Option<String>,
        pre_tokens: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub turn_id: Option<String>,
    pub timestamp: String,
    pub kind: ItemKind,
}

impl Item {
    fn streamable(&self) -> bool {
        matches!(
            self.kind,
            ItemKind::Assistant { .. } | ItemKind::Thought { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub start_seq: u64,
    pub end_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Turn {
    #[default]
    Idle,
    Open {
        turn_id: Option<String>,
        started_at: Option<String>,
    },
}

/// What the caller should do after feeding the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Follow {
    Nothing,
    /// Fetch `after` this cursor (a live gap, or more pages owed).
    CatchUp {
        epoch: String,
        seq: u64,
    },
    /// Fetch the tail and replace.
    Reload,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Permission {
    pub id: String,
    pub request: Value,
}

#[derive(Debug, Clone, Default)]
pub struct Timeline {
    pub tail: Vec<Item>,
    pub head: Vec<Item>,
    pub cursor: Option<Cursor>,
    pub has_older: bool,
    pub turn: Turn,
    pub permissions: Vec<Permission>,
    /// The first page has landed; live frames go through the seq gate.
    pub baseline: bool,
    /// clientMessageIds sent but not yet echoed.
    pub pending_sends: HashSet<String>,
    /// The latest todo list, for a task panel.
    pub tasks: Vec<TodoEntry>,
    buffered: Vec<Value>,
    /// Live frames that arrived past a gap, waiting for the catch-up page.
    ahead: Vec<Value>,
    next_local: u64,
    /// Bumped on every change, so views can cache cheaply.
    pub revision: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Live,
    Canonical,
}

impl Timeline {
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.tail.iter().chain(self.head.iter())
    }

    pub fn len(&self) -> usize {
        self.tail.len() + self.head.len()
    }

    /// The text of the newest assistant reply loaded that has any.
    pub fn newest_reply(&self) -> Option<&str> {
        let mut newest_first = self.tail.iter().chain(&self.head).rev();
        newest_first.find_map(|item| match &item.kind {
            ItemKind::Assistant { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_running(&self) -> bool {
        matches!(self.turn, Turn::Open { .. })
    }

    /// Feed `turn` state from an agent snapshot's `activeTurn`.
    pub fn sync_snapshot(&mut self, agent_raw: &Value) {
        let active = &agent_raw["activeTurn"];
        if active.is_object() {
            let turn_id = active["turnId"].as_str().map(str::to_string);
            let started_at = active["startedAt"].as_str().map(str::to_string);
            match &mut self.turn {
                Turn::Open {
                    turn_id: current,
                    started_at: current_start,
                } if *current == turn_id => {
                    if current_start.is_none() {
                        *current_start = started_at;
                    }
                }
                _ => {
                    self.turn = Turn::Open {
                        turn_id,
                        started_at,
                    }
                }
            }
        } else if !agent_raw.is_null() {
            self.turn = Turn::Idle;
        }
        if let Some(pending) = agent_raw["pendingPermissions"].as_array() {
            self.permissions = pending
                .iter()
                .filter_map(|request| {
                    Some(Permission {
                        id: request["id"].as_str()?.to_string(),
                        request: request.clone(),
                    })
                })
                .collect();
        }
        self.revision += 1;
    }

    /// An `agent_permission_request` / `agent_permission_resolved` payload.
    pub fn apply_permission_event(&mut self, kind: &str, payload: &Value) {
        match kind {
            "agent_permission_request" => {
                let request = &payload["request"];
                if let Some(id) = request["id"].as_str()
                    && !self.permissions.iter().any(|p| p.id == id)
                {
                    self.permissions.push(Permission {
                        id: id.to_string(),
                        request: request.clone(),
                    });
                }
            }
            "agent_permission_resolved" => {
                if let Some(id) = payload["requestId"].as_str() {
                    self.permissions.retain(|p| p.id != id);
                }
            }
            _ => return,
        }
        self.revision += 1;
    }

    /// Adds the optimistic row for a message the user just sent.
    pub fn add_local_user(&mut self, id: &str, text: &str, timestamp: &str) {
        self.add_local_prompt(
            id,
            &crate::prompt::Prompt {
                text: text.to_string(),
                ..Default::default()
            },
            timestamp,
        );
    }

    pub fn add_local_prompt(&mut self, id: &str, prompt: &crate::prompt::Prompt, timestamp: &str) {
        self.pending_sends.insert(id.to_string());
        let item = Item {
            id: id.to_string(),
            turn_id: None,
            timestamp: timestamp.to_string(),
            kind: ItemKind::User {
                text: prompt.text.clone(),
                images: prompt.images.clone(),
                image_files: Vec::new(),
                seq: None,
                client_message_id: Some(id.to_string()),
                message_id: None,
            },
        };
        if self.head.is_empty() {
            self.tail.push(item);
        } else {
            self.head.push(item);
        }
        self.revision += 1;
    }

    /// The send failed: drop the optimistic row if it was never echoed.
    pub fn remove_local_user(&mut self, client_message_id: &str) {
        if self.pending_sends.remove(client_message_id) {
            self.tail.retain(|item| item.id != client_message_id);
            self.head.retain(|item| item.id != client_message_id);
            self.revision += 1;
        }
    }

    // ---- live ----------------------------------------------------------

    /// An `agent_stream` payload: `{agentId, event, timestamp, seq?, epoch?}`.
    pub fn apply_live(&mut self, payload: &Value) -> Follow {
        let event = &payload["event"];
        let event_type = event["type"].as_str().unwrap_or_default();
        let timestamp = payload["timestamp"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        // Turn and permission events have no seq and always apply.
        match event_type {
            "turn_started" => {
                let turn_id = event["turnId"].as_str().map(str::to_string);
                match &self.turn {
                    Turn::Open {
                        turn_id: current, ..
                    } if *current == turn_id => {}
                    _ => {
                        self.turn = Turn::Open {
                            turn_id,
                            started_at: Some(timestamp.clone()),
                        }
                    }
                }
            }
            "turn_completed" | "turn_failed" | "turn_canceled" => {
                let closing = event["turnId"].as_str();
                let stale = matches!(&self.turn, Turn::Open { turn_id: Some(open), .. } if closing.is_some_and(|c| c != open));
                if !stale {
                    self.turn = Turn::Idle;
                }
            }
            "permission_requested" => {
                self.apply_permission_event("agent_permission_request", event);
            }
            "permission_resolved" => {
                self.apply_permission_event("agent_permission_resolved", event);
            }
            _ => {}
        }

        if event_type != "timeline" {
            self.reduce(event, &timestamp, Source::Live, None);
            self.revision += 1;
            return Follow::Nothing;
        }

        if !self.baseline {
            self.buffered.push(payload.clone());
            return Follow::Nothing;
        }

        let (Some(seq), Some(epoch)) = (payload["seq"].as_u64(), payload["epoch"].as_str()) else {
            self.reduce(event, &timestamp, Source::Live, None);
            self.revision += 1;
            return Follow::Nothing;
        };
        match self.cursor.clone() {
            None => {
                self.cursor = Some(Cursor {
                    epoch: epoch.to_string(),
                    start_seq: seq,
                    end_seq: seq,
                });
            }
            Some(cursor) if cursor.epoch != epoch => {
                if seq == 1 {
                    self.tail.clear();
                    self.head.clear();
                    self.cursor = Some(Cursor {
                        epoch: epoch.to_string(),
                        start_seq: 1,
                        end_seq: 1,
                    });
                } else {
                    return Follow::Reload;
                }
            }
            Some(cursor) if seq <= cursor.end_seq => return Follow::Nothing,
            Some(cursor) if seq > cursor.end_seq + 1 => {
                // Keep it: the catch-up page may land after this frame, and
                // the frame applies once the gap before it is filled.
                if !self.ahead.iter().any(|f| f["seq"] == payload["seq"]) {
                    self.ahead.push(payload.clone());
                }
                return Follow::CatchUp {
                    epoch: cursor.epoch,
                    seq: cursor.end_seq,
                };
            }
            Some(_) => {
                if let Some(cursor) = &mut self.cursor {
                    cursor.end_seq = seq;
                }
            }
        }
        let cursor = Some((epoch.to_string(), seq));
        self.reduce(event, &timestamp, Source::Live, cursor);
        self.revision += 1;
        Follow::Nothing
    }

    // ---- fetched pages -------------------------------------------------

    /// A `fetch_agent_timeline_response` payload. Returns what to fetch next.
    pub fn apply_page(&mut self, payload: &Value) -> Follow {
        if payload["error"].is_string() {
            return Follow::Nothing;
        }
        let direction = payload["direction"].as_str().unwrap_or("tail");
        let reset = payload["reset"].as_bool().unwrap_or(false);
        let epoch = payload["epoch"].as_str().unwrap_or_default().to_string();
        let entries: Vec<Value> = payload["entries"].as_array().cloned().unwrap_or_default();
        let start = payload["startCursor"]["seq"].as_u64();
        let end = payload["endCursor"]["seq"].as_u64();
        let max_seq = payload["window"]["maxSeq"].as_u64().unwrap_or(0);
        let has_older = payload["hasOlder"].as_bool().unwrap_or(false);
        let has_newer = payload["hasNewer"].as_bool().unwrap_or(false);

        let follow = match (direction, reset) {
            ("before", false) => {
                let Some(cursor) = self.cursor.clone() else {
                    return Follow::Nothing;
                };
                if end.is_some_and(|end| end + 1 != cursor.start_seq) {
                    return Follow::Nothing;
                }
                // Share the local id counter so the page's generated ids
                // cannot collide with rows created later in this timeline.
                let mut older = Timeline {
                    next_local: self.next_local,
                    ..Timeline::default()
                };
                for entry in &entries {
                    older.apply_entry(entry, &epoch);
                }
                older.flush_head();
                self.next_local = older.next_local;
                let mut seam = older.tail;
                if let (Some(last), Some(first)) = (seam.last(), self.tail.first())
                    && let (ItemKind::Tool(a), ItemKind::Tool(b)) = (&last.kind, &first.kind)
                    && a.identity == b.identity
                {
                    seam.pop();
                }
                seam.append(&mut self.tail);
                self.tail = seam;
                self.has_older = has_older;
                if let Some(cursor) = &mut self.cursor {
                    cursor.start_seq = start.unwrap_or(cursor.start_seq);
                }
                Follow::Nothing
            }
            ("after", false) if self.cursor.as_ref().is_some_and(|c| c.epoch == epoch) => {
                let current_end = self.cursor.as_ref().map(|c| c.end_seq).unwrap_or(0);
                for entry in &entries {
                    let seq_end = entry["seqEnd"].as_u64().unwrap_or(0);
                    if seq_end <= current_end {
                        continue;
                    }
                    let seq_start = entry["seqStart"].as_u64().unwrap_or(seq_end);
                    if seq_start <= current_end && self.replace_streaming_text(entry) {
                        continue;
                    }
                    self.apply_entry(entry, &epoch);
                }
                if let (Some(cursor), Some(end)) = (&mut self.cursor, end) {
                    cursor.end_seq = cursor.end_seq.max(end);
                }
                if let Some(follow) = self.drain_ahead() {
                    self.revision += 1;
                    return follow;
                }
                if has_newer {
                    let cursor = self.cursor.as_ref().expect("cursor");
                    Follow::CatchUp {
                        epoch: cursor.epoch.clone(),
                        seq: cursor.end_seq,
                    }
                } else {
                    Follow::Nothing
                }
            }
            _ => {
                // Tail, reset, or an `after` we cannot stitch: replace.
                let locals: Vec<Item> = self
                    .items()
                    .filter(|item| {
                        matches!(&item.kind, ItemKind::User { client_message_id: Some(id), .. } if self.pending_sends.contains(id))
                    })
                    .cloned()
                    .collect();
                self.tail.clear();
                self.head.clear();
                for entry in &entries {
                    self.apply_entry(entry, &epoch);
                }
                for local in locals {
                    let id = local.id.clone();
                    if !self.items().any(|item| item.id == id) {
                        self.tail.push(local);
                    }
                }
                self.cursor = Some(Cursor {
                    epoch: epoch.clone(),
                    start_seq: start.unwrap_or(max_seq + 1),
                    end_seq: end.unwrap_or(max_seq),
                });
                self.has_older = has_older;
                self.baseline = true;
                let mut buffered = std::mem::take(&mut self.buffered);
                buffered.append(&mut self.ahead);
                // Frames from an older incarnation of the timeline are void.
                buffered.retain(|f| f["epoch"].as_str().is_none_or(|e| e == epoch));
                let mut follow = Follow::Nothing;
                for frame in buffered {
                    let next = self.apply_live(&frame);
                    if next != Follow::Nothing {
                        follow = next;
                    }
                }
                follow
            }
        };
        self.revision += 1;
        follow
    }

    /// Re-runs frames held past a gap now that more of the timeline is
    /// known. Returns a follow-up if a gap remains.
    fn drain_ahead(&mut self) -> Option<Follow> {
        let mut held = std::mem::take(&mut self.ahead);
        held.sort_by_key(|f| f["seq"].as_u64().unwrap_or(0));
        let mut follow = None;
        for frame in held {
            match self.apply_live(&frame) {
                Follow::Nothing => {}
                next => follow = Some(next),
            }
        }
        follow
    }

    fn apply_entry(&mut self, entry: &Value, epoch: &str) {
        let event = json!({
            "type": "timeline",
            "item": entry["item"].clone(),
            "turnId": entry["turnId"].clone(),
            "provider": entry["provider"].clone(),
        });
        let timestamp = entry["timestamp"].as_str().unwrap_or_default().to_string();
        let seq = entry["seqEnd"].as_u64().unwrap_or(0);
        self.reduce(
            &event,
            &timestamp,
            Source::Canonical,
            Some((epoch.to_string(), seq)),
        );
    }

    /// A projected assistant/reasoning row that straddles what we streamed:
    /// replace the partial text rather than append a duplicate.
    fn replace_streaming_text(&mut self, entry: &Value) -> bool {
        let item = &entry["item"];
        let text = item["text"].as_str().unwrap_or_default();
        let message_id = item["messageId"].as_str();
        let target = self
            .head
            .iter_mut()
            .rev()
            .chain(self.tail.iter_mut().rev())
            .find(|existing| match (&existing.kind, item["type"].as_str()) {
                (ItemKind::Assistant { message_id: id, .. }, Some("assistant_message")) => {
                    message_id.is_none() || id.as_deref() == message_id
                }
                (ItemKind::Thought { .. }, Some("reasoning")) => true,
                _ => false,
            });
        match target.map(|existing| &mut existing.kind) {
            Some(ItemKind::Assistant { text: current, .. })
            | Some(ItemKind::Thought { text: current, .. })
                if text.starts_with(current.as_str()) =>
            {
                *current = text.to_string();
                true
            }
            _ => false,
        }
    }

    // ---- reducer -------------------------------------------------------

    fn reduce(
        &mut self,
        event: &Value,
        timestamp: &str,
        source: Source,
        cursor: Option<(String, u64)>,
    ) {
        let event_type = event["type"].as_str().unwrap_or_default();
        if event_type == "turn_completed"
            || event_type == "turn_failed"
            || event_type == "turn_canceled"
        {
            self.flush_head();
            finalize_thoughts(&mut self.tail);
            return;
        }
        if event_type != "timeline" {
            finalize_thoughts(&mut self.tail);
            finalize_thoughts(&mut self.head);
            return;
        }
        // A prompt in an agent message's envelope is shown as 0.12 shows it.
        let projected = (event["item"]["type"] == "user_message")
            .then(|| crate::agent_messages::project_user_message(&event["item"]))
            .flatten();
        let item = projected.as_ref().unwrap_or(&event["item"]);
        let item_type = item["type"].as_str().unwrap_or_default();
        let turn_id = event["turnId"].as_str().map(str::to_string);
        let provider = event["provider"].as_str().unwrap_or_default();

        if item_type == "user_message" {
            self.apply_user(item, timestamp, turn_id, cursor.map(|(_, seq)| seq));
            return;
        }

        let streamable = matches!(item_type, "assistant_message" | "reasoning");
        let known = matches!(
            item_type,
            "assistant_message"
                | "reasoning"
                | "tool_call"
                | "todo"
                | "error"
                | "notification"
                | "compaction"
        );

        if known && !self.head.is_empty() {
            let last = self.head.iter().rev().find(|i| i.streamable());
            let flush = !streamable
                || match (last.map(|i| &i.kind), item_type) {
                    (Some(ItemKind::Assistant { .. }), "reasoning") => true,
                    (Some(ItemKind::Thought { .. }), "assistant_message") => true,
                    (
                        Some(ItemKind::Assistant {
                            message_id: Some(current),
                            ..
                        }),
                        "assistant_message",
                    ) => item["messageId"]
                        .as_str()
                        .is_some_and(|incoming| incoming != current),
                    _ => false,
                };
            if flush {
                self.flush_head();
            }
        }

        if item_type == "assistant_message" && self.head.is_empty() {
            let incoming = item["messageId"].as_str();
            let resume = matches!(
                self.tail.last().map(|i| &i.kind),
                Some(ItemKind::Assistant { message_id, .. }) if incoming.is_none() || message_id.as_deref() == incoming
            );
            if resume {
                let last = self.tail.pop().expect("tail item");
                self.head.push(last);
            }
        }

        match item_type {
            "assistant_message" => {
                let text = item["text"].as_str().unwrap_or_default().replace('\r', "");
                let message_id = item["messageId"].as_str().map(str::to_string);
                self.append_assistant(text, message_id, timestamp, source);
            }
            "reasoning" => {
                let text = item["text"].as_str().unwrap_or_default().replace('\r', "");
                if let Some(Item {
                    kind:
                        ItemKind::Thought {
                            text: current,
                            loading,
                        },
                    ..
                }) = self.head.last_mut()
                {
                    current.push_str(&text);
                    *loading = true;
                } else if !text.trim().is_empty() {
                    let id = self.local_id("thought");
                    self.head.push(Item {
                        id,
                        turn_id: None,
                        timestamp: timestamp.to_string(),
                        kind: ItemKind::Thought {
                            text,
                            loading: true,
                        },
                    });
                }
            }
            "tool_call" => {
                self.apply_tool(item, turn_id.as_deref(), provider, timestamp);
                finalize_thoughts(&mut self.tail);
            }
            "todo" => {
                let items = parse_todo(&item["items"]);
                self.apply_todo(provider, items, timestamp);
                finalize_thoughts(&mut self.tail);
            }
            "error" => {
                let id = self.local_id("error");
                self.tail.push(Item {
                    id,
                    turn_id: None,
                    timestamp: timestamp.to_string(),
                    kind: ItemKind::Notification {
                        level: "error".into(),
                        message: item["message"].as_str().unwrap_or_default().to_string(),
                    },
                });
                finalize_thoughts(&mut self.tail);
            }
            "notification" => {
                let id = self.local_id("notice");
                self.tail.push(Item {
                    id,
                    turn_id: None,
                    timestamp: timestamp.to_string(),
                    kind: ItemKind::Notification {
                        level: item["level"].as_str().unwrap_or("info").to_string(),
                        message: item["message"].as_str().unwrap_or_default().to_string(),
                    },
                });
                finalize_thoughts(&mut self.tail);
            }
            "compaction" => {
                let loading = item["status"] == "loading";
                let trigger = item["trigger"].as_str().map(str::to_string);
                let pre_tokens = item["preTokens"].as_u64();
                let open = self
                    .tail
                    .iter_mut()
                    .find(|i| matches!(i.kind, ItemKind::Compaction { loading: true, .. }));
                match (loading, open) {
                    (false, Some(open)) => {
                        open.kind = ItemKind::Compaction {
                            loading: false,
                            trigger,
                            pre_tokens,
                        }
                    }
                    _ => {
                        let id = self.local_id("compaction");
                        self.tail.push(Item {
                            id,
                            turn_id: None,
                            timestamp: timestamp.to_string(),
                            kind: ItemKind::Compaction {
                                loading,
                                trigger,
                                pre_tokens,
                            },
                        })
                    }
                }
                finalize_thoughts(&mut self.tail);
            }
            _ => {
                finalize_thoughts(&mut self.tail);
                return;
            }
        }

        // Stamp the turn on the item this event produced.
        if let Some(turn_id) = turn_id {
            let lane = if streamable {
                &mut self.head
            } else {
                &mut self.tail
            };
            if let Some(last) = lane.last_mut()
                && (item_type != "tool_call" || last.turn_id.is_none())
            {
                last.turn_id = Some(turn_id);
            }
        }
    }

    fn append_assistant(
        &mut self,
        text: String,
        message_id: Option<String>,
        timestamp: &str,
        source: Source,
    ) {
        if text.is_empty() {
            return;
        }
        if let Some(Item {
            kind:
                ItemKind::Assistant {
                    text: current,
                    message_id: current_id,
                },
            timestamp: ts,
            ..
        }) = self.head.last_mut()
            && (message_id.is_none() || *current_id == message_id)
        {
            current.push_str(&text);
            *ts = timestamp.to_string();
            return;
        }
        if source == Source::Live && self.head.len() >= 2 {
            let len = self.head.len();
            let user_last = matches!(self.head[len - 1].kind, ItemKind::User { .. });
            if user_last
                && let ItemKind::Assistant {
                    text: current,
                    message_id: current_id,
                } = &mut self.head[len - 2].kind
                && (message_id.is_none() || *current_id == message_id)
            {
                current.push_str(&text);
                return;
            }
        }
        if text.trim().is_empty() {
            return;
        }
        let id = match &message_id {
            Some(id) if !self.items().any(|item| &item.id == id) => id.clone(),
            Some(id) => format!("{id}:segment:{}", self.bump()),
            None => self.local_id("assistant"),
        };
        self.head.push(Item {
            id,
            turn_id: None,
            timestamp: timestamp.to_string(),
            kind: ItemKind::Assistant { text, message_id },
        });
    }

    fn apply_user(
        &mut self,
        item: &Value,
        timestamp: &str,
        turn_id: Option<String>,
        seq: Option<u64>,
    ) {
        self.flush_head();
        let text = item["text"].as_str().unwrap_or_default().to_string();
        let client_id = item["clientMessageId"].as_str().map(str::to_string);
        let message_id = item["messageId"].as_str().map(str::to_string);
        let listed = item["images"].as_array().map_or(&[][..], Vec::as_slice);
        let incoming_images: Vec<crate::prompt::ImageAttachment> = listed
            .iter()
            .filter_map(|image| serde_json::from_value(image.clone()).ok())
            .collect();
        let incoming_files: Vec<crate::images::ImageFile> = listed
            .iter()
            .filter_map(crate::images::ImageFile::listed)
            .collect();
        let position = self.tail.iter().position(|existing| match &existing.kind {
            ItemKind::User {
                client_message_id: local_client,
                message_id: local_message,
                text: local_text,
                ..
            } => {
                if let (Some(a), Some(b)) = (local_client, &client_id) {
                    a == b
                } else if let (Some(a), Some(b)) = (local_message, &message_id) {
                    a == b
                } else {
                    local_client.is_some()
                        && local_message.is_none()
                        && (message_id.as_ref() == local_client.as_ref() || *local_text == text)
                }
            }
            _ => false,
        });
        if let Some(index) = position {
            let mut existing = self.tail.remove(index);
            if let ItemKind::User {
                client_message_id,
                message_id: existing_message,
                images,
                image_files,
                seq: existing_seq,
                ..
            } = &mut existing.kind
            {
                *existing_seq = seq.or(*existing_seq);
                if !incoming_images.is_empty() {
                    *images = incoming_images.clone();
                }
                if !incoming_files.is_empty() {
                    *image_files = incoming_files.clone();
                }
                if client_message_id.is_none() {
                    *client_message_id = client_id.clone();
                }
                if existing_message.is_none() {
                    *existing_message = message_id.clone();
                }
            }
            existing.turn_id = turn_id.or(existing.turn_id);
            existing.timestamp = timestamp.to_string();
            self.tail.push(existing);
        } else if !text.is_empty() || !incoming_images.is_empty() || !incoming_files.is_empty() {
            let id = client_id
                .clone()
                .or_else(|| message_id.clone())
                .unwrap_or_else(|| self.local_id("user"));
            self.tail.push(Item {
                id,
                turn_id,
                timestamp: timestamp.to_string(),
                kind: ItemKind::User {
                    text,
                    images: incoming_images,
                    image_files: incoming_files,
                    seq,
                    client_message_id: client_id.clone(),
                    message_id,
                },
            });
        }
        if let Some(id) = client_id {
            self.pending_sends.remove(&id);
        }
    }

    fn apply_tool(&mut self, item: &Value, turn_id: Option<&str>, provider: &str, timestamp: &str) {
        let name = item["name"].as_str().unwrap_or_default().to_string();
        let normalized = normalize_tool_name(&name);
        let detail = item["detail"].clone();

        if matches!(normalized.as_str(), "todowrite" | "todo_write") {
            if let Some(todos) = detail["input"]["todos"].as_array() {
                let items = todos
                    .iter()
                    .map(|todo| TodoEntry {
                        text: todo["activeForm"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .or(todo["content"].as_str())
                            .unwrap_or_default()
                            .to_string(),
                        status: todo["status"].as_str().unwrap_or("pending").to_string(),
                    })
                    .collect();
                self.apply_todo(provider, items, timestamp);
            }
            return;
        }
        if matches!(
            normalized.as_str(),
            "taskcreate" | "taskupdate" | "tasklist"
        ) {
            return;
        }
        if normalized == "update_plan" {
            if let Some(plan) = detail["input"]["plan"].as_array() {
                let items = plan
                    .iter()
                    .map(|step| TodoEntry {
                        text: step["step"].as_str().unwrap_or_default().to_string(),
                        status: step["status"].as_str().unwrap_or("pending").to_string(),
                    })
                    .collect();
                self.apply_todo(provider, items, timestamp);
            }
            return;
        }

        let call_id = item["callId"].as_str().unwrap_or_default().to_string();
        let identity = match turn_id {
            Some(turn) => format!("turn:{turn}/{call_id}"),
            None => call_id.clone(),
        };
        let incoming = ToolCall {
            identity: identity.clone(),
            call_id,
            name,
            status: item["status"].as_str().unwrap_or("running").to_string(),
            error: item["error"]
                .clone()
                .as_object()
                .map(|_| item["error"].clone())
                .or_else(|| item["error"].as_str().map(|s| Value::String(s.to_string()))),
            detail,
            metadata: item["metadata"].as_object().cloned().unwrap_or_default(),
            agent_message: AgentMessage::from_wire(&item["agentMessage"]).map(Box::new),
        };
        let item_id = format!("agent_tool_{identity}");
        if let Some(existing) = self
            .tail
            .iter_mut()
            .chain(self.head.iter_mut())
            .find(|i| i.id == item_id)
        {
            if let ItemKind::Tool(current) = &mut existing.kind {
                merge_tool(current, incoming);
            }
            return;
        }
        self.tail.push(Item {
            id: item_id,
            turn_id: turn_id.map(str::to_string),
            timestamp: timestamp.to_string(),
            kind: ItemKind::Tool(Box::new(incoming)),
        });
    }

    fn apply_todo(&mut self, provider: &str, items: Vec<TodoEntry>, timestamp: &str) {
        self.tasks = items.clone();
        let previous = self.tail.iter().rev().find_map(|item| match &item.kind {
            ItemKind::Todo {
                provider: p, items, ..
            } if p == provider => Some(items.clone()),
            _ => None,
        });
        let activities = match &previous {
            None => vec![TaskActivity::Created(items.len())],
            Some(previous) => {
                let key = |index: usize, entry: &TodoEntry| format!("{index}:{}", entry.text);
                let before: std::collections::HashMap<String, &TodoEntry> = previous
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (key(i, e), e))
                    .collect();
                let mut out = Vec::new();
                for (index, entry) in items.iter().enumerate() {
                    // Match by text when indices shift.
                    let old = before
                        .get(&key(index, entry))
                        .copied()
                        .or_else(|| previous.iter().find(|p| p.text == entry.text));
                    match old {
                        None => out.push(TaskActivity::Added(entry.text.clone())),
                        Some(old) if old.status != entry.status => match entry.status.as_str() {
                            "completed" => out.push(TaskActivity::Completed(entry.text.clone())),
                            "in_progress" => out.push(TaskActivity::Started(entry.text.clone())),
                            _ => {}
                        },
                        _ => {}
                    }
                }
                out
            }
        };
        if activities.is_empty() {
            if let Some(last) =
                self.tail.iter_mut().rev().find(
                    |i| matches!(&i.kind, ItemKind::Todo { provider: p, .. } if p == provider),
                )
                && let ItemKind::Todo { items: current, .. } = &mut last.kind
            {
                *current = items;
            }
            return;
        }
        for activity in activities {
            let id = self.local_id("todo");
            self.tail.push(Item {
                id,
                turn_id: None,
                timestamp: timestamp.to_string(),
                kind: ItemKind::Todo {
                    provider: provider.to_string(),
                    items: items.clone(),
                    activity,
                },
            });
        }
    }

    fn flush_head(&mut self) {
        if self.head.is_empty() {
            return;
        }
        let mut head = std::mem::take(&mut self.head);
        finalize_thoughts(&mut head);
        for item in head {
            if !self.tail.iter().any(|existing| existing.id == item.id) {
                self.tail.push(item);
            }
        }
    }

    fn bump(&mut self) -> u64 {
        self.next_local += 1;
        self.next_local
    }

    fn local_id(&mut self, prefix: &str) -> String {
        format!("{prefix}_{}", self.bump())
    }
}

fn finalize_thoughts(items: &mut [Item]) {
    for item in items {
        if let ItemKind::Thought { loading, .. } = &mut item.kind {
            *loading = false;
        }
    }
}

fn parse_todo(items: &Value) -> Vec<TodoEntry> {
    items
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| TodoEntry {
            text: entry["activeForm"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or(entry["text"].as_str())
                .unwrap_or_default()
                .to_string(),
            status: entry["status"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if entry["completed"].as_bool() == Some(true) {
                        "completed".into()
                    } else {
                        "pending".into()
                    }
                }),
        })
        .collect()
}

pub fn normalize_tool_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_sep = false;
    for c in name.trim().chars() {
        if c == '.' || c == '-' || c.is_whitespace() {
            if !last_sep {
                out.push('_');
            }
            last_sep = true;
        } else {
            out.extend(c.to_lowercase());
            last_sep = false;
        }
    }
    out
}

fn merge_tool(current: &mut ToolCall, incoming: ToolCall) {
    let status = match (current.status.as_str(), incoming.status.as_str()) {
        ("failed", _) | (_, "failed") => "failed",
        ("canceled", _) => "canceled",
        ("completed", "canceled") => "completed",
        ("completed", _) => "completed",
        (_, other) => other,
    }
    .to_string();
    current.error = if status == "failed" {
        Some(
            incoming
                .error
                .clone()
                .or(current.error.take())
                .unwrap_or_else(|| json!({"message": "Tool call failed"})),
        )
    } else {
        None
    };
    current.status = status;

    let current_type = current.detail["type"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let incoming_type = incoming.detail["type"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    match (current_type.as_str(), incoming_type.as_str()) {
        (_, "unknown") if current_type != "unknown" => {}
        ("unknown", "unknown") => {
            for key in ["input", "output"] {
                let next = &incoming.detail[key];
                let keep = next.is_null()
                    || (next.as_object().is_some_and(Map::is_empty)
                        && !current.detail[key].is_null());
                if !keep {
                    current.detail[key] = next.clone();
                }
            }
        }
        (a, b) if a == b => {
            if let (Some(target), Some(source)) =
                (current.detail.as_object_mut(), incoming.detail.as_object())
            {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
        _ => current.detail = incoming.detail,
    }
    for (key, value) in incoming.metadata {
        current.metadata.insert(key, value);
    }
    if incoming.agent_message.is_some() {
        current.agent_message = incoming.agent_message;
    }
    current.name = incoming.name;
}

/// Each item's turn, as the index of the turn's first item. A turn starts at
/// a user message, or where the turn id changes: an agent that resumes
/// itself, as an orchestrator does, runs turns of its own that no message
/// starts. Items without an id belong to the turn they fall in.
pub fn turn_starts(items: &[&Item]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(items.len());
    // The open turn: where it starts, and its id once known.
    let mut open: Option<(usize, Option<&str>)> = None;
    for (index, item) in items.iter().enumerate() {
        let id = item.turn_id.as_deref();
        let begins = matches!(item.kind, ItemKind::User { .. })
            || match open {
                None => true,
                Some((_, Some(current))) => id.is_some_and(|id| id != current),
                Some((_, None)) => false,
            };
        if begins {
            open = Some((index, id));
        } else if let Some((_, current @ None)) = &mut open {
            // A message sent from here has no id until its turn begins.
            *current = id;
        }
        starts.push(open.map_or(index, |(start, _)| start));
    }
    starts
}

/// The finished turns among `items`, as the first and last index of each,
/// for a footer after each one; turns are as [`turn_starts`] has them. A
/// turn the user started counts only once something answered it; the last
/// turn only once it has ended.
pub fn finished_turns(items: &[&Item], running: bool) -> Vec<(usize, usize)> {
    let starts = turn_starts(items);
    let mut turns = Vec::new();
    let mut index = 0;
    while index < starts.len() {
        let start = starts[index];
        let end = (index..starts.len())
            .take_while(|&i| starts[i] == start)
            .last()
            .unwrap_or(index);
        let last = end + 1 == starts.len();
        let by_user = matches!(items[start].kind, ItemKind::User { .. });
        if !(by_user && end == start) && !(last && running) {
            turns.push((start, end));
        }
        index = end + 1;
    }
    turns
}

/// What a finished turn's footer shows: when it ended and how long it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    /// The last item's timestamp.
    pub ended_at: String,
    /// Seconds from the first item to the last; `None` for a turn shorter
    /// than a second, as a turn of one message is.
    pub duration_secs: Option<i64>,
}

/// The footer of the turn `items[start..=end]`, as [`finished_turns`]
/// gives them.
pub fn turn_footer(items: &[&Item], start: usize, end: usize) -> Footer {
    let ended_at = items
        .get(end)
        .map(|item| item.timestamp.clone())
        .unwrap_or_default();
    let first = items
        .get(start)
        .and_then(|item| crate::clock::parse_iso(&item.timestamp));
    let duration_secs = match (first, crate::clock::parse_iso(&ended_at)) {
        (Some(a), Some(b)) if b >= a + 1000 => Some((b - a) / 1000),
        _ => None,
    };
    Footer {
        ended_at,
        duration_secs,
    }
}

/// The steps among `items`, as the index of the item each began with and of
/// its last item shown: an agent's message and the tool calls, thinking and
/// task changes under it, or a run of those with no message above. Anything
/// else (a prompt, a notice) and a new turn end the step before it; items a
/// conversation leaves out belong to none.
pub fn steps(items: &[&Item]) -> Vec<(usize, usize)> {
    let turns = turn_starts(items);
    let mut steps = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    for (index, item) in items.iter().enumerate() {
        if crate::display::hidden_in_stream(&item.kind) {
            continue;
        }
        if open.is_some_and(|(start, _)| turns[start] != turns[index]) {
            steps.extend(open.take());
        }
        match &item.kind {
            ItemKind::Assistant { .. } => steps.extend(open.replace((index, index))),
            ItemKind::Tool(_) | ItemKind::Thought { .. } | ItemKind::Todo { .. } => {
                open = Some((open.map_or(index, |(start, _)| start), index));
            }
            _ => steps.extend(open.take()),
        }
    }
    steps.extend(open);
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_item(kind: &str, turn: Option<&str>) -> Item {
        let kind = match kind {
            "user" => ItemKind::User {
                text: "do it".into(),
                images: Vec::new(),
                image_files: Vec::new(),
                seq: None,
                client_message_id: None,
                message_id: None,
            },
            _ => ItemKind::Assistant {
                text: "done".into(),
                message_id: None,
            },
        };
        Item {
            id: String::new(),
            turn_id: turn.map(str::to_string),
            timestamp: String::new(),
            kind,
        }
    }

    #[test]
    fn every_turn_ends_with_a_footer_even_ones_no_message_started() {
        let items = [
            turn_item("user", Some("f1")),
            turn_item("assistant", Some("f1")),
            turn_item("assistant", Some("auto-2")),
            turn_item("assistant", None),
            turn_item("assistant", Some("auto-3")),
            turn_item("user", Some("f4")),
            turn_item("assistant", Some("f4")),
        ];
        let items: Vec<&Item> = items.iter().collect();
        assert_eq!(
            finished_turns(&items, false),
            [(0, 1), (2, 3), (4, 4), (5, 6)]
        );
        // The turn still running has none yet.
        assert_eq!(finished_turns(&items, true), [(0, 1), (2, 3), (4, 4)]);
    }

    #[test]
    fn a_footer_has_a_duration_only_from_a_second_up() {
        let mut items = [
            turn_item("user", Some("f1")),
            turn_item("assistant", Some("f1")),
            turn_item("assistant", Some("auto-2")),
        ];
        items[0].timestamp = "2026-10-13T09:00:00.000Z".into();
        items[1].timestamp = "2026-10-13T09:01:12.000Z".into();
        items[2].timestamp = "2026-10-13T09:05:00.400Z".into();
        let items: Vec<&Item> = items.iter().collect();
        let footer = turn_footer(&items, 0, 1);
        assert_eq!(footer.ended_at, "2026-10-13T09:01:12.000Z");
        assert_eq!(footer.duration_secs, Some(72));
        assert_eq!(turn_footer(&items, 2, 2).duration_secs, None);
    }

    #[test]
    fn a_message_takes_its_turns_id_and_unanswered_ones_have_no_footer() {
        let items = [
            turn_item("assistant", None),
            turn_item("user", None),
            turn_item("assistant", Some("f2")),
            turn_item("assistant", Some("f2")),
            turn_item("user", Some("f3")),
            turn_item("user", Some("f4")),
        ];
        let items: Vec<&Item> = items.iter().collect();
        assert_eq!(finished_turns(&items, false), [(0, 0), (1, 3)]);
        assert_eq!(turn_starts(&items), [0, 1, 1, 1, 4, 5]);
    }

    #[test]
    fn a_step_is_a_message_and_the_work_under_it() {
        let thought = |turn| Item {
            kind: ItemKind::Thought {
                text: String::new(),
                loading: false,
            },
            ..turn_item("", Some(turn))
        };
        let items = [
            turn_item("user", Some("f1")),
            thought("f1"),
            turn_item("assistant", Some("f1")),
            thought("f1"),
            thought("f1"),
            turn_item("assistant", Some("f1")),
            turn_item("assistant", Some("f1")),
            thought("f1"),
            thought("auto-2"),
            turn_item("user", Some("f3")),
        ];
        let items: Vec<&Item> = items.iter().collect();
        assert_eq!(steps(&items), [(1, 1), (2, 4), (5, 5), (6, 7), (8, 8)]);
    }

    #[test]
    fn typed_detail_survives_unknown_update() {
        let mut tl = Timeline {
            baseline: true,
            ..Default::default()
        };
        let frame = |seq: u64, detail: Value, status: &str| {
            json!({"agentId": "a", "timestamp": "t", "seq": seq, "epoch": "e", "event": {"type": "timeline", "turnId": "t1",
                "item": {"type": "tool_call", "callId": "c1", "name": "Bash", "detail": detail, "status": status, "error": null}}})
        };
        tl.apply_live(&frame(
            1,
            json!({"type": "unknown", "input": {}, "output": null}),
            "running",
        ));
        tl.apply_live(&frame(
            2,
            json!({"type": "shell", "command": "ls"}),
            "running",
        ));
        tl.apply_live(&frame(
            3,
            json!({"type": "unknown", "input": {}, "output": null}),
            "completed",
        ));
        assert_eq!(tl.len(), 1);
        let ItemKind::Tool(tool) = &tl.tail[0].kind else {
            panic!()
        };
        assert_eq!(tool.detail["command"], "ls");
        assert_eq!(tool.status, "completed");
    }

    #[test]
    fn agent_messages_keep_their_sender() {
        let mut tl = Timeline::default();
        let entry = |seq: u64, turn: &str, item: Value| json!({"item": item, "turnId": turn, "timestamp": "t", "seqStart": seq, "seqEnd": seq});
        tl.apply_page(&json!({"direction": "tail", "epoch": "e", "window": {"maxSeq": 3},
            "startCursor": {"seq": 1}, "endCursor": {"seq": 3}, "entries": [
            // Paseo 0.12's projection.
            entry(1, "t1", json!({"type": "tool_call", "callId": "paseo-agent-message:m1",
                "name": "agent_message", "status": "completed", "error": null,
                "detail": {"type": "plain_text", "text": "Review it", "icon": "bot"},
                "agentMessage": {"event": "message", "sender": {"id": "p1", "title": "planner"}, "text": "Review it"}})),
            // The same from a replayed history: still tagged.
            entry(2, "t2", json!({"type": "user_message", "messageId": "u2", "text":
                "<paseo-system version=\"1\" kind=\"agent-notification\" source-agent-id=\"w1\" message-id=\"m2\" event=\"finished\">\nAgent w1 (worker) finished.\n</paseo-system>"})),
            // An older daemon: a plain prompt, with no sender to show.
            entry(3, "t3", json!({"type": "user_message", "messageId": "u3", "text": "Review it too"})),
        ]}));
        let kinds: Vec<_> = tl.items().map(|item| &item.kind).collect();
        assert_eq!(kinds.len(), 3);
        let ItemKind::Tool(first) = kinds[0] else {
            panic!("{:?}", kinds[0])
        };
        let message = first.agent_message.as_ref().unwrap();
        assert_eq!(
            (message.sender_id.as_str(), message.sender_title.as_deref()),
            ("p1", Some("planner"))
        );
        let ItemKind::Tool(second) = kinds[1] else {
            panic!("{:?}", kinds[1])
        };
        assert_eq!(
            second.agent_message.as_ref().unwrap().event,
            crate::agent_messages::AgentEvent::Finished
        );
        assert_eq!(tl.items().nth(1).unwrap().turn_id.as_deref(), Some("t2"));
        assert!(matches!(kinds[2], ItemKind::User { text, .. } if text == "Review it too"));
        // A notice starts no turn of the user's, but its own.
        let items: Vec<_> = tl.items().collect();
        assert_eq!(turn_starts(&items), [0, 1, 2]);
    }

    #[test]
    fn gap_asks_for_catch_up() {
        let mut tl = Timeline::default();
        tl.apply_page(
            &json!({"direction": "tail", "epoch": "e", "entries": [], "window": {"maxSeq": 4}}),
        );
        let follow = tl.apply_live(
            &json!({"agentId": "a", "timestamp": "t", "seq": 7, "epoch": "e",
            "event": {"type": "timeline", "item": {"type": "assistant_message", "text": "x"}}}),
        );
        assert_eq!(
            follow,
            Follow::CatchUp {
                epoch: "e".into(),
                seq: 4
            }
        );
    }

    #[test]
    fn frames_past_a_gap_apply_after_catch_up() {
        let mut tl = Timeline::default();
        tl.apply_page(
            &json!({"direction": "tail", "epoch": "e", "entries": [], "window": {"maxSeq": 0}}),
        );
        let text = |seq: u64, t: &str| {
            json!({"agentId": "a", "timestamp": "t", "seq": seq, "epoch": "e",
            "event": {"type": "timeline", "item": {"type": "assistant_message", "text": t, "messageId": "m"}}})
        };
        // seq 2 arrives before the page that holds seq 1.
        assert!(matches!(
            tl.apply_live(&text(2, " world")),
            Follow::CatchUp { .. }
        ));
        let page = json!({"direction": "after", "epoch": "e", "entries": [
            {"item": {"type": "assistant_message", "text": "hello", "messageId": "m"}, "timestamp": "t", "seqStart": 1, "seqEnd": 1}],
            "endCursor": {"epoch": "e", "seq": 1}, "hasNewer": false});
        assert_eq!(tl.apply_page(&page), Follow::Nothing);
        let texts: Vec<String> = tl
            .items()
            .filter_map(|i| match &i.kind {
                ItemKind::Assistant { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hello world"]);
    }

    #[test]
    fn optimistic_user_row_is_reconciled() {
        let mut tl = Timeline::default();
        tl.apply_page(
            &json!({"direction": "tail", "epoch": "e", "entries": [], "window": {"maxSeq": 0}}),
        );
        tl.add_local_user("m1", "hello", "t0");
        tl.apply_live(&json!({"agentId": "a", "timestamp": "t1", "seq": 1, "epoch": "e",
            "event": {"type": "timeline", "item": {"type": "user_message", "text": "hello", "messageId": "m1", "clientMessageId": "m1"}}}));
        assert_eq!(tl.len(), 1);
        assert!(tl.pending_sends.is_empty());
    }
    #[test]
    fn image_only_history_keeps_prompt_seq_and_reconciles_optimistic_image() {
        use crate::prompt::{ImageAttachment, Prompt};
        let image = ImageAttachment::new("cG5n", "image/png");
        let mut tl = Timeline::default();
        tl.apply_page(
            &json!({"direction":"tail", "epoch":"e", "entries":[], "window":{"maxSeq":0}}),
        );
        tl.add_local_prompt(
            "m",
            &Prompt {
                text: String::new(),
                images: vec![image.clone()],
            },
            "t0",
        );
        tl.apply_live(&json!({"agentId":"a", "timestamp":"t1", "seq":1, "epoch":"e", "event":{
            "type":"timeline", "item":{"type":"user_message", "text":"", "messageId":"m", "clientMessageId":"m"}
        }}));
        assert_eq!(tl.len(), 1);
        assert!(tl.pending_sends.is_empty());
        assert!(matches!(&tl.items().next().unwrap().kind,
            ItemKind::User { images, seq: Some(1), .. } if images.as_slice() == std::slice::from_ref(&image)));
        tl.apply_page(&json!({"direction":"tail", "epoch":"e", "entries":[{
            "seqStart":1, "seqEnd":1, "timestamp":"t1", "item":{
                "type":"user_message", "text":"", "images":[image]
            }}], "startCursor":{"seq":1}, "endCursor":{"seq":1}, "window":{"maxSeq":1}}));
        assert_eq!(tl.len(), 1);
        assert!(
            matches!(&tl.items().next().unwrap().kind, ItemKind::User { seq: Some(1), images, .. } if images.len() == 1)
        );
    }

    #[test]
    fn kept_image_files_are_listed_beside_inline_images() {
        use crate::images::ImageFile;
        use crate::prompt::{ImageAttachment, Prompt};
        let kept = |name: &str| json!({"path": format!("/srv/attachments/{name}.png"), "mimeType": "image/png"});
        let file = |name: &str| ImageFile {
            cwd: "/".into(),
            path: format!("/srv/attachments/{name}.png"),
        };
        // From another client: only the files the daemon kept.
        let mut tl = Timeline::default();
        tl.apply_page(&json!({"direction":"tail", "epoch":"e", "entries":[{
            "seqStart":1, "seqEnd":1, "timestamp":"t1", "item":{
                "type":"user_message", "text":"", "messageId":"x", "images":[kept("a"), kept("b")]
            }}], "startCursor":{"seq":1}, "endCursor":{"seq":1}, "window":{"maxSeq":1}}));
        assert!(matches!(&tl.items().next().unwrap().kind,
            ItemKind::User { images, image_files, .. } if images.is_empty() && *image_files == [file("a"), file("b")]));
        // Sent from here: the bytes in hand stay, and the files join them.
        let image = ImageAttachment::new("cG5n", "image/png");
        tl.add_local_prompt(
            "m",
            &Prompt {
                text: "look".into(),
                images: vec![image.clone()],
            },
            "t2",
        );
        tl.apply_live(
            &json!({"agentId":"a", "timestamp":"t3", "seq":2, "epoch":"e", "event":{
                "type":"timeline", "item":{"type":"user_message", "text":"look", "messageId":"m",
                    "clientMessageId":"m", "images":[kept("c")]}
            }}),
        );
        assert_eq!(tl.len(), 2);
        assert!(matches!(&tl.items().nth(1).unwrap().kind,
            ItemKind::User { images, image_files, .. }
                if images.as_slice() == std::slice::from_ref(&image) && *image_files == [file("c")]));
    }

    #[test]
    fn prompt_index_history_prepend_preserves_live_tail_and_pending_images() {
        use crate::prompt::{ImageAttachment, Prompt};
        let entry = |seq, text| json!({"seqStart":seq,"seqEnd":seq,"timestamp":"t","item":{"type":"user_message","text":text}});
        let mut tl = Timeline::default();
        tl.apply_page(
            &json!({"direction":"tail","epoch":"e","entries":[entry(3,"latest")],
            "startCursor":{"seq":3},"endCursor":{"seq":3},"hasOlder":true,"window":{"maxSeq":3}}),
        );
        tl.add_local_prompt(
            "pending",
            &Prompt {
                text: String::new(),
                images: vec![ImageAttachment::new("cG5n", "image/png")],
            },
            "t",
        );
        tl.apply_page(
            &json!({"direction":"before","epoch":"e","entries":[entry(1,"old"),entry(2,"middle")],
            "startCursor":{"seq":1},"endCursor":{"seq":2},"hasOlder":false}),
        );
        assert_eq!(tl.len(), 4);
        assert_eq!(tl.cursor.as_ref().unwrap().start_seq, 1);
        assert!(tl.pending_sends.contains("pending"));
        let prompts: Vec<_> = tl
            .items()
            .filter_map(|item| match &item.kind {
                ItemKind::User { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect();
        assert_eq!(prompts, vec![Some(1), Some(2), Some(3), None]);
    }
}
