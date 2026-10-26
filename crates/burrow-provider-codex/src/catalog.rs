//! Codex's modes and models, as Paseo 0.12 offers them
//! (`codex-app-server-agent.ts`, `@getpaseo/protocol` `provider-manifest.ts`).
//!
//! Models are not a fixed list: they come from the app-server's `model/list`,
//! with the defaults the user configured (`config/read`) applied on top.

use burrow_provider::{Mode, Model, ThinkingOption};
use serde_json::{Map, Value, json};

/// The mode a session without one runs in (`DEFAULT_CODEX_MODE_ID`).
pub const FALLBACK_MODE: &str = "auto";

/// Codex 0.115 is the first with the auto-reviewer, and Paseo then makes it
/// the default mode.
const AUTO_REVIEW_SINCE: [u32; 3] = [0, 115, 0];

pub fn auto_review_available(version: Option<&[u32]>) -> bool {
    version.is_some_and(|v| v >= AUTO_REVIEW_SINCE.as_slice())
}

pub fn modes(auto_review: bool) -> Vec<Mode> {
    let mode = |id: &str, label: &str, description: &str, icon: &str, tier: &str| Mode {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        icon: Some(icon.into()),
        color_tier: Some(tier.into()),
    };
    let mut modes = vec![
        mode(
            "auto",
            "Default Permissions",
            "Edit files and run commands with Codex's default approval flow.",
            "Shield",
            "moderate",
        ),
        mode(
            "auto-review",
            "Auto-review",
            "Same workspace-write permissions as Default, but eligible `on-request` approvals are routed through the auto-reviewer subagent.",
            "ShieldCheck",
            "moderate",
        ),
        mode(
            "full-access",
            "Full Access",
            "Edit files, run commands, and access the network without additional prompts.",
            "ShieldOff",
            "dangerous",
        ),
    ];
    if !auto_review {
        modes.retain(|m| m.id != "auto-review");
    }
    modes
}

pub fn default_mode(auto_review: bool) -> &'static str {
    if auto_review {
        "auto-review"
    } else {
        FALLBACK_MODE
    }
}

/// What a mode asks of Codex (`MODE_PRESETS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub approval_policy: &'static str,
    pub sandbox: &'static str,
    pub reviewer: &'static str,
}

/// The preset of `mode`; an unknown mode runs as `auto`.
pub fn preset(mode: &str) -> Preset {
    let (approval_policy, sandbox, reviewer) = match mode {
        "read-only" => ("on-request", "read-only", "user"),
        "auto-review" => ("on-request", "workspace-write", "auto_review"),
        "full-access" => ("never", "danger-full-access", "user"),
        _ => ("on-request", "workspace-write", "user"),
    };
    Preset {
        approval_policy,
        sandbox,
        reviewer,
    }
}

pub fn known_mode(mode: &str) -> bool {
    matches!(mode, "read-only" | "auto" | "auto-review" | "full-access")
}

/// The `sandboxPolicy` of a turn for a sandbox mode (`toSandboxPolicy`).
pub fn sandbox_policy(sandbox: &str) -> Value {
    match sandbox {
        "read-only" => json!({"type": "readOnly"}),
        "danger-full-access" => json!({"type": "dangerFullAccess"}),
        _ => json!({"type": "workspaceWrite", "networkAccess": false, "writableRoots": [],
                    "excludeSlashTmp": false, "excludeTmpdirEnvVar": false}),
    }
}

/// The app-server's name for a sandbox mode's policy type.
pub fn sandbox_policy_type(sandbox: &str) -> &'static str {
    match sandbox {
        "workspace-write" => "workspaceWrite",
        "read-only" => "readOnly",
        _ => "dangerFullAccess",
    }
}

/// `codex-cli 0.162.1` → `[0, 162, 1]`: the first `x.y.z` in the text.
pub fn parse_version(text: &str) -> Option<Vec<u32>> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter_map(|word| {
            let parts: Option<Vec<u32>> = word.split('.').map(|p| p.parse().ok()).collect();
            parts.filter(|p| p.len() >= 3).map(|p| p[..3].to_vec())
        })
        .next()
}

/// `normalizeCodexThinkingOptionId`: `default` and blanks mean "unset".
pub fn normalize_effort(effort: Option<&str>) -> Option<String> {
    effort
        .map(str::trim)
        .filter(|e| !e.is_empty() && *e != "default")
        .map(str::to_string)
}

/// What `model/list` and `config/read` said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    /// `model/list` `data`, as sent.
    pub models: Vec<Value>,
    /// `model` from the user's config.
    pub configured_model: Option<String>,
    /// `model_reasoning_effort` from the user's config.
    pub configured_effort: Option<String>,
}

