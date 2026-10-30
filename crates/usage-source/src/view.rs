//! What each account's gauge shows, worked out once for every screen: the
//! desktop's title-bar meters and Reef's `usage` view are both made
//! from these rows, so they always agree.

use serde_json::{Value, json};

use crate::merge::Merged;
use crate::{Pace, Pool, Severity, Window};

/// One account's gauge.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub pool: Pool,
    /// What tells the account apart, when its provider has several.
    pub account: Option<String>,
    /// When its numbers were read, in unix seconds.
    pub served_at_unix: i64,
    /// Its numbers are over ten minutes old: shown dimmed.
    pub stale: bool,
    /// Its latest read failed and its numbers are the last successful
    /// read's ([`Pool::failure`] says why).
    pub retained: bool,
    /// The window that constrains it most.
    pub binding: Option<Window>,
    pub pace: Option<Pace>,
    pub severity: Severity,
    /// Quota left minus time left in that window: "+4", "-16", or "—".
    pub slack: String,
    /// The pace dial's needle, in degrees from straight up.
    pub needle: Option<f64>,
    /// A window to use before it resets, unless the numbers are stale.
    pub use_before: Option<Window>,
    /// Which account, its binding window with its pace, and staleness.
    pub tip: String,
    /// Its windows as the usage table lists them.
    pub groups: Vec<Group>,
}

/// A gauge per pool with windows, in the snapshot's order.
pub fn rows(merged: &Merged, now: i64) -> Vec<Row> {
    let Some(snapshot) = &merged.snapshot else {
        return Vec::new();
    };
    let pools = &snapshot.pools;
    (pools.iter())
        .filter(|pool| !pool.windows.is_empty())
        .map(|pool| {
            let served =
                (merged.pool_served.get(&pool.id).copied()).unwrap_or(snapshot.served_at_unix);
            row(pool, pools, served, now)
        })
        .collect()
}

fn row(pool: &Pool, pools: &[Pool], served_at_unix: i64, now: i64) -> Row {
    // Each gauge dims on its own numbers going stale.
    let stale = crate::is_stale(served_at_unix, now);
    let retained = pool.retained();
    let binding = crate::constraining_window(pool, now);
    let pace = binding.and_then(|window| crate::pace(window, now));
    let severity = binding
        .map(|window| crate::severity(window, now))
        .unwrap_or(Severity::Unknown);
    let slack = pace
        .map(|pace| {
            let rounded = pace.slack.round() as i64;
            if rounded > 0 {
                format!("+{rounded}")
            } else {
                rounded.to_string()
            }
        })
        .unwrap_or_else(|| "—".into());
    Row {
        account: crate::accounts::account_short(pool, pools),
        served_at_unix,
        stale,
        retained,
        tip: window_tip(pool, binding, served_at_unix, now, stale),
        groups: groups(pool, now),
        needle: pace.as_ref().and_then(crate::needle_degrees),
        // Stale numbers, or ones a failed read left, must not invite a
        // launch.
        use_before: if stale || retained {
            None
        } else {
            crate::use_before_reset(pool, now).cloned()
        },
        binding: binding.cloned(),
        pace,
        severity,
        slack,
        pool: pool.clone(),
    }
}

/// Which subscription, and its binding window with its pace. When its
/// latest read failed, why, and when its numbers were read, as Paseo's
/// usage card says.
pub fn window_tip(
    pool: &Pool,
    binding: Option<&Window>,
    served_at_unix: i64,
    now: i64,
    stale: bool,
) -> String {
    let mut lines = vec![format!("{} {}", pool.provider, pool.account_label)];
    let retained = pool.retained();
    if let Some(binding) = binding {
        lines.push(describe_window(binding, now, retained));
    }
    if retained {
        lines.extend(failure_text(pool));
        lines.push(last_updated(served_at_unix, now));
    } else if stale {
        lines.push(STALE.into());
    }
    lines.join("\n")
}

/// What a stale gauge says.
pub const STALE: &str = "Last update is over 10 minutes old";

/// Why a pool's latest read failed, as Paseo's card says it: its status,
/// "Error" or "Unavailable", and what went wrong.
pub fn failure_text(pool: &Pool) -> Option<String> {
    let failure = pool.failure.as_ref()?;
    let status = if failure.status == "error" {
        "Error"
    } else {
        "Unavailable"
    };
    Some(if failure.problem.is_empty() {
        status.to_string()
    } else {
        format!("{status}: {}", failure.problem)
    })
}

