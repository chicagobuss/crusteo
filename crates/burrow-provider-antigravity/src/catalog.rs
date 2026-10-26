//! What Antigravity offers: its one mode, the models `agy models` lists,
//! the supported CLI versions, and the texts Paseo 0.12's plugin shows.

use burrow_provider::{Mode, Model};
use serde_json::{Value, json};

pub const PROVIDER_ID: &str = "antigravity";
pub const MODE: &str = "full-access";
/// The first `agy` with the stream-json driver mode Paseo relies on.
pub const MIN_VERSION: [u32; 3] = [1, 1, 15];

const NOTICE_TITLE: &str = "Antigravity is running with full access";
const NOTICE_TEXT: &str = "Antigravity's CLI cannot ask for permission when another app drives it, so Paseo starts it with --dangerously-skip-permissions. Every tool call, including shell commands, runs without asking.";

/// The CLI has no stdio permission prompt, so the only mode skips them.
pub fn modes() -> Vec<Mode> {
    vec![Mode {
        id: MODE.into(),
        label: "Full access".into(),
        description: Some(
            "Antigravity cannot ask for permission when another app drives it. Paseo starts it with --dangerously-skip-permissions.".into(),
        ),
        icon: Some("ShieldOff".into()),
        color_tier: Some("dangerous".into()),
    }]
}

/// The warning Paseo shows each time a session opens.
pub fn full_access_notice() -> Value {
    notification(NOTICE_TITLE, NOTICE_TEXT)
}

/// A `session.notice` as Paseo's plugin bridge turns it into a timeline item.
pub fn notification(title: &str, description: &str) -> Value {
    json!({"type": "notification", "level": "warning", "message": format!("{title}\n{description}")})
}

/// `agy models`: one `id<TAB>label` per line. Effort is part of the id
/// (`-low`, `-high`), so models carry no thinking options.
pub fn parse_models(output: &str) -> Result<Vec<Model>, String> {
    let mut models: Vec<Model> = Vec::new();
    for line in output.trim().lines() {
        let line = line.trim_end_matches('\r');
        let mut fields = line.split('\t');
        let (Some(id), Some(label), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err("Invalid `agy models` output".into());
        };
        if id.is_empty() || label.is_empty() {
            return Err("Invalid `agy models` output".into());
        }
        if models.iter().any(|m| m.id == id) {
            return Err(format!("Duplicate Antigravity model: {id}"));
        }
        models.push(Model {
            id: id.into(),
            label: label.into(),
            ..Model::default()
        });
    }
    Ok(models)
}

/// `1.4.2`, and nothing else on the line, as Paseo requires.
pub fn parse_version(text: &str) -> Option<[u32; 3]> {
    let mut parts = text.trim().split('.');
    let mut version = [0; 3];
    for slot in &mut version {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(version)
}

pub fn supported(version: [u32; 3]) -> bool {
    version >= MIN_VERSION
}

/// Paseo's rewording of sign-in failures; other messages pass through.
pub fn diagnostic(message: &str) -> String {
    let lower = message.to_lowercase();
    let spaced = |a: &str, b: &str| {
        lower.match_indices(a).any(|(at, _)| {
            let rest = &lower[at + a.len()..];
            let mut chars = rest.chars();
            chars.next().is_some() && chars.as_str().starts_with(b)
        })
    };
    if lower.contains("auth") || spaced("sign", "in") || spaced("log", "in") {
        return "Antigravity authentication failed. Run `agy` and sign in, then retry.".into();
    }
    message.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_are_tab_separated() {
        let models =
            parse_models("gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\r\nm2\tM 2\n").unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gemini-3.8-flash-low");
        assert_eq!(models[0].label, "Gemini 3.8 Flash (Low)");
        assert!(!models[0].is_default && models[0].thinking.is_empty());
        assert!(parse_models("a\tb\tc").is_err());
        assert!(parse_models("just text").is_err());
        assert!(parse_models("a\tb\na\tc").is_err());
        assert_eq!(parse_models("").unwrap(), Vec::new());
    }

    #[test]
    fn versions_from_1_1_15() {
        assert_eq!(parse_version("1.4.2\n"), Some([1, 4, 2]));
        assert_eq!(parse_version("agy 1.4.2"), None);
        assert_eq!(parse_version("1.4"), None);
        assert!(supported([1, 1, 15]) && supported([1, 2, 0]) && supported([2, 0, 0]));
        assert!(!supported([1, 1, 14]));
    }

    #[test]
    fn sign_in_failures_are_reworded() {
        let reworded = "Antigravity authentication failed. Run `agy` and sign in, then retry.";
        assert_eq!(diagnostic("authentication failed or timed out"), reworded);
        assert_eq!(diagnostic("please log in first"), reworded);
        assert_eq!(diagnostic("Sign-in required"), reworded);
        assert_eq!(diagnostic("quota exceeded"), "quota exceeded");
    }
}