/// `normalizeCodexModelLabel`: `gpt` as a word becomes `GPT`.
fn label(display_name: &str) -> String {
    let mut out = String::with_capacity(display_name.len());
    let chars: Vec<char> = display_name.chars().collect();
    // JavaScript's `\b`: ASCII letters, digits and `_` are word characters.
    let word = |c: Option<&char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_');
    let mut i = 0;
    while i < chars.len() {
        let candidate: String = chars[i..chars.len().min(i + 3)].iter().collect();
        if candidate.eq_ignore_ascii_case("gpt")
            && !word(i.checked_sub(1).and_then(|p| chars.get(p)))
            && !word(chars.get(i + 3))
        {
            out.push_str("GPT");
            i += 3;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

impl Catalog {
    /// From a `model/list` result and a `config/read` result.
    pub fn from_responses(model_list: &Value, config: &Value) -> Self {
        Self {
            models: model_list["data"].as_array().cloned().unwrap_or_default(),
            configured_model: config["config"]["model"]
                .as_str()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string),
            configured_effort: normalize_effort(
                config["config"]["model_reasoning_effort"].as_str(),
            ),
        }
    }

    fn configured_model_listed(&self) -> bool {
        self.configured_model
            .as_deref()
            .is_some_and(|id| self.models.iter().any(|m| m["id"] == id))
    }

    /// One `AgentModelDefinition` (`buildCodexModelDefinition`), without
    /// `provider`.
    pub fn definition(&self, model: &Value) -> Value {
        let configured = self.configured_effort.as_deref();
        let model_default = normalize_effort(model["defaultReasoningEffort"].as_str());
        let resolved = configured.map(str::to_string).or(model_default);
        let mut options: Vec<Value> = Vec::new();
        for entry in model["supportedReasoningEfforts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(id) = normalize_effort(entry["reasoningEffort"].as_str()) else {
                continue;
            };
            let mut option = json!({"id": id, "label": id});
            if let Some(description) = entry["description"]
                .as_str()
                .filter(|d| !d.trim().is_empty())
            {
                option["description"] = description.into();
            }
            match options.iter_mut().find(|o| o["id"] == id.as_str()) {
                Some(existing) => *existing = option,
                None => options.push(option),
            }
        }
        if let Some(resolved) = &resolved
            && !options.iter().any(|o| o["id"] == resolved.as_str())
        {
            let description = if configured == Some(resolved.as_str()) {
                "Configured default reasoning effort"
            } else {
                "Model default reasoning effort"
            };
            options.push(json!({"id": resolved, "label": resolved, "description": description}));
        }
        for option in &mut options {
            let default = resolved.as_deref() == option["id"].as_str();
            option["isDefault"] = default.into();
        }
        let default_option = resolved.clone().or_else(|| {
            options
                .iter()
                .find(|o| o["isDefault"] == true)
                .or(options.first())
                .and_then(|o| o["id"].as_str().map(str::to_string))
        });
        let id = model["id"].as_str().unwrap_or_default();
        let mut definition = json!({
            "id": id,
            "label": label(model["displayName"].as_str().unwrap_or_default()),
        });
        if let Some(description) = model["description"].as_str() {
            definition["description"] = description.into();
        }
        let is_default = if self.configured_model_listed() {
            Some(self.configured_model.as_deref() == Some(id))
        } else {
            model["isDefault"].as_bool()
        };
        if let Some(is_default) = is_default {
            definition["isDefault"] = is_default.into();
        }
        if !options.is_empty() {
            definition["thinkingOptions"] = options.into();
        }
        if let Some(default_option) = default_option {
            definition["defaultThinkingOptionId"] = default_option.into();
        }
        let mut metadata = Map::new();
        for key in [
            "model",
            "defaultReasoningEffort",
            "supportedReasoningEfforts",
        ] {
            if let Some(value) = model.get(key).filter(|v| !v.is_null()) {
                metadata.insert(key.into(), value.clone());
            }
        }
        definition["metadata"] = Value::Object(metadata);
        definition
    }

    /// The typed catalog for the providers snapshot.
    pub fn models(&self) -> Vec<Model> {
        self.models
            .iter()
            .filter(|m| m["id"].as_str().is_some_and(|id| !id.is_empty()))
            .map(|m| {
                let definition = self.definition(m);
                let thinking = definition["thinkingOptions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|o| ThinkingOption {
                        id: o["id"].as_str().unwrap_or_default().into(),
                        label: o["label"].as_str().unwrap_or_default().into(),
                        is_default: o["isDefault"] == true,
                    })
                    .collect();
                Model {
                    id: definition["id"].as_str().unwrap_or_default().into(),
                    label: definition["label"].as_str().unwrap_or_default().into(),
                    description: definition["description"].as_str().map(str::to_string),
                    is_default: definition["isDefault"] == true,
                    context_window_max_tokens: None,
                    thinking,
                }
            })
            .collect()
    }

    /// What the typed [`Model`] cannot say, for `Provider::model_fields`.
    pub fn fields(&self, model_id: &str) -> Map<String, Value> {
        let mut fields = Map::new();
        let Some(model) = self.models.iter().find(|m| m["id"] == model_id) else {
            return fields;
        };
        let definition = self.definition(model);
        for key in ["thinkingOptions", "defaultThinkingOptionId", "metadata"] {
            if let Some(value) = definition.get(key) {
                fields.insert(key.into(), value.clone());
            }
        }
        if definition.get("isDefault").is_none() {
            fields.insert("isDefault".into(), Value::Null);
        }
        // Paseo's Codex models carry no `isSelectable`.
        fields.insert("isSelectable".into(), Value::Null);
        fields
    }

    /// `resolveModelAndThinking`: the model and effort a session runs with
    /// when it names none (the configured defaults, then the listed default).
    pub fn resolve(
        &self,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> (Option<String>, Option<String>) {
        let mut model = model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        let mut effort = normalize_effort(effort);
        if model.is_none() {
            model = self.configured_model.clone();
        }
        if effort.is_none() {
            effort = self.configured_effort.clone();
        }
        if model.is_none() || effort.is_none() {
            let listed: Vec<&Value> = self
                .models
                .iter()
                .filter(|m| m["id"].as_str().is_some_and(|id| !id.is_empty()))
                .collect();
            let default = listed
                .iter()
                .find(|m| m["isDefault"] == true)
                .or(listed.first());
            let selected = model
                .as_deref()
                .and_then(|id| listed.iter().find(|m| m["id"] == id))
                .or(default);
            if let Some(selected) = selected {
                if model.is_none() {
                    model = selected["id"].as_str().map(str::to_string);
                }
                if effort.is_none() {
                    effort = normalize_effort(selected["defaultReasoningEffort"].as_str());
                }
            }
        }
        (model, effort)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::from_responses(
            &json!({"data": [
                {"id": "gpt-6.1-sol", "model": "gpt-6.1-sol", "displayName": "gpt-6.1-Sol",
                 "description": "Workhorse.", "isDefault": true, "defaultReasoningEffort": "low",
                 "supportedReasoningEfforts": [
                    {"reasoningEffort": "low", "description": "Fast"},
                    {"reasoningEffort": "high", "description": "Deep"}]},
                {"id": "gpt-6-luna", "model": "gpt-6-luna", "displayName": "GPT-6-Luna",
                 "isDefault": false, "defaultReasoningEffort": "medium",
                 "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": ""}]},
            ]}),
            &json!({"config": {"model": "gpt-6-luna", "model_reasoning_effort": "high"}}),
        )
    }

    #[test]
    fn the_configured_defaults_win() {
        let catalog = catalog();
        let sol = catalog.definition(&catalog.models[0]);
        assert_eq!(sol["label"], "GPT-6.1-Sol");
        assert_eq!(
            sol["isDefault"], false,
            "the configured model is the default"
        );
        assert_eq!(sol["defaultThinkingOptionId"], "high");
        assert_eq!(
            sol["thinkingOptions"],
            json!([{"id": "low", "label": "low", "description": "Fast", "isDefault": false},
                   {"id": "high", "label": "high", "description": "Deep", "isDefault": true}])
        );
        assert_eq!(sol["metadata"]["defaultReasoningEffort"], "low");
        let luna = catalog.definition(&catalog.models[1]);
        assert_eq!(luna["isDefault"], true);
        assert_eq!(
            luna["thinkingOptions"][1],
            json!({"id": "high", "label": "high",
                   "description": "Configured default reasoning effort", "isDefault": true})
        );
        assert!(luna["thinkingOptions"][0].get("description").is_none());
        let fields = catalog.fields("gpt-6-luna");
        assert_eq!(fields["isSelectable"], Value::Null);
        assert!(fields.contains_key("metadata"));
    }

    #[test]
    fn a_session_without_settings_runs_on_the_defaults() {
        let catalog = catalog();
        assert_eq!(
            catalog.resolve(None, Some("default")),
            (Some("gpt-6-luna".into()), Some("high".into()))
        );
        let bare = Catalog {
            configured_model: None,
            configured_effort: None,
            ..catalog
        };
        assert_eq!(
            bare.resolve(None, None),
            (Some("gpt-6.1-sol".into()), Some("low".into()))
        );
        assert_eq!(
            bare.resolve(Some("gpt-6-luna"), None),
            (Some("gpt-6-luna".into()), Some("medium".into()))
        );
    }

    #[test]
    fn versions_gate_auto_review() {
        let v = parse_version("codex-cli 0.162.1").unwrap();
        assert_eq!(v, [0, 162, 1]);
        assert!(auto_review_available(Some(&v)));
        assert!(!auto_review_available(
            parse_version("codex-cli 0.98.0").as_deref()
        ));
        assert_eq!(modes(false).len(), 2);
        assert_eq!(default_mode(true), "auto-review");
        assert_eq!(preset("full-access").sandbox, "danger-full-access");
        assert_eq!(preset("bogus"), preset("auto"));
    }
}
