//! What Crusteo remembers between launches: saved hosts, the client id, and
//! small UI preferences. Everything lives under the data dir as JSON.

use std::fs;
use std::path::PathBuf;

use fleet::HostConfig;
use serde::{Deserialize, Serialize};

const DEFAULT_HOSTS: &str = include_str!("../../hosts.json");

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CRUSTEO_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("crusteo");
    }
    if let Some(dir) = std::env::var_os("APPDATA").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("crusteo");
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if cfg!(target_os = "macos") {
            return home.join("Library/Application Support/crusteo");
        }
        return home.join(".local/share/crusteo");
    }
    PathBuf::from(".crusteo")
}

fn read<T: for<'de> Deserialize<'de>>(name: &str) -> Option<T> {
    let bytes = fs::read(data_dir().join(name)).ok()?;
    // Windows editors and PowerShell often save UTF-8 with a byte-order
    // mark, which JSON parsers refuse.
    let json = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    serde_json::from_slice(json).ok()
}

pub fn write<T: Serialize>(name: &str, value: &T) {
    let dir = data_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(value) {
        let tmp = dir.join(format!("{name}.tmp"));
        if fs::write(&tmp, bytes).is_ok() {
            let _ = fs::rename(tmp, dir.join(name));
        }
    }
}

/// The Reef this desktop takes its hosts from (`reef.json` in the data dir),
/// if any; see [`crate::app::reef`].
pub fn reef_link() -> Option<crate::app::reef::ReefLink> {
    read::<crate::app::reef::ReefLink>("reef.json").filter(|link| !link.url.trim().is_empty())
}

/// Where hosts are kept: `hosts.json`, or in reef mode the last list Reef
/// gave, so a start without Reef still has hosts.
fn hosts_file() -> &'static str {
    if reef_link().is_some() {
        "reef-hosts.json"
    } else {
        "hosts.json"
    }
}

/// Saved hosts. First launch seeds them from the bundled `hosts.json`;
/// after that the file in the data dir is the source of truth.
/// `CRUSTEO_HOSTS` points at an alternative file (handy for testing). In
/// reef mode they are the last list Reef gave, none before the first.
pub fn load_hosts() -> Vec<HostConfig> {
    if let Some(path) = std::env::var_os("CRUSTEO_HOSTS")
        && let Ok(bytes) = fs::read(&path)
        && let Ok(hosts) = serde_json::from_slice::<Vec<HostConfig>>(&bytes)
    {
        return hosts;
    }
    if reef_link().is_some() {
        return read::<Vec<HostConfig>>(hosts_file()).unwrap_or_default();
    }
    if let Some(mut hosts) = read::<Vec<HostConfig>>("hosts.json") {
        // Saved before hosts had ids: each takes its name as its id, once.
        if fleet::assign_ids(&mut hosts) {
            write("hosts.json", &hosts);
        }
        return hosts;
    }
    let mut hosts: Vec<HostConfig> = serde_json::from_str(DEFAULT_HOSTS).unwrap_or_default();
    fleet::assign_ids(&mut hosts);
    write("hosts.json", &hosts);
    hosts
}

pub fn save_hosts(hosts: &[HostConfig]) {
    write(hosts_file(), &hosts.to_vec());
}

pub fn client_id() -> String {
    if let Some(id) = read::<String>("client-id.json") {
        return id;
    }
    let id = paseo_client::generate_client_id();
    write("client-id.json", &id);
    id
}

