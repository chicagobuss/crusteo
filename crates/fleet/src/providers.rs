//! The providers snapshot (`get_providers_snapshot_response`), in either the
//! plain or the compact encoding.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub is_default: bool,
    pub thinking: Vec<Choice>,
    pub default_thinking: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    pub id: String,
    pub label: String,
    pub status: String,
    pub enabled: bool,
    pub default_mode: Option<String>,
    pub modes: Vec<Choice>,
    pub models: Vec<Model>,
}

impl Provider {
    pub fn ready(&self) -> bool {
        self.enabled && self.status == "ready"
    }

    pub fn default_model(&self) -> Option<&Model> {
        self.models
            .iter()
            .find(|m| m.is_default)
            .or_else(|| self.models.first())
    }
}

fn choices(value: &Value) -> Vec<Choice> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(Choice {
                id: c["id"].as_str()?.to_string(),
                label: c["label"]
                    .as_str()
                    .unwrap_or_else(|| c["id"].as_str().unwrap_or(""))
                    .to_string(),
            })
        })
        .collect()
}

pub fn parse(payload: &Value) -> Vec<Provider> {
    let compact = &payload["compactSnapshot"];
    let (entries, sets) = if compact.is_object() {
        (
            compact["entries"].clone(),
            compact["thinkingSets"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        )
    } else {
        (payload["entries"].clone(), Vec::new())
    };
    entries
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let models = entry["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["isSelectable"] != false)
                .filter_map(|m| {
                    let (thinking, default_thinking) = match m["thinkingSet"].as_u64() {
                        Some(index) => {
                            let set = sets.get(index as usize).cloned().unwrap_or(Value::Null);
                            (
                                choices(&set["options"]),
                                set["defaultOptionId"].as_str().map(str::to_string),
                            )
                        }
                        None => (
                            choices(&m["thinkingOptions"]),
                            m["defaultThinkingOptionId"].as_str().map(str::to_string),
                        ),
                    };
                    Some(Model {
                        id: m["id"].as_str()?.to_string(),
                        label: m["label"].as_str().unwrap_or_default().to_string(),
                        is_default: m["isDefault"] == true,
                        thinking,
                        default_thinking,
                    })
                })
                .collect();
            Some(Provider {
                id: entry["provider"].as_str()?.to_string(),
                label: entry["label"].as_str().unwrap_or_default().to_string(),
                status: entry["status"].as_str().unwrap_or_default().to_string(),
                enabled: entry["enabled"].as_bool().unwrap_or(true),
                default_mode: entry["defaultModeId"].as_str().map(str::to_string),
                modes: choices(&entry["modes"]),
                models,
            })
        })
        .collect()
}

/// What a `providers_snapshot_update` push asks of a client that keeps one
/// host-wide snapshot, given the `snapshotHash` it last loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// The push carries the snapshot itself (older daemons): keep it.
    Keep,
    /// Only a new hash came (clients that take snapshot references, as
    /// crusteo does, are sent just that): load the snapshot again.
    Reload,
    /// Nothing changed, or it is one project directory's snapshot.
    Ignore,
}

pub fn update(payload: &Value, known_hash: Option<&str>) -> Update {
    if payload["cwd"].as_str().is_some_and(|cwd| !cwd.is_empty()) {
        return Update::Ignore;
    }
    let carries_snapshot = payload["compactSnapshot"].is_object()
        || payload["entries"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty());
    if carries_snapshot {
        return Update::Keep;
    }
    match payload["snapshotHash"].as_str() {
        Some(hash) if Some(hash) == known_hash => Update::Ignore,
        _ => Update::Reload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_hash_only_push_reloads_when_the_hash_moved() {
        let push = json!({"entries": [], "snapshotHash": "h2", "generatedAt": "t"});
        assert_eq!(update(&push, Some("h1")), Update::Reload);
        assert_eq!(update(&push, None), Update::Reload);
        assert_eq!(update(&push, Some("h2")), Update::Ignore);
    }

    #[test]
    fn a_push_with_the_snapshot_is_kept() {
        let compact = json!({"entries": [], "snapshotHash": "h2",
            "compactSnapshot": {"entries": [], "thinkingSets": []}});
        assert_eq!(update(&compact, Some("h1")), Update::Keep);
        let plain = json!({"entries": [{"provider": "claude", "status": "ready"}]});
        assert_eq!(update(&plain, None), Update::Keep);
    }

    #[test]
    fn a_project_directory_snapshot_is_not_the_host_wide_one() {
        let push = json!({"cwd": "/home/user/app", "entries": [], "snapshotHash": "h9"});
        assert_eq!(update(&push, Some("h1")), Update::Ignore);
    }

    #[test]
    fn a_snapshot_taken_while_probes_run_has_nothing_ready() {
        // What a daemon answers just after it starts: the reason a cached
        // snapshot must be reloaded when the daemon says it changed.
        let payload = json!({"entries": [], "compactSnapshot": {"thinkingSets": [], "entries": [
            {"provider": "claude", "status": "loading", "enabled": true, "label": "Claude"},
            {"provider": "codex", "status": "loading", "enabled": true, "label": "Codex"}]}});
        assert!(parse(&payload).iter().all(|provider| !provider.ready()));
    }

    #[test]
    fn compact_snapshot() {
        let payload = json!({"entries": [], "compactSnapshot": {
            "entries": [{"provider": "claude", "status": "ready", "enabled": true, "label": "Claude", "defaultModeId": "auto",
                         "modes": [{"id": "auto", "label": "Auto"}],
                         "models": [{"id": "opus", "label": "Opus", "isDefault": true, "thinkingSet": 0}]}],
            "thinkingSets": [{"defaultOptionId": "medium", "options": [{"id": "low", "label": "Low"}, {"id": "medium", "label": "Medium"}]}]}});
        let providers = parse(&payload);
        assert_eq!(providers.len(), 1);
        let model = providers[0].default_model().unwrap();
        assert_eq!(model.thinking.len(), 2);
        assert_eq!(model.default_thinking.as_deref(), Some("medium"));
    }
}
