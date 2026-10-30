//! A usage feed: a service that keeps every account's limits and serves
//! them as JSON at `/v1/usage`, with `/v1/eligibility?pool=<id>` saying
//! whether one is worth spending now. Parsing is here; fetching is up to
//! the caller.
//!
//! ```json
//! {"revision": 7, "served_at": "2026-10-02T18:04:48Z",
//!  "pools": {"<pool id>": {"provider": "claude", "account_label": "work",
//!    "account_fingerprint": "anthropic:…",
//!    "plan": "max", "status": "ok", "windows": [{"key": "weekly_all",
//!    "label": "weekly", "scope": null, "used_percent": 39, "remaining_percent": 61,
//!    "window_seconds": 604800, "resets_at": 1791223199}]}}}
//! ```
//!
//! What a feed lists is not all worth a gauge; [`shown_windows`] keeps what
//! is.

use serde::Deserialize;

use crate::{Eligibility, Failure, Pool, UsageSnapshot, Window, parse_unix};

/// Where a feed serves its usage, below its base URL.
pub const USAGE_PATH: &str = "/v1/usage";
/// Where it serves one pool's eligibility; the pool id follows.
pub const ELIGIBILITY_PATH: &str = "/v1/eligibility?pool=";

pub fn parse_usage(body: &str) -> Result<UsageSnapshot, String> {
    let raw: RawUsage = serde_json::from_str(body).map_err(|err| err.to_string())?;
    let served_at_unix = parse_unix(&raw.served_at)
        .ok_or_else(|| format!("unreadable served_at {}", raw.served_at))?;
    let mut pools = Vec::with_capacity(raw.pools.len());
    for (id, value) in raw.pools {
        let pool: RawPool = serde_json::from_value(value).map_err(|err| err.to_string())?;
        let windows = shown_windows(pool.windows.into_iter().map(window).collect());
        // A pool that is not "ok" and has no numbers failed its latest
        // read; the feed's previous reading has its last good ones.
        let failed = windows.is_empty() && !matches!(pool.status.as_str(), "" | "ok");
        pools.push(Pool {
            id,
            provider: pool.provider,
            account_label: pool.account_label,
            fingerprint: pool.account_fingerprint.unwrap_or_default(),
            plan: pool.plan.unwrap_or_default(),
            failure: failed.then(|| Failure {
                status: pool.status.clone(),
                ..Default::default()
            }),
            status: pool.status,
            windows,
        });
    }
    Ok(UsageSnapshot {
        revision: raw.revision,
        served_at: raw.served_at,
        served_at_unix,
        pools,
    })
}

pub fn parse_eligibility(body: &str) -> Result<Eligibility, String> {
    let raw: RawEligibility = serde_json::from_str(body).map_err(|err| err.to_string())?;
    Ok(Eligibility {
        pool: raw.pool,
        eligible: raw.eligible,
        reasons: raw.reasons,
        reserve_percent: raw.reserve_percent,
        binding_window: raw.binding_window.map(window),
    })
}

/// The windows worth a gauge, named as a person reads them:
///
/// - A window that is another seen again, its key that one's key with a
///   prefix (`all:codex:primary` beside `codex:primary`) and the same
///   numbers, shows once.
/// - A window with no use, no reset and no length says nothing, and is
///   left out.
/// - A window whose label a scoped sibling extends, such as "weekly"
///   beside "weekly Fable", is the account-wide one: "weekly Global".
pub fn shown_windows(windows: Vec<Window>) -> Vec<Window> {
    let mirrors: Vec<bool> = (windows.iter())
        .map(|window| windows.iter().any(|other| mirrors(window, other)))
        .collect();
    let mut windows: Vec<Window> = (windows.into_iter().zip(mirrors))
        .filter(|(window, mirror)| {
            !mirror
                && (window.resets_at > 0. || window.window_seconds > 0. || window.used_percent > 0.)
        })
        .map(|(window, _)| window)
        .collect();
    let labels: Vec<String> = windows.iter().map(|window| window.label.clone()).collect();
    for window in &mut windows {
        let prefix = format!("{} ", window.label);
        if labels.iter().any(|other| other.starts_with(&prefix)) {
            window.label.push_str(" Global");
        }
    }
    windows
}

/// Whether `window` is `other` seen again under a prefixed key.
fn mirrors(window: &Window, other: &Window) -> bool {
    window.key.len() > other.key.len()
        && window.key.ends_with(&format!(":{}", other.key))
        && (window.used_percent - other.used_percent).abs() < 0.5
        && (window.resets_at - other.resets_at).abs() < 60.
        && window.window_seconds == other.window_seconds
}

