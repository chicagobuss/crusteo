//! Per-agent timelines and their paged reads.
//!
//! Rows are canonical: one per (coalesced) provider event, `seq` 1, 2, 3, ...
//! within an epoch, and immutable once appended, which is what lets them be
//! stored append-only (see `persist`). Pages are served *projected*, as
//! Paseo serves them, a tool call's lifecycle and adjacent text chunks
//! folded into one entry ([`crate::projection`]); a client that asks for
//! `canonical` gets one entry per row. Live `agent_stream` frames carry rows.

use burrow_protocol::timeline::{Direction, TimelineCursor, TimelineEntry, TimelineWindow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids;
use crate::projection::{self, Projection};

/// One canonical row. Also the line format of the durable timeline file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub seq: u64,
    pub provider: String,
    pub item: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub timestamp: String,
}

#[derive(Debug, Clone)]
pub struct Timeline {
    pub epoch: String,
    rows: Vec<Row>,
    /// The rows folded, kept up to date as they come.
    projection: Projection,
    next_seq: u64,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of one read, before it is wrapped into a response.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub epoch: String,
    pub reset: bool,
    pub stale_cursor: bool,
    pub gap: bool,
    pub window: TimelineWindow,
    pub start_cursor: Option<TimelineCursor>,
    pub end_cursor: Option<TimelineCursor>,
    pub has_older: bool,
    pub has_newer: bool,
    pub entries: Vec<TimelineEntry>,
}

impl Timeline {
    pub fn new() -> Self {
        Self {
            epoch: ids::generation(),
            rows: Vec::new(),
            projection: Projection::default(),
            next_seq: 1,
        }
    }

