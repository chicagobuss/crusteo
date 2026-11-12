//! Claude's modes and models, as Paseo 0.11/0.12 offers them
//! (`providers/claude/agent.ts`, `model-manifest.ts`).

use burrow_provider::{Mode, Model, ThinkingOption};

pub fn modes() -> Vec<Mode> {
    let mode = |id: &str, label: &str, description: &str, icon: &str, tier: &str| Mode {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        icon: Some(icon.into()),
        color_tier: Some(tier.into()),
    };
    vec![
        mode(
            "plan",
            "Plan Mode",
            "Analyze the codebase without executing tools or edits",
            "ShieldEllipsis",
            "planning",
        ),
        mode(
            "default",
            "Always Ask",
            "Prompts for permission the first time a tool is used",
            "Shield",
            "safe",
        ),
        mode(
            "acceptEdits",
            "Accept File Edits",
            "Automatically approves edit-focused tools without prompting",
            "ShieldPlus",
            "moderate",
        ),
        mode(
            "auto",
            "Auto mode",
            "Uses a model classifier to review permission prompts automatically",
            "ShieldCheck",
            "moderate",
        ),
        mode(
            "bypassPermissions",
            "Bypass",
            "Skip all permission prompts (use with caution)",
            "ShieldOff",
            "dangerous",
        ),
    ]
}

/// Paseo's catalog default.
pub const DEFAULT_MODE: &str = "auto";

const STANDARD: &[&str] = &["low", "medium", "high", "max"];
const XHIGH: &[&str] = &["low", "medium", "high", "xhigh", "max"];
pub const DEFAULT_THINKING: &str = "high";
pub const THINKING_OFF: &str = "off";
pub const ULTRACODE: &str = "ultracode";

struct Entry {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    priority: u8,
    minimum: Option<&'static str>,
    default_thinking: Option<&'static str>,
    context: u64,
    effort: Option<&'static [&'static str]>,
    thinking_off: bool,
}

const fn entry(id: &'static str, label: &'static str, description: &'static str) -> Entry {
    Entry {
        id,
        label,
        description,
        priority: 0,
        minimum: None,
        default_thinking: None,
        context: 200_000,
        effort: None,
        thinking_off: false,
    }
}

