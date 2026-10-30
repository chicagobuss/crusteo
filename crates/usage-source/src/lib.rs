//! Usage quotas: pools of an account's limits, each a set of windows that
//! reset, and the arithmetic the gauges draw from them (which window binds,
//! pace against the clock, how urgent).
//!
//! It is the one usage model crusteo and `reef` share, so both show
//! the same accounts the same way:
//!
//! - [`reports`] reads the daemons' own usage reports, and [`feed`] a usage
//!   feed's JSON, into [`Reading`]s;
//! - [`merge`] puts every source's reading together, each account once;
//! - [`accounts`] names accounts apart, and [`view`] works out what a
//!   gauge or a phone's row shows for each.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub mod accounts;
pub mod feed;
pub mod merge;
pub mod reports;
pub mod view;

/// A pool is "use it before reset" when this much remains and the reset is near.
pub const USE_BEFORE_REMAINING: f64 = 25.0;
pub const USE_BEFORE_HORIZON: Duration = Duration::from_secs(6 * 60 * 60);
/// Last good snapshot older than this is shown dimmed, not dropped.
pub const STALE_AFTER: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageSnapshot {
    pub revision: u64,
    pub served_at: String,
    pub served_at_unix: i64,
    pub pools: Vec<Pool>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pool {
    pub id: String,
    pub provider: String,
    pub account_label: String,
    /// The account's plan, such as "max" or "team", when the source says.
    pub plan: String,
    pub status: String,
    /// Who the account is, as a usage feed fingerprints it
    /// ([`email_fingerprint`]), when the source can say; empty otherwise.
    /// Two pools with the same one are the same account whatever their
    /// numbers say, such as when one source has read a reset and the other
    /// not yet.
    pub fingerprint: String,
    pub windows: Vec<Window>,
    /// The latest read of this account failed. Its windows, if it has
    /// any, are then the last successful read's, kept on screen as Paseo
    /// keeps them ("retained"); with none it has no gauge.
    pub failure: Option<Failure>,
}

impl Pool {
    /// Its numbers are an earlier read's: the latest one failed.
    pub fn retained(&self) -> bool {
        self.failure.is_some() && !self.windows.is_empty()
    }
}

/// Why an account's latest read failed, and when the numbers kept from
/// before were read.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Failure {
    /// "error" or "unavailable", as Paseo reports say, or a feed's status.
    pub status: String,
    /// What went wrong, as Paseo words it, such as "Rate limited" or
    /// "Login expired 2d ago. Run claude to refresh it."; may be empty.
    pub problem: String,
    /// When the numbers kept were read, in unix seconds; zero with none.
    pub successful_at: i64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Window {
    pub key: String,
    /// The name it is shown by, with its scope when it has one: "weekly
    /// Fable", "Gemini Models (weekly) Gemini Flash, Gemini Pro".
    pub label: String,
    /// What the limit is narrowed to, as a feed names it: one model, or
    /// the models a group covers. Empty when it is not narrowed.
    pub scope: String,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub window_seconds: f64,
    pub resets_at: f64,
}

/// One source's numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub snapshot: UsageSnapshot,
    /// Which pools are worth spending now; empty when the source cannot say.
    pub eligibility: Vec<Eligibility>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Eligibility {
    pub pool: String,
    pub eligible: bool,
    pub reasons: Vec<String>,
    pub reserve_percent: f64,
    pub binding_window: Option<Window>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuelBand {
    Green,
    Amber,
    Red,
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `YYYY-MM-DDTHH:MM:SS` with optional fractional seconds and a zone: `Z`
/// or an offset such as `+00:00`, as Claude's usage API writes its resets.
/// A stamp without a zone is rejected.
pub fn parse_unix(stamp: &str) -> Option<i64> {
    let (date, time) = stamp.trim().split_once('T')?;
    let (time, offset) = match time.strip_suffix(['Z', 'z']) {
        Some(time) => (time, 0),
        None => {
            let at = time.rfind(['+', '-'])?;
            let (time, zone) = time.split_at(at);
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let zone = zone[1..].replace(':', "");
            if zone.len() != 4 || !zone.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let hours: i64 = zone[..2].parse().ok()?;
            let minutes: i64 = zone[2..].parse().ok()?;
            (time, sign * (hours * 3_600 + minutes * 60))
        }
    };
    let mut date = date.split('-');
    let year: i32 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let time = time.split('.').next()?;
    let mut time = time.split(':');
    let hour: u32 = time.next()?.parse().ok()?;
    let minute: u32 = time.next()?.parse().ok()?;
    let second: u32 = time.next()?.parse().ok()?;
    if time.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(
        days_from_civil(year, month, day) * 86_400
            + hour as i64 * 3_600
            + minute as i64 * 60
            + second as i64
            - offset,
    )
}

/// The window that binds the pool: the one with the least remaining.
/// Ties break toward the sooner reset.
pub fn binding_window(pool: &Pool) -> Option<&Window> {
    pool.windows.iter().min_by(|a, b| {
        cmp_f64(a.remaining_percent, b.remaining_percent)
            .then_with(|| cmp_f64(a.resets_at, b.resets_at))
    })
}

/// Soonest reset among windows that still have fuel and close within six hours.
pub fn use_before_reset(pool: &Pool, now_unix: i64) -> Option<&Window> {
    let horizon = USE_BEFORE_HORIZON.as_secs_f64();
    pool.windows
        .iter()
        .filter(|window| window.remaining_percent > USE_BEFORE_REMAINING)
        .filter(|window| {
            let left = window.resets_at - now_unix as f64;
            left > 0. && left <= horizon
        })
        .min_by(|a, b| cmp_f64(a.resets_at, b.resets_at))
}

/// Tightest pool for a provider id, so a tint reflects the worst account.
pub fn worst_pool<'a>(pools: &'a [Pool], provider: &str) -> Option<&'a Pool> {
    pools
        .iter()
        .filter(|pool| pool.provider.eq_ignore_ascii_case(provider))
        .min_by(|a, b| {
            let left = |pool: &Pool| {
                binding_window(pool)
                    .map(|window| window.remaining_percent)
                    .unwrap_or(f64::MAX)
            };
            cmp_f64(left(a), left(b))
        })
}

/// Pace: quota left measured against time left in
/// the same window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pace {
    /// Quota left minus time left, in percentage points. Zero is on pace;
    /// below zero the window runs out before it resets.
    pub slack: f64,
    /// How many times the rate so far could be spent and still land on the
    /// limit at reset. None at either end of the window or before any use.
    pub ratio: Option<f64>,
    pub remaining: f64,
    pub used: f64,
    pub time_left_percent: f64,
}

