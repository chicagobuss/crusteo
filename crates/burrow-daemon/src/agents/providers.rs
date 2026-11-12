//! The providers snapshot clients read: what each registered provider offers,
//! compacted as Paseo compacts it, and the updates subscribers get once a
//! probe lands.

use std::sync::Arc;
use std::time::Duration;

use burrow_provider::ProviderInfo;
use serde_json::{Value, json};

use super::publish;
use crate::clock;
use crate::daemon::Daemon;
use crate::store::StoreEvent;

/// `get_providers_snapshot_response` entries for every registered provider.
/// With `compact`, the shape clients get when they advertise
/// `compact_provider_snapshots`: thinking options shared through `thinkingSets`.
pub fn providers_snapshot(daemon: &Daemon, compact: bool) -> Value {
    let providers = daemon.agents.all();
    let infos: Vec<ProviderInfo> = providers.iter().map(|p| p.info()).collect();
    let mode_json = |m: &burrow_provider::Mode| {
        let mut mode = json!({"id": m.id, "label": m.label});
        if let Some(d) = &m.description {
            mode["description"] = d.clone().into();
        }
        if let Some(i) = &m.icon {
            mode["icon"] = i.clone().into();
        }
        if let Some(c) = &m.color_tier {
            mode["colorTier"] = c.clone().into();
        }
        mode
    };
    let thinking_json = |options: &[burrow_provider::ThinkingOption]| -> Value {
        options
            .iter()
            .map(|o| json!({"id": o.id, "label": o.label, "isDefault": o.is_default}))
            .collect()
    };
    let mut thinking_sets: Vec<Value> = Vec::new();
    let mut entries = Vec::new();
    for (provider, info) in providers.iter().zip(&infos) {
        if provider.loading() {
            // Paseo's entry while it probes: no models or modes yet.
            let mut entry = json!({"provider": info.id, "label": info.label, "status": "loading",
                                   "enabled": true, "defaultModeId": info.default_mode_id});
            if let Some(d) = &info.description {
                entry["description"] = d.clone().into();
            }
            entries.push(entry);
            continue;
        }
        if let Some(reason) = provider.unavailable() {
            // Paseo's entry for a provider that cannot run: offered no
            // more, with the reason clients show beside its name.
            let mut entry = json!({"provider": info.id, "label": info.label, "status": "error",
                                   "enabled": true, "error": reason,
                                   "defaultModeId": info.default_mode_id});
            if let Some(d) = &info.description {
                entry["description"] = d.clone().into();
            }
            entries.push(entry);
            continue;
        }
        let models: Vec<Value> =
            info.models
                .iter()
                .map(|m| {
                    let mut model = json!({"id": m.id, "label": m.label, "isDefault": m.is_default,
                                       "isSelectable": true});
                    // `AgentModelDefinitionSchema` requires it; the compact
                    // form, whose models sit under their entry, has none.
                    if !compact {
                        model["provider"] = info.id.clone().into();
                    }
                    if let Some(d) = &m.description {
                        model["description"] = d.clone().into();
                    }
                    if let Some(max) = m.context_window_max_tokens {
                        model["contextWindowMaxTokens"] = max.into();
                    }
                    if !m.thinking.is_empty() {
                        let default = m
                            .thinking
                            .iter()
                            .find(|o| o.is_default)
                            .or(m.thinking.first())
                            .map(|o| o.id.clone());
                        model["thinkingOptions"] = thinking_json(&m.thinking);
                        model["defaultThinkingOptionId"] = default.into();
                    }
                    if let Some(fields) = model.as_object_mut() {
                        for (key, value) in provider.model_fields(&m.id) {
                            if value.is_null() {
                                fields.remove(&key);
                            } else {
                                fields.insert(key, value);
                            }
                        }
                    }
                    if compact && let Some(fields) = model.as_object_mut() {
                        // Paseo's `compactModel`: the options move to a shared set.
                        let default = fields.remove("defaultThinkingOptionId");
                        if let Some(options) = fields.remove("thinkingOptions") {
                            let mut set = json!({"options": options});
                            if let Some(default) = default.filter(|d| !d.is_null()) {
                                set["defaultOptionId"] = default;
                            }
                            let index = thinking_sets
                                .iter()
                                .position(|s| *s == set)
                                .unwrap_or_else(|| {
                                    thinking_sets.push(set);
                                    thinking_sets.len() - 1
                                });
                            fields.insert("thinkingSet".into(), index.into());
                        } else if let Some(default) = default {
                            fields.insert("defaultThinkingOptionId".into(), default);
                        }
                    }
                    model
                })
                .collect();
        let mut entry = json!({
            "provider": info.id, "label": info.label, "status": "ready", "enabled": true,
            "defaultModeId": info.default_mode_id,
            "modes": info.modes.iter().map(mode_json).collect::<Vec<_>>(),
            "models": models,
        });
        if let Some(d) = &info.description {
            entry["description"] = d.clone().into();
        }
        // Burrow's, not Paseo's: which CLI the models came from.
        if let Some(cli) = provider.cli() {
            if let Some(version) = cli.version {
                entry["version"] = version.into();
            }
            entry["installKind"] = cli.install_kind.as_str().into();
        }
        entries.push(entry);
    }
    let now = clock::now_iso();
    // Paseo reports when each provider was last probed.
    let fetched: serde_json::Map<String, Value> = infos
        .iter()
        .map(|info| (info.id.clone(), Value::String(now.clone())))
        .collect();
    let hash = format!(
        "{:x}",
        fxhash(&serde_json::to_string(&entries).unwrap_or_default())
    );
    if compact {
        json!({"entries": [], "compactSnapshot": {"entries": entries, "thinkingSets": thinking_sets},
               "snapshotHash": hash, "generatedAt": now, "fetchedAt": fetched})
    } else {
        json!({"entries": entries, "snapshotHash": hash, "generatedAt": now, "fetchedAt": fetched})
    }
}

/// How often providers that are still loading are looked at.
const LOADING_POLL: Duration = Duration::from_millis(250);

/// Pushes `providers_snapshot_update` to its subscribers once every provider
/// that is loading now has finished, as Paseo does when a probe lands.
/// Clients that fetched the snapshot while it said `loading` (Crusteo and
/// Reef fetch it once per host) otherwise never see what it found.
pub fn announce_when_loaded(daemon: &Arc<Daemon>) {
    if !daemon.agents.all().iter().any(|p| p.loading()) {
        return;
    }
    let daemon = daemon.clone();
    tokio::spawn(async move {
        while daemon.agents.all().iter().any(|p| p.loading()) {
            tokio::time::sleep(LOADING_POLL).await;
        }
        publish_providers(&daemon);
    });
}

/// Publishes the providers snapshot as a `providers_snapshot_update`.
pub fn publish_providers(daemon: &Daemon) {
    let push = |compact: bool| {
        let mut payload = providers_snapshot(daemon, compact);
        if let Some(fields) = payload.as_object_mut() {
            fields.remove("requestId");
        }
        payload
    };
    let (plain, compact) = (push(false), push(true));
    publish(daemon, vec![StoreEvent::Providers { plain, compact }]);
}

/// A stable, cheap content hash for `snapshotHash`.
fn fxhash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}
