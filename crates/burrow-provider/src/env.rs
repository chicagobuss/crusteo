//! Environment variables by their Burrow names.
//!
//! Burrow was called carapace before 0.1.0-beta.1. For one release, a
//! `BURROW_*` variable that is not set falls back to its old `CARAPACE_*`
//! name, and the first use of each old name logs that it is deprecated.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::sync::Mutex;

/// Where a variable's value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Its `BURROW_*` name.
    Current,
    /// Its old name, given here.
    Legacy(String),
}

/// A `BURROW_*` variable's name from before the rename.
pub fn legacy_name(name: &str) -> Option<String> {
    name.strip_prefix("BURROW_")
        .map(|rest| format!("CARAPACE_{rest}"))
}

/// `name` as `get` reads it, else its old name: the value and which name
/// had it.
pub fn resolve(name: &str, get: impl Fn(&str) -> Option<OsString>) -> Option<(OsString, Source)> {
    if let Some(value) = get(name) {
        return Some((value, Source::Current));
    }
    let old = legacy_name(name)?;
    get(&old).map(|value| (value, Source::Legacy(old)))
}

/// `name` from the environment, else its old name, which then logs that it
/// is deprecated (once per name).
pub fn var_os(name: &str) -> Option<OsString> {
    let (value, source) = resolve(name, |key| std::env::var_os(key))?;
    if let Source::Legacy(old) = source {
        warn_deprecated(&old, name);
    }
    Some(value)
}

/// Logs that `old` is deprecated in favour of `new`, the first time only.
/// Returns whether it logged.
pub fn warn_deprecated(old: &str, new: &str) -> bool {
    static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let first = WARNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(old.to_string());
    if first {
        tracing::warn!(
            "{old} is deprecated and stops working in a later release; set {new} instead"
        );
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| {
            vars.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn the_new_name_wins_and_the_old_one_is_a_fallback() {
        assert_eq!(legacy_name("BURROW_HOME").as_deref(), Some("CARAPACE_HOME"));
        assert_eq!(legacy_name("REEF_PAGE"), None);
        let both = env(&[("BURROW_HOME", "/new"), ("CARAPACE_HOME", "/old")]);
        assert_eq!(
            resolve("BURROW_HOME", both),
            Some(("/new".into(), Source::Current))
        );
        let old = env(&[("CARAPACE_CLAUDE_TRACE", "/traces")]);
        assert_eq!(
            resolve("BURROW_CLAUDE_TRACE", old),
            Some((
                "/traces".into(),
                Source::Legacy("CARAPACE_CLAUDE_TRACE".into())
            ))
        );
        assert_eq!(resolve("BURROW_LOG", env(&[])), None);
        // Every variable Burrow reads has its old name.
        for name in [
            "BURROW_HOME",
            "BURROW_LISTEN",
            "BURROW_LOG",
            "BURROW_WORKTREES_ROOT",
            "BURROW_CLAUDE_TRACE",
            "BURROW_CODEX_TRACE",
            "BURROW_PI_TRACE",
            "BURROW_ACP_TRACE",
            "BURROW_AGY_TRACE",
        ] {
            let old = legacy_name(name).unwrap();
            let vars = [(old.as_str(), "x")];
            let get = env(&vars);
            assert_eq!(
                resolve(name, get).map(|(_, s)| s),
                Some(Source::Legacy(old))
            );
        }
    }

    #[test]
    fn a_deprecated_name_is_reported_once() {
        assert!(warn_deprecated("CARAPACE_TEST_ONCE", "BURROW_TEST_ONCE"));
        assert!(!warn_deprecated("CARAPACE_TEST_ONCE", "BURROW_TEST_ONCE"));
    }
}