/// When retained numbers were read: "Last updated 5m ago", as Paseo says
/// it once a refresh has failed.
pub fn last_updated(read_at: i64, now: i64) -> String {
    format!("Last updated {}", crate::ago(now - read_at))
}

/// "weekly 52% left, resets in 4.7d, 1.9x too fast". A retained window
/// whose reset has gone by says so rather than that it resets now, as
/// Paseo's does: its numbers are from before.
pub fn describe_window(window: &Window, now: i64, retained: bool) -> String {
    let pace = crate::pace(window, now)
        .and_then(|pace| crate::ratio_text(&pace))
        .map(|text| format!(", {text}"))
        .unwrap_or_default();
    let reset = if retained && window.resets_at > 0. && window.resets_at <= now as f64 {
        "reset time passed".to_string()
    } else {
        format!("resets in {}", crate::format_reset(window.resets_at, now))
    };
    format!(
        "{} {:.0}% left, {reset}{pace}",
        short_name(window),
        window.remaining_percent,
    )
}

/// What the use-before-reset hint explains: "62% of weekly left, resets in
/// 3.3h."
pub fn use_before_text(window: &Window, now: i64) -> String {
    format!(
        "{:.0}% of {} left, resets in {}.",
        window.remaining_percent,
        short_name(window),
        crate::format_reset(window.resets_at, now)
    )
}

/// A window named in a sentence, without the models it covers: "weekly",
/// "Gemini Models (5h)", "Fable (weekly)".
pub fn short_name(window: &Window) -> String {
    match name_and_period(window) {
        (name, period) if name.is_empty() => period,
        (name, period) if period.is_empty() => name,
        (name, period) => format!("{name} ({period})"),
    }
}

/// A pool's windows as the usage table lists them: under a heading, a line
/// per period.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The heading: "Gemini Models", "Fable", "All models". Empty when it
    /// would only repeat the provider, for a pool that is all one group.
    pub name: String,
    /// The models its limits cover, when the source lists them and the
    /// heading does not say: hover text, for whoever wants it.
    pub models: String,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// "weekly", "5h", "session"; empty when nothing says.
    pub period: String,
    /// How hard it can be pushed: "1.8× room", "1.6× too fast", "unused".
    pub pace: Option<String>,
    /// How it is doing, which colours its bar.
    pub severity: Severity,
    pub window: Window,
}

/// "Gemini Models (weekly)" is the weekly line under "Gemini Models";
/// Claude's weekly limit scoped to Fable is the weekly line under "Fable";
/// an account-wide "session" or "weekly Global" is under no heading of its
/// own, "All models" beside others. A name that says no period takes one
/// from the window's length.
pub fn groups(pool: &Pool, now: i64) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for window in &pool.windows {
        let (name, period) = name_and_period(window);
        let models = if window.scope == name {
            String::new()
        } else {
            window.scope.clone()
        };
        let line = Line {
            period,
            pace: crate::pace(window, now).and_then(|pace| crate::pace_brief(&pace)),
            severity: crate::severity(window, now),
            window: window.clone(),
        };
        match groups.iter_mut().find(|group| group.name == name) {
            Some(group) => {
                if group.models.is_empty() {
                    group.models = models;
                }
                group.lines.push(line);
            }
            None => groups.push(Group {
                name,
                models,
                lines: vec![line],
            }),
        }
    }
    if let [only] = groups.as_mut_slice() {
        if only.name.eq_ignore_ascii_case(&pool.provider) {
            only.name.clear();
        }
    } else {
        for group in groups.iter_mut().filter(|group| group.name.is_empty()) {
            group.name = "All models".into();
        }
    }
    groups
}

fn name_and_period(window: &Window) -> (String, String) {
    let label = match window.scope.as_str() {
        "" => window.label.as_str(),
        scope => window.label.strip_suffix(scope).unwrap_or(&window.label),
    }
    .trim();
    let label = label.strip_suffix(" Global").unwrap_or(label);
    if let Some((name, inner)) = label
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once(" ("))
    {
        if is_period(inner) {
            return (name.trim().to_string(), inner.to_lowercase());
        }
        if is_period(name) {
            return (inner.trim().to_string(), name.to_lowercase());
        }
    }
    if is_period(label) {
        return (window.scope.clone(), label.to_lowercase());
    }
    (label.to_string(), period_of(window.window_seconds))
}