/// How a window is doing, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Unknown,
    Surplus,
    OnPace,
    Warn,
    Critical,
}

/// A window's pace, when it has a length and a reset to measure against.
pub fn pace(window: &Window, now_unix: i64) -> Option<Pace> {
    if window.window_seconds <= 0. || window.resets_at <= 0. {
        return None;
    }
    let time_left =
        ((window.resets_at - now_unix as f64) / window.window_seconds * 100.).clamp(0., 100.);
    let elapsed = 100. - time_left;
    let remaining = window.remaining_percent.clamp(0., 100.);
    let used = window.used_percent.clamp(0., 100.);
    let ratio = (used > 0. && elapsed > 0. && time_left > 0.)
        .then(|| (remaining / time_left) / (used / elapsed));
    Some(Pace {
        slack: remaining - time_left,
        ratio,
        remaining,
        used,
        time_left_percent: time_left,
    })
}

/// The worse of what pace and absolute headroom say: pace alone reads a
/// window spent with minutes left as on pace.
pub fn severity(window: &Window, now_unix: i64) -> Severity {
    let remaining = window.remaining_percent;
    let by_headroom = if remaining <= 5. {
        Some(Severity::Critical)
    } else if remaining <= 15. {
        Some(Severity::Warn)
    } else {
        None
    };
    let by_pace = match pace(window, now_unix) {
        None => Severity::Unknown,
        Some(pace) if pace.slack <= -25. => Severity::Critical,
        Some(pace) if pace.slack <= -10. => Severity::Warn,
        Some(pace) if pace.slack >= 15. => Severity::Surplus,
        Some(_) => Severity::OnPace,
    };
    by_headroom.map_or(by_pace, |headroom| headroom.max(by_pace))
}

