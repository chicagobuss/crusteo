//! Usage from the daemons themselves. A daemon with usage sources (Paseo
//! 0.11, the `usageSources` feature) answers `usage.list_reports.request`
//! with one update per account, each naming it; an older one answers
//! `provider.usage.list.request` with its providers only, so its pools go
//! by host. Paseo 0.12 adds, beside a failed report, the last successful
//! one (`lastSuccessful`), and to an available one when its read finished
//! (`successfulAt`).

use serde_json::Value;

use crate::{Failure, Pool, Reading, UsageSnapshot, Window};

/// The read a daemon is asked for its usage reports, and the type of the
/// message that ends its answer; the reports come before it, one each.
pub const REPORTS_REQUEST: &str = "usage.list_reports.request";
pub const REPORTS_DONE: &str = "usage.list_reports.response";
/// What an older daemon is asked instead.
pub const PROVIDERS_REQUEST: &str = "provider.usage.list.request";

/// How often each daemon is asked. Providers rate-limit their own usage
/// endpoints, so not often.
pub const ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(120);
/// How often to look for daemons due an ask, so one is asked soon after it
/// comes online rather than a whole [`ASK_EVERY`] later.
pub const ASK_TICK: std::time::Duration = std::time::Duration::from_secs(10);

/// A daemon's usage reports (the payloads of its `usage.list_reports.update`
/// messages) as a reading: a pool per account, keyed by the account, so
/// the same account through several hosts is one pool. None when there
/// are none.
///
/// An account whose report failed is a pool with a [`Failure`]: with the
/// last successful report's windows when the daemon kept it (Paseo 0.12's
/// `lastSuccessful`), else with none, for [`crate::merge::set`] to fill
/// from the host's previous reading.
pub fn parse_reports(host: &str, updates: &[Value], now_unix: i64) -> Option<Reading> {
    let entries: Vec<&Value> = updates.iter().map(|update| &update["report"]).collect();
    let pools: Vec<Pool> = (entries.iter())
        .filter_map(|entry| report_pool(host, entry, now_unix))
        .collect();
    // Only reports that gave numbers say how fresh they are: an unavailable
    // one can be as old as the provider's last answer. Paseo 0.12 says when
    // the read finished (`successfulAt`); older daemons when it began.
    let oldest = (entries.iter())
        .filter(|entry| entry["report"]["status"] == "available")
        .filter_map(|entry| text(&entry["successfulAt"]).or(text(&entry["fetchedAt"])))
        .min()
        .unwrap_or_default();
    (!pools.is_empty()).then(|| Reading {
        snapshot: UsageSnapshot {
            revision: 0,
            served_at: oldest.to_string(),
            served_at_unix: crate::parse_unix(oldest).unwrap_or(now_unix),
            pools,
        },
        eligibility: Vec::new(),
    })
}

/// One report as a pool; None for an available one without windows.
fn report_pool(host: &str, entry: &Value, now: i64) -> Option<Pool> {
    let id = entry["id"].as_str()?;
    let provider = entry["sourceId"].as_str()?;
    let status = entry["report"]["status"].as_str()?;
    let (report, failure) = if status == "available" {
        (&entry["report"], None)
    } else {
        // Paseo 0.12 keeps the last successful report beside a failed one;
        // an older daemon sends none, and the hosts' previous reading
        // stands in for it.
        let kept = &entry["lastSuccessful"];
        let successful_at = text(&kept["fetchedAt"]).and_then(crate::parse_unix);
        let report = match successful_at {
            Some(_) if kept["report"]["status"] == "available" => &kept["report"],
            _ => &Value::Null,
        };
        let failure = Failure {
            status: status.to_string(),
            problem: problem_text(entry, now),
            successful_at: successful_at.unwrap_or(0),
        };
        (report, Some(failure))
    };
    let windows: Vec<Window> = (report["windows"].as_array().into_iter().flatten())
        .filter_map(window)
        .collect();
    if windows.is_empty() && failure.is_none() {
        return None;
    }
    let plan = text(&report["planLabel"]);
    // Paseo keys a source's only login "default" when it cannot tell
    // accounts apart (Cursor before 0.12, Copilot and Grok still). That
    // names the host's login, not an account: two hosts' may be different
    // accounts, so they go by host and match on their numbers, as Paseo
    // 0.12 keeps distinct Cursor accounts apart.
    let by_host = id.rsplit_once(':').is_some_and(|(_, key)| key == "default");
    let label = match (text(&entry["account"]["label"]), plan) {
        (Some(label), _) => label.to_string(),
        (None, Some(plan)) if by_host => format!("{host} · {plan}"),
        (None, None) if by_host => host.to_string(),
        (None, plan) => (plan.or(text(&entry["sourceLabel"])))
            .unwrap_or(provider)
            .to_string(),
    };
    Some(Pool {
        id: if by_host {
            format!("paseo:{host}:{id}")
        } else {
            format!("paseo:{id}")
        },
        provider: provider.to_string(),
        account_label: label,
        plan: plan.unwrap_or_default().to_string(),
        status: "ok".into(),
        windows,
        failure,
    })
}

