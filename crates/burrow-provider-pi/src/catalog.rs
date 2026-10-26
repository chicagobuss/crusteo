//! pi's provider entry, models and thinking levels, as Paseo 0.12 offers
//! them (`protocol/src/provider-manifest.ts`, `providers/pi/agent.ts`).
//!
//! pi has no modes. Its models are whatever it is configured with, read from
//! a pi process (`get_available_models`), with the model pi starts on
//! (`get_state`) as the default. A model id is `<provider>/<id>`.

use burrow_provider::{Model, ThinkingOption};
use serde_json::Value;

pub const LABEL: &str = "Pi";
pub const DESCRIPTION: &str = "Minimal terminal-based coding agent with multi-provider LLM support";

/// pi's thinking levels, lowest first.
pub const THINKING_LEVELS: &[(&str, &str)] = &[
    ("off", "Off"),
    ("minimal", "Minimal"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "XHigh"),
    ("max", "Max"),
];

/// What Paseo asks for when no level is chosen.
pub const DEFAULT_THINKING: &str = "medium";

/// A level pi accepts, or `None`.
pub fn thinking_level(option: Option<&str>) -> Option<&'static str> {
    let option = option?;
    THINKING_LEVELS
        .iter()
        .map(|(id, _)| *id)
        .find(|id| *id == option)
}

/// `"<provider>/<id>"` of one of pi's model objects.
pub fn model_id(model: &Value) -> Option<String> {
    let provider = model["provider"].as_str().filter(|p| !p.is_empty())?;
    let id = model["id"].as_str().filter(|i| !i.is_empty())?;
    Some(format!("{provider}/{id}"))
}

/// `"<provider>/<id>"` (or `"<provider>:<id>"`) split; `None` without a
/// provider.
pub fn split_model(id: &str) -> Option<(&str, &str)> {
    for separator in ['/', ':'] {
        if let Some((provider, model)) = id.split_once(separator)
            && !provider.is_empty()
            && !model.is_empty()
        {
            return Some((provider, model));
        }
    }
    None
}

/// Paseo's label: the last `/` segment of `<provider>/<name>`, with
/// underscores as spaces and any `Vendor: ` prefix dropped.
fn label(provider: &str, name: &str) -> String {
    let full = format!("{provider}/{name}");
    let last = full
        .split('/')
        .rfind(|s| !s.is_empty())
        .unwrap_or(&full)
        .trim();
    let spaced = last
        .split(|c: char| c == '_' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    match spaced.split_once(": ") {
        Some((_, rest)) => rest.trim().to_string(),
        None => spaced,
    }
}

/// The thinking levels a model offers, with Paseo's default: `medium`, or
/// the next level up it has, or else its highest.
fn thinking(model: &Value) -> Vec<ThinkingOption> {
    if model["reasoning"] != true {
        return Vec::new();
    }
    let map = &model["thinkingLevelMap"];
    let supported: Vec<(usize, &str, &str)> = THINKING_LEVELS
        .iter()
        .enumerate()
        .filter(|(_, (id, _))| {
            let mapped = map.get(*id);
            if mapped.is_some_and(Value::is_null) {
                return false;
            }
            // Only models that name them get the two highest levels.
            !matches!(*id, "xhigh" | "max") || mapped.is_some()
        })
        .map(|(at, (id, label))| (at, *id, *label))
        .collect();
    let medium = THINKING_LEVELS
        .iter()
        .position(|(id, _)| *id == DEFAULT_THINKING)
        .unwrap_or(0);
    let default = supported
        .iter()
        .find(|(at, _, _)| *at >= medium)
        .or(supported.last())
        .map_or("off", |(_, id, _)| *id);
    supported
        .into_iter()
        .map(|(_, id, label)| ThinkingOption {
            id: id.into(),
            label: label.into(),
            is_default: id == default,
        })
        .collect()
}

/// The models of a `get_available_models` answer; `current` is the model a
/// fresh pi starts on.
pub fn models(available: &[Value], current: Option<&str>) -> Vec<Model> {
    available
        .iter()
        .filter_map(|model| {
            let id = model_id(model)?;
            let provider = model["provider"].as_str().unwrap_or_default();
            let name = model["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .or(model["id"].as_str())
                .unwrap_or_default();
            Some(Model {
                label: label(provider, name),
                description: Some(id.clone()),
                is_default: current == Some(id.as_str()),
                context_window_max_tokens: None,
                thinking: thinking(model),
                id,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn models_are_labelled_like_paseo() {
        let available = [
            json!({"provider": "google", "id": "gemini-2.5-flash-lite",
                   "name": "Gemini 2.5 Flash-Lite", "reasoning": true}),
            json!({"provider": "doubleword", "id": "deepseek-ai/DeepSeek-V4-Pro",
                   "reasoning": false}),
            json!({"provider": "openrouter", "id": "x/y", "name": "Vendor: Some_Model",
                   "reasoning": true, "thinkingLevelMap": {"minimal": null, "max": "max"}}),
        ];
        let models = models(&available, Some("google/gemini-2.5-flash-lite"));
        assert_eq!(models[0].id, "google/gemini-2.5-flash-lite");
        assert_eq!(models[0].label, "Gemini 2.5 Flash-Lite");
        assert_eq!(
            models[0].description.as_deref(),
            Some("google/gemini-2.5-flash-lite")
        );
        assert!(models[0].is_default && !models[1].is_default);
        let levels: Vec<&str> = models[0].thinking.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(levels, ["off", "minimal", "low", "medium", "high"]);
        assert!(models[0].thinking[3].is_default);
        assert_eq!(models[1].label, "DeepSeek-V4-Pro");
        assert!(models[1].thinking.is_empty(), "no reasoning, no levels");
        assert_eq!(models[2].label, "Some Model");
        let levels: Vec<&str> = models[2].thinking.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(levels, ["off", "low", "medium", "high", "max"]);
    }

    #[test]
    fn model_ids_split_at_the_provider() {
        assert_eq!(
            split_model("doubleword/deepseek-ai/DeepSeek-V4-Pro"),
            Some(("doubleword", "deepseek-ai/DeepSeek-V4-Pro"))
        );
        assert_eq!(
            split_model("google:gemini-flash"),
            Some(("google", "gemini-flash"))
        );
        assert_eq!(split_model("gemini"), None);
        assert_eq!(thinking_level(Some("xhigh")), Some("xhigh"));
        assert_eq!(thinking_level(Some("ultracode")), None);
    }
}
