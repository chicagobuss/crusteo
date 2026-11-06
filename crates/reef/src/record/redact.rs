//! Scrubbing before a record leaves Reef: an ordered chain of patterns and
//! the parts of a record left out. A record names the chain that scrubbed
//! it, so an old one can be told from a new one when the rules change.

use std::collections::BTreeSet;

use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::config::{Pattern, Redact, SinkRedact};

/// Bumped whenever a built-in pattern changes.
const BUILTIN_VERSION: &str = "builtin/1";

/// The hook's (`fleet-emit.sh`) patterns, in its order: keys and tokens,
/// PEM blocks, credentials in URLs, email addresses. Work agents can hold
/// production credentials and customer data, and a store that keeps every
/// blob it is sent keeps those for good.
const BUILTIN: &[(&str, &str)] = &[
    (
        r"(?s)-----BEGIN[^-]{0,40}-----.*?-----END[^-]{0,40}-----",
        "[PEM]",
    ),
    (
        r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]+",
        "[JWT]",
    ),
    (r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", "[AWS-KEY]"),
    (r"\bsk-[A-Za-z0-9_-]{16,}", "[API-KEY]"),
    (
        r"\b(?:ghp|gho|ghu|ghs|ghr|github_pat)_[A-Za-z0-9_]{16,}",
        "[GITHUB-TOKEN]",
    ),
    (r"\bxox[abposr]-[A-Za-z0-9-]{10,}", "[SLACK-TOKEN]"),
    (r"\bAIza[0-9A-Za-z_-]{30,}", "[GOOGLE-KEY]"),
    (
        r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{16,}",
        "Bearer [REDACTED]",
    ),
    (
        r"(?i)\b([A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|APIKEY|API_KEY|PRIVATE_KEY)[A-Z0-9_]*)\s*[=:]\s*\S+",
        "${1}=[REDACTED]",
    ),
    (r"\b[a-z]{2,10}://[^\s/@]+:[^\s/@]+@", "[URL-CREDS]@"),
    (r"\b[\w.+-]+@[\w-]+\.[\w.-]+\b", "[EMAIL]"),
];

/// What a record may drop.
const PARTS: &[&str] = &["thought", "tool_input", "tool_output"];

/// The fields of a record that are scrubbed: what agents and people wrote.
const CONTENT: &[&str] = &["text", "prompt", "reply", "tool", "todo"];

#[derive(Debug, Clone)]
pub struct Chain {
    stages: Vec<(Regex, String)>,
    drop: BTreeSet<String>,
    /// Which rules scrubbed a record: `builtin/1`, plus a short hash of
    /// the custom rules when there are any.
    pub version: String,
}

impl Chain {
    pub fn new(config: &Redact) -> Result<Self, String> {
        let mut chain = Self {
            stages: Vec::new(),
            drop: BTreeSet::new(),
            version: if config.builtin {
                BUILTIN_VERSION
            } else {
                "none"
            }
            .into(),
        };
        if config.builtin {
            for (regex, replace) in BUILTIN {
                let regex = Regex::new(regex).expect("a built-in pattern compiles");
                chain.stages.push((regex, (*replace).into()));
            }
        }
        chain.add(&config.patterns, &config.drop)?;
        Ok(chain)
    }

    /// This chain followed by a sink's own stages.
    pub fn then(&self, sink: &SinkRedact) -> Result<Self, String> {
        let mut chain = self.clone();
        chain.add(&sink.patterns, &sink.drop)?;
        Ok(chain)
    }

    fn add(&mut self, patterns: &[Pattern], drop: &[String]) -> Result<(), String> {
        for part in drop {
            if !PARTS.contains(&part.as_str()) {
                return Err(format!("drop {part:?}: one of {}", PARTS.join(", ")));
            }
            self.drop.insert(part.clone());
        }
        for pattern in patterns {
            let regex = Regex::new(&pattern.regex)
                .map_err(|err| format!("pattern {:?}: {err}", pattern.name))?;
            self.stages.push((regex, pattern.replace.clone()));
        }
        // Names the rules, not their order of mention in some file: two
        // files with the same rules name the same chain.
        let custom: Vec<(&str, &str)> = (patterns.iter())
            .map(|p| (p.regex.as_str(), p.replace.as_str()))
            .collect();
        if !custom.is_empty() || !drop.is_empty() {
            let rules = serde_json::to_vec(&(custom, drop)).expect("rules serialize");
            let hash = Sha256::digest(rules);
            let short: String = hash[..4].iter().map(|b| format!("{b:02x}")).collect();
            self.version = format!("{}+{short}", self.version);
        }
        Ok(())
    }

