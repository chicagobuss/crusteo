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
        // The searchable text is made from what the drops left, so a part
        // dropped is in no field and not in `body` either, and then scrubbed
        // whole, so a pattern that spans two of its parts is caught too.
        let body = self.text(&body(record));
        // What agents and people wrote, not Reef's own fields: `reef@0.1.0`
        // in `source` looks like an email address.
        for field in CONTENT {
            if let Some(value) = record.get_mut(*field) {
                self.scrub(value);
            }
        }
        record["body"] = Value::String(body);
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

/// A record's searchable text, one string whatever its kind (the table in
/// `docs/reef/record.md`): its words, and none of its structure.
fn body(record: &Value) -> String {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let (parts, gap): (Vec<String>, &str) = match record["kind"].as_str().unwrap_or_default() {
        "prompt" | "reply" | "thought" | "error" | "notice" => (vec![text(&record["text"])], ""),
        "tool_call" => {
            let tool = &record["tool"];
            let parts = vec![
                text(&tool["name"]),
                plain(&tool["input"]),
                plain(&tool["output"]),
            ];
            (parts, "\n")
        }
        "todo" => {
            let items = record["todo"].as_array().into_iter().flatten();
            let lines = items.map(|item| match item["text"].as_str() {
                Some(text) => text.to_string(),
                None => plain(item),
            });
            (lines.collect(), "\n")
        }
        "turn_finished" => (
            vec![text(&record["prompt"]), text(&record["reply"])],
            "\n\n",
        ),
        // Nothing to search: a restart marker, a deletion, a compaction.
        _ => (Vec::new(), ""),
    };
    let parts: Vec<&str> = (parts.iter())
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect();
    parts.join(gap)
}

/// A tool's input or output as text: a string as it is, a structure as the
/// strings in it, one a line (its keys are structure, not words).
fn plain(value: &Value) -> String {
    fn walk(value: &Value, lines: &mut Vec<String>) {
        match value {
            Value::String(text) => lines.push(text.clone()),
            Value::Array(items) => items.iter().for_each(|item| walk(item, lines)),
            Value::Object(fields) => fields.values().for_each(|field| walk(field, lines)),
            _ => {}
        }
    }
    match value {
        Value::Null => String::new(),
        Value::Bool(_) | Value::Number(_) => value.to_string(),
        _ => {
            let mut lines = Vec::new();
            walk(value, &mut lines);
            lines.join("\n")
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

    /// A record's `body` after the chain, as a sink sees it.
    fn body_of(chain: &Chain, mut record: Value) -> Option<String> {
        chain
            .apply(&mut record)
            .then(|| record["body"].as_str().expect("a string").to_string())
    }

    fn keeping_everything() -> Chain {
        Chain::new(&Redact {
            drop: vec![],
            ..Redact::default()
        })
        .unwrap()
    }

    #[test]
    fn every_kind_has_a_body_of_its_searchable_text() {
        let chain = keeping_everything();
        let text = |kind: &str| json!({ "kind": kind, "text": "  the words\n" });
        for kind in ["prompt", "reply", "thought", "error", "notice"] {
            assert_eq!(body_of(&chain, text(kind)).as_deref(), Some("the words"));
        }
        // The name, then the input as its strings, then the output.
        let call = json!({ "kind": "tool_call", "tool": { "name": "Bash", "status": "completed",
            "input": { "command": "cargo build", "timeout": 120, "flags": ["--release"] },
            "output": "Finished in 3s" } });
        assert_eq!(
            body_of(&chain, call).as_deref(),
            Some("Bash\ncargo build\n--release\nFinished in 3s")
        );
        let read = json!({ "kind": "tool_call", "tool": { "name": "Read",
            "input": "src/main.rs", "output": { "content": "fn main() {}" } } });
        assert_eq!(
            body_of(&chain, read).as_deref(),
            Some("Read\nsrc/main.rs\nfn main() {}")
        );
        let todo = json!({ "kind": "todo", "todo": [
            { "text": "Write the tests", "completed": true }, "A bare item",
            { "text": "Ship it", "completed": false } ] });
        assert_eq!(
            body_of(&chain, todo).as_deref(),
            Some("Write the tests\nA bare item\nShip it")
        );
        // The prompt, then the reply; either may be missing.
        let turn = json!({ "kind": "turn_finished", "prompt": "Fix the build", "reply": "Done." });
        assert_eq!(
            body_of(&chain, turn).as_deref(),
            Some("Fix the build\n\nDone.")
        );
        let unanswered = json!({ "kind": "turn_finished", "prompt": "Fix it", "reply": null });
        assert_eq!(body_of(&chain, unanswered).as_deref(), Some("Fix it"));
        // Nothing to search, but the field is there.
        for kind in ["compaction", "gap", "forget"] {
            let record = json!({ "kind": kind, "text": "not searched" });
            assert_eq!(body_of(&chain, record).as_deref(), Some(""));
        }
    }

    #[test]
    fn a_body_is_scrubbed_and_leaves_out_what_is_dropped() {
        let secret = "AKIAABCDEFGHIJKLMNOP";
        let call = |input: &str, output: &str| {
            json!({ "kind": "tool_call", "tool": { "name": "Bash",
                "input": { "command": input }, "output": output } })
        };
        let kept = keeping_everything();
        let body = body_of(
            &kept,
            call(&format!("echo {secret}"), "mail dev@example.test"),
        );
        assert_eq!(
            body.as_deref(),
            Some("Bash\necho [AWS-KEY]\nmail [EMAIL]"),
            "the same patterns as the fields"
        );
        // The default drops tool output: no output in the field, none in the body.
        let default = builtin();
        let mut record = call("ls", "SECRET_STUFF");
        assert!(default.apply(&mut record));
        assert!(record["tool"].get("output").is_none());
        assert_eq!(record["body"], "Bash\nls");
        // Dropped input likewise, and a custom pattern reaches the body too.
        let chain = Chain::new(&Redact {
            patterns: vec![Pattern {
                name: "customer".into(),
                regex: r"CUS-\d+".into(),
                replace: "[CUSTOMER]".into(),
            }],
            drop: vec!["tool_input".into()],
            ..Redact::default()
        })
        .unwrap();
        let mut record = call("curl CUS-42", "CUS-7 paid");
        assert!(chain.apply(&mut record));
        assert_eq!(record["body"], "Bash\n[CUSTOMER] paid");
        // A sink's own drops count too, and a dropped thought has no record.
        let sink = default
            .then(&SinkRedact {
                patterns: vec![],
                drop: vec!["tool_input".into()],
            })
            .unwrap();
        let mut record = call("ls /secret", "x");
        assert!(sink.apply(&mut record));
        assert_eq!(record["body"], "Bash");
        let thoughtless = default
            .then(&SinkRedact {
                patterns: vec![],
                drop: vec!["thought".into()],
            })
            .unwrap();
        let mut thought = json!({ "kind": "thought", "text": secret });
        assert!(!thoughtless.apply(&mut thought));
        // Scrubbed as one text, so a block is caught across its lines.
        let mut turn = json!({ "kind": "turn_finished", "prompt": "key:", "reply":
            "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----" });
        assert!(default.apply(&mut turn));
        assert_eq!(turn["body"], "key:\n\n[PEM]");
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
