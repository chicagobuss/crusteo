//! Modes, models and thinking options from an ACP session's state, as
//! Paseo's `acp-agent.ts` derives them (`deriveModesFromACP`,
//! `deriveModelDefinitionsFromACP`, `deriveSelectorOptions`).
//!
//! A session reports `modes` (`availableModes`, `currentModeId`), `models`
//! (`availableModels`, `currentModelId`) and `configOptions`, a list of
//! selects with a `category` (`mode`, `model`, `thought_level`, ...). Where
//! the dedicated field is missing, the config option of that category
//! stands in for it.

use burrow_provider::{Mode, Model, ThinkingOption};
use serde_json::Value;

/// The first `select` config option of `category` (and `id`, if given).
pub fn select_option<'a>(
    options: &'a [Value],
    category: &str,
    id: Option<&str>,
) -> Option<&'a Value> {
    options.iter().find(|option| {
        option["type"] == "select"
            && option["category"] == category
            && id.is_none_or(|id| option["id"] == id)
    })
}

/// A `select` config option by id, whatever its category.
pub fn select_option_by_id<'a>(options: &'a [Value], id: &str) -> Option<&'a Value> {
    options
        .iter()
        .find(|option| option["type"] == "select" && option["id"] == id)
}

/// A select's choices with groups flattened: `(value, name, description)`.
pub fn choices(option: &Value) -> Vec<(String, String, Option<String>)> {
    let mut flat = Vec::new();
    for choice in option["options"].as_array().into_iter().flatten() {
        if choice.get("value").is_some() {
            flat.push(choice_of(choice));
            continue;
        }
        for grouped in choice["options"].as_array().into_iter().flatten() {
            flat.push(choice_of(grouped));
        }
    }
    flat
}

fn choice_of(choice: &Value) -> (String, String, Option<String>) {
    (
        choice["value"].as_str().unwrap_or_default().to_string(),
        choice["name"].as_str().unwrap_or_default().to_string(),
        choice["description"].as_str().map(str::to_string),
    )
}

/// Whether `option` offers `value`.
pub fn has_choice(option: &Value, value: &str) -> bool {
    choices(option).iter().any(|(v, _, _)| v == value)
}

/// The current value of the first select of `category`.
pub fn current_value(options: &[Value], category: &str) -> Option<String> {
    select_option(options, category, None)
        .and_then(|option| option["currentValue"].as_str())
        .map(str::to_string)
}

/// The choices of the first select of `category`, the current one default.
pub fn selector_options(options: &[Value], category: &str) -> Vec<ThinkingOption> {
    let Some(option) = select_option(options, category, None) else {
        return Vec::new();
    };
    let current = option["currentValue"].as_str();
    choices(option)
        .into_iter()
        .map(|(value, name, _)| ThinkingOption {
            is_default: Some(value.as_str()) == current,
            id: value,
            label: name,
        })
        .collect()
}

/// The session's modes and its current mode: `modes` if it lists any, else
/// the `mode` config option.
pub fn modes(mode_state: &Value, options: &[Value]) -> (Vec<Mode>, Option<String>) {
    if let Some(available) = mode_state["availableModes"]
        .as_array()
        .filter(|a| !a.is_empty())
    {
        let modes = available
            .iter()
            .map(|mode| Mode {
                id: mode["id"].as_str().unwrap_or_default().into(),
                label: mode["name"].as_str().unwrap_or_default().into(),
                description: mode["description"].as_str().map(str::to_string),
                icon: None,
                color_tier: None,
            })
            .collect();
        let current = mode_state["currentModeId"].as_str().map(str::to_string);
        return (modes, current);
    }
    if let Some(option) = select_option(options, "mode", None) {
        let modes = choices(option)
            .into_iter()
            .map(|(id, label, description)| Mode {
                id,
                label,
                description,
                icon: None,
                color_tier: None,
            })
            .collect();
        return (modes, option["currentValue"].as_str().map(str::to_string));
    }
    (Vec::new(), None)
}