    /// A timeline read back from disk: same epoch, same seqs.
    pub fn restore(epoch: String, rows: Vec<Row>) -> Self {
        let next_seq = rows.last().map_or(1, |row| row.seq + 1);
        Self {
            epoch,
            projection: Projection::new(&rows),
            rows,
            next_seq,
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn append(
        &mut self,
        provider: &str,
        item: Value,
        turn_id: Option<String>,
        timestamp: String,
    ) -> Row {
        let row = Row {
            seq: self.next_seq,
            provider: provider.to_string(),
            item,
            turn_id,
            timestamp,
        };
        self.next_seq += 1;
        self.projection.append(&row);
        self.rows.push(row.clone());
        row
    }

    /// The rows as history reads serve them, folded.
    pub fn entries(&self) -> &[TimelineEntry] {
        self.projection.entries()
    }

    pub fn window(&self) -> TimelineWindow {
        TimelineWindow {
            min_seq: self.rows.first().map_or(0, |row| row.seq),
            max_seq: self.next_seq - 1,
            next_seq: self.next_seq,
        }
    }

    /// `limit` counts entries; 0 means all. With `whole_turns`, as Paseo
    /// 0.14 pages for clients that fold finished turns, a tail or older page
    /// never starts partway into a turn, and an older page runs on to the
    /// end of the turn it stops in; newer pages stay as they are. Without
    /// `projected`, each row is an entry of its own.
    pub fn fetch(
        &self,
        direction: Direction,
        cursor: Option<&TimelineCursor>,
        limit: usize,
        whole_turns: bool,
        projected: bool,
    ) -> Page {
        let window = self.window();
        let stale_cursor = cursor.is_some_and(|cursor| cursor.epoch != self.epoch);
        let gap = !stale_cursor
            && direction == Direction::After
            && !self.rows.is_empty()
            && cursor.is_some_and(|cursor| cursor.seq.saturating_add(1) < window.min_seq);
        let reset = stale_cursor || gap;
        let direction = if reset { Direction::Tail } else { direction };
        let cursor_seq = cursor.map(|cursor| cursor.seq);
        let selection = if projected {
            self.select_projected(direction, cursor_seq, limit, window, whole_turns)
        } else {
            let canonical = self.select(direction, cursor_seq, limit, window, whole_turns);
            projection::Selection {
                entries: canonical.rows.iter().map(projection::canonical).collect(),
                start_seq: canonical.start_seq,
                end_seq: canonical.end_seq,
                has_older: canonical.has_older,
                has_newer: canonical.has_newer,
            }
        };
        let at = |seq: Option<u64>| {
            seq.map(|seq| TimelineCursor {
                epoch: self.epoch.clone(),
                seq,
            })
        };
        Page {
            epoch: self.epoch.clone(),
            reset,
            stale_cursor,
            gap,
            window,
            start_cursor: at(selection.start_seq),
            end_cursor: at(selection.end_seq),
            has_older: selection.has_older,
            has_newer: selection.has_newer,
            entries: selection.entries,
        }
    }

    /// A page of folded entries, as Paseo's `TimelineProjection.selectPage`.
    fn select_projected(
        &self,
        direction: Direction,
        cursor_seq: Option<u64>,
        limit: usize,
        window: TimelineWindow,
        whole_turns: bool,
    ) -> projection::Selection {
        let projection = &self.projection;
        if self.rows.is_empty() {
            let flags = self.select(direction, cursor_seq, limit, window, whole_turns);
            return projection::Selection::empty(flags.has_older, flags.has_newer);
        }
        match direction {
            Direction::Tail => projection.tail(limit, whole_turns, window.max_seq),
            Direction::After => {
                let cursor = cursor_seq.unwrap_or(window.min_seq.saturating_sub(1));
                let start_seq = window.min_seq.max(cursor.saturating_add(1));
                projection.after(start_seq, limit, window.min_seq, window.max_seq)
            }
            Direction::Before => {
                let cursor = cursor_seq.unwrap_or(window.max_seq.saturating_add(1));
                let end_seq = window.max_seq.min(cursor.saturating_sub(1));
                if cursor == 0 || end_seq < window.min_seq {
                    return projection::Selection::empty(false, true);
                }
                projection.before(end_seq, limit, whole_turns, window.max_seq)
            }
        }
    }

    fn select(
        &self,
        direction: Direction,
        cursor_seq: Option<u64>,
        limit: usize,
        window: TimelineWindow,
        whole_turns: bool,
    ) -> Selection<'_> {
        let rows = &self.rows[..];
        // Back to the first row of the turn `start` is in.
        let turn_start = |mut start: usize| {
            if whole_turns {
                while start > 0 && continues_turn(&rows[start - 1], &rows[start]) {
                    start -= 1;
                }
            }
            start
        };
        let take_last = |eligible: &'_ [Row]| -> usize {
            if limit == 0 {
                0
            } else {
                eligible.len().saturating_sub(limit)
            }
        };
        if rows.is_empty() {
            // No rows: only the flags carry information.
            let (has_older, has_newer) = match direction {
                Direction::After => {
                    let cursor = cursor_seq.unwrap_or(window.min_seq.saturating_sub(1));
                    (cursor >= window.min_seq, cursor < window.max_seq)
                }
                Direction::Before => {
                    let cursor = cursor_seq.unwrap_or(window.max_seq.saturating_add(1));
                    (cursor > window.min_seq, cursor <= window.max_seq)
                }
                Direction::Tail => (false, false),
            };
            return Selection::empty(has_older, has_newer);
        }
        match direction {
            Direction::Tail => {
                let start = turn_start(take_last(rows));
                Selection {
                    rows: &rows[start..],
                    start_seq: Some(rows[start].seq),
                    end_seq: Some(window.max_seq),
                    has_older: start > 0,
                    has_newer: false,
                }
            }
            Direction::After => {
                let cursor = cursor_seq.unwrap_or(window.min_seq.saturating_sub(1));
                let start_seq = window.min_seq.max(cursor.saturating_add(1));
                let from = rows.partition_point(|row| row.seq < start_seq);
                let eligible = &rows[from..];
                let count = if limit == 0 {
                    eligible.len()
                } else {
                    limit.min(eligible.len())
                };
                let selected = &eligible[..count];
                let end_seq = selected.last().map(|row| row.seq);
                Selection {
                    rows: selected,
                    start_seq: end_seq.map(|_| start_seq),
                    end_seq,
                    has_older: start_seq > window.min_seq,
                    has_newer: end_seq.is_some_and(|end| end < window.max_seq),
                }
            }
            Direction::Before => {
                let cursor = cursor_seq.unwrap_or(window.max_seq.saturating_add(1));
                let end_seq = window.max_seq.min(cursor.saturating_sub(1));
                if cursor == 0 || end_seq < window.min_seq {
                    return Selection::empty(false, end_seq < window.max_seq);
                }
                let mut until = rows.partition_point(|row| row.seq <= end_seq);
                let start = turn_start(take_last(&rows[..until]));
                // The window is chosen before its right edge moves, so a
                // jump to a prompt keeps it however long the next turn is.
                let mut end_seq = end_seq;
                if whole_turns && until > 0 {
                    while until < rows.len() && continues_turn(&rows[until - 1], &rows[until]) {
                        until += 1;
                    }
                    end_seq = rows.get(until).map_or(window.max_seq, |row| row.seq - 1);
                }
                let selected = &rows[start..until];
                Selection {
                    rows: selected,
                    start_seq: selected.first().map(|row| row.seq),
                    end_seq: Some(end_seq),
                    has_older: start > 0,
                    has_newer: end_seq < window.max_seq,
                }
            }
        }
    }
}

