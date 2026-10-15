//! What the usage gauges draw: every source's latest reading, merged.
//! [`UsageSource`]s are polled off the UI thread on their own schedules;
//! an extension that gets numbers some other way, such as from the
//! daemons, hands them over with [`Usage::set_reading`].
//!
//! A failed poll leaves that source's previous reading in place; the title
//! bar dims a gauge once its own numbers are older than ten minutes.

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

    /// The readings as one snapshot: all pools, each with the time of the
    /// freshest source reporting it, and the newest source's time.
    fn merge(&mut self) {
        // Polled sources first: they name their accounts, where the
        // daemons only name hosts, so theirs win when both report one.
        let (polled, pushed): (Vec<_>, Vec<_>) =
            (self.readings.iter()).partition(|(key, _)| key.starts_with("source:"));
        let readings: Vec<&Reading> = (polled.into_iter().chain(pushed))
            .map(|(_, reading)| reading)
            .collect();
        let Some(newest) = readings
            .iter()
            .max_by_key(|reading| reading.snapshot.served_at_unix)
        else {
            self.snapshot = None;
            self.eligibility.clear();
            self.pool_served.clear();
            return;
        };
        let reported: Vec<(&usage_source::Pool, i64)> = (readings.iter())
            .flat_map(|r| (r.snapshot.pools.iter()).map(|pool| (pool, r.snapshot.served_at_unix)))
            .collect();
        // The same account reported by several sources, or through several
        // hosts, shows once.
        let pools =
            usage_source::dedupe_pools(reported.iter().map(|(pool, _)| (*pool).clone()).collect());
        // Its numbers are as fresh as the freshest source reporting it.
        self.pool_served = (pools.iter())
            .map(|pool| {
                let served = (reported.iter())
                    .filter(|(other, _)| usage_source::same_account(pool, other))
                    .map(|(_, served)| *served)
                    .max()
                    .unwrap_or(newest.snapshot.served_at_unix);
                (pool.id.clone(), served)
            })
            .collect();
        self.snapshot = Some(UsageSnapshot {
            revision: readings.iter().map(|r| r.snapshot.revision).sum(),
            served_at: newest.snapshot.served_at.clone(),
            served_at_unix: newest.snapshot.served_at_unix,
            pools,
        });
        self.eligibility = readings
            .iter()
            .flat_map(|r| r.eligibility.iter().cloned())
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_source::{Pool, Window};

    fn reading(served: i64, pools: &[(&str, &str, f64, f64)]) -> Reading {
        Reading {
            snapshot: UsageSnapshot {
                revision: 0,
                served_at: String::new(),
                served_at_unix: served,
                pools: (pools.iter())
                    .map(|(id, provider, used, resets)| Pool {
                        id: id.to_string(),
                        provider: provider.to_string(),
                        account_label: id.to_string(),
                        status: "ok".into(),
                        windows: vec![Window {
                            key: "weekly".into(),
                            label: "Weekly".into(),
                            used_percent: *used,
                            remaining_percent: 100. - used,
                            window_seconds: 604_800.,
                            resets_at: *resets,
                        }],
                    })
                    .collect(),
            },
            eligibility: Vec::new(),
        }
    }

    #[test]
    fn a_stale_source_dims_only_its_own_pools() {
        let mut usage = Usage::default();
        // A daemon that went quiet an hour ago, and a hub read just now.
        usage.readings.insert(
            "paseo:old".into(),
            reading(1_000, &[("paseo:old:claude", "claude", 40., 9e6)]),
        );
        usage.readings.insert(
            "source:0".into(),
            reading(4_600, &[("hub:codex", "codex", 10., 8e6)]),
        );
        usage.merge();
        assert_eq!(usage.pool_served["paseo:old:claude"], 1_000);
        assert_eq!(usage.pool_served["hub:codex"], 4_600);
        assert_eq!(usage.snapshot.as_ref().unwrap().served_at_unix, 4_600);
    }

    #[test]
    fn an_account_is_as_fresh_as_its_freshest_report() {
        let mut usage = Usage::default();
        // The hub's reading of an account is old; a daemon reports the same
        // account (same use, same reset) freshly.
        usage.readings.insert(
            "source:0".into(),
            reading(1_000, &[("hub:claude", "claude", 40., 9e6)]),
        );
        usage.readings.insert(
            "paseo:host".into(),
            reading(4_600, &[("paseo:claude:1", "claude", 40., 9e6)]),
        );
        usage.merge();
        let pools = &usage.snapshot.as_ref().unwrap().pools;
        assert_eq!(pools.len(), 1, "one account shows once");
        assert_eq!(pools[0].id, "hub:claude", "the hub names it");
        assert_eq!(usage.pool_served["hub:claude"], 4_600);
    }
}
