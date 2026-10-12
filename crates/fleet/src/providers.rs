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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