/// The window that constrains a pool most: worst
/// severity, then whichever runs out first at the rate so far. Some
/// providers have a headline window that always wins.
pub fn constraining_window(pool: &Pool, now_unix: i64) -> Option<&Window> {
    let candidates = pool.windows.iter();
    if pool.provider == "cursor"
        && let Some(primary) = pool
            .windows
            .iter()
            .find(|window| window.key == "cursor_models")
    {
        return Some(primary);
    }
    // How many times its rate a window can afford; unmeasurable ones last.
    let rank = |window: &Window| match pace(window, now_unix) {
        None => 2e6 + window.remaining_percent,
        Some(Pace {
            ratio: Some(ratio), ..
        }) if ratio.is_finite() => ratio,
        Some(_) => 1e6 + window.remaining_percent,
    };
    candidates.min_by(|a, b| {
        severity(b, now_unix)
            .cmp(&severity(a, now_unix))
            .then_with(|| cmp_f64(rank(a), rank(b)))
    })
}

/// The pace dial's needle, in degrees from straight up, on a log scale:
/// on pace is vertical, 99 times affordable or better is -90 (as far as
/// pace is shown, so 10× and 30× room read apart), ten times too fast or
/// worse is +90.
pub fn needle_degrees(pace: &Pace) -> Option<f64> {
    if pace.used == 0. {
        return Some(-90.);
    }
    let ratio = pace
        .ratio
        .filter(|ratio| ratio.is_finite() && *ratio > 0.)?;
    Some(if ratio >= 1. {
        -(ratio.ln() / 99f64.ln()).min(1.) * 90.
    } else {
        -ratio.log10().max(-1.) * 90.
    })
}

/// How hard it can be pushed: "1.5x your pace available", "5.5x too fast".
pub fn ratio_text(pace: &Pace) -> Option<String> {
    if pace.used == 0. {
        return Some("unused so far".into());
    }
    let ratio = pace.ratio.filter(|ratio| ratio.is_finite())?;
    Some(if ratio >= 1. {
        format!("{}x your pace available", times(ratio))
    } else {
        format!("{}x too fast", times(1. / ratio))
    })
}

/// [`ratio_text`] for a table cell: "1.8× room", "1.6× too fast", "on
/// pace" within a twentieth either way, "unused".
pub fn pace_brief(pace: &Pace) -> Option<String> {
    if pace.used == 0. {
        return Some("unused".into());
    }
    let ratio = pace.ratio.filter(|ratio| ratio.is_finite())?;
    Some(if (0.95..1.05).contains(&ratio) {
        "on pace".into()
    } else if ratio >= 1. {
        format!("{}× room", times(ratio))
    } else {
        format!("{}× too fast", times(1. / ratio))
    })
}

/// A pace multiple: a tenth below ten, whole above, at most 99.
fn times(value: f64) -> String {
    let value = value.min(99.);
    if value >= 10. {
        format!("{}", value.round())
    } else {
        format!("{value:.1}")
    }
}

pub fn fuel_band(remaining_percent: f64) -> FuelBand {
    if remaining_percent > 40. {
        FuelBand::Green
    } else if remaining_percent >= 15. {
        FuelBand::Amber
    } else {
        FuelBand::Red
    }
}

pub fn is_stale(served_at_unix: i64, now_unix: i64) -> bool {
    now_unix.saturating_sub(served_at_unix) > STALE_AFTER.as_secs() as i64
}

