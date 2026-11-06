//! `record.json` in Reef's data dir: whether Reef follows every agent, how
//! records are scrubbed, and where they go. Read again whenever it changes.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

pub const FILE: &str = "record.json";

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Follow every agent on every host, not only the ones phones have
    /// open. Recording needs it; off, the sinks hear nothing.
    #[serde(default)]
    pub follow_all: bool,
    /// The chain every record goes through before any sink.
    #[serde(default)]
    pub redact: Redact,
    #[serde(default)]
    pub sinks: Vec<SinkConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Redact {
    /// The built-in patterns: keys, tokens, PEM blocks, credentials in URLs,
    /// email addresses.
    #[serde(default = "yes")]
    pub builtin: bool,
    #[serde(default)]
    pub patterns: Vec<Pattern>,
    /// Parts of a record left out: `thought`, `tool_input`, `tool_output`.
    #[serde(default = "default_drop")]
    pub drop: Vec<String>,
}

impl Default for Redact {
    fn default() -> Self {
        Self {
            builtin: true,
            patterns: Vec::new(),
            drop: default_drop(),
        }
    }
}

/// A sink's own stages, after the shared chain.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SinkRedact {
    #[serde(default)]
    pub patterns: Vec<Pattern>,
    #[serde(default)]
    pub drop: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pattern {
    pub name: String,
    pub regex: String,
    #[serde(default = "redacted")]
    pub replace: String,
}

// No deny_unknown_fields: serde can't have it with a flattened field. The
// kind's own fields are checked there.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SinkConfig {
    /// Names the sink's cursors, kept in `record/<name>.json`.
    pub name: String,
    #[serde(flatten)]
    pub kind: SinkKind,
    /// Only these kinds of record (`prompt`, `reply`, `turn_finished`, …);
    /// empty is all.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Only agents on these hosts; empty is every host.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Only agents whose workspace has one of these labels; empty is all.
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub redact: SinkRedact,
    /// The most records in one batch.
    #[serde(default = "default_batch")]
    pub batch: usize,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum SinkKind {
    /// One JSON line per record in `<dir>/<date>.jsonl`, a file a day.
    Jsonl { dir: PathBuf },
    /// A program Reef starts and feeds over its stdin; see
    /// `docs/reef/record.md` for the `reef-sink/1` protocol.
    Exec {
        command: Vec<String>,
        #[serde(default)]
        mode: Mode,
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
        /// Handed to the program in its hello line.
        #[serde(default)]
        settings: Value,
    },
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The program answers each batch; a batch counts once acknowledged,
    /// so an outage resumes where it stopped.
    #[default]
    Acks,
    /// Bare JSON lines, for tools that read them (Vector, kcat, Fluent
    /// Bit). A batch counts once written, so a crash can lose or repeat a
    /// few records.
    Plain,
}

fn yes() -> bool {
    true
}

fn redacted() -> String {
    "[REDACTED]".into()
}

fn default_drop() -> Vec<String> {
    vec!["tool_output".into()]
}

fn default_batch() -> usize {
    200
}

impl Config {
    /// The config at `path`; none when the file is missing. An error names
    /// what is wrong, so a broken edit can be told apart from no file.
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };
        let config: Self =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        let mut names = std::collections::HashSet::new();
        for sink in &config.sinks {
            let valid = !sink.name.is_empty()
                && (sink.name.chars()).all(|c| c.is_ascii_alphanumeric() || "-_".contains(c));
            if !valid {
                return Err(format!(
                    "sink name {:?}: letters, digits, - and _ only",
                    sink.name
                ));
            }
            if !names.insert(&sink.name) {
                return Err(format!("two sinks are named {:?}", sink.name));
            }
            if let SinkKind::Exec { command, .. } = &sink.kind
                && command.is_empty()
            {
                return Err(format!("sink {:?}: an empty command", sink.name));
            }
        }
        Ok(Some(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Option<Config>, String> {
        // A folder per call: tests run side by side.
        static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = (std::env::temp_dir())
            .join(format!("reef-record-config-{}-{call}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        std::fs::write(&path, text).unwrap();
        let config = Config::load(&path);
        std::fs::remove_dir_all(&dir).ok();
        config
    }

    #[test]
    fn a_sink_of_each_kind_with_the_defaults_filled_in() {
        let config = parse(
            r#"{ "follow_all": true, "sinks": [
                { "name": "log", "type": "jsonl", "dir": "record/log" },
                { "name": "tracker", "type": "exec", "command": ["reef-sink-tracker"],
                  "kinds": ["turn_finished"], "env": { "TRACKER_URL": "http://tracker.test" } }
            ] }"#,
        )
        .unwrap()
        .unwrap();
        assert!(config.follow_all && config.redact.builtin);
        assert_eq!(config.redact.drop, ["tool_output"]);
        assert_eq!(config.sinks[1].batch, 200);
        assert!(matches!(
            &config.sinks[1].kind,
            SinkKind::Exec {
                mode: Mode::Acks,
                ..
            }
        ));
    }

    #[test]
    fn a_broken_file_says_what_is_wrong() {
        assert!(parse("{ \"sinks\": [ { \"name\": \"x\" } ] }").is_err());
        let twice = r#"{ "sinks": [ { "name": "a", "type": "jsonl", "dir": "a" },
                                     { "name": "a", "type": "jsonl", "dir": "b" } ] }"#;
        assert!(parse(twice).unwrap_err().contains("two sinks"));
        let path = r#"{ "sinks": [ { "name": "../up", "type": "jsonl", "dir": "a" } ] }"#;
        assert!(parse(path).unwrap_err().contains("letters"));
        assert!(parse("{ \"follow_al\": true }").is_err());
    }
}
