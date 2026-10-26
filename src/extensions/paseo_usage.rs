//! Usage from the daemons themselves: each connected host that reports
//! usage feeds the usage gauges, one pool per account. A host with usage
//! sources (Paseo 0.11) is asked for its reports
//! (`usage.list_reports.request`), which name each account; an older one
//! for its provider usage (`provider.usage.list.request`), which names only
//! providers, so its pools go by host. Reading the answers is
//! [`usage_source::reports`], which `reef` uses too.
//!
//! On by default; `"paseo_usage": "off"` in `integrations.json` turns it
//! off, such as when another source already reports the same accounts.

use std::collections::HashMap;
use std::time::Instant;

use gpui_kit::gpui::{App, Window};
use serde_json::{Value, json};
use usage_source::reports::{
    ASK_EVERY as EVERY, ASK_TICK as TICK, PROVIDERS_REQUEST, REPORTS_DONE, REPORTS_REQUEST,
    parse_providers as parse, parse_reports,
};

use super::{Contributions, Extension, Host};

pub struct PaseoUsage;

impl Extension for PaseoUsage {
    fn contribute(&self, host: &Host, _: &mut Window, cx: &mut App) -> Contributions {
        if crate::app::config::integration("paseo_usage").as_deref() == Some("off") {
            return Contributions::default();
        }
        let core = host.core.clone();
        cx.spawn(async move |cx| {
            // When each host was last asked.
            let mut asked: HashMap<fleet::HostId, Instant> = HashMap::new();
            loop {
                let asks = cx.update(|cx| {
                    let store = core.store.read(cx);
                    (store.fleet.hosts.iter())
                        .filter(|host| host.connection.is_online())
                        .filter(|host| asked.get(host.id()).is_none_or(|at| at.elapsed() >= EVERY))
                        .filter_map(|host| {
                            // Asked by id; gauges still name the host as shown.
                            let (id, name) = (host.id().clone(), host.name().to_string());
                            let ask = if host.has("usageSources") {
                                Ask::Reports(store.request_until(
                                    &id,
                                    json!({ "type": REPORTS_REQUEST }),
                                    REPORTS_DONE,
                                    cx,
                                ))
                            } else if host.has("providerUsageList") {
                                Ask::Providers(store.write(
                                    &id,
                                    json!({ "type": PROVIDERS_REQUEST }),
                                    cx,
                                ))
                            } else {
                                return None;
                            };
                            Some((id, name, ask))
                        })
                        .collect::<Vec<_>>()
                });
                for (id, name, ask) in asks {
                    asked.insert(id.clone(), Instant::now());
                    let now = usage_source::unix_now();
                    // A host that fails keeps its last reading; an account
                    // whose report failed keeps its last numbers.
                    let reading = match ask {
                        Ask::Reports(ask) => match ask.await {
                            Ok((updates, last)) if last["error"].is_null() => {
                                parse_reports(&name, &updates, now)
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
                            usage.set_reading(&format!("paseo:{id}"), reading, cx)
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