/// What a failed report says, as Paseo's usage card words it: each
/// login's failure by its harness when the daemon names them (0.11),
/// else the report's own.
fn problem_text(entry: &Value, now: i64) -> String {
    match entry["loginErrors"].as_array() {
        Some(logins) => (logins.iter())
            .map(|login| {
                let harness = login["harness"].as_str().unwrap_or_default();
                format!("{harness}: {}", failure_text(&login["report"], now))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => failure_text(&entry["report"], now),
    }
}

/// An error report's error, or an unavailable one's problem in words.
fn failure_text(report: &Value, now: i64) -> String {
    if report["status"] == "error" {
        return report["error"].as_str().unwrap_or_default().to_string();
    }
    let problem = &report["problem"];
    let remedy = match text(&problem["refreshedBy"]) {
        Some(harness) => format!("Run {harness} to refresh it."),
        None => "Sign in again.".to_string(),
    };
    match problem["kind"].as_str() {
        Some("no_quota") => problem["detail"].as_str().unwrap_or_default().to_string(),
        Some("rejected") => format!("Login rejected (HTTP {}). {remedy}", problem["status"]),
        Some("expired") => {
            let ago = (text(&problem["expiresAt"]).and_then(crate::parse_unix))
                .map(|at| format!(" {}", crate::ago(now - at)))
                .unwrap_or_default();
            format!("Login expired{ago}. {remedy}")
        }
        _ => String::new(),
    }
}

/// A daemon's `provider.usage.list.response` payload as a reading: a pool
/// per provider, named for the host, failed ones without windows. None
/// when it has none.
pub fn parse_providers(host: &str, payload: &Value, now_unix: i64) -> Option<Reading> {
    let fetched = payload["fetchedAt"].as_str().unwrap_or_default();
    let served_at_unix = crate::parse_unix(fetched).unwrap_or(now_unix);
    let pools: Vec<Pool> = (payload["providers"].as_array()?.iter())
        .filter_map(|provider| {
            let id = provider["providerId"].as_str()?;
            let status = provider["status"].as_str()?;
            let failure = (status != "available").then(|| Failure {
                status: status.to_string(),
                problem: text(&provider["error"]).unwrap_or_default().to_string(),
                successful_at: 0,
            });
            // A failed provider's windows are not numbers to show.
            let windows: Vec<Window> = (provider["windows"].as_array().into_iter().flatten())
                .filter(|_| failure.is_none())
                .filter_map(window)
                .collect();
            let plan = provider["planLabel"]
                .as_str()
                .filter(|plan| !plan.is_empty());
            (!windows.is_empty() || failure.is_some()).then(|| Pool {
                id: format!("paseo:{host}:{id}"),
                provider: id.to_string(),
                account_label: match plan {
                    Some(plan) => format!("{host} · {plan}"),
                    None => host.to_string(),
                },
                plan: plan.unwrap_or_default().to_string(),
                status: "ok".into(),
                windows,
                failure,
            })
        })
        .collect();
    (!pools.is_empty()).then(|| Reading {
        snapshot: UsageSnapshot {
            revision: 0,
            served_at: fetched.to_string(),
            served_at_unix,
            pools,
        },
        eligibility: Vec::new(),
    })
}

/// A string that says something.
fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

fn window(value: &Value) -> Option<Window> {
    let id = value["id"].as_str()?;
    let label = value["label"].as_str().unwrap_or(id);
    let used = value["usedPct"].as_f64();
    let remaining = value["remainingPct"].as_f64();
    let (used, remaining) = match (used, remaining) {
        (Some(used), _) => (used, remaining.unwrap_or(100. - used)),
        (None, Some(remaining)) => (100. - remaining, remaining),
        (None, None) => return None,
    };
    Some(Window {
        key: id.to_string(),
        label: label.to_string(),
        used_percent: used,
        remaining_percent: remaining,
        window_seconds: window_seconds(&format!("{id} {label}")),
        resets_at: (value["resetsAt"].as_str())
            .and_then(crate::parse_unix)
            .map_or(0., |at| at as f64),
    })
}

/// How long a window is, read from its id and label, since the daemon does
/// not say. Zero when it cannot tell, which leaves the window without a
/// pace.
pub fn window_seconds(name: &str) -> f64 {
    const HOUR: f64 = 3_600.;
    let name = name.to_ascii_lowercase();
    let words: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    for word in &words {
        if let Some(hours) = word.strip_suffix('h').and_then(|n| n.parse::<f64>().ok()) {
            return hours * HOUR;
        }
        if let Some(days) = word.strip_suffix('d').and_then(|n| n.parse::<f64>().ok()) {
            return days * 24. * HOUR;
        }
    }
    let has = |any: &[&str]| words.iter().any(|word| any.contains(word));
    if has(&["week", "weekly", "seven"]) {
        7. * 24. * HOUR
    } else if has(&["month", "monthly"]) {
        30. * 24. * HOUR
    } else if has(&["day", "daily"]) {
        24. * HOUR
    } else if has(&["five"]) && has(&["hour", "hours"]) {
        5. * HOUR
    } else {
        0.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> Value {
        json!({
            "requestId": "r1",
            "fetchedAt": "2026-10-10T08:00:00.000Z",
            "providers": [
                {
                    "providerId": "claude",
                    "displayName": "Claude",
                    "status": "available",
                    "planLabel": "Max",
                    "windows": [
                        {"id": "five_hour", "label": "5h", "usedPct": 40, "resetsAt": "2026-10-10T10:00:00.000Z"},
                        {"id": "seven_day", "label": "Weekly", "remainingPct": 70, "resetsAt": "2026-10-14T00:00:00Z"}
                    ]
                },
                {"providerId": "codex", "displayName": "Codex", "status": "error", "planLabel": null, "windows": [], "error": "rate limited"},
                {"providerId": "gemini", "displayName": "Gemini", "status": "available", "planLabel": null,
                 "windows": [{"id": "x", "label": "Something"}]}
            ]
        })
    }

    #[test]
    fn available_providers_become_pools() {
        let reading = parse_providers("alpha", &payload(), 0).unwrap();
        let pools = &reading.snapshot.pools;
        assert_eq!(pools.len(), 2, "windowless providers are left out");
        let codex = &pools[1];
        assert!(codex.windows.is_empty(), "an error has no numbers");
        let failure = codex.failure.as_ref().unwrap();
        assert_eq!(
            (failure.status.as_str(), failure.problem.as_str()),
            ("error", "rate limited")
        );
        let claude = &pools[0];
        assert_eq!(claude.failure, None);
        assert_eq!(claude.id, "paseo:alpha:claude");
        assert_eq!(claude.provider, "claude");
        assert_eq!(claude.account_label, "alpha · Max");
        assert_eq!(claude.plan, "Max");
        assert_eq!(claude.windows[0].used_percent, 40.);
        assert_eq!(claude.windows[0].remaining_percent, 60.);
        assert_eq!(claude.windows[0].window_seconds, 5. * 3_600.);
        assert_eq!(claude.windows[1].used_percent, 30.);
        assert_eq!(claude.windows[1].window_seconds, 7. * 86_400.);
        assert!(claude.windows[0].resets_at > 0.);
        assert_eq!(
            reading.snapshot.served_at_unix,
            crate::parse_unix("2026-10-10T08:00:00Z").unwrap()
        );
    }

    #[test]
    fn window_lengths_from_names() {
        assert_eq!(window_seconds("five_hour 5h"), 18_000.);
        assert_eq!(window_seconds("primary 24h"), 86_400.);
        assert_eq!(window_seconds("seven_day Weekly"), 604_800.);
        assert_eq!(window_seconds("monthly Credits"), 2_592_000.);
        assert_eq!(window_seconds("models Models"), 0.);
    }

    fn report(id: &str, source: &str, account: &str, used: f64) -> Value {
        json!({"requestId": "r", "report": {
            "id": id, "sourceId": source, "sourceLabel": "Claude", "icon": "claude",
            "account": {"label": account}, "fetchedAt": "2026-10-11T08:00:00.000Z",
            "report": {"status": "available", "planLabel": "Max", "windows": [
                {"id": "five_hour", "label": "5h", "usedPct": used, "resetsAt": "2026-10-11T10:00:00Z"}
            ]}
        }})
    }

    #[test]
    fn reports_become_one_pool_per_account() {
        let updates = vec![
            report("claude:work", "claude", "work@example.com", 40.),
            report("claude:home", "claude", "home@example.com", 10.),
            json!({"requestId": "r", "report": {
                "id": "codex:a", "sourceId": "codex", "sourceLabel": "Codex",
                "account": {}, "fetchedAt": "2026-10-11T07:59:00Z",
                "report": {"status": "unavailable", "problem": {"kind": "no_quota", "detail": ""}}
            }}),
        ];
        let reading = parse_reports("alpha", &updates, 0).unwrap();
        let pools = &reading.snapshot.pools;
        assert_eq!(pools.len(), 3);
        assert_eq!(pools[0].id, "paseo:claude:work");
        assert_eq!(pools[0].provider, "claude");
        assert_eq!(pools[0].account_label, "work@example.com");
        assert_eq!(pools[0].plan, "Max");
        assert_eq!(pools[1].windows[0].used_percent, 10.);
        assert!(
            pools[2].windows.is_empty(),
            "an unavailable report has no numbers"
        );
        assert_eq!(pools[2].failure.as_ref().unwrap().status, "unavailable");
        assert_eq!(
            reading.snapshot.served_at_unix,
            crate::parse_unix("2026-10-11T08:00:00Z").unwrap(),
            "the oldest available report's time; the unavailable one's older time doesn't count"
        );
    }

    #[test]
    fn the_same_account_through_two_hosts_is_one_pool() {
        let a = parse_reports("a", &[report("claude:work", "claude", "work", 40.)], 0).unwrap();
        let b = parse_reports("b", &[report("claude:work", "claude", "work", 40.)], 0).unwrap();
        let pools = [a.snapshot.pools, b.snapshot.pools].concat();
        assert_eq!(crate::dedupe_pools(pools).len(), 1);
    }

    /// Paseo 0.12: a failed report carries the last successful one.
    #[test]
    fn a_failed_report_shows_the_last_successful_one() {
        let mut update = report("claude:work", "claude", "work", 40.);
        let entry = &mut update["report"];
        entry["lastSuccessful"] = json!({
            "fetchedAt": "2026-10-11T07:40:00Z", "report": entry["report"].clone()
        });
        entry["report"] = json!({"status": "error", "error": "Rate limited"});
        let reading = parse_reports("alpha", &[update], 0).unwrap();
        let pool = &reading.snapshot.pools[0];
        assert!(pool.retained());
        assert_eq!(pool.windows[0].used_percent, 40.);
        assert_eq!(pool.plan, "Max", "the plan is the kept report's");
        let failure = pool.failure.as_ref().unwrap();
        assert_eq!(failure.status, "error");
        assert_eq!(failure.problem, "Rate limited");
        assert_eq!(
            failure.successful_at,
            crate::parse_unix("2026-10-11T07:40:00Z").unwrap()
        );
    }

    #[test]
    fn freshness_is_when_the_read_finished() {
        let mut update = report("claude:work", "claude", "work", 40.);
        update["report"]["successfulAt"] = json!("2026-10-11T08:00:15Z");
        let reading = parse_reports("alpha", &[update], 0).unwrap();
        assert_eq!(
            reading.snapshot.served_at_unix,
            crate::parse_unix("2026-10-11T08:00:15Z").unwrap()
        );
    }

    #[test]
    fn failures_are_worded_as_paseo_words_them() {
        let now = crate::parse_unix("2026-10-11T08:00:00Z").unwrap();
        let failed = |report: Value, logins: Option<Value>| {
            let mut entry = json!({"requestId": "r", "report": {
                "id": "claude:work", "sourceId": "claude", "account": {"label": "work"},
                "fetchedAt": "2026-10-11T08:00:00Z", "report": report
            }});
            if let Some(logins) = logins {
                entry["report"]["loginErrors"] = logins;
            }
            let reading = parse_reports("alpha", &[entry], now).unwrap();
            reading.snapshot.pools[0].failure.clone().unwrap().problem
        };
        let expired = json!({"status": "unavailable", "problem": {
            "kind": "expired", "expiresAt": "2026-10-09T08:00:00Z", "refreshedBy": "claude"}});
        assert_eq!(
            failed(expired.clone(), None),
            "Login expired 2d ago. Run claude to refresh it."
        );
        let rejected =
            json!({"status": "unavailable", "problem": {"kind": "rejected", "status": 401}});
        assert_eq!(
            failed(rejected, None),
            "Login rejected (HTTP 401). Sign in again."
        );
        let logins =
            json!([{"harness": "pi", "report": {"status": "error", "error": "Store deleted"}}]);
        assert_eq!(failed(expired, Some(logins)), "pi: Store deleted");
    }

    /// Paseo keys some sources' only login "default" (Cursor before 0.12):
    /// two hosts' are not the same account for it.
    #[test]
    fn default_logins_go_by_host() {
        let cursor = |used: f64| {
            json!({"requestId": "r", "report": {
                "id": "cursor:default", "sourceId": "cursor", "sourceLabel": "Cursor",
                "account": {}, "fetchedAt": "2026-10-11T08:00:00Z",
                "report": {"status": "available", "planLabel": "Pro", "windows": [
                    {"id": "cursor_models", "label": "Models", "usedPct": used, "resetsAt": "2026-10-20T00:00:00Z"}
                ]}
            }})
        };
        let a = parse_reports("alpha", &[cursor(10.)], 0).unwrap();
        let b = parse_reports("beta", &[cursor(60.)], 0).unwrap();
        let shared = parse_reports("gamma", &[cursor(10.)], 0).unwrap();
        assert_eq!(a.snapshot.pools[0].id, "paseo:alpha:cursor:default");
        assert_eq!(a.snapshot.pools[0].account_label, "alpha · Pro");
        let pools = [a.snapshot.pools, b.snapshot.pools, shared.snapshot.pools].concat();
        let kept = crate::dedupe_pools(pools);
        let ids: Vec<&str> = kept.iter().map(|pool| pool.id.as_str()).collect();
        assert_eq!(
            ids,
            ["paseo:alpha:cursor:default", "paseo:beta:cursor:default"],
            "distinct accounts stay apart; the same one through two hosts shows once"
        );
    }

    #[test]
    fn nothing_available_is_no_reading() {
        let payload = json!({"fetchedAt": "2026-10-10T08:00:00Z", "providers": []});
        assert!(parse_providers("alpha", &payload, 0).is_none());
    }
}
