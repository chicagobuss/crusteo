//! The rows a usage feed and a daemon's reports make, pinned in
//! `fixtures/agreement/rows.json`. crusteo's gauges and Reef's `usage`
//! view each check their own path against the same file, so they agree.
//!
//! `UPDATE_USAGE_ROWS=1 cargo test -p usage-source --test agreement`
//! rewrites the file after a deliberate change.

use std::collections::BTreeMap;

use serde_json::Value;
use usage_source::{Reading, feed, merge, reports, view};

/// The clock the fixture is read at: 2027-01-15T08:00:00Z.
const NOW: i64 = 1_800_000_000;

fn rows() -> Value {
    let mut readings = BTreeMap::new();
    let snapshot = feed::parse_usage(include_str!("fixtures/agreement/feed.json")).unwrap();
    readings.insert(
        "source:0".to_string(),
        Reading {
            snapshot,
            eligibility: Vec::new(),
        },
    );
    let updates: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/agreement/reports.json")).unwrap();
    let daemon = reports::parse_reports("alpha", &updates, NOW).unwrap();
    readings.insert("paseo:alpha".to_string(), daemon);
    let merged = merge::merge(&readings);
    view::rows(&merged, NOW)
        .iter()
        .map(view::Row::to_json)
        .collect()
}

#[test]
fn the_fixture_makes_the_pinned_rows() {
    let rows = rows();
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/agreement/rows.json"
    );
    if std::env::var_os("UPDATE_USAGE_ROWS").is_some() {
        let text = serde_json::to_string_pretty(&rows).unwrap() + "\n";
        std::fs::write(path, text).unwrap();
    }
    let pinned: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(rows, pinned);
}

#[test]
fn the_fixture_shows_each_account_once_named_by_the_feed() {
    let rows = rows();
    let keys: Vec<&str> = (rows.as_array().unwrap().iter())
        .map(|row| row["key"].as_str().unwrap())
        .collect();
    // The daemon's codex account is the feed's, seen again.
    assert_eq!(
        keys,
        [
            "claude:work",
            "codex:personal",
            "gemini:personal",
            "paseo:claude:home"
        ]
    );
    let codex = &rows[1];
    assert_eq!(codex["windows"].as_array().unwrap().len(), 1, "no mirror");
    assert_eq!(codex["plan"], "pro");
    let claude = &rows[0];
    assert_eq!(claude["account"], "work");
    assert_eq!(claude["windows"][1]["name"], "weekly Global");
    assert_eq!(rows[3]["account"], "home");
    assert_eq!(rows[2]["use_before"]["window"], "Gemini models (5h)");
    let gemini: Vec<&str> = (rows[2]["groups"].as_array().unwrap().iter())
        .map(|group| group["name"].as_str().unwrap())
        .collect();
    assert_eq!(gemini, ["Gemini models", "Gemini 4 Argon"]);
    assert_eq!(rows[2]["binding"], "argon-weekly");
}
