//! Usage from the daemons themselves: each connected host that reports
//! usage feeds the usage gauges, one pool per account. A host with usage
//! sources (Paseo 0.11) is asked for its reports
//! (`usage.list_reports.request`), which name each account; an older one
//! for its provider usage (`provider.usage.list.request`), which names only
//! providers, so its pools go by host.
//!
//! On by default; `"paseo_usage": "off"` in `integrations.json` turns it
//! off, such as when another source already reports the same accounts.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::gpui::{App, Window};
use serde_json::{Value, json};
use usage_source::{Pool, UsageSnapshot, Window as UsageWindow};

use super::{Contributions, Extension, Host};
use crate::app::usage::Reading;

/// How often each host is asked. Providers rate-limit their own usage
/// endpoints, so not often.
const EVERY: Duration = Duration::from_secs(120);
/// How often to look for hosts due an ask, so one is asked soon after it
/// comes online rather than a whole [`EVERY`] later.
const TICK: Duration = Duration::from_secs(10);

pub struct PaseoUsage;

impl Extension for PaseoUsage {
    fn contribute(&self, host: &Host, _: &mut Window, cx: &mut App) -> Contributions {
        if crate::app::config::integration("paseo_usage").as_deref() == Some("off") {
            return Contributions::default();
        }
        let core = host.core.clone();
        cx.spawn(async move |cx| {
            // When each host was last asked.
            let mut asked: HashMap<String, Instant> = HashMap::new();
            loop {
                let asks = cx.update(|cx| {
                    let store = core.store.read(cx);
                    (store.fleet.hosts.iter())
                        .filter(|host| host.connection.is_online())
                        .filter(|host| {
                            asked
                                .get(host.name())
                                .is_none_or(|at| at.elapsed() >= EVERY)
                        })
                        .filter_map(|host| {
                            let name = host.name().to_string();
                            let ask = if host.has("usageSources") {
                                Ask::Reports(store.request_until(
                                    &name,
                                    json!({ "type": "usage.list_reports.request" }),
                                    "usage.list_reports.response",
                                    cx,
                                ))
                            } else if host.has("providerUsageList") {
                                Ask::Providers(store.write(
                                    &name,
                                    json!({ "type": "provider.usage.list.request" }),
                                    cx,
                                ))
                            } else {
                                return None;
                            };
                            Some((name, ask))
                        })
                        .collect::<Vec<_>>()
                });
                for (name, ask) in asks {
                    asked.insert(name.clone(), Instant::now());
                    let now = usage_source::unix_now();
                    // A host that fails keeps its last reading.
                    let reading = match ask {
                        Ask::Reports(ask) => match ask.await {
                            Ok((updates, last)) if last["error"].is_null() => {
                                parse_reports(&updates, now)
                            }
                            _ => continue,
                        },
                        Ask::Providers(ask) => match ask.await {
                            Ok(payload) => parse(&name, &payload, now),
                            Err(_) => continue,
                        },
                    };
                    let usage = core.usage.clone();
                    cx.update(|cx| {
                        usage.update(cx, |usage, cx| {
                            usage.set_reading(&format!("paseo:{name}"), reading, cx)
                        })
                    });
                }
                cx.background_executor().timer(TICK).await;
            }
        })
        .detach();
        Contributions::default()
    }
}

/// What a host is asked, by what it supports.
enum Ask {
    Reports(gpui_kit::gpui::Task<Result<(Vec<Value>, Value), String>>),
    Providers(gpui_kit::gpui::Task<Result<Value, String>>),
}

/// A host's usage reports (the payloads of its `usage.list_reports.update`
/// messages) as a reading: a pool per account with an available report,
/// keyed by the account, so the same account through several hosts is one
/// pool. None when there are none.
pub fn parse_reports(updates: &[Value], now_unix: i64) -> Option<Reading> {
    let entries: Vec<&Value> = updates.iter().map(|update| &update["report"]).collect();
    let pools: Vec<Pool> = (entries.iter())
        .filter(|entry| entry["report"]["status"] == "available")
        .filter_map(|entry| {
            let id = entry["id"].as_str()?;
            let provider = entry["sourceId"].as_str()?;
            let windows: Vec<UsageWindow> = (entry["report"]["windows"].as_array()?.iter())
                .filter_map(window)
                .collect();
            let label = [
                &entry["account"]["label"],
                &entry["report"]["planLabel"],
                &entry["sourceLabel"],
            ]
            .into_iter()
            .find_map(|value| value.as_str().filter(|text| !text.is_empty()))
            .unwrap_or(provider);
            (!windows.is_empty()).then(|| Pool {
                id: format!("paseo:{id}"),
                provider: provider.to_string(),
                account_label: label.to_string(),
                status: "ok".into(),
                windows,
            })
        })
        .collect();
    // Only reports that gave numbers say how fresh they are: an unavailable
    // one can be as old as the provider's last answer.
    let oldest = (entries.iter())
        .filter(|entry| entry["report"]["status"] == "available")
        .filter_map(|entry| entry["fetchedAt"].as_str())
        .min()
        .unwrap_or_default();
    (!pools.is_empty()).then(|| Reading {
        snapshot: UsageSnapshot {
            revision: 0,
            served_at: oldest.to_string(),
            served_at_unix: usage_source::parse_unix(oldest).unwrap_or(now_unix),
            pools,
        },
        eligibility: Vec::new(),
    })
}