/// "weekly", "Session", "5h", "7d".
fn is_period(text: &str) -> bool {
    let text = text.trim().to_ascii_lowercase();
    const WORDS: [&str; 8] = [
        "session", "hourly", "daily", "weekly", "monthly", "day", "week", "month",
    ];
    WORDS.contains(&text.as_str())
        || (text.len() > 1
            && text.ends_with(['h', 'd', 'w', 'm'])
            && text[..text.len() - 1].bytes().all(|b| b.is_ascii_digit()))
}

/// A window's length as a period: "5h", "daily", "weekly", "monthly".
fn period_of(seconds: f64) -> String {
    const HOUR: f64 = 3_600.;
    const DAY: f64 = 24. * HOUR;
    if seconds <= 0. {
        String::new()
    } else if (seconds - DAY).abs() < 1. {
        "daily".into()
    } else if (seconds - 7. * DAY).abs() < 1. {
        "weekly".into()
    } else if (28. * DAY..=31. * DAY).contains(&seconds) {
        "monthly".into()
    } else if seconds < 2. * DAY {
        format!("{}h", (seconds / HOUR).round())
    } else {
        format!("{}d", (seconds / DAY).round())
    }
}

pub fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Critical => "critical",
        Severity::Warn => "warn",
        Severity::OnPace => "on_pace",
        Severity::Surplus => "surplus",
        Severity::Unknown => "unknown",
    }
}

/// The row that most needs watching: fresh numbers before stale or
/// retained ones, the worst severity, then whichever window runs out first
/// at its pace, then the least left.
pub fn tightest(rows: &[Row]) -> Option<&Row> {
    let left = |row: &Row| {
        (row.pace.and_then(|pace| pace.ratio))
            .filter(|ratio| ratio.is_finite())
            .unwrap_or(f64::MAX)
    };
    let remaining = |row: &Row| {
        row.binding
            .as_ref()
            .map_or(f64::MAX, |w| w.remaining_percent)
    };
    (rows.iter())
        .filter(|row| row.binding.is_some())
        .min_by(|a, b| {
            ((a.stale || a.retained).cmp(&(b.stale || b.retained)))
                .then_with(|| b.severity.cmp(&a.severity))
                .then_with(|| cmp(left(a), left(b)))
                .then_with(|| cmp(remaining(a), remaining(b)))
        })
}