/// `CLAUDE_MODEL_MANIFEST`, without the models that need account discovery.
const MANIFEST: &[Entry] = &[
    Entry {
        priority: 3,
        minimum: Some("2.1.280"),
        default_thinking: Some("medium"),
        context: 1_000_000,
        effort: Some(XHIGH),
        ..entry("claude-opus-5-5", "Opus 5.5", "Opus 5.5 · Latest release")
    },
    Entry {
        priority: 2,
        minimum: Some("2.1.219"),
        context: 1_000_000,
        effort: Some(XHIGH),
        thinking_off: true,
        ..entry("claude-opus-5", "Opus 5", "Opus 5 · Previous release")
    },
    Entry {
        context: 1_000_000,
        effort: Some(XHIGH),
        ..entry(
            "claude-fable-5-1",
            "Fable 5.1",
            "Fable 5.1 · Most powerful model",
        )
    },
    Entry {
        minimum: Some("2.1.169"),
        context: 1_000_000,
        effort: Some(XHIGH),
        ..entry("claude-fable-5", "Fable 5", "Fable 5 · Previous release")
    },
    Entry {
        context: 1_000_000,
        effort: Some(XHIGH),
        thinking_off: true,
        ..entry(
            "claude-opus-4-8[1m]",
            "Opus 4.8 1M",
            "Opus 4.8 with 1M context window",
        )
    },
    Entry {
        priority: 1,
        effort: Some(XHIGH),
        thinking_off: true,
        ..entry("claude-opus-4-8", "Opus 4.8", "Opus 4.8 · Previous release")
    },
    Entry {
        minimum: Some("2.1.284"),
        default_thinking: Some("medium"),
        context: 1_000_000,
        effort: Some(XHIGH),
        ..entry(
            "claude-sonnet-5-5",
            "Sonnet 5.5",
            "Sonnet 5.5 · Best for everyday tasks",
        )
    },
    Entry {
        effort: Some(XHIGH),
        thinking_off: true,
        ..entry("claude-sonnet-5", "Sonnet 5", "Sonnet 5 · Previous release")
    },
    Entry {
        context: 1_000_000,
        effort: Some(XHIGH),
        thinking_off: true,
        ..entry(
            "claude-sonnet-5[1m]",
            "Sonnet 5 1M",
            "Sonnet 5 with 1M context window",
        )
    },
    Entry {
        context: 1_000_000,
        effort: Some(XHIGH),
        thinking_off: true,
        ..entry(
            "claude-opus-4-7[1m]",
            "Opus 4.7 1M",
            "Opus 4.7 with 1M context window",
        )
    },
    Entry {
        effort: Some(XHIGH),
        thinking_off: true,
        ..entry("claude-opus-4-7", "Opus 4.7", "Opus 4.7 · Previous release")
    },
    Entry {
        context: 1_000_000,
        effort: Some(STANDARD),
        thinking_off: true,
        ..entry(
            "claude-opus-4-6[1m]",
            "Opus 4.6 1M",
            "Opus 4.6 with 1M context window",
        )
    },
    Entry {
        effort: Some(STANDARD),
        thinking_off: true,
        ..entry(
            "claude-opus-4-6",
            "Opus 4.6",
            "Opus 4.6 · Most capable for complex work",
        )
    },
    Entry {
        context: 1_000_000,
        effort: Some(STANDARD),
        thinking_off: true,
        ..entry(
            "claude-sonnet-4-6[1m]",
            "Sonnet 4.6 1M",
            "Sonnet 4.6 with 1M context window",
        )
    },
    Entry {
        effort: Some(STANDARD),
        thinking_off: true,
        ..entry(
            "claude-sonnet-4-6",
            "Sonnet 4.6",
            "Sonnet 4.6 · Best for everyday tasks",
        )
    },
    Entry {
        minimum: Some("2.1.293"),
        default_thinking: Some("medium"),
        context: 1_000_000,
        effort: Some(XHIGH),
        thinking_off: true,
        ..entry(
            "claude-haiku-5-5",
            "Haiku 5.5",
            "Haiku 5.5 · Fastest for quick answers",
        )
    },
    entry(
        "claude-haiku-4-5",
        "Haiku 4.5",
        "Haiku 4.5 · Previous release",
    ),
];

/// `2.1.303 (Claude Code)` → `[2, 1, 303]`.
pub fn parse_version(text: &str) -> Option<Vec<u32>> {
    let word = text.split_whitespace().next()?;
    let parts: Option<Vec<u32>> = word.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| p.len() == 3)
}

fn at_least(version: Option<&[u32]>, minimum: Option<&str>) -> bool {
    match (version, minimum.and_then(parse_version)) {
        (Some(version), Some(minimum)) => version >= minimum.as_slice(),
        _ => true,
    }
}

fn label(effort: &str) -> &'static str {
    match effort {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        _ => "Max",
    }
}

/// The models a Claude Code of this version offers; the default is the
/// highest-priority one available.
pub fn models(version: Option<&[u32]>) -> Vec<Model> {
    let available: Vec<&Entry> = MANIFEST
        .iter()
        .filter(|e| at_least(version, e.minimum))
        .collect();
    let default = available.iter().max_by_key(|e| e.priority).map(|e| e.id);
    available
        .into_iter()
        .map(|e| {
            let default_thinking = e.default_thinking.unwrap_or(DEFAULT_THINKING);
            let mut thinking = Vec::new();
            if let Some(levels) = e.effort {
                if e.thinking_off {
                    thinking.push(ThinkingOption {
                        id: THINKING_OFF.into(),
                        label: "Off".into(),
                        is_default: false,
                    });
                }
                for level in levels {
                    thinking.push(ThinkingOption {
                        id: (*level).into(),
                        label: label(level).into(),
                        is_default: *level == default_thinking,
                    });
                }
                if levels.contains(&"xhigh") {
                    thinking.push(ThinkingOption {
                        id: ULTRACODE.into(),
                        label: "Ultra Code".into(),
                        is_default: false,
                    });
                }
            }
            Model {
                id: e.id.into(),
                label: e.label.into(),
                description: Some(e.description.into()),
                is_default: Some(e.id) == default,
                context_window_max_tokens: Some(e.context),
                thinking,
            }
        })
        .collect()
}