/// UI state worth restoring: where the user was and how the sidebar looked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub layout: Option<String>,
    pub host: Option<fleet::HostId>,
    pub workspace: Option<String>,
    pub sidebar_width: Option<f32>,
    pub sidebar_open: Option<bool>,
    pub group_by_status: bool,
    pub collapsed: Vec<String>,
    pub collapsed_pinned: bool,
    pub host_filter: Vec<fleet::HostId>,
    /// Whether views list Paseo's background workspaces and their agents;
    /// None preserves the default, shown.
    pub show_background: Option<bool>,
    /// Open tabs per `host:workspace`: tab keys like `agent:<id>`.
    pub tabs: std::collections::BTreeMap<String, Vec<String>>,
    pub focused_tab: std::collections::BTreeMap<String, String>,
    /// Workspaces whose agent tabs the user closed, so they stay closed.
    pub hidden_agents: std::collections::BTreeMap<String, Vec<String>>,
    /// Lanes grouping: `label`, `project`, `host`, or `workspace`.
    pub lane_grouping: String,
    /// Label-derived lanes: labels, combinations, or both. Empty defaults to labels.
    pub lane_label_mode: String,
    /// Column keys in the user's order.
    pub lane_order: Vec<String>,
    /// Column keys the user collapsed to a strip.
    pub lane_collapsed: Vec<String>,
    /// Lane keys of labels shown as bands.
    pub lane_bands: Vec<String>,
    /// Lanes column width the user picked; `None` means the default.
    pub lane_width: Option<f32>,
    /// Lanes columns shrink, down to a floor, so all of them fit in view.
    pub lane_fit: bool,
    /// None preserves the default: follow the expanded card after relabeling.
    pub follow_relabel: Option<bool>,
    pub colored_lanes: bool,
    pub recency_headers: Option<bool>,
    /// None preserves the default: Lanes boxes every workspace, not only
    /// those with several agents or none.
    pub box_single_workspaces: Option<bool>,
    /// Window zoom; None is 100%.
    pub zoom: Option<f32>,
    /// Where each toolbar item sits.
    pub toolbars: crate::toolbars::Arrangement,
    /// Which agents Lanes shows, by host, tool and label.
    pub lane_filter: crate::app::nav::LaneFilter,
    /// Workspaces pinned to the top of their Lanes lane, as `host:workspace`.
    pub pinned_workspaces: Vec<String>,
    /// Card order the user gave inside a workspace's box: `host:workspace`
    /// to agent ids, first first.
    pub card_order: std::collections::BTreeMap<String, Vec<String>>,
    /// Local display overrides: canonical label-derived lane key → stable LANE_COLORS name.
    pub lane_color_choices: std::collections::BTreeMap<String, String>,
    /// A colour pinned for an agent (its popped-out window's bar and its open
    /// card's), by `agent_key`, as a LANE_COLORS name.
    pub agent_colors: std::collections::BTreeMap<String, String>,
    /// Where each agent's popped-out window was last, by `agent_key`:
    /// x, y, width, height.
    pub popout_bounds: std::collections::BTreeMap<String, [f32; 4]>,
    /// Window corner for in-app notifications; empty is the default.
    pub notification_corner: String,
    /// `in-app`, `system` or `both`; empty is the default.
    pub notification_route: String,
    /// Seconds a card opened from a notification blinks its outline; None
    /// is the default, 0 none.
    pub notification_flash: Option<u8>,
    /// Seconds an agent's notification stays up, 0 until closed; None is
    /// the default.
    pub notification_seconds: Option<f32>,
    /// `stack`, `list` or `newest`; empty is the default.
    pub notification_layout: String,
    /// The most notifications kept at once; None is the default.
    pub notification_keep: Option<u8>,
    /// A solid border around the window this many pixels wide; 0 is none.
    pub window_border: u8,
    /// Its color as `#rrggbb`; empty is the default.
    pub window_border_color: String,
    /// Column widths the user picked in layouts other than Lanes, by
    /// layout id; a layout missing has its default.
    pub column_widths: std::collections::BTreeMap<String, f32>,
    /// Layouts other than Lanes whose columns shrink to fit in view.
    pub column_fit: Vec<String>,
    /// How the last new workspace in each project was made, by
    /// `host:project`, so the next one starts the same way.
    pub workspace_choices: std::collections::BTreeMap<String, fleet::creation::Remembered>,
    /// The host the last project was added on.
    pub add_project_host: Option<fleet::HostId>,
    /// What the last new agent on each host was made with, by host id.
    pub agent_choices: std::collections::BTreeMap<String, fleet::creation::AgentChoice>,
}

pub fn load_ui() -> UiState {
    read("ui.json").unwrap_or_default()
}

pub fn save_ui(state: &UiState) {
    write("ui.json", state);
}

/// Settings for extensions, from `integrations.json` in the data dir: a
/// flat object of strings, such as `{"notes": "http://host:9000"}`. Each
/// extension reads its own keys; none means it stays off.
pub fn integrations() -> std::collections::BTreeMap<String, String> {
    read("integrations.json").unwrap_or_default()
}

