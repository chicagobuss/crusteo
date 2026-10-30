//! Projected timelines: the entries history reads are served as, a tool
//! call's lifecycle folded into one and adjacent text chunks into one, kept
//! up to date as rows are appended. A port of Paseo 0.14's
//! `TimelineProjection` (`timeline-projection.ts`), selection included, so
//! a page reads the same from Burrow as from Paseo.
//!
//! Folding a tool call can make an entry cover seqs that are not adjacent
//! (its start, then its end after other rows). Pages are chosen by entry,
//! then widened so no entry is cut and no seq is skipped: a *span* is a run
//! of entries whose seqs interleave, and a tail never starts inside one.

use std::collections::{BTreeSet, HashMap};

use burrow_protocol::timeline::{SeqRange, TimelineEntry};
use serde_json::{Map, Value};

use crate::timeline::{Row, continues_turn_of};

const ASSISTANT_MERGE: &str = "assistant_merge";
const REASONING_MERGE: &str = "reasoning_merge";
const TOOL_LIFECYCLE: &str = "tool_lifecycle";
const IDENTITY: &str = "identity";

#[derive(Debug, Clone, Default)]
pub struct Projection {
    entries: Vec<TimelineEntry>,
    /// The entry each tool call or plugin item is folded into, by identity.
    identities: HashMap<String, usize>,
    /// Runs of entries whose seqs interleave, in entry order.
    spans: Vec<Span>,
    /// Which entry each seq went into, as contiguous ranges in seq order.
    sources: Vec<Source>,
}

#[derive(Debug, Clone, Copy)]
struct Span {
    /// Its first entry.
    start: usize,
    end_seq: u64,
}

#[derive(Debug, Clone, Copy)]
struct Source {
    start_seq: u64,
    end_seq: u64,
    entry: usize,
}

/// A page's entries and where it starts and ends, as
/// `ProjectedTimelinePageSelection`.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub entries: Vec<TimelineEntry>,
    pub start_seq: Option<u64>,
    pub end_seq: Option<u64>,
    pub has_older: bool,
    pub has_newer: bool,
}

