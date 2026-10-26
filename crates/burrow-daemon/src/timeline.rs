//! Per-agent timelines and their paged reads.
//!
//! Rows are canonical: one per (coalesced) provider event, `seq` 1, 2, 3, ...
//! within an epoch, and immutable once appended, which is what lets them be
//! stored append-only (see `persist`). Paseo serves *projected* pages, folding a tool call's lifecycle and
//! adjacent text chunks into one entry (`timeline-projection.ts`). That fold
//! is phase 1 work; until then each row is its own entry, which is a valid
//! (unfolded) projection: clients already merge raw live rows the same way.
//! The page selection follows `selectProjectedTimelinePage`.

use burrow_protocol::timeline::{
    Direction, SeqRange, TimelineCursor, TimelineEntry, TimelineWindow,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids;

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

impl Row {
    fn entry(&self) -> TimelineEntry {
        TimelineEntry {
            provider: self.provider.clone(),
            item: self.item.clone(),
            turn_id: self.turn_id.clone(),
            timestamp: self.timestamp.clone(),
            seq_start: self.seq,
            seq_end: self.seq,
            source_seq_ranges: vec![SeqRange {
                start_seq: self.seq,
                end_seq: self.seq,
            }],
            collapsed: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Timeline {
    pub epoch: String,
    rows: Vec<Row>,
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
            next_seq: 1,
        }
    }

    /// A timeline read back from disk: same epoch, same seqs.
    pub fn restore(epoch: String, rows: Vec<Row>) -> Self {
        let next_seq = rows.last().map_or(1, |row| row.seq + 1);
        Self {
            epoch,
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
        self.rows.push(row.clone());
        row
    }

    pub fn window(&self) -> TimelineWindow {
        TimelineWindow {
            min_seq: self.rows.first().map_or(0, |row| row.seq),
            max_seq: self.next_seq - 1,
            next_seq: self.next_seq,
        }
    }

    /// `limit` counts entries; 0 means all.
    pub fn fetch(
        &self,
        direction: Direction,
        cursor: Option<&TimelineCursor>,
        limit: usize,
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
        let selection = self.select(direction, cursor_seq, limit, window);
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
            entries: selection.rows.iter().map(|row| row.entry()).collect(),
        }
    }

    fn select(
        &self,
        direction: Direction,
        cursor_seq: Option<u64>,
        limit: usize,
        window: TimelineWindow,
    ) -> Selection<'_> {
        let rows = &self.rows[..];
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
                let start = take_last(rows);
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
                let until = rows.partition_point(|row| row.seq <= end_seq);
                let eligible = &rows[..until];
                let start = take_last(eligible);
                let selected = &eligible[start..];
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
        let page = timeline.fetch(Direction::Tail, None, 40);
        assert_eq!(page.entries.len(), 40);
        assert_eq!(page.start_cursor.unwrap().seq, 61);
        assert_eq!(page.end_cursor.unwrap().seq, 100);
        assert!(page.has_older && !page.has_newer && !page.reset);
        assert_eq!(page.window.next_seq, 101);
    }

    #[test]
    fn before_and_after_page_from_a_cursor() {
        let timeline = timeline(100);
        let older = timeline.fetch(Direction::Before, Some(&cursor(&timeline, 12)), 3);
        assert_eq!(older.start_cursor.unwrap().seq, 9);
        assert_eq!(older.end_cursor.unwrap().seq, 11);
        assert!(older.has_older && older.has_newer);

        let newer = timeline.fetch(Direction::After, Some(&cursor(&timeline, 90)), 3);
        assert_eq!(newer.start_cursor.unwrap().seq, 91);
        assert_eq!(newer.end_cursor.unwrap().seq, 93);
        assert!(newer.has_newer);

        let caught_up = timeline.fetch(Direction::After, Some(&cursor(&timeline, 100)), 40);
        assert!(caught_up.entries.is_empty() && !caught_up.has_newer);
    }

    #[test]
    fn a_foreign_epoch_resets_to_the_tail() {
        let timeline = timeline(5);
        let stale = TimelineCursor {
            epoch: "other".into(),
            seq: 2,
        };
        let page = timeline.fetch(Direction::After, Some(&stale), 40);
        assert!(page.reset && page.stale_cursor && !page.gap);
        assert_eq!(page.entries.len(), 5);
    }

    #[test]
    fn the_largest_cursor_reads_without_overflowing() {
        let timeline = timeline(5);
        let last = cursor(&timeline, u64::MAX);
        let after = timeline.fetch(Direction::After, Some(&last), 40);
        assert!(after.entries.is_empty() && !after.reset && !after.has_newer);
        let before = timeline.fetch(Direction::Before, Some(&last), 40);
        assert_eq!(before.entries.len(), 5);
        let empty = Timeline::new();
        let after = empty.fetch(Direction::After, Some(&cursor(&empty, u64::MAX)), 40);
        assert!(after.entries.is_empty());
    }

    #[test]
    fn an_empty_timeline_has_an_empty_window() {
        let page = Timeline::new().fetch(Direction::Tail, None, 40);
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