    pub fn text(&self, text: &str) -> String {
        let mut text = std::borrow::Cow::Borrowed(text);
        for (regex, replace) in &self.stages {
            if let std::borrow::Cow::Owned(changed) = regex.replace_all(&text, replace.as_str()) {
                text = std::borrow::Cow::Owned(changed);
            }
        }
        text.into_owned()
    }

    /// Scrubs a record in place; false when the record is dropped whole.
    pub fn apply(&self, record: &mut Value) -> bool {
        if record["kind"] == "thought" && self.drop.contains("thought") {
            return false;
        }
        if let Some(tool) = record.get_mut("tool").and_then(Value::as_object_mut) {
            if self.drop.contains("tool_input") {
                tool.remove("input");
            }
            if self.drop.contains("tool_output") {
                tool.remove("output");
            }
        }
        // What agents and people wrote, not Reef's own fields: `reef@0.1.0`
        // in `source` looks like an email address.
        for field in CONTENT {
            if let Some(value) = record.get_mut(*field) {
                self.scrub(value);
            }
        }
        record["redaction"] = Value::String(self.version.clone());
        true
    }

    fn scrub(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.text(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.scrub(item)),
            Value::Object(fields) => fields.values_mut().for_each(|field| self.scrub(field)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn builtin() -> Chain {
        Chain::new(&Redact::default()).unwrap()
    }

    #[test]
    fn the_hooks_patterns_scrub_what_the_hook_scrubs() {
        let chain = builtin();
        let cases = [
            ("key AKIAABCDEFGHIJKLMNOP here", "key [AWS-KEY] here"),
            (
                "export API_TOKEN=abc123 next",
                "export API_TOKEN=[REDACTED] next",
            ),
            (
                "Authorization: Bearer abcdefghijklmnopqrstu",
                "Authorization: Bearer [REDACTED]",
            ),
            (
                "git clone https://me:hunter2@git.test/x",
                "git clone [URL-CREDS]@git.test/x",
            ),
            ("mail dev@example.test now", "mail [EMAIL] now"),
            ("tok ghp_abcdefghijklmnopqrst1234", "tok [GITHUB-TOKEN]"),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIB\n-----END RSA PRIVATE KEY----- after",
                "[PEM] after",
            ),
        ];
        for (text, want) in cases {
            assert_eq!(chain.text(text), want, "{text}");
        }
        // The default drops tool output, which the name says too.
        assert!(chain.version.starts_with("builtin/1+"));
        let bare = Chain::new(&Redact {
            drop: vec![],
            ..Redact::default()
        })
        .unwrap();
        assert_eq!(bare.version, "builtin/1");
    }

    #[test]
    fn custom_patterns_and_drops_follow_the_builtins_and_name_the_chain() {
        let config = Redact {
            patterns: vec![Pattern {
                name: "customer".into(),
                regex: r"CUS-\d+".into(),
                replace: "[CUSTOMER]".into(),
            }],
            drop: vec!["tool_input".into(), "tool_output".into()],
            ..Redact::default()
        };
        let chain = Chain::new(&config).unwrap();
        assert!(chain.version.starts_with("builtin/1+") && chain.version.len() == 18);
        let mut record = json!({ "kind": "tool_call", "text": "CUS-42 for dev@example.test",
            "source": "reef@0.1.0-beta.4",
            "tool": { "name": "Bash", "input": { "command": "ls" }, "output": "x" } });
        assert!(chain.apply(&mut record));
        assert_eq!(record["text"], "[CUSTOMER] for [EMAIL]");
        // Reef's own fields are left as they are.
        assert_eq!(record["source"], "reef@0.1.0-beta.4");
        assert_eq!(record["tool"], json!({ "name": "Bash" }));
        assert_eq!(record["redaction"], json!(chain.version));
        // A sink's stages come after, and a thought it drops goes whole.
        let sink = chain.then(&SinkRedact {
            patterns: vec![],
            drop: vec!["thought".into()],
        });
        let mut thought = json!({ "kind": "thought", "text": "hm" });
        assert!(!sink.unwrap().apply(&mut thought));
    }

    #[test]
    fn a_bad_rule_is_refused_by_name() {
        let bad = Redact {
            patterns: vec![Pattern {
                name: "oops".into(),
                regex: "(".into(),
                replace: "".into(),
            }],
            ..Redact::default()
        };
        assert!(Chain::new(&bad).unwrap_err().contains("oops"));
        let part = Redact {
            drop: vec!["images".into()],
            ..Redact::default()
        };
        assert!(Chain::new(&part).unwrap_err().contains("images"));
    }
}