impl Projection {
    pub fn new<'a>(rows: impl IntoIterator<Item = &'a Row>) -> Self {
        let mut projection = Self::default();
        for row in rows {
            projection.append(row);
        }
        projection
    }

    pub fn entries(&self) -> &[TimelineEntry] {
        &self.entries
    }

    pub fn append(&mut self, row: &Row) {
        let index = self.fold(row);
        match self.sources.last_mut() {
            Some(last) if last.entry == index && last.end_seq + 1 == row.seq => {
                last.end_seq = row.seq;
            }
            _ => self.sources.push(Source {
                start_seq: row.seq,
                end_seq: row.seq,
                entry: index,
            }),
        }
    }

    /// Folds `row` into an entry, saying which.
    fn fold(&mut self, row: &Row) -> usize {
        let entry = canonical(row);
        let identity = identity(&row.item);
        if let Some(index) = identity
            .as_ref()
            .and_then(|id| self.identities.get(id))
            .copied()
            && let Some(merged) = merge_identity(&self.entries[index], &entry)
        {
            self.entries[index] = merged;
            self.extend_span(index);
            return index;
        }
        if let Some(previous) = self.entries.last()
            && let Some(merged) = merge_text(previous, &entry)
        {
            let index = self.entries.len() - 1;
            self.entries[index] = merged;
            self.extend_span(index);
            return index;
        }
        let index = self.entries.len();
        if let Some(identity) = identity {
            self.identities.insert(identity, index);
        }
        let (start_seq, end_seq) = (entry.seq_start, entry.seq_end);
        self.entries.push(entry);
        match self.spans.last_mut() {
            Some(last) if last.end_seq >= start_seq => last.end_seq = last.end_seq.max(end_seq),
            _ => self.spans.push(Span {
                start: index,
                end_seq,
            }),
        }
        index
    }

    /// The span `entry` is in.
    fn span_of(&self, entry: usize) -> usize {
        self.spans
            .partition_point(|span| span.start <= entry)
            .saturating_sub(1)
    }

    /// After `entry` took in a later seq, its span runs at least that far,
    /// and takes in the spans after it that now start inside it.
    fn extend_span(&mut self, entry: usize) {
        let index = self.span_of(entry);
        let mut end_seq = self.spans[index].end_seq.max(self.entries[entry].seq_end);
        let mut end = index + 1;
        while end < self.spans.len() && self.entries[self.spans[end].start].seq_start <= end_seq {
            end_seq = end_seq.max(self.spans[end].end_seq);
            end += 1;
        }
        self.spans[index].end_seq = end_seq;
        self.spans.drain(index + 1..end);
    }

    /// The latest `limit` entries (all with 0), widened back to the start of
    /// the span the first is in and, with `whole_turns`, of its turn.
    pub fn tail(&self, limit: usize, whole_turns: bool, max_seq: u64) -> Selection {
        if self.entries.is_empty() {
            return Selection::empty(false, false);
        }
        let requested = self.entries.len().saturating_sub(limit_or_all(limit));
        let covered = self.spans[self.span_of(requested)].start;
        let start = if whole_turns {
            self.whole_turn_start(covered)
        } else {
            covered
        };
        Selection {
            entries: self.entries[start..].to_vec(),
            start_seq: Some(self.entries[start].seq_start),
            end_seq: Some(max_seq),
            has_older: start > 0,
            has_newer: false,
        }
    }

    /// Back to where the turn `start` is in starts, and the span that is in,
    /// until both agree: a span can pull in an earlier tool call, and so its
    /// whole turn.
    fn whole_turn_start(&self, mut start: usize) -> usize {
        loop {
            while start > 0 && self.continues_turn(start) {
                start -= 1;
            }
            let covered = self.spans[self.span_of(start)].start;
            if covered == start {
                return start;
            }
            start = covered;
        }
    }

    /// Whether entry `next` is in the same turn as the one before it.
    fn continues_turn(&self, next: usize) -> bool {
        let (previous, next) = (&self.entries[next - 1], &self.entries[next]);
        continues_turn_of(
            previous.turn_id.as_deref(),
            next.turn_id.as_deref(),
            &next.item,
        )
    }

    /// The entries holding seqs from `start_seq` (`limit` of them, all with
    /// 0), up to where the seqs they cover stop being contiguous.
    pub fn after(&self, start_seq: u64, limit: usize, min_seq: u64, max_seq: u64) -> Selection {
        let from = self
            .sources
            .partition_point(|source| source.end_seq < start_seq);
        let mut indexes: BTreeSet<usize> = BTreeSet::new();
        for source in &self.sources[from..] {
            if source.start_seq > max_seq {
                break;
            }
            indexes.insert(source.entry);
            if limit > 0 && indexes.len() == limit {
                break;
            }
        }
        // Each by the first seq it holds in range, earliest first.
        let mut eligible: Vec<(u64, usize)> = (indexes.iter())
            .filter_map(|&index| {
                first_seq_in(&self.entries[index], start_seq, max_seq).map(|seq| (seq, index))
            })
            .collect();
        eligible.sort_unstable();
        if limit > 0 {
            eligible.truncate(limit);
        }
        let mut chosen: Vec<usize> = eligible.into_iter().map(|(_, index)| index).collect();
        chosen.sort_unstable();
        let entries: Vec<TimelineEntry> = chosen.iter().map(|&i| self.entries[i].clone()).collect();
        // A wide entry can hold later seqs past a gap; the page ends where
        // the seqs it covers stop running on.
        let mut ranges: Vec<&SeqRange> =
            entries.iter().flat_map(|e| &e.source_seq_ranges).collect();
        ranges.sort_unstable_by_key(|range| (range.start_seq, range.end_seq));
        let mut end = start_seq.saturating_sub(1);
        for range in ranges {
            if range.end_seq <= end {
                continue;
            }
            if range.start_seq > end + 1 {
                break;
            }
            end = range.end_seq.min(max_seq);
        }
        let end_seq = (!entries.is_empty() && end >= start_seq).then_some(end);
        Selection {
            entries,
            start_seq: end_seq.map(|_| start_seq),
            end_seq,
            has_older: start_seq > min_seq,
            has_newer: end_seq.is_some_and(|end| end < max_seq),
        }
    }

    /// The `limit` entries (all with 0) that start at or before `end_seq`,
    /// with `whole_turns` from the start of a turn and on to the end of the
    /// turn the last is in.
    pub fn before(&self, end_seq: u64, limit: usize, whole_turns: bool, max_seq: u64) -> Selection {
        let mut low = self
            .entries
            .partition_point(|entry| entry.seq_start <= end_seq);
        if low == 0 {
            return Selection::empty(false, true);
        }
        let requested = low.saturating_sub(limit_or_all(limit));
        let start = if whole_turns {
            self.whole_turn_start(requested)
        } else {
            requested
        };
        // The window is chosen before its right edge moves, so a jump to a
        // prompt keeps it however long the next turn is.
        let mut selected_end = end_seq;
        if whole_turns {
            while low < self.entries.len() && self.continues_turn(low) {
                low += 1;
            }
            selected_end = self
                .entries
                .get(low)
                .map_or(max_seq, |entry| entry.seq_start - 1);
        }
        Selection {
            entries: self.entries[start..low].to_vec(),
            start_seq: Some(self.entries[start].seq_start),
            end_seq: Some(selected_end),
            has_older: start > 0,
            has_newer: selected_end < max_seq,
        }
    }
}