fn cmp(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

/// Rounded to a tenth, so a phone's row changes only when what it shows
/// does.
fn tenth(value: f64) -> f64 {
    (value * 10.).round() / 10.
}

impl Row {
    /// The row as a phone gets it. Times are unix seconds, for the phone to
    /// count down itself; nothing in it changes with the clock alone except
    /// pace, severity and staleness. `served_at` is when its numbers were
    /// read, the last successful read's when `retained`. `groups` lists
    /// the windows' keys as the usage table shows them.
    pub fn to_json(&self) -> Value {
        let binding = self.binding.as_ref().map(|w| w.key.as_str());
        let lines = || self.groups.iter().flat_map(|group| &group.lines);
        let windows: Vec<Value> = (self.pool.windows.iter())
            .map(|window| {
                let line = lines().find(|line| line.window.key == window.key);
                json!({
                    "key": window.key,
                    "name": window.label,
                    "short": short_name(window),
                    "period": line.map(|line| line.period.as_str()),
                    "pace": line.and_then(|line| line.pace.as_deref()),
                    "severity": line.map(|line| severity_name(line.severity)),
                    "used": tenth(window.used_percent.clamp(0., 100.)),
                    "remaining": tenth(window.remaining_percent.clamp(0., 100.)),
                    "resets_at": window.resets_at.round() as i64,
                    "length": window.window_seconds.round() as i64,
                    "binding": Some(window.key.as_str()) == binding,
                })
            })
            .collect();
        let groups: Vec<Value> = (self.groups.iter())
            .map(|group| {
                json!({
                    "name": group.name,
                    "models": group.models,
                    "keys": group.lines.iter().map(|line| &line.window.key).collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut out = json!({
            "key": self.pool.id,
            "provider": self.pool.provider,
            "account": self.account,
            "label": self.pool.account_label,
            "plan": self.pool.plan,
            "status": self.pool.status,
            "served_at": self.served_at_unix,
            "stale": self.stale,
            "retained": self.retained,
            "failure": self.pool.failure.as_ref().map(|failure| json!({
                "status": failure.status,
                "problem": failure.problem,
            })),
            "severity": severity_name(self.severity),
            "slack": self.slack,
            "needle": self.needle.map(|n| n.round() as i64),
            "binding": binding,
            "windows": windows,
            "groups": groups,
            "use_before": null,
        });
        if let Some(window) = &self.use_before {
            out["use_before"] = json!({
                "window": short_name(window),
                "remaining": tenth(window.remaining_percent),
                "resets_at": window.resets_at.round() as i64,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Reading, UsageSnapshot};
    use std::collections::BTreeMap;

    const NOW: i64 = 1_800_000_000;

    fn window(key: &str, remaining: f64, seconds: f64, resets_in: f64) -> Window {
        Window {
            key: key.into(),
            label: key.into(),
            used_percent: 100. - remaining,
            remaining_percent: remaining,
            window_seconds: seconds,
            resets_at: NOW as f64 + resets_in,
            ..Default::default()
        }
    }

    fn merged(served: i64, pools: Vec<Pool>) -> Merged {
        let mut readings = BTreeMap::new();
        readings.insert(
            "source:0".to_string(),
            Reading {
                snapshot: UsageSnapshot {
                    served_at_unix: served,
                    pools,
                    ..Default::default()
                },
                eligibility: Vec::new(),
            },
        );
        crate::merge::merge(&readings)
    }

    fn pool(id: &str, provider: &str, label: &str, windows: Vec<Window>) -> Pool {
        Pool {
            id: id.into(),
            provider: provider.into(),
            account_label: label.into(),
            status: "ok".into(),
            windows,
            ..Default::default()
        }
    }

    #[test]
    fn a_row_says_what_the_gauge_shows() {
        let pools = vec![
            pool(
                "claude:work",
                "claude",
                "work@example.com",
                // Half the week gone, a fifth left: too fast.
                vec![window("weekly", 20., 604_800., 302_400.)],
            ),
            pool(
                "claude:home",
                "claude",
                "home@example.com",
                vec![window("session", 80., 18_000., 3_600.)],
            ),
            pool("codex:empty", "codex", "x", Vec::new()),
        ];
        let rows = rows(&merged(NOW, pools), NOW);
        assert_eq!(rows.len(), 2, "a pool without windows has no gauge");
        let work = &rows[0];
        assert_eq!(work.account.as_deref(), Some("work"));
        assert_eq!(work.slack, "-30");
        assert_eq!(work.severity, Severity::Critical);
        assert!(!work.stale);
        assert!(work.use_before.is_none());
        let home = &rows[1];
        assert_eq!(home.use_before.as_ref().unwrap().key, "session");
        assert_eq!(tightest(&rows).unwrap().pool.id, "claude:work");
        let json = work.to_json();
        assert_eq!(json["severity"], "critical");
        assert_eq!(json["binding"], "weekly");
        assert_eq!(json["windows"][0]["binding"], true);
        assert_eq!(json["windows"][0]["remaining"], 20.);
    }

    #[test]
    fn old_numbers_are_stale_and_invite_nothing() {
        let pools = vec![pool(
            "claude:home",
            "claude",
            "home",
            vec![window("session", 80., 18_000., 3_600.)],
        )];
        let rows = rows(&merged(NOW - 3_600, pools), NOW);
        assert!(rows[0].stale);
        assert!(rows[0].use_before.is_none());
        assert!(rows[0].tip.ends_with(STALE));
    }

    fn scoped(key: &str, label: &str, scope: &str, seconds: f64) -> Window {
        Window {
            label: match scope {
                "" => label.into(),
                scope => format!("{label} {scope}"),
            },
            scope: scope.into(),
            ..window(key, 50., seconds, 3_600.)
        }
    }

    fn listed(pool: &Pool) -> Vec<(String, Vec<String>, String)> {
        (groups(pool, NOW).into_iter())
            .map(|group| {
                let periods = group.lines.iter().map(|l| l.period.clone()).collect();
                (group.name, periods, group.models)
            })
            .collect()
    }

    fn named(name: &str, periods: &[&str], models: &str) -> (String, Vec<String>, String) {
        let periods = periods.iter().map(|p| p.to_string()).collect();
        (name.into(), periods, models.into())
    }

    /// The table names each group once and each window by its period,
    /// with the models a group covers kept for hover.
    #[test]
    fn windows_are_listed_by_group_and_period() {
        const WEEK: f64 = 604_800.;
        const FIVE_HOURS: f64 = 18_000.;
        let models = "Gemini Flash, Gemini Pro";
        let others = "Claude Opus, Claude Sonnet, GPT-OSS";
        let gemini = pool(
            "gemini:personal",
            "gemini",
            "personal",
            vec![
                scoped("gemini-weekly", "Gemini Models (weekly)", models, WEEK),
                scoped("gemini-5h", "Gemini Models (5h)", models, FIVE_HOURS),
                scoped("3p-weekly", "Claude and GPT models (weekly)", others, WEEK),
                scoped("3p-5h", "Claude and GPT models (5h)", others, FIVE_HOURS),
                scoped("argon", "Gemini 4 Argon (weekly)", "", WEEK),
            ],
        );
        assert_eq!(
            listed(&gemini),
            [
                named("Gemini Models", &["weekly", "5h"], models),
                named("Claude and GPT models", &["weekly", "5h"], others),
                named("Gemini 4 Argon", &["weekly"], ""),
            ]
        );
        let mut claude = pool(
            "claude:work",
            "claude",
            "work",
            vec![
                scoped("session", "session", "", FIVE_HOURS),
                scoped("weekly_all", "weekly", "", WEEK),
                scoped("weekly_scoped:Fable", "weekly", "Fable", WEEK),
            ],
        );
        claude.windows = crate::feed::shown_windows(claude.windows);
        assert_eq!(
            listed(&claude),
            [
                named("All models", &["session", "weekly"], ""),
                named("Fable", &["weekly"], ""),
            ]
        );
        let cursor = pool(
            "cursor:personal",
            "cursor",
            "personal",
            vec![
                scoped("cursor_models", "Cursor Models", "", 2_678_400.),
                scoped("on_demand", "On-demand spend", "", 2_678_400.),
            ],
        );
        assert_eq!(
            listed(&cursor),
            [
                named("Cursor Models", &["monthly"], ""),
                named("On-demand spend", &["monthly"], ""),
            ]
        );
        // One group named for its provider needs no heading.
        let codex = pool(
            "codex:personal",
            "codex",
            "personal",
            vec![scoped("codex:primary", "codex", "", WEEK)],
        );
        assert_eq!(listed(&codex), [named("", &["weekly"], "")]);
        let row = &rows(&merged(NOW, vec![gemini]), NOW)[0];
        let json = row.to_json();
        assert_eq!(
            json["groups"][0]["keys"],
            json!(["gemini-weekly", "gemini-5h"])
        );
        assert_eq!(json["windows"][1]["period"], "5h");
        assert!(json["windows"][0]["pace"].as_str().unwrap().contains('×'));
    }

    /// As Paseo's card shows retained usage: the numbers, the failure, and
    /// "Last updated" rather than "Updated".
    #[test]
    fn a_retained_row_says_its_read_failed_and_when_it_last_worked() {
        let mut pool = pool(
            "claude:home",
            "claude",
            "home",
            vec![
                window("session", 80., 18_000., 3_600.),
                window("weekly", 60., 604_800., -60.),
            ],
        );
        pool.failure = Some(crate::Failure {
            status: "unavailable".into(),
            problem: "Login expired 2d ago. Run claude to refresh it.".into(),
            successful_at: NOW - 300,
        });
        let rows = rows(&merged(NOW, vec![pool]), NOW);
        let row = &rows[0];
        assert!(row.retained);
        assert!(!row.stale, "five minutes old is not stale yet");
        assert_eq!(row.served_at_unix, NOW - 300);
        assert!(row.use_before.is_none(), "kept numbers invite nothing");
        let lines: Vec<&str> = row.tip.lines().collect();
        assert_eq!(
            &lines[2..],
            [
                "Unavailable: Login expired 2d ago. Run claude to refresh it.",
                "Last updated 5m ago"
            ]
        );
        assert_eq!(
            describe_window(&row.pool.windows[1], NOW, true),
            "weekly 60% left, reset time passed"
        );
        let json = row.to_json();
        assert_eq!(json["retained"], true);
        assert_eq!(json["failure"]["status"], "unavailable");
        assert_eq!(json["served_at"], NOW - 300);
    }
}