#[derive(Deserialize)]
struct RawUsage {
    revision: u64,
    served_at: String,
    pools: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct RawPool {
    provider: String,
    #[serde(default)]
    account_label: String,
    /// Who the account is ([`crate::email_fingerprint`] for one known by
    /// email), so a daemon's report of it, named by email, is known as it.
    #[serde(default)]
    account_fingerprint: Option<String>,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    windows: Vec<RawWindow>,
}

#[derive(Deserialize)]
struct RawWindow {
    #[serde(default)]
    key: String,
    #[serde(default)]
    label: String,
    /// What a limit is narrowed to, such as one model's weekly limit.
    #[serde(default)]
    scope: Option<String>,
    #[serde(default, deserialize_with = "null_as_zero")]
    used_percent: f64,
    #[serde(default, deserialize_with = "null_as_zero")]
    remaining_percent: f64,
    #[serde(default, deserialize_with = "null_as_zero")]
    window_seconds: f64,
    #[serde(default, deserialize_with = "null_as_zero")]
    resets_at: f64,
}

fn window(raw: RawWindow) -> Window {
    let scope = raw.scope.unwrap_or_default();
    Window {
        label: match scope.as_str() {
            "" => raw.label,
            scope => format!("{} {scope}", raw.label),
        },
        scope,
        key: raw.key,
        used_percent: raw.used_percent,
        remaining_percent: raw.remaining_percent,
        window_seconds: raw.window_seconds,
        resets_at: raw.resets_at,
    }
}

#[derive(Deserialize)]
struct RawEligibility {
    pool: String,
    #[serde(default)]
    eligible: bool,
    #[serde(default)]
    reasons: Vec<String>,
    #[serde(default, deserialize_with = "null_as_zero")]
    reserve_percent: f64,
    #[serde(default)]
    binding_window: Option<RawWindow>,
}

/// A number a feed may send as `null`, such as a window with no reset yet.
fn null_as_zero<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    Ok(Option::<f64>::deserialize(deserializer)?.unwrap_or(0.))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(key: &str, label: &str, used: f64, seconds: f64, resets_at: f64) -> Window {
        Window {
            key: key.into(),
            label: label.into(),
            used_percent: used,
            remaining_percent: 100. - used,
            window_seconds: seconds,
            resets_at,
            ..Default::default()
        }
    }

    #[test]
    fn a_window_seen_twice_shows_once() {
        let shown = shown_windows(vec![
            window("codex:primary", "codex", 71., 604_800., 9e8),
            window("all:codex:primary", "all:codex", 71., 604_800., 9e8),
        ]);
        let keys: Vec<&str> = shown.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["codex:primary"]);
    }

    #[test]
    fn windows_alike_by_chance_both_show() {
        // Two model groups, both unused, resetting together: not mirrors.
        let shown = shown_windows(vec![
            window("models-5h", "Models (5h)", 0., 18_000., 9e8),
            window("other-5h", "Other models (5h)", 0., 18_000., 9e8),
        ]);
        assert_eq!(shown.len(), 2);
    }

    #[test]
    fn a_window_that_says_nothing_is_left_out() {
        let shown = shown_windows(vec![
            window("weekly", "weekly", 10., 604_800., 9e8),
            window("preview", "preview", 0., 0., 0.),
            window("credits", "credits", 20., 0., 0.),
        ]);
        let keys: Vec<&str> = shown.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["weekly", "credits"]);
    }

    #[test]
    fn the_account_wide_window_beside_scoped_ones_is_global() {
        let shown = shown_windows(vec![
            window("session", "session", 10., 18_000., 9e8),
            window("weekly_all", "weekly", 30., 604_800., 9e8),
            window("weekly_scoped", "weekly Fable", 0., 604_800., 9e8),
        ]);
        let labels: Vec<&str> = shown.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["session", "weekly Global", "weekly Fable"]);
    }

    #[test]
    fn a_feed_reads_into_pools_with_plans() {
        let body = r#"{"revision": 3, "served_at": "2026-10-10T08:00:00Z", "pools": {
            "codex:work": {"provider": "codex", "account_label": "work", "plan": "pro",
                "account_fingerprint": "openai:b7f5e7db797892b3d29cdbfa", "status": "ok", "windows": [
                {"key": "w", "label": "weekly", "used_percent": 20, "remaining_percent": 80,
                 "window_seconds": 604800, "resets_at": 1791000000, "binding": true}]}}}"#;
        let snapshot = parse_usage(body).unwrap();
        assert_eq!(snapshot.revision, 3);
        let pool = &snapshot.pools[0];
        assert_eq!(
            (pool.id.as_str(), pool.plan.as_str()),
            ("codex:work", "pro")
        );
        assert_eq!(
            Some(pool.fingerprint.clone()),
            crate::email_fingerprint("codex", "work@example.com")
        );
        assert_eq!(pool.windows[0].remaining_percent, 80.);
        assert!(parse_usage("{}").is_err());
    }
}