/// How long ago, as Paseo says it in a sentence: "just now", "5m ago",
/// "3h ago", "2d ago".
pub fn ago(secs: i64) -> String {
    match secs {
        s if s < 45 => "just now".into(),
        s if s < 3_600 => format!("{}m ago", (s / 60).max(1)),
        s if s < 86_400 => format!("{}h ago", s / 3_600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// Compact time-to-reset: `26m`, `3.3h`, `2.1d`.
pub fn format_reset(resets_at: f64, now_unix: i64) -> String {
    let secs = resets_at - now_unix as f64;
    if secs <= 0. {
        return "now".into();
    }
    if secs < 90. * 60. {
        let mins = (secs / 60.).round().max(1.) as i64;
        return format!("{mins}m");
    }
    if secs < 48. * 3600. {
        return format!("{:.1}h", secs / 3600.);
    }
    format!("{:.1}d", secs / 86_400.)
}

fn cmp_f64(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

/// Howard Hinnant's civil-from-days, inverted. Days since 1970-01-01.
fn days_from_civil(mut year: i32, month: u32, day: u32) -> i64 {
    year -= i32::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = (year - era * 400) as u64;
    let doy = (153 * (month as i64 + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as u64;
    era as i64 * 146_097 + doe as i64 - 719_468
}

/// The fingerprint a usage feed gives an account its provider knows by
/// email: a namespace, `:`, and the first 24 hex digits of the SHA-256 of
/// the namespace, `:` and the email. None for providers whose accounts are
/// not one per email (a Claude login can be a person's and a team's) and
/// for labels that are not emails.
pub fn email_fingerprint(provider: &str, label: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let namespace = match provider {
        "codex" => "openai",
        "gemini" => "gemini",
        _ => return None,
    };
    let email = label.trim();
    if !email.contains('@') || email.contains(char::is_whitespace) {
        return None;
    }
    let digest = Sha256::digest(format!("{namespace}:{email}").as_bytes());
    let hex: String = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some(format!("{namespace}:{hex}"))
}

/// Whether two pools are the same account seen twice, such as by two
/// sources or through two hosts: the same id or fingerprint, or the same
/// provider and a window that resets at the same moment with the same use.
/// Windows without a reset time match only on the same key and use.
pub fn same_account(a: &Pool, b: &Pool) -> bool {
    if a.id == b.id || (!a.fingerprint.is_empty() && a.fingerprint == b.fingerprint) {
        return true;
    }
    a.provider == b.provider
        && a.windows.iter().any(|x| {
            b.windows.iter().any(|y| {
                let same_use = (x.used_percent - y.used_percent).abs() <= 2.;
                if x.resets_at > 0. && y.resets_at > 0. {
                    same_use && (x.resets_at - y.resets_at).abs() <= 300.
                } else {
                    x.key == y.key && (x.used_percent - y.used_percent).abs() < 0.5
                }
            })
        })
}

/// `pools` with each account once: of pools that are [`same_account`],
/// the first is kept, so put the source that names accounts best first.
/// What the others know about its windows' lengths it keeps too, and when
/// its own latest read failed, another's successful numbers.
pub fn dedupe_pools(pools: Vec<Pool>) -> Vec<Pool> {
    let mut kept: Vec<Pool> = Vec::with_capacity(pools.len());
    for pool in pools {
        match kept.iter_mut().find(|seen| same_account(seen, &pool)) {
            Some(seen) if seen.failure.is_some() && pool.failure.is_none() => {
                seen.windows = pool.windows;
                seen.failure = None;
                if seen.plan.is_empty() {
                    seen.plan = pool.plan;
                }
            }
            Some(seen) => learn_lengths(seen, &pool),
            None => kept.push(pool),
        }
    }
    kept
}

/// Each of `pool`'s windows whose length is unknown takes the length and
/// name of the same window in `other`: one resetting at the same moment
/// with the same use. Paseo 0.10 calls Codex's weekly window "session",
/// for one, which a newer daemon on the same account names properly.
fn learn_lengths(pool: &mut Pool, other: &Pool) {
    for window in (pool.windows.iter_mut()).filter(|w| w.window_seconds <= 0. && w.resets_at > 0.) {
        if let Some(known) = other.windows.iter().find(|o| {
            o.window_seconds > 0.
                && (o.resets_at - window.resets_at).abs() <= 300.
                && (o.used_percent - window.used_percent).abs() <= 2.
        }) {
            window.window_seconds = known.window_seconds;
            window.label = known.label.clone();
        }
    }
}