impl Selection {
    pub fn empty(has_older: bool, has_newer: bool) -> Self {
        Self {
            entries: Vec::new(),
            start_seq: None,
            end_seq: None,
            has_older,
            has_newer,
        }
    }
}

/// A limit of 0 is all of them.
fn limit_or_all(limit: usize) -> usize {
    if limit == 0 { usize::MAX } else { limit }
}

/// A row as an entry of its own.
pub fn canonical(row: &Row) -> TimelineEntry {
    TimelineEntry {
        provider: row.provider.clone(),
        item: row.item.clone(),
        turn_id: row.turn_id.clone(),
        timestamp: row.timestamp.clone(),
        seq_start: row.seq,
        seq_end: row.seq,
        source_seq_ranges: vec![SeqRange {
            start_seq: row.seq,
            end_seq: row.seq,
        }],
        collapsed: Vec::new(),
    }
}

/// What makes later rows the same item (`timelineItemIdentity`): a tool
/// call's id, or a plugin item's plugin and id.
fn identity(item: &Value) -> Option<String> {
    match item["type"].as_str()? {
        "tool_call" => item["callId"].as_str().map(str::to_string),
        "plugin" => Some(format!(
            "{}/{}",
            text_of(&item["pluginId"]),
            text_of(&item["id"])
        )),
        _ => None,
    }
}

/// A string as JavaScript's template literal writes it.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

/// `entry`, a later row of the item `existing` is, folded into it; None
/// when it starts a new entry (a tool call reused in another turn).
fn merge_identity(existing: &TimelineEntry, entry: &TimelineEntry) -> Option<TimelineEntry> {
    match entry.item["type"].as_str() {
        Some("tool_call") => {
            if existing.item["type"] != "tool_call" || existing.turn_id != entry.turn_id {
                return None;
            }
            Some(TimelineEntry {
                item: merge_tool_calls(&existing.item, &entry.item),
                timestamp: entry.timestamp.clone(),
                seq_end: existing.seq_end.max(entry.seq_end),
                source_seq_ranges: merge_ranges(
                    &existing.source_seq_ranges,
                    &entry.source_seq_ranges,
                ),
                collapsed: with_kind(&existing.collapsed, &[], TOOL_LIFECYCLE),
                ..existing.clone()
            })
        }
        Some("plugin") => {
            if existing.item["type"] != "plugin" {
                return None;
            }
            Some(TimelineEntry {
                seq_start: existing.seq_start,
                source_seq_ranges: merge_ranges(
                    &existing.source_seq_ranges,
                    &entry.source_seq_ranges,
                ),
                collapsed: with_kind(&existing.collapsed, &[], IDENTITY),
                ..entry.clone()
            })
        }
        _ => None,
    }
}

