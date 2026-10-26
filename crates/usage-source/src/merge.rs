//! Every source's latest reading as one: each account once, as fresh as
//! the freshest source reporting it.

use std::collections::{BTreeMap, HashMap};

use crate::{Eligibility, Pool, Reading, UsageSnapshot};

/// Readings are kept by source. Keys starting with this are sources that
/// name their accounts (a usage feed); theirs win over the daemons', which
/// may only name hosts, when both report one account.
pub const NAMED_PREFIX: &str = "source:";

/// The readings, merged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Merged {
    /// Every source's pools together. None until one has answered.
    pub snapshot: Option<UsageSnapshot>,
    pub eligibility: Vec<Eligibility>,
    /// When each pool's numbers were read, by pool id, in unix seconds: a
    /// gauge dims on its own numbers going stale, not on some other
    /// source's.
    pub pool_served: HashMap<String, i64>,
}

/// Puts `reading` under `key` in `readings`, or with None takes the
/// key's away, saying whether that changed anything. Every screen keeps
/// its sources' readings this way, so that, as in Paseo 0.12:
///
/// - A failed read doesn't blank an account. One whose latest read failed
///   without the source keeping its last good numbers (a daemon before
///   0.12, a feed) takes them from the key's previous reading, and is
///   shown with them as retained.
/// - Accounts keep their places. A daemon lists its reports as they
///   finish, in a new order each time; accounts already there stay where
///   they were, and new ones go last.
pub fn set(readings: &mut BTreeMap<String, Reading>, key: &str, reading: Option<Reading>) -> bool {
    let Some(mut reading) = reading else {
        return readings.remove(key).is_some();
    };
    if let Some(previous) = readings.get(key) {
        keep_last_good(&mut reading, previous);
        let pools = &previous.snapshot.pools;
        let place = |pool: &Pool| pools.iter().position(|p| p.id == pool.id);
        // Stable, so new accounts stay in their own order at the end.
        (reading.snapshot.pools).sort_by_key(|pool| place(pool).unwrap_or(usize::MAX));
    }
    readings.insert(key.to_string(), reading.clone()) != Some(reading)
}

/// Each failed pool without numbers takes the previous reading's, and
/// when they were read.
fn keep_last_good(reading: &mut Reading, previous: &Reading) {
    for pool in &mut reading.snapshot.pools {
        if pool.failure.is_none() || !pool.windows.is_empty() {
            continue;
        }
        let Some(before) = (previous.snapshot.pools.iter())
            .find(|before| before.id == pool.id && !before.windows.is_empty())
        else {
            continue;
        };
        if let Some(failure) = &mut pool.failure {
            failure.successful_at = (before.failure.as_ref())
                .map_or(previous.snapshot.served_at_unix, |kept| kept.successful_at);
        }
        pool.windows = before.windows.clone();
        pool.account_label = before.account_label.clone();
        pool.plan = before.plan.clone();
    }
}