/// One of [`integrations`], trimmed; none when unset or empty.
pub fn integration(key: &str) -> Option<String> {
    integrations()
        .remove(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Sets each environment variable from its `integrations.json` key, for
/// extensions that read their settings from the environment. A variable
/// already set wins.
///
/// # Safety
/// Sets environment variables, so call it before any other thread starts.
pub unsafe fn integrations_to_env(pairs: &[(&str, &str)]) {
    let map = integrations();
    for (key, var) in pairs {
        if let Some(value) = map.get(*key).filter(|value| !value.trim().is_empty())
            && std::env::var_os(var).is_none()
        {
            // Safe per this function's contract: no other threads yet.
            unsafe { std::env::set_var(var, value) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_border_is_off_by_default_and_survives_restart() {
        use crate::app::nav::parse_hex;
        let old: UiState = serde_json::from_str("{}").unwrap();
        assert_eq!(old.window_border, 0);
        let chosen = UiState {
            window_border: 3,
            window_border_color: "#ff9f0a".into(),
            ..UiState::default()
        };
        let restored: UiState =
            serde_json::from_value(serde_json::to_value(chosen).unwrap()).unwrap();
        assert_eq!(restored.window_border, 3);
        assert_eq!(parse_hex(&restored.window_border_color), Some(0xff9f0a));
        assert_eq!(parse_hex("3b9eff"), Some(0x3b9eff));
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#zzzzzz"), None);
    }

    #[test]
    fn label_lane_mode_defaults_to_labels_and_each_choice_survives_restart() {
        use fleet::lanes::LabelLaneMode;
        let old: UiState = serde_json::from_str("{}").unwrap();
        assert_eq!(
            LabelLaneMode::parse(&old.lane_label_mode),
            LabelLaneMode::Labels
        );
        assert_eq!(
            LabelLaneMode::parse("unknown future choice"),
            LabelLaneMode::Labels
        );
        for mode in LabelLaneMode::ALL {
            let state = UiState {
                lane_label_mode: mode.as_str().into(),
                ..old.clone()
            };
            let restored: UiState =
                serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
            assert_eq!(LabelLaneMode::parse(&restored.lane_label_mode), mode);
        }
    }

    #[test]
    fn follow_relabel_defaults_on_for_old_state_and_preserves_opt_out() {
        let old: UiState = serde_json::from_str(r#"{"layout":"lanes"}"#).unwrap();
        assert!(old.follow_relabel.unwrap_or(true));
        let disabled = UiState {
            follow_relabel: Some(false),
            ..old
        };
        let restored: UiState =
            serde_json::from_value(serde_json::to_value(disabled).unwrap()).unwrap();
        assert_eq!(restored.follow_relabel, Some(false));
    }

    #[test]
    fn background_workspaces_show_by_default_and_hiding_survives_restart() {
        let old: UiState = serde_json::from_str(r#"{"layout":"lanes"}"#).unwrap();
        assert!(old.show_background.unwrap_or(true));
        let hidden = UiState {
            show_background: Some(false),
            ..old
        };
        let restored: UiState =
            serde_json::from_value(serde_json::to_value(hidden).unwrap()).unwrap();
        assert_eq!(restored.show_background, Some(false));
    }

    #[test]
    fn new_workspace_choices_survive_restart() {
        use fleet::creation::{Isolation, Remembered};
        let old: UiState = serde_json::from_str("{}").unwrap();
        assert!(old.workspace_choices.is_empty());
        assert!(old.add_project_host.is_none());
        let chosen = UiState {
            workspace_choices: std::collections::BTreeMap::from([(
                "devbox:prj_1".into(),
                Remembered {
                    isolation: Isolation::Directory,
                    start: Some("refs/remotes/origin/main".into()),
                },
            )]),
            add_project_host: Some(fleet::HostId::new("devbox")),
            ..old
        };
        let restored: UiState =
            serde_json::from_value(serde_json::to_value(chosen).unwrap()).unwrap();
        let choice = &restored.workspace_choices["devbox:prj_1"];
        assert_eq!(choice.isolation, Isolation::Directory);
        assert_eq!(choice.start().label(), "origin/main");
        assert_eq!(
            restored.add_project_host,
            Some(fleet::HostId::new("devbox"))
        );
    }

    #[test]
    fn lane_color_preferences_default_and_survive_restart() {
        let old: UiState = serde_json::from_str("{}").unwrap();
        assert!(!old.colored_lanes);
        assert!(old.recency_headers.unwrap_or(true));
        assert!(old.lane_color_choices.is_empty());
        let custom = UiState {
            colored_lanes: true,
            recency_headers: Some(false),
            lane_color_choices: std::collections::BTreeMap::from([(
                "devtools".into(),
                "Rose".into(),
            )]),
            ..old
        };
        let restored: UiState =
            serde_json::from_value(serde_json::to_value(custom).unwrap()).unwrap();
        assert!(restored.colored_lanes);
        assert_eq!(restored.recency_headers, Some(false));
        assert_eq!(
            restored.lane_color_choices.get("devtools"),
            Some(&"Rose".to_string())
        );
    }
}