/// A tool call's later update over the one it follows
/// (`mergeToolCallItems`): its fields win, but a known detail beats an
/// unknown one, metadata merges, and a call that finished well has no error.
pub fn merge_tool_calls(existing: &Value, incoming: &Value) -> Value {
    let (Some(old), Some(new)) = (existing.as_object(), incoming.as_object()) else {
        return incoming.clone();
    };
    let mut merged: Map<String, Value> = old.clone();
    merged.extend(new.iter().map(|(key, value)| (key.clone(), value.clone())));
    let unknown = |detail: Option<&Value>| detail.is_some_and(|d| d["type"] == "unknown");
    let (old_detail, new_detail) = (old.get("detail"), new.get("detail"));
    let detail = if unknown(new_detail) && old_detail.is_some_and(|d| !unknown(Some(d))) {
        old_detail
    } else {
        new_detail.or(old_detail)
    };
    match detail {
        Some(detail) => merged.insert("detail".into(), detail.clone()),
        None => merged.remove("detail"),
    };
    let metadata =
        |item: &Map<String, Value>| item.get("metadata").and_then(Value::as_object).cloned();
    match (metadata(old), metadata(new)) {
        (None, None) => {
            merged.remove("metadata");
        }
        (old, new) => {
            let mut both = old.unwrap_or_default();
            both.extend(new.unwrap_or_default());
            merged.insert("metadata".into(), Value::Object(both));
        }
    }
    match new.get("status").and_then(Value::as_str) {
        Some("failed") => match new.get("error") {
            Some(error) => merged.insert("error".into(), error.clone()),
            None => merged.remove("error"),
        },
        Some("completed" | "canceled") => merged.insert("error".into(), Value::Null),
        _ => None,
    };
    Value::Object(merged)
}

/// `entry` run on from `previous`, the entry before it, when both are
/// chunks of one assistant message or one reasoning block: adjacent seqs,
/// one turn, and for an assistant message the same message id when the
/// later chunk has one.
fn merge_text(previous: &TimelineEntry, entry: &TimelineEntry) -> Option<TimelineEntry> {
    let kind = previous.item["type"].as_str()?;
    if entry.item["type"] != kind
        || previous.seq_end + 1 != entry.seq_start
        || previous.turn_id != entry.turn_id
    {
        return None;
    }
    let text = format!(
        "{}{}",
        previous.item["text"].as_str().unwrap_or_default(),
        entry.item["text"].as_str().unwrap_or_default()
    );
    let (item, collapse) = match kind {
        "assistant_message" => {
            let (before, now) = (previous.item.get("messageId"), entry.item.get("messageId"));
            if now.is_some() && before != now {
                return None;
            }
            let mut item = serde_json::json!({"type": kind, "text": text});
            // Kept when it says something, as JavaScript's truthiness has it.
            if let Some(id) = before.filter(|id| !matches!(id, Value::Null | Value::Bool(false)))
                && id.as_str() != Some("")
            {
                item["messageId"] = id.clone();
            }
            (item, ASSISTANT_MERGE)
        }
        "reasoning" => (
            serde_json::json!({"type": kind, "text": text}),
            REASONING_MERGE,
        ),
        _ => return None,
    };
    Some(TimelineEntry {
        item,
        timestamp: entry.timestamp.clone(),
        seq_end: entry.seq_end,
        source_seq_ranges: merge_ranges(&previous.source_seq_ranges, &entry.source_seq_ranges),
        collapsed: with_kind(&previous.collapsed, &entry.collapsed, collapse),
        ..previous.clone()
    })
}

/// `existing`, then those of `more` it lacks, then `kind` if still missing.
fn with_kind(existing: &[String], more: &[String], kind: &str) -> Vec<String> {
    let mut kinds = existing.to_vec();
    for extra in more.iter().map(String::as_str).chain([kind]) {
        if !kinds.iter().any(|k| k == extra) {
            kinds.push(extra.to_string());
        }
    }
    kinds
}

