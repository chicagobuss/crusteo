//! What the usage gauges draw: every source's latest reading, merged.
//! [`UsageSource`]s are polled off the UI thread on their own schedules;
//! an extension that gets numbers some other way, such as from the
//! daemons, hands them over with [`Usage::set_reading`].
//!
//! A failed poll leaves that source's previous reading in place; an
//! account whose own read failed keeps its last numbers, marked as such
//! (as Paseo 0.12 keeps them). The title bar dims a gauge once its own
//! numbers are older than ten minutes. Nothing is kept on disk.

use std::collections::BTreeMap;
use std::sync::Arc;

use gpui_kit::gpui::Context;
pub use usage_source::Reading;
use usage_source::{Eligibility, UsageSnapshot};

use crate::extensions::UsageSource;

#[derive(Default)]
pub struct Usage {
    /// Every source's pools together. None until one has answered.
    pub snapshot: Option<UsageSnapshot>,
    pub eligibility: Vec<Eligibility>,
    /// When each pool's numbers were read, by pool id, in unix seconds: a
    /// gauge dims on its own numbers going stale, not on some other
    /// source's.
    pub pool_served: std::collections::HashMap<String, i64>,
    pub last_error: Option<String>,
    /// By source: `"source:<n>"` for polled ones, any other key for the
    /// rest.
    readings: BTreeMap<String, Reading>,
}

impl Usage {
    /// Starts polling `sources`. Without any there are no gauges.
    pub fn start(&mut self, sources: Vec<Arc<dyn UsageSource>>, cx: &mut Context<Self>) {
        for (ix, source) in sources.into_iter().enumerate() {
            let key = format!("source:{ix}");
            cx.spawn(async move |this, cx| {
                loop {
                    let polled = cx
                        .background_executor()
                        .spawn({
                            let source = source.clone();
                            async move { source.poll() }
                        })
                        .await;
                    let alive = this
                        .update(cx, |usage, cx| {
                            match polled {
                                Ok(reading) => {
                                    usage_source::merge::set(
                                        &mut usage.readings,
                                        &key,
                                        Some(reading),
                                    );
                                    usage.last_error = None;
                                    usage.merge();
                                }
                                Err(err) => {
                                    if usage.last_error.as_deref() != Some(err.as_str()) {
                                        eprintln!("crusteo: usage: {err}");
                                    }
                                    usage.last_error = Some(err);
                                }
                            }
                            cx.notify();
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                    cx.background_executor().timer(source.every()).await;
                }
            })
            .detach();
        }
    }

    /// Sets, or with `None` clears, the reading under `key`. Accounts
    /// whose read failed keep their last numbers, and every account its
    /// place ([`usage_source::merge::set`]).
    pub fn set_reading(&mut self, key: &str, reading: Option<Reading>, cx: &mut Context<Self>) {
        usage_source::merge::set(&mut self.readings, key, reading);
        self.merge();
        cx.notify();
    }

    /// The readings as one snapshot, by the rules every screen shares
    /// ([`usage_source::merge`]).
    fn merge(&mut self) {
        let merged = usage_source::merge::merge(&self.readings);
        self.snapshot = merged.snapshot;
        self.eligibility = merged.eligibility;
        self.pool_served = merged.pool_served;
    }

    /// Each account's gauge, as Reef's phones get it too.
    pub fn rows(&self, now: i64) -> Vec<usage_source::view::Row> {
        let merged = usage_source::merge::Merged {
            snapshot: self.snapshot.clone(),
            eligibility: self.eligibility.clone(),
            pool_served: self.pool_served.clone(),
        };
        usage_source::view::rows(&merged, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reef's `usage` view checks its rows against the same file
    /// (`crates/reef/src/usage.rs`), so the title bar and the phones agree.
    #[test]
    fn the_gauges_make_the_rows_reef_sends() {
        const NOW: i64 = 1_800_000_000;
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/crates/usage-source/tests/fixtures/agreement"
        );
        let read = |name: &str| std::fs::read_to_string(format!("{dir}/{name}")).unwrap();
        let mut usage = Usage::default();
        // A usage feed, polled as the first usage source, and a daemon's
        // reports, as the daemon extension keys them.
        let snapshot = usage_source::feed::parse_usage(&read("feed.json")).unwrap();
        usage.readings.insert(
            "source:0".into(),
            Reading {
                snapshot,
                eligibility: Vec::new(),
            },
        );
        let updates: Vec<serde_json::Value> = serde_json::from_str(&read("reports.json")).unwrap();
        let daemon = usage_source::reports::parse_reports("alpha", &updates, NOW).unwrap();
        usage.readings.insert("paseo:alpha".into(), daemon);
        usage.merge();
        let rows: Vec<serde_json::Value> = usage.rows(NOW).iter().map(|r| r.to_json()).collect();
        let pinned: serde_json::Value = serde_json::from_str(&read("rows.json")).unwrap();
        assert_eq!(serde_json::Value::Array(rows), pinned);
    }
}