/// A host's `provider.usage.list.response` payload as a reading: a pool
/// per available provider, named for the host. None when it has none.
pub fn parse(host: &str, payload: &Value, now_unix: i64) -> Option<Reading> {
    let fetched = payload["fetchedAt"].as_str().unwrap_or_default();
    let served_at_unix = usage_source::parse_unix(fetched).unwrap_or(now_unix);
    let pools: Vec<Pool> = (payload["providers"].as_array()?.iter())
        .filter(|provider| provider["status"] == "available")
        .filter_map(|provider| {
            let id = provider["providerId"].as_str()?;
            let windows: Vec<UsageWindow> = (provider["windows"].as_array()?.iter())
                .filter_map(window)
                .collect();
            (!windows.is_empty()).then(|| Pool {
                id: format!("paseo:{host}:{id}"),
                provider: id.to_string(),
                account_label: match provider["planLabel"].as_str() {
                    Some(plan) if !plan.is_empty() => format!("{host} · {plan}"),
                    _ => host.to_string(),
                },
                status: "ok".into(),
                windows,
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

fn window(value: &Value) -> Option<UsageWindow> {
    let id = value["id"].as_str()?;
    let label = value["label"].as_str().unwrap_or(id);
    let used = value["usedPct"].as_f64();
    let remaining = value["remainingPct"].as_f64();
    let (used, remaining) = match (used, remaining) {
        (Some(used), _) => (used, remaining.unwrap_or(100. - used)),
        (None, Some(remaining)) => (100. - remaining, remaining),
        (None, None) => return None,
    };
    Some(UsageWindow {
        key: id.to_string(),
        label: label.to_string(),
        used_percent: used,
        remaining_percent: remaining,
        window_seconds: window_seconds(&format!("{id} {label}")),
        resets_at: (value["resetsAt"].as_str())
            .and_then(usage_source::parse_unix)
            .map_or(0., |at| at as f64),
    })
}

/// How long a window is, read from its id and label, since the daemon does
/// not say. Zero when it cannot tell, which leaves the window without a
/// pace.
fn window_seconds(name: &str) -> f64 {
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
        let reading = parse("alpha", &payload(), 0).unwrap();
        let pools = &reading.snapshot.pools;
        assert_eq!(
            pools.len(),
            1,
            "errors and windowless providers are left out"
        );
        let claude = &pools[0];
        assert_eq!(claude.id, "paseo:alpha:claude");
        assert_eq!(claude.provider, "claude");
        assert_eq!(claude.account_label, "alpha · Max");
        assert_eq!(claude.windows[0].used_percent, 40.);
        assert_eq!(claude.windows[0].remaining_percent, 60.);
        assert_eq!(claude.windows[0].window_seconds, 5. * 3_600.);
        assert_eq!(claude.windows[1].used_percent, 30.);
        assert_eq!(claude.windows[1].window_seconds, 7. * 86_400.);
        assert!(claude.windows[0].resets_at > 0.);
        assert_eq!(
            reading.snapshot.served_at_unix,
            usage_source::parse_unix("2026-10-10T08:00:00Z").unwrap()
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
        let reading = parse_reports(&updates, 0).unwrap();
        let pools = &reading.snapshot.pools;
        assert_eq!(pools.len(), 2, "unavailable reports are left out");
        assert_eq!(pools[0].id, "paseo:claude:work");
        assert_eq!(pools[0].provider, "claude");
        assert_eq!(pools[0].account_label, "work@example.com");
        assert_eq!(pools[1].windows[0].used_percent, 10.);
        assert_eq!(
            reading.snapshot.served_at_unix,
            usage_source::parse_unix("2026-10-11T08:00:00Z").unwrap(),
            "the oldest available report's time; the unavailable one's older time doesn't count"
        );
    }

    #[test]
    fn the_same_account_through_two_hosts_is_one_pool() {
        let a = parse_reports(&[report("claude:work", "claude", "work", 40.)], 0).unwrap();
        let b = parse_reports(&[report("claude:work", "claude", "work", 40.)], 0).unwrap();
        let pools = [a.snapshot.pools, b.snapshot.pools].concat();
        assert_eq!(usage_source::dedupe_pools(pools).len(), 1);
    }

    #[test]
    fn nothing_available_is_no_reading() {
        let payload = json!({"fetchedAt": "2026-10-10T08:00:00Z", "providers": []});
        assert!(parse("alpha", &payload, 0).is_none());
    }
}
