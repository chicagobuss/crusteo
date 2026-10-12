//! What the usage gauges draw: every source's latest reading, merged.
//! [`UsageSource`]s are polled off the UI thread on their own schedules;
//! an extension that gets numbers some other way, such as from the
//! daemons, hands them over with [`Usage::set_reading`].
//!
//! A failed poll leaves that source's previous reading in place; the title
//! bar dims a snapshot once `served_at` is older than ten minutes.

use std::collections::BTreeMap;
use std::sync::Arc;

use gpui_kit::gpui::Context;
use usage_source::{Eligibility, UsageSnapshot};

use crate::extensions::UsageSource;

/// One source's numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub snapshot: UsageSnapshot,
    /// Which pools are worth spending now; empty when the source cannot say.
    pub eligibility: Vec<Eligibility>,
}

#[derive(Default)]
pub struct Usage {
    /// Every source's pools together. None until one has answered.
    pub snapshot: Option<UsageSnapshot>,
    pub eligibility: Vec<Eligibility>,
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
                                    usage.readings.insert(key.clone(), reading);
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

    /// Sets, or with `None` clears, the reading under `key`.
    pub fn set_reading(&mut self, key: &str, reading: Option<Reading>, cx: &mut Context<Self>) {
        match reading {
            Some(reading) => self.readings.insert(key.to_string(), reading),
            None => self.readings.remove(key),
        };
        self.merge();
        cx.notify();
    }

    /// The readings as one snapshot: all pools, and the oldest source's
    /// time, so staleness shows if any source falls behind.
    fn merge(&mut self) {
        // Polled sources first: they name their accounts, where the
        // daemons only name hosts, so theirs win when both report one.
        let (polled, pushed): (Vec<_>, Vec<_>) =
            (self.readings.iter()).partition(|(key, _)| key.starts_with("source:"));
        let readings: Vec<&Reading> = (polled.into_iter().chain(pushed))
            .map(|(_, reading)| reading)
            .collect();
        let Some(oldest) = readings
            .iter()
            .min_by_key(|reading| reading.snapshot.served_at_unix)
        else {
            self.snapshot = None;
            self.eligibility.clear();
            return;
        };
        self.snapshot = Some(UsageSnapshot {
            revision: readings.iter().map(|r| r.snapshot.revision).sum(),
            served_at: oldest.snapshot.served_at.clone(),
            served_at_unix: oldest.snapshot.served_at_unix,
            // The same account reported by several sources, or through
            // several hosts, shows once.
            pools: usage_source::dedupe_pools(
                readings
                    .iter()
                    .flat_map(|r| r.snapshot.pools.iter().cloned())
                    .collect(),
            ),
        });
        self.eligibility = readings
            .iter()
            .flat_map(|r| r.eligibility.iter().cloned())
            .collect();
    }
}