/// The models of a session: `models.availableModels` if any, else the
/// `model` config option. Every model gets the session's thinking options.
pub fn models(model_state: &Value, options: &[Value]) -> Vec<Model> {
    let thinking = selector_options(options, "thought_level");
    if let Some(available) = model_state["availableModels"]
        .as_array()
        .filter(|a| !a.is_empty())
    {
        let current = model_state["currentModelId"].as_str();
        return available
            .iter()
            .map(|model| {
                let id = model["modelId"].as_str().unwrap_or_default();
                Model {
                    id: id.into(),
                    label: model["name"].as_str().unwrap_or_default().into(),
                    description: model["description"].as_str().map(str::to_string),
                    is_default: Some(id) == current,
                    context_window_max_tokens: None,
                    thinking: thinking.clone(),
                }
            })
            .collect();
    }
    let Some(option) = select_option(options, "model", None) else {
        return Vec::new();
    };
    let current = option["currentValue"].as_str();
    choices(option)
        .into_iter()
        .map(|(id, label, description)| Model {
            is_default: Some(id.as_str()) == current,
            id,
            label,
            description,
            context_window_max_tokens: None,
            thinking: thinking.clone(),
        })
        .collect()
}

/// Cursor's `cursor/list_available_models` answer: every model with its own
/// thinking options; the session's current model is the default
/// (`resolveCursorCatalogModels`).
pub fn cursor_models(catalog: &Value, current: Option<&str>) -> Vec<Model> {
    catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model["value"].as_str().filter(|id| !id.is_empty())?;
            let options = model["configOptions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            Some(Model {
                id: id.into(),
                label: model["name"].as_str().unwrap_or_default().into(),
                description: None,
                is_default: Some(id) == current,
                context_window_max_tokens: None,
                thinking: selector_options(&options, "thought_level"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cursor_session() -> Value {
        json!({
            "sessionId": "s1",
            "modes": {"currentModeId": "agent", "availableModes": [
                {"id": "agent", "name": "Agent", "description": "Full agent capabilities with tool access"},
                {"id": "plan", "name": "Plan"},
            ]},
            "models": {"currentModelId": "gpt-5-mini", "availableModels": [
                {"modelId": "default", "name": "Auto"},
                {"modelId": "gpt-5-mini", "name": "GPT-5 Mini"},
            ]},
            "configOptions": [
                {"id": "mode", "category": "mode", "type": "select", "currentValue": "agent",
                 "options": [{"value": "agent", "name": "Agent"}]},
                {"id": "reasoning_effort", "category": "thought_level", "type": "select",
                 "currentValue": "high", "options": [
                    {"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
            ],
        })
    }

    #[test]
    fn modes_and_models_come_from_the_session() {
        let session = cursor_session();
        let options = session["configOptions"].as_array().unwrap().clone();
        let (modes, current) = modes(&session["modes"], &options);
        assert_eq!(current.as_deref(), Some("agent"));
        assert_eq!(modes.len(), 2);
        assert_eq!(modes[1].description, None);
        let models = models(&session["models"], &options);
        assert_eq!(models.len(), 2);
        assert!(models[1].is_default && !models[0].is_default);
        assert_eq!(models[0].thinking.len(), 2);
        assert!(models[0].thinking[1].is_default);
    }

    #[test]
    fn config_options_stand_in_for_missing_fields() {
        let options = vec![
            json!({"id": "m", "category": "mode", "type": "select", "currentValue": "b",
                   "options": [{"group": "g", "options": [
                       {"value": "a", "name": "A"}, {"value": "b", "name": "B"}]}]}),
            json!({"id": "model", "category": "model", "type": "select", "currentValue": "x",
                   "options": [{"value": "x", "name": "X"}, {"value": "y", "name": "Y"}]}),
        ];
        let (modes, current) = modes(&Value::Null, &options);
        assert_eq!(
            modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(current.as_deref(), Some("b"));
        let models = models(&Value::Null, &options);
        assert!(models[0].is_default && models[0].thinking.is_empty());
    }

    #[test]
    fn the_cursor_catalog_carries_thinking_per_model() {
        let catalog = json!({"models": [
            {"value": "default", "name": "Auto", "configOptions": []},
            {"value": "claude-opus-5", "name": "Claude Opus 5", "configOptions": [
                {"id": "thinking", "category": "thought_level", "type": "select", "currentValue": "true",
                 "options": [{"value": "false", "name": "Off"}, {"value": "true", "name": "On"}]},
                {"id": "effort", "category": "thought_level", "type": "select", "currentValue": "high",
                 "options": [{"value": "high", "name": "High"}]},
            ]},
        ]});
        let models = cursor_models(&catalog, Some("claude-opus-5"));
        assert!(!models[0].is_default && models[1].is_default);
        assert!(models[0].thinking.is_empty());
        // The first thought_level select wins, as in Paseo.
        assert_eq!(models[1].thinking[1].id, "true");
        assert!(models[1].thinking[1].is_default);
    }
}