/// Whether `next` is part of the same turn as `previous`, as Paseo's
/// `continuesTimelineTurn` says: by their turn ids when both have one, or
/// else unless `next` is what starts a turn.
fn continues_turn(previous: &Row, next: &Row) -> bool {
    continues_turn_of(
        previous.turn_id.as_deref(),
        next.turn_id.as_deref(),
        &next.item,
    )
}

/// [`continues_turn`] by turn ids and the later item.
pub(crate) fn continues_turn_of(previous: Option<&str>, next: Option<&str>, item: &Value) -> bool {
    match (previous, next) {
        (Some(previous), Some(next)) => previous == next,
        _ => !starts_turn(item),
    }
}

/// A prompt, a message from another agent, or a heartbeat: what a turn
/// opens with (Paseo's `isTimelineTurnTrigger`).
fn starts_turn(item: &Value) -> bool {
    match item["type"].as_str() {
        Some("user_message") => true,
        Some("tool_call") => item.get("agentMessage").is_some() || item["name"] == "heartbeat",
        _ => false,
    }
}

struct Selection<'a> {
    rows: &'a [Row],
    start_seq: Option<u64>,
    end_seq: Option<u64>,
    has_older: bool,
    has_newer: bool,
}

impl Selection<'_> {
    fn empty(has_older: bool, has_newer: bool) -> Self {
        Self {
            rows: &[],
            start_seq: None,
            end_seq: None,
            has_older,
            has_newer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn timeline(rows: u64) -> Timeline {
        let mut timeline = Timeline::new();
        for n in 1..=rows {
            timeline.append(
                "echo",
                json!({"type": "assistant_message", "text": n.to_string()}),
                Some("foreground-turn-1".into()),
                "2026-01-01T00:00:00.000Z".into(),
            );
        }
        timeline
    }

    fn cursor(timeline: &Timeline, seq: u64) -> TimelineCursor {
        TimelineCursor {
            epoch: timeline.epoch.clone(),
            seq,
        }
    }

    #[test]
    fn tail_takes_the_latest_page() {
        let timeline = timeline(100);
        let page = timeline.fetch(Direction::Tail, None, 40, false, false);
        assert_eq!(page.entries.len(), 40);
        assert_eq!(page.start_cursor.unwrap().seq, 61);
        assert_eq!(page.end_cursor.unwrap().seq, 100);
        assert!(page.has_older && !page.has_newer && !page.reset);
        assert_eq!(page.window.next_seq, 101);
    }

    #[test]
    fn before_and_after_page_from_a_cursor() {
        let timeline = timeline(100);
        let older = timeline.fetch(
            Direction::Before,
            Some(&cursor(&timeline, 12)),
            3,
            false,
            false,
        );
        assert_eq!(older.start_cursor.unwrap().seq, 9);
        assert_eq!(older.end_cursor.unwrap().seq, 11);
        assert!(older.has_older && older.has_newer);

        let newer = timeline.fetch(
            Direction::After,
            Some(&cursor(&timeline, 90)),
            3,
            false,
            false,
        );
        assert_eq!(newer.start_cursor.unwrap().seq, 91);
        assert_eq!(newer.end_cursor.unwrap().seq, 93);
        assert!(newer.has_newer);

        let caught_up = timeline.fetch(
            Direction::After,
            Some(&cursor(&timeline, 100)),
            40,
            false,
            false,
        );
        assert!(caught_up.entries.is_empty() && !caught_up.has_newer);
    }

    #[test]
    fn a_foreign_epoch_resets_to_the_tail() {
        let timeline = timeline(5);
        let stale = TimelineCursor {
            epoch: "other".into(),
            seq: 2,
        };
        let page = timeline.fetch(Direction::After, Some(&stale), 40, false, false);
        assert!(page.reset && page.stale_cursor && !page.gap);
        assert_eq!(page.entries.len(), 5);
    }

    #[test]
    fn the_largest_cursor_reads_without_overflowing() {
        let timeline = timeline(5);
        let last = cursor(&timeline, u64::MAX);
        let after = timeline.fetch(Direction::After, Some(&last), 40, false, false);
        assert!(after.entries.is_empty() && !after.reset && !after.has_newer);
        let before = timeline.fetch(Direction::Before, Some(&last), 40, false, false);
        assert_eq!(before.entries.len(), 5);
        let empty = Timeline::new();
        let after = empty.fetch(
            Direction::After,
            Some(&cursor(&empty, u64::MAX)),
            40,
            false,
            false,
        );
        assert!(after.entries.is_empty());
    }

    /// Three turns of four rows (a prompt, then three replies), the last
    /// read back from history without turn ids.
    fn turns() -> Timeline {
        let mut timeline = Timeline::new();
        for turn in 1..=3 {
            for n in 0..4 {
                let item = if n == 0 {
                    json!({"type": "user_message", "text": format!("prompt {turn}")})
                } else {
                    json!({"type": "assistant_message", "text": format!("{turn}.{n}")})
                };
                let id = (turn < 3).then(|| format!("foreground-turn-{turn}"));
                timeline.append("echo", item, id, "2026-01-01T00:00:00.000Z".into());
            }
        }
        timeline
    }

    #[test]
    fn whole_turn_pages_start_at_a_turn_and_older_ones_finish_it() {
        let timeline = turns();
        let seqs = |page: &Page| page.entries.iter().map(|e| e.seq_start).collect::<Vec<_>>();
        // A tail of two would start inside the third turn.
        let tail = timeline.fetch(Direction::Tail, None, 2, true, false);
        assert_eq!(seqs(&tail), [9, 10, 11, 12]);
        assert!(tail.has_older);
        assert_eq!(
            seqs(&timeline.fetch(Direction::Tail, None, 2, false, false)),
            [11, 12]
        );
        // Older than seq 7: two rows back is seq 5, the second turn's
        // prompt; the page runs on to that turn's end.
        let older = timeline.fetch(
            Direction::Before,
            Some(&cursor(&timeline, 7)),
            2,
            true,
            false,
        );
        assert_eq!(seqs(&older), [5, 6, 7, 8]);
        assert_eq!(older.end_cursor.unwrap().seq, 8);
        assert!(older.has_older && older.has_newer);
        // Newer pages are as they were.
        let newer = timeline.fetch(
            Direction::After,
            Some(&cursor(&timeline, 1)),
            2,
            true,
            false,
        );
        assert_eq!(seqs(&newer), [2, 3]);
    }

    #[test]
    fn a_message_from_another_agent_or_a_heartbeat_starts_a_turn() {
        assert!(starts_turn(&json!({"type": "user_message", "text": "hi"})));
        assert!(starts_turn(
            &json!({"type": "tool_call", "name": "agent_message", "agentMessage": {}})
        ));
        assert!(starts_turn(
            &json!({"type": "tool_call", "name": "heartbeat"})
        ));
        assert!(!starts_turn(&json!({"type": "tool_call", "name": "Bash"})));
        assert!(!starts_turn(
            &json!({"type": "assistant_message", "text": "ok"})
        ));
    }

    /// Pages are folded by default; read back from disk, a timeline folds
    /// the same as it did live.
    #[test]
    fn projected_pages_fold_and_survive_a_restore() {
        let mut live = Timeline::new();
        let rows = [
            json!({"type": "user_message", "text": "go"}),
            json!({"type": "tool_call", "callId": "c", "status": "running"}),
            json!({"type": "assistant_message", "text": "a"}),
            json!({"type": "assistant_message", "text": "b"}),
            json!({"type": "tool_call", "callId": "c", "status": "completed"}),
        ];
        for item in rows {
            live.append("echo", item, Some("t1".into()), String::new());
        }
        let page = live.fetch(Direction::Tail, None, 40, false, true);
        let items: Vec<&Value> = page.entries.iter().map(|e| &e.item).collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[1]["status"], "completed");
        assert_eq!(items[2]["text"], "ab");
        assert_eq!(page.end_cursor.unwrap().seq, 5);
        let canonical = live.fetch(Direction::Tail, None, 40, false, false);
        assert_eq!(canonical.entries.len(), 5);

        let restored = Timeline::restore(live.epoch.clone(), live.rows().to_vec());
        assert_eq!(restored.entries(), live.entries());
        let older = restored.fetch(
            Direction::Before,
            Some(&cursor(&restored, 3)),
            1,
            false,
            true,
        );
        assert_eq!(older.entries.len(), 1);
        assert_eq!(
            older.entries[0].item["status"], "completed",
            "the whole call"
        );
    }

    #[test]
    fn an_empty_timeline_has_an_empty_window() {
        let page = Timeline::new().fetch(Direction::Tail, None, 40, false, false);
        assert!(page.entries.is_empty());
        assert_eq!(
            (
                page.window.min_seq,
                page.window.max_seq,
                page.window.next_seq
            ),
            (0, 0, 1)
        );
        assert!(page.start_cursor.is_none() && page.end_cursor.is_none());
    }
}