/// The readings as one snapshot: all pools, each with the time of the
/// freshest source reporting it, and the newest source's time. An account
/// whose read failed with no numbers kept has no pool.
pub fn merge(readings: &BTreeMap<String, Reading>) -> Merged {
    // Sources that name accounts first: theirs win when both report one.
    let (named, rest): (Vec<_>, Vec<_>) =
        (readings.iter()).partition(|(key, _)| key.starts_with(NAMED_PREFIX));
    let readings: Vec<&Reading> = (named.into_iter().chain(rest))
        .map(|(_, reading)| reading)
        .collect();
    let Some(newest) = readings
        .iter()
        .max_by_key(|reading| reading.snapshot.served_at_unix)
    else {
        return Merged::default();
    };
    // Kept numbers are as old as the read that got them.
    let read_at = |pool: &Pool, reading: &Reading| {
        (pool.failure.as_ref())
            .map(|failure| failure.successful_at)
            .filter(|at| *at > 0)
            .unwrap_or(reading.snapshot.served_at_unix)
    };
    let reported: Vec<(&Pool, i64)> = (readings.iter())
        .flat_map(|r| {
            (r.snapshot.pools.iter())
                .filter(|pool| !pool.windows.is_empty() || pool.failure.is_none())
                .map(|pool| (pool, read_at(pool, r)))
        })
        .collect();
    // The same account reported by several sources, or through several
    // hosts, shows once.
    let pools = crate::dedupe_pools(reported.iter().map(|(pool, _)| (*pool).clone()).collect());
    // Its numbers are as fresh as the freshest source reporting it.
    let pool_served = (pools.iter())
        .map(|pool| {
            let served = (reported.iter())
                .filter(|(other, _)| crate::same_account(pool, other))
                .map(|(_, served)| *served)
                .max()
                .unwrap_or(newest.snapshot.served_at_unix);
            (pool.id.clone(), served)
        })
        .collect();
    Merged {
        snapshot: Some(UsageSnapshot {
            revision: readings.iter().map(|r| r.snapshot.revision).sum(),
            served_at: newest.snapshot.served_at.clone(),
            served_at_unix: newest.snapshot.served_at_unix,
            pools,
        }),
        eligibility: readings
            .iter()
            .flat_map(|r| r.eligibility.iter().cloned())
            .collect(),
        pool_served,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Pool, Window};

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
                        ..Default::default()
                    })
                    .collect(),
            },
            eligibility: Vec::new(),
        }
    }

    #[test]
    fn a_stale_source_dims_only_its_own_pools() {
        let mut readings = BTreeMap::new();
        // A daemon that went quiet an hour ago, and a feed read just now.
        readings.insert(
            "paseo:old".into(),
            reading(1_000, &[("paseo:old:claude", "claude", 40., 9e6)]),
        );
        readings.insert(
            "source:0".into(),
            reading(4_600, &[("hub:codex", "codex", 10., 8e6)]),
        );
        let merged = merge(&readings);
        assert_eq!(merged.pool_served["paseo:old:claude"], 1_000);
        assert_eq!(merged.pool_served["hub:codex"], 4_600);
        assert_eq!(merged.snapshot.as_ref().unwrap().served_at_unix, 4_600);
    }

    #[test]
    fn an_account_is_as_fresh_as_its_freshest_report() {
        let mut readings = BTreeMap::new();
        // The feed's reading of an account is old; a daemon reports the same
        // account (same use, same reset) freshly.
        readings.insert(
            "source:0".into(),
            reading(1_000, &[("hub:claude", "claude", 40., 9e6)]),
        );
        readings.insert(
            "paseo:host".into(),
            reading(4_600, &[("paseo:claude:1", "claude", 40., 9e6)]),
        );
        let merged = merge(&readings);
        let pools = &merged.snapshot.as_ref().unwrap().pools;
        assert_eq!(pools.len(), 1, "one account shows once");
        assert_eq!(pools[0].id, "hub:claude", "the feed names it");
        assert_eq!(merged.pool_served["hub:claude"], 4_600);
    }

    #[test]
    fn no_readings_is_nothing() {
        assert_eq!(merge(&BTreeMap::new()), Merged::default());
    }

    /// A daemon's report of one account, as `usage.list_reports.update`
    /// carries it: used percent, or None for a failed read.
    fn update(id: &str, used: Option<f64>, at: &str) -> serde_json::Value {
        let report = match used {
            Some(used) => {
                serde_json::json!({"status": "available", "planLabel": "Max", "windows": [
                    {"id": "seven_day", "label": "Weekly", "usedPct": used, "resetsAt": "2026-10-14T00:00:00Z"}
                ]})
            }
            None => serde_json::json!({"status": "error", "error": "Rate limited"}),
        };
        serde_json::json!({"report": {
            "id": id, "sourceId": "claude", "sourceLabel": "Claude",
            "account": {"label": id}, "fetchedAt": at, "report": report
        }})
    }

    fn read(updates: &[serde_json::Value]) -> Option<Reading> {
        crate::reports::parse_reports("alpha", updates, 0)
    }

    fn pools(readings: &BTreeMap<String, Reading>) -> Vec<Pool> {
        merge(readings)
            .snapshot
            .map(|s| s.pools)
            .unwrap_or_default()
    }

    /// A daemon before 0.12 sends a failed report without the last
    /// successful one: the previous reading's numbers stay, retained.
    #[test]
    fn a_failed_refresh_keeps_the_last_numbers() {
        let mut readings = BTreeMap::new();
        let first = "2026-10-11T08:00:00Z";
        set(
            &mut readings,
            "paseo:alpha",
            read(&[update("work", Some(40.), first)]),
        );
        let changed = set(
            &mut readings,
            "paseo:alpha",
            read(&[update("work", None, "2026-10-11T08:02:00Z")]),
        );
        assert!(changed);
        let merged = merge(&readings);
        let pool = &merged.snapshot.as_ref().unwrap().pools[0];
        assert!(pool.retained());
        assert_eq!(pool.windows[0].used_percent, 40.);
        assert_eq!(pool.plan, "Max");
        let failure = pool.failure.as_ref().unwrap();
        assert_eq!(failure.problem, "Rate limited");
        let read_at = crate::parse_unix(first).unwrap();
        assert_eq!(failure.successful_at, read_at);
        assert_eq!(
            merged.pool_served["paseo:work"], read_at,
            "as old as the good read"
        );

        // Failing again keeps the same numbers and their time.
        set(
            &mut readings,
            "paseo:alpha",
            read(&[update("work", None, "2026-10-11T08:04:00Z")]),
        );
        assert_eq!(
            pools(&readings)[0].failure.as_ref().unwrap().successful_at,
            read_at
        );
        // A good read replaces them.
        set(
            &mut readings,
            "paseo:alpha",
            read(&[update("work", Some(45.), "2026-10-11T08:06:00Z")]),
        );
        let pool = &pools(&readings)[0];
        assert!(!pool.retained());
        assert_eq!(pool.windows[0].used_percent, 45.);
    }

    #[test]
    fn a_failed_first_read_has_no_gauge() {
        let mut readings = BTreeMap::new();
        set(
            &mut readings,
            "paseo:alpha",
            read(&[update("work", None, "2026-10-11T08:00:00Z")]),
        );
        assert!(pools(&readings).is_empty());
    }

    /// Paseo 0.12 keeps the last successful report itself; it wins over
    /// what Reef or crusteo kept.
    #[test]
    fn the_daemons_own_last_successful_report_wins() {
        let mut readings = BTreeMap::new();
        set(
            &mut readings,
            "paseo:alpha",
            read(&[update("work", Some(40.), "2026-10-11T08:00:00Z")]),
        );
        let mut failed = update("work", None, "2026-10-11T08:02:00Z");
        failed["report"]["lastSuccessful"] = serde_json::json!({
            "fetchedAt": "2026-10-11T08:01:00Z",
            "report": update("work", Some(42.), "")["report"]["report"].clone(),
        });
        set(&mut readings, "paseo:alpha", read(&[failed]));
        let pool = &pools(&readings)[0];
        assert_eq!(pool.windows[0].used_percent, 42.);
        assert_eq!(
            pool.failure.as_ref().unwrap().successful_at,
            crate::parse_unix("2026-10-11T08:01:00Z").unwrap()
        );
    }

    /// Paseo #6947: a daemon lists reports as they finish, so the order
    /// changes from read to read; the accounts shown stay in place.
    #[test]
    fn accounts_keep_their_places_across_reads() {
        let at = "2026-10-11T08:00:00Z";
        let mut readings = BTreeMap::new();
        let ids = |readings: &BTreeMap<String, Reading>| -> Vec<String> {
            pools(readings).into_iter().map(|pool| pool.id).collect()
        };
        set(
            &mut readings,
            "paseo:alpha",
            read(&[
                update("a", Some(10.), at),
                update("b", Some(20.), at),
                update("c", Some(30.), at),
            ]),
        );
        set(
            &mut readings,
            "paseo:alpha",
            read(&[
                update("d", Some(5.), at),
                update("c", Some(31.), at),
                update("a", Some(11.), at),
            ]),
        );
        assert_eq!(
            ids(&readings),
            ["paseo:a", "paseo:c", "paseo:d"],
            "the gone one drops, the new one goes last, the rest stay put"
        );
        assert_eq!(pools(&readings)[1].windows[0].used_percent, 31.);
    }

    /// The same account through another host, read fine, shows its fresh
    /// numbers rather than the failed host's kept ones.
    #[test]
    fn a_fresh_read_elsewhere_beats_kept_numbers() {
        let mut readings = BTreeMap::new();
        set(
            &mut readings,
            "paseo:alpha",
            read(&[update("work", Some(40.), "2026-10-11T08:00:00Z")]),
        );
        set(
            &mut readings,
            "paseo:alpha",
            read(&[update("work", None, "2026-10-11T08:02:00Z")]),
        );
        let fresh = crate::reports::parse_reports(
            "beta",
            &[update("work", Some(43.), "2026-10-11T08:02:00Z")],
            0,
        );
        set(&mut readings, "paseo:beta", fresh);
        let merged = merge(&readings);
        let pools = &merged.snapshot.as_ref().unwrap().pools;
        assert_eq!(pools.len(), 1);
        assert!(!pools[0].retained());
        assert_eq!(pools[0].windows[0].used_percent, 43.);
        assert_eq!(
            merged.pool_served["paseo:work"],
            crate::parse_unix("2026-10-11T08:02:00Z").unwrap()
        );
    }

    #[test]
    fn a_feed_pool_that_failed_keeps_its_numbers() {
        let body = |pool: &str| {
            format!(
                r#"{{"revision": 1, "served_at": "2026-10-11T08:00:00Z", "pools": {{"work": {pool}}}}}"#
            )
        };
        let ok = r#"{"provider": "codex", "account_label": "work", "status": "ok", "windows": [
            {"key": "w", "label": "weekly", "used_percent": 20, "remaining_percent": 80,
             "window_seconds": 604800, "resets_at": 1791000000}]}"#;
        let failed =
            r#"{"provider": "codex", "account_label": "work", "status": "error", "windows": []}"#;
        let reading = |pool: &str| Reading {
            snapshot: crate::feed::parse_usage(&body(pool)).unwrap(),
            eligibility: Vec::new(),
        };
        let mut readings = BTreeMap::new();
        set(&mut readings, "source:0", Some(reading(ok)));
        set(&mut readings, "source:0", Some(reading(failed)));
        let pool = &pools(&readings)[0];
        assert!(pool.retained());
        assert_eq!(pool.windows[0].remaining_percent, 80.);
        assert_eq!(crate::view::failure_text(pool).as_deref(), Some("Error"));
    }

    #[test]
    fn an_unchanged_reading_changes_nothing() {
        let mut readings = BTreeMap::new();
        let at = "2026-10-11T08:00:00Z";
        assert!(set(
            &mut readings,
            "paseo:alpha",
            read(&[update("a", Some(1.), at)])
        ));
        assert!(!set(
            &mut readings,
            "paseo:alpha",
            read(&[update("a", Some(1.), at)])
        ));
        assert!(set(&mut readings, "paseo:alpha", None));
        assert!(!set(&mut readings, "paseo:alpha", None));
    }
}