/// `existing` with each seq of `incoming` added, as `mergeSeqRanges`: a seq
/// at or just past the last range's end runs it on, a later one starts a
/// new range.
fn merge_ranges(existing: &[SeqRange], incoming: &[SeqRange]) -> Vec<SeqRange> {
    let mut merged = existing.to_vec();
    for range in incoming {
        match merged.last_mut() {
            Some(last) if range.start_seq <= last.end_seq + 1 => {
                last.end_seq = last.end_seq.max(range.end_seq);
            }
            _ => merged.push(range.clone()),
        }
    }
    merged
}

/// The first seq of `entry` between `start` and `end`.
fn first_seq_in(entry: &TimelineEntry, start: u64, end: u64) -> Option<u64> {
    (entry.source_seq_ranges.iter())
        .map(|range| (range.start_seq.max(start), range.end_seq.min(end)))
        .find(|(first, last)| first <= last)
        .map(|(first, _)| first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(items: &[(Value, Option<&str>)]) -> Vec<Row> {
        (items.iter().enumerate())
            .map(|(n, (item, turn))| Row {
                seq: n as u64 + 1,
                provider: "echo".into(),
                item: item.clone(),
                turn_id: turn.map(str::to_string),
                timestamp: format!("2026-01-01T00:00:{n:02}.000Z"),
            })
            .collect()
    }

    fn text(kind: &str, text: &str) -> Value {
        json!({"type": kind, "text": text})
    }

    fn tool(id: &str, status: &str) -> Value {
        json!({"type": "tool_call", "callId": id, "name": "Bash", "status": status,
               "detail": {"type": "shell", "command": "ls"}, "error": null})
    }

    const T: Option<&str> = Some("turn-1");

    #[test]
    fn text_chunks_run_together_and_a_tool_call_folds_its_lifecycle() {
        let rows = rows(&[
            (text("user_message", "go"), T),
            (text("reasoning", "think "), T),
            (text("reasoning", "more"), T),
            (text("assistant_message", "Hel"), T),
            (text("assistant_message", "lo"), T),
            (tool("c1", "running"), T),
            (text("assistant_message", "while it runs"), T),
            (tool("c1", "completed"), T),
        ]);
        let projection = Projection::new(&rows);
        let entries = projection.entries();
        let items: Vec<&Value> = entries.iter().map(|e| &e.item).collect();
        assert_eq!(entries.len(), 5);
        assert_eq!(items[1]["text"], "think more");
        assert_eq!(entries[1].collapsed, ["reasoning_merge"]);
        assert_eq!(items[2]["text"], "Hello");
        assert_eq!((entries[2].seq_start, entries[2].seq_end), (4, 5));
        assert_eq!(items[3]["status"], "completed");
        assert_eq!(entries[3].collapsed, ["tool_lifecycle"]);
        assert_eq!(
            entries[3].source_seq_ranges,
            [
                SeqRange {
                    start_seq: 6,
                    end_seq: 6
                },
                SeqRange {
                    start_seq: 8,
                    end_seq: 8
                }
            ]
        );
        assert_eq!(
            entries[3].timestamp, rows[7].timestamp,
            "its latest row's time"
        );
        assert_eq!(items[4]["text"], "while it runs");
    }

    #[test]
    fn chunks_of_other_turns_or_messages_stay_apart() {
        let message = |id: &str, chunk: &str| json!({"type": "assistant_message", "text": chunk, "messageId": id});
        let projection = Projection::new(&rows(&[
            (text("assistant_message", "a"), T),
            (text("assistant_message", "b"), Some("turn-2")),
            (message("m1", "c"), Some("turn-2")),
            (message("m1", "d"), Some("turn-2")),
            (message("m2", "e"), Some("turn-2")),
        ]));
        let texts: Vec<&str> = (projection.entries().iter())
            .map(|e| e.item["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["a", "b", "cd", "e"]);
    }

    #[test]
    fn a_tool_calls_update_keeps_what_it_does_not_say() {
        let started = json!({"type": "tool_call", "callId": "c", "status": "running",
            "detail": {"type": "shell", "command": "ls"}, "metadata": {"a": 1}});
        let failed = json!({"type": "tool_call", "callId": "c", "status": "failed",
            "detail": {"type": "unknown"}, "metadata": {"b": 2}, "error": "boom"});
        let merged = merge_tool_calls(&started, &failed);
        assert_eq!(merged["detail"]["type"], "shell", "known beats unknown");
        assert_eq!(merged["metadata"], json!({"a": 1, "b": 2}));
        assert_eq!(merged["error"], "boom");
        let done = json!({"type": "tool_call", "callId": "c", "status": "completed"});
        let merged = merge_tool_calls(&merged, &done);
        assert_eq!(merged["error"], Value::Null);
        assert_eq!(merged["detail"]["command"], "ls");
    }

    /// A tool call started early and finished late spans everything
    /// between: a tail of one still starts at its start.
    #[test]
    fn a_tail_never_starts_inside_a_tool_call() {
        let rows = rows(&[
            (text("user_message", "go"), T),
            (tool("c1", "running"), T),
            (text("assistant_message", "x"), T),
            (tool("c2", "running"), T),
            (tool("c1", "completed"), T),
            (tool("c2", "completed"), T),
        ]);
        let projection = Projection::new(&rows);
        assert_eq!(projection.entries().len(), 4);
        let tail = projection.tail(1, false, 6);
        let starts: Vec<u64> = tail.entries.iter().map(|e| e.seq_start).collect();
        assert_eq!(starts, [2, 3, 4]);
        assert_eq!(tail.start_seq, Some(2));
        assert!(tail.has_older);
        let whole = projection.tail(1, true, 6);
        assert_eq!(whole.entries.len(), 4, "and with whole turns, its turn");
    }

    #[test]
    fn newer_pages_stop_where_their_seqs_stop_running_on() {
        let rows = rows(&[
            (text("user_message", "go"), T),
            (tool("c1", "running"), T),
            (text("assistant_message", "x"), T),
            (tool("c1", "completed"), T),
            (text("assistant_message", "y"), T),
        ]);
        let projection = Projection::new(&rows);
        // From seq 2, one entry: the tool call, which holds 2 and 4; the
        // page covers only 2, since 3 is another entry's.
        let page = projection.after(2, 1, 1, 5);
        assert_eq!(page.entries.len(), 1);
        assert_eq!((page.start_seq, page.end_seq), (Some(2), Some(2)));
        assert!(page.has_older && page.has_newer);
        // From seq 3: the text, then the tool call by its seq 4, then y.
        let page = projection.after(3, 0, 1, 5);
        let starts: Vec<u64> = page.entries.iter().map(|e| e.seq_start).collect();
        assert_eq!(starts, [2, 3, 5]);
        assert_eq!(page.end_seq, Some(5));
        assert!(!page.has_newer);
        // Caught up.
        let page = projection.after(6, 0, 1, 5);
        assert!(page.entries.is_empty() && page.end_seq.is_none() && !page.has_newer);
    }

    #[test]
    fn older_pages_count_entries() {
        let mut items = Vec::new();
        for turn in 1..=3 {
            let id = format!("turn-{turn}");
            items.push((text("user_message", "go"), Some(id.clone())));
            items.push((text("assistant_message", "a"), Some(id.clone())));
            items.push((text("assistant_message", "b"), Some(id.clone())));
            items.push((tool(&format!("c{turn}"), "completed"), Some(id)));
        }
        let rows: Vec<Row> = (items.iter().enumerate())
            .map(|(n, (item, turn))| Row {
                seq: n as u64 + 1,
                provider: "echo".into(),
                item: item.clone(),
                turn_id: turn.clone(),
                timestamp: String::new(),
            })
            .collect();
        let projection = Projection::new(&rows);
        assert_eq!(projection.entries().len(), 9, "three entries a turn");
        // Before seq 9 (the third turn's prompt): two entries back.
        let page = projection.before(8, 2, false, 12);
        let starts: Vec<u64> = page.entries.iter().map(|e| e.seq_start).collect();
        assert_eq!(starts, [6, 8]);
        assert_eq!(page.end_seq, Some(8));
        // With whole turns, from the second turn's prompt.
        let page = projection.before(8, 2, true, 12);
        let starts: Vec<u64> = page.entries.iter().map(|e| e.seq_start).collect();
        assert_eq!(starts, [5, 6, 8]);
        assert!(page.has_older && page.has_newer);
    }
}
