//! Reads a Burrow timeline file and prints the projected pages a grid of
//! reads gets, one JSON line each with keys sorted, to compare with what
//! Paseo's own `TimelineProjection` serves for the same rows.
//!
//!   cargo run -p burrow-daemon --example projection_pages -- <agent>.jsonl
use burrow_daemon::timeline::{Row, Timeline};
use burrow_protocol::timeline::{Direction, TimelineCursor};
use serde_json::{Value, json};

fn main() {
    let path = std::env::args().nth(1).expect("a timeline file");
    let text = std::fs::read_to_string(path).expect("readable");
    let rows: Vec<Row> = (text.lines())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["seq"].is_u64())
        .map(|value| serde_json::from_value(value).expect("a row"))
        .collect();
    let timeline = Timeline::restore("epoch".into(), rows);
    let max_seq = timeline.window().max_seq;
    let at = |seq: u64| TimelineCursor {
        epoch: "epoch".into(),
        seq,
    };
    let emit = |q: String, direction: Direction, cursor: Option<u64>, limit: usize, whole: bool| {
        let page = timeline.fetch(direction, cursor.map(at).as_ref(), limit, whole, true);
        let entries: Vec<Value> = (page.entries.iter())
            .map(|e| {
                json!({"item": e.item, "turnId": e.turn_id, "timestamp": e.timestamp,
                       "seqStart": e.seq_start, "seqEnd": e.seq_end,
                       "sourceSeqRanges": e.source_seq_ranges, "collapsed": e.collapsed})
            })
            .collect();
        let out = json!({
            "q": q, "entries": entries,
            "startSeq": page.start_cursor.map(|c| c.seq),
            "endSeq": page.end_cursor.map(|c| c.seq),
            "hasOlder": page.has_older, "hasNewer": page.has_newer,
        });
        println!("{}", sorted(&out));
    };
    let step = (max_seq / 60).max(1);
    for whole in [false, true] {
        for limit in [0, 1, 2, 3, 5, 10, 40, 200] {
            emit(
                format!("tail {limit} {whole}"),
                Direction::Tail,
                None,
                limit,
                whole,
            );
            for cursor in (1..=max_seq + 1).step_by(step as usize) {
                let q = format!("before {cursor} {limit} {whole}");
                emit(q, Direction::Before, Some(cursor), limit, whole);
            }
            if !whole {
                for cursor in (0..=max_seq).step_by(step as usize) {
                    emit(
                        format!("after {cursor} {limit}"),
                        Direction::After,
                        Some(cursor),
                        limit,
                        false,
                    );
                }
            }
        }
    }
}

/// `value` as JSON with every object's keys sorted and nulls of absent
/// fields left out, as `JSON.stringify` writes `undefined`.
fn sorted(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = (keys.into_iter())
                .filter(|key| !(key.as_str() == "turnId" && map[*key].is_null()))
                .map(|key| format!("{}:{}", Value::String(key.clone()), sorted(&map[key])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => {
            format!(
                "[{}]",
                items.iter().map(sorted).collect::<Vec<_>>().join(",")
            )
        }
        other => other.to_string(),
    }
}
