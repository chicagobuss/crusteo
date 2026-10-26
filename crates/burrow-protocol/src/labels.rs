//! Workspace labels: Paseo's `workspace-labels.ts` (`@getpaseo/protocol`).
//!
//! A label has no id. It is known by its name with whitespace collapsed,
//! matched without case; the catalog keeps the name as it was first written.
//! Its colour is one of ten names, not a hex value.

use serde::{Deserialize, Serialize};

/// `WORKSPACE_LABEL_COLORS`, in Paseo's order.
pub const COLORS: &[&str] = &[
    "violet", "sky", "emerald", "orange", "pink", "indigo", "teal", "red", "amber", "blue",
];

/// `WorkspaceLabelDefinitionSchema`: a catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelDefinition {
    pub name: String,
    pub color: LabelColor,
}

/// `WorkspaceLabelColorSchema`: one of [`COLORS`]. Anything else does not
/// parse, so a request carrying it fails validation, as in Paseo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LabelColor(String);

impl LabelColor {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for LabelColor {
    type Error = String;

    fn try_from(color: String) -> Result<Self, String> {
        if COLORS.contains(&color.as_str()) {
            Ok(Self(color))
        } else {
            Err(format!("not a label colour: {color}"))
        }
    }
}

impl From<LabelColor> for String {
    fn from(color: LabelColor) -> String {
        color.0
    }
}

/// `normalizeWorkspaceLabelName`: whitespace runs become one space, and the
/// ends are trimmed.
pub fn normalize_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `workspaceLabelKey`: what two names must share to be the same label.
pub fn key(name: &str) -> String {
    normalize_name(name).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_on_collapsed_whitespace_and_any_case() {
        assert_eq!(normalize_name("  Ops \t  team "), "Ops team");
        assert_eq!(key(" Ops   Team"), key("ops team"));
        assert_eq!(normalize_name("   "), "");
    }

    #[test]
    fn only_the_ten_colours_parse() {
        let label: LabelDefinition =
            serde_json::from_str(r#"{"name": "infra", "color": "teal"}"#).unwrap();
        assert_eq!(label.color.as_str(), "teal");
        let hex = serde_json::json!({"name": "x", "color": "#ff0000"});
        assert!(serde_json::from_value::<LabelDefinition>(hex).is_err());
        assert_eq!(
            serde_json::to_value(&label).unwrap(),
            serde_json::json!({"name": "infra", "color": "teal"})
        );
    }
}
