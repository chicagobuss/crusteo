//! Settings Reef keeps for every phone alike: whether turn-finished
//! notifications are on by default, and each agent's own choice, and
//! whether lists show background workspaces' agents on a phone that has
//! not chosen. They live in `reef-settings.json` in the data dir, so they
//! hold on every device and across restarts.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config;

pub const FILE: &str = "reef-settings.json";

fn on() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Notify when a turn ends, for agents with no choice of their own.
    #[serde(default = "on")]
    pub notify_default: bool,
    /// Per agent, by row key (`host/agent`).
    #[serde(default)]
    pub notify: BTreeMap<String, bool>,
    /// Lists show the agents of Paseo's background workspaces, unless a
    /// phone's own filter says otherwise.
    #[serde(default = "on")]
    pub show_background: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify_default: true,
            notify: BTreeMap::new(),
            show_background: true,
        }
    }
}

impl Settings {
    pub fn notify_for(&self, key: &str) -> bool {
        self.notify.get(key).copied().unwrap_or(self.notify_default)
    }

    /// Sets an agent's choice; `None` makes it follow the default again.
    pub fn set_notify(&mut self, key: &str, on: Option<bool>) {
        match on {
            Some(on) => self.notify.insert(key.to_string(), on),
            None => self.notify.remove(key),
        };
    }

    /// What phones are told. Per-agent choices travel on the agent rows.
    pub fn public(&self) -> Value {
        json!({ "notify_default": self.notify_default, "show_background": self.show_background })
    }

    /// The saved settings, or the defaults when there are none yet.
    pub fn load(path: &Path) -> Self {
        if !path.exists() {
            return Self::default();
        }
        config::read_json(path).unwrap_or_else(|err| {
            eprintln!("reef: {err}; using the defaults");
            Self::default()
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        config::write_json(path, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agents_choice_wins_over_the_default_and_survives_a_restart() {
        let mut settings = Settings::default();
        assert!(settings.notify_for("h/a"));
        settings.set_notify("h/a", Some(false));
        settings.notify_default = false;
        assert!(!settings.notify_for("h/b"));
        settings.set_notify("h/b", Some(true));
        assert!(settings.notify_for("h/b"));
        let dir = std::env::temp_dir().join(format!(
            "reef-settings-{}",
            crate::config::new_token().unwrap()
        ));
        let path = dir.join(FILE);
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
        settings.set_notify("h/b", None);
        assert!(!settings.notify_for("h/b"));
        // Background workspaces show by default, also for a file from
        // before the setting existed.
        assert!(Settings::default().show_background);
        let old: Settings = serde_json::from_str(r#"{"notify_default": false}"#).unwrap();
        assert!(old.show_background);
        settings.show_background = false;
        settings.save(&path).unwrap();
        assert!(!Settings::load(&path).show_background);
        assert_eq!(
            Settings::load(&dir.join("missing.json")),
            Settings::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