/// A context window for a model id the CLI offered: the manifest's when it
/// knows the model (a dated id such as `claude-haiku-4-5-20251001` counts
/// as its undated name), one million for a `[1m]` id, else 200k.
fn probed_context(id: &str) -> u64 {
    let undated = id
        .rsplit_once('-')
        .filter(|(_, tail)| tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()))
        .map_or(id, |(head, _)| head);
    context_window(Some(id))
        .or_else(|| context_window(Some(undated)))
        .unwrap_or(200_000)
}

/// The models Claude Code reports in its `initialize` reply (`models`).
///
/// An alias entry (`opus`, `fable`, `default`, ...) is offered under the
/// model it resolves to, so a model keeps one stable id however many names
/// the CLI gives it, and an entry the CLI lists under its full id wins over
/// an alias of the same model. The model the `default` alias resolves to is
/// the default. `None` when the reply lists no models (an old CLI).
pub fn from_initialize(reply: &serde_json::Value) -> Option<Vec<Model>> {
    let entries = reply["models"].as_array()?;
    let resolved = |e: &serde_json::Value| -> Option<String> {
        let id = e["resolvedModel"]
            .as_str()
            .or_else(|| e["value"].as_str())?;
        Some(id.to_string())
    };
    let mut ids: Vec<String> = Vec::new();
    let mut picked: Vec<&serde_json::Value> = Vec::new();
    let mut default = None;
    for entry in entries {
        let Some(id) = resolved(entry) else { continue };
        if entry["value"] == "default" {
            default = Some(id);
            continue;
        }
        match ids.iter().position(|seen| *seen == id) {
            Some(at) => {
                if entry["value"].as_str() == Some(id.as_str()) {
                    picked[at] = entry;
                }
            }
            None => {
                ids.push(id);
                picked.push(entry);
            }
        }
    }
    // A default that no other entry names is still offered.
    if let Some(id) = default.as_ref().filter(|id| !ids.contains(id))
        && let Some(entry) = entries.iter().find(|e| e["value"] == "default")
    {
        ids.insert(0, id.clone());
        picked.insert(0, entry);
    }
    if ids.is_empty() {
        return None;
    }
    let default = default.unwrap_or_else(|| ids[0].clone());
    let known = |id: &str| MANIFEST.iter().find(|e| e.id == id);
    Some(
        ids.iter()
            .zip(picked)
            .map(|(id, entry)| {
                let levels: Vec<&str> = entry["supportedEffortLevels"]
                    .as_array()
                    .map(|l| l.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let default_thinking = known(id)
                    .and_then(|e| e.default_thinking)
                    .filter(|d| levels.contains(d))
                    .or_else(|| {
                        levels
                            .contains(&DEFAULT_THINKING)
                            .then_some(DEFAULT_THINKING)
                    })
                    .or_else(|| levels.first().copied());
                let mut thinking = Vec::new();
                if entry["supportsEffort"].as_bool() != Some(false) && !levels.is_empty() {
                    if entry["supportsThinkingOff"].as_bool() == Some(true) {
                        thinking.push(ThinkingOption {
                            id: THINKING_OFF.into(),
                            label: "Off".into(),
                            is_default: false,
                        });
                    }
                    for level in &levels {
                        thinking.push(ThinkingOption {
                            id: (*level).into(),
                            label: label(level).into(),
                            is_default: Some(*level) == default_thinking,
                        });
                    }
                    if levels.contains(&"xhigh") {
                        thinking.push(ThinkingOption {
                            id: ULTRACODE.into(),
                            label: "Ultra Code".into(),
                            is_default: false,
                        });
                    }
                }
                Model {
                    id: id.clone(),
                    label: entry["displayName"].as_str().unwrap_or(id).into(),
                    description: entry["description"].as_str().map(Into::into),
                    is_default: *id == default,
                    context_window_max_tokens: Some(probed_context(id)),
                    thinking,
                }
            })
            .collect(),
    )
}

pub fn context_window(model: Option<&str>) -> Option<u64> {
    let model = model?;
    if model.ends_with("[1m]") {
        return Some(1_000_000);
    }
    MANIFEST.iter().find(|e| e.id == model).map(|e| e.context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_gate_models_and_pick_a_default() {
        let old = models(parse_version("2.1.200 (Claude Code)").as_deref());
        assert!(!old.iter().any(|m| m.id == "claude-opus-5-5"));
        assert!(old.iter().any(|m| m.id == "claude-haiku-4-5"));
        let new = models(parse_version("2.1.303 (Claude Code)").as_deref());
        let default = new.iter().find(|m| m.is_default).unwrap();
        assert_eq!(default.id, "claude-opus-5-5");
        let haiku = new.iter().find(|m| m.id == "claude-haiku-4-5").unwrap();
        assert!(
            haiku.thinking.is_empty(),
            "no thinking control on Haiku 4.5"
        );
        let opus = new.iter().find(|m| m.id == "claude-opus-5").unwrap();
        assert_eq!(opus.thinking[0].id, "off");
        assert!(opus.thinking.iter().any(|t| t.id == "ultracode"));
        assert!(opus.thinking.iter().any(|t| t.id == "high" && t.is_default));
    }

    /// A real `initialize` reply of Claude Code 2.1.317 (account removed).
    fn reply() -> serde_json::Value {
        let frame: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/initialize.json")).unwrap();
        frame["response"]["response"].clone()
    }

    #[test]
    fn the_cli_reported_models_are_offered_by_resolved_id() {
        let models = from_initialize(&reply()).unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        // Aliases collapse into the model they resolve to, once each.
        assert_eq!(
            ids[..3],
            ["claude-opus-5-5", "claude-fable-5-2", "claude-sonnet-5-5"]
        );
        assert!(ids.contains(&"claude-fable-5-1"), "the `fable` alias");
        assert!(
            !ids.iter()
                .any(|id| ["default", "opus", "fable", "sonnet", "haiku"].contains(id))
        );
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
        // The `default` alias picks the default.
        let defaults: Vec<&str> = (models.iter().filter(|m| m.is_default))
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(defaults, ["claude-opus-5-5"]);
        // A model the manifest has never heard of still gets a window.
        let fable = models.iter().find(|m| m.id == "claude-fable-5-2").unwrap();
        assert_eq!(fable.label, "Fable 5.2");
        assert_eq!(fable.context_window_max_tokens, Some(200_000));
        assert!(fable.thinking.iter().any(|t| t.id == "ultracode"));
        assert!(!fable.thinking.iter().any(|t| t.id == "off"));
        // One the manifest knows keeps its window and its default effort.
        let opus = models.iter().find(|m| m.id == "claude-opus-5-5").unwrap();
        assert_eq!(opus.context_window_max_tokens, Some(1_000_000));
        assert!(
            opus.thinking
                .iter()
                .any(|t| t.id == "medium" && t.is_default)
        );
        // supportsThinkingOff and a missing effort list.
        let sonnet = models.iter().find(|m| m.id == "claude-sonnet-5").unwrap();
        assert_eq!(sonnet.thinking[0].id, "off");
        let haiku = models
            .iter()
            .find(|m| m.id == "claude-haiku-4-5-20251001")
            .unwrap();
        assert!(haiku.thinking.is_empty());
        assert_eq!(haiku.context_window_max_tokens, Some(200_000));
        let opus46 = models.iter().find(|m| m.id == "claude-opus-4-6").unwrap();
        assert!(
            !opus46
                .thinking
                .iter()
                .any(|t| t.id == "xhigh" || t.id == "ultracode")
        );
    }

    #[test]
    fn a_reply_without_models_is_no_catalog() {
        assert!(from_initialize(&json!({"claude_code_version": "2.1.1"})).is_none());
        assert!(from_initialize(&json!({"models": []})).is_none());
    }

    #[test]
    fn a_full_id_entry_wins_over_an_alias_of_the_same_model() {
        let models = from_initialize(&json!({"models": [
            {"value": "default", "resolvedModel": "claude-x-1", "displayName": "Default"},
            {"value": "x", "resolvedModel": "claude-x-1", "displayName": "X alias"},
            {"value": "claude-x-1", "resolvedModel": "claude-x-1", "displayName": "X 1"},
        ]}))
        .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].label, "X 1");
        assert!(models[0].is_default);
    }
}
