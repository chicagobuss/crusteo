//! Reef was crusteo-hub before 0.1.0-beta.1. What it left behind is moved
//! into place at start, and the environment variable it read still works
//! for one release, with a line saying it is deprecated.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use crate::config::Args;

/// The data dir beside the hosts file, by its old names: `hub` from
/// crusteo-hub, and `reef` from the first Reef builds, before a binary named
/// `reef` beside the hosts file showed that name could not be the folder's.
pub const OLD_DATA_DIRS: [&str; 2] = ["hub", "reef"];
/// What the data dir's files were called: `hub-settings.json`,
/// `hub-tokens.json`, `hub-client-id.json`.
pub const FILE_PREFIX: &str = "hub-";
/// What they are called now.
pub const NEW_FILE_PREFIX: &str = "reef-";

/// Moves the old data dir and its files into place, and says what moved,
/// one line each:
/// - with no `--data-dir`, the first old folder beside the hosts file (see
///   [`OLD_DATA_DIRS`]; a file of that name, such as the binary, is not one)
///   becomes the `reef-data` folder, when there is none yet;
/// - in the data dir, each `hub-*` file becomes `reef-*`, when that does
///   not exist yet.
///
/// A move that fails is an error, so that Reef never starts on an empty
/// data dir and makes new device tokens while the old ones are still there.
/// So is a data dir that is a file.
pub fn migrate(args: &Args) -> Result<Vec<String>, String> {
    let data_dir = args.data_dir();
    let mut lines = Vec::new();
    if args.data_dir.is_none() && !data_dir.exists() {
        let old = OLD_DATA_DIRS
            .iter()
            .map(|name| data_dir.with_file_name(name))
            .find(|old| old.is_dir());
        if let Some(old) = old {
            fs::rename(&old, &data_dir).map_err(|err| {
                format!(
                    "cannot move {} to {}: {err}",
                    old.display(),
                    data_dir.display()
                )
            })?;
            lines.push(format!("moved {} to {}", old.display(), data_dir.display()));
        }
    }
    if data_dir.exists() && !data_dir.is_dir() {
        return Err(format!(
            "{} is a file, not a folder; pass --data-dir with another place",
            data_dir.display()
        ));
    }
    lines.extend(migrate_files(&data_dir)?);
    Ok(lines)
}

/// Renames each `hub-*` file in `data_dir` to `reef-*`, unless that exists.
pub fn migrate_files(data_dir: &Path) -> Result<Vec<String>, String> {
    let Ok(entries) = fs::read_dir(data_dir) else {
        return Ok(Vec::new());
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(FILE_PREFIX))
        .collect();
    names.sort();
    let mut lines = Vec::new();
    for name in names {
        let old = data_dir.join(&name);
        let new = data_dir.join(format!("{NEW_FILE_PREFIX}{}", &name[FILE_PREFIX.len()..]));
        if new.exists() {
            lines.push(format!(
                "{} is not read: {} is",
                old.display(),
                new.display()
            ));
            continue;
        }
        fs::rename(&old, &new).map_err(|err| {
            format!(
                "cannot rename {} to {}: {err}",
                old.display(),
                new.display()
            )
        })?;
        lines.push(format!("renamed {} to {}", old.display(), new.display()));
    }
    Ok(lines)
}

/// `name` as `get` reads it, else `old`: the value, and whether it came
/// from `old`.
pub fn resolve(
    name: &str,
    old: &str,
    get: impl Fn(&str) -> Option<OsString>,
) -> Option<(OsString, bool)> {
    get(name)
        .map(|value| (value, false))
        .or_else(|| get(old).map(|value| (value, true)))
}

/// `name` from the environment, else its old name `old`, which then says it
/// is deprecated on stderr (once per name).
pub fn var_os(name: &str, old: &str) -> Option<OsString> {
    let (value, legacy) = resolve(name, old, |key| std::env::var_os(key))?;
    if legacy {
        warn_deprecated(old, name);
    }
    Some(value)
}

/// Says that `old` is deprecated in favour of `new`, the first time only.
/// Returns whether it did.
pub fn warn_deprecated(old: &str, new: &str) -> bool {
    static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let first = WARNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(old.to_string());
    if first {
        eprintln!(
            "reef: {old} is deprecated and stops working in a later release; set {new} instead"
        );
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, parse_args};
    use crate::settings::{self, Settings};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "reef-legacy-{name}-{}",
            config::new_token().unwrap()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn args(list: &[&str]) -> Args {
        parse_args(list.iter().map(|s| s.to_string())).unwrap()
    }

    /// A data dir as crusteo-hub left it: its client id, a phone's token
    /// and a setting.
    fn hub_files(dir: &Path) -> (String, Vec<config::Device>) {
        fs::create_dir_all(dir).unwrap();
        let id = "cid_from_the_hub".to_string();
        let devices = vec![config::Device {
            name: "phone".into(),
            token: "ab".repeat(32),
        }];
        fs::write(
            dir.join("hub-client-id.json"),
            serde_json::to_vec(&id).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("hub-tokens.json"),
            serde_json::to_vec(&devices).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("hub-settings.json"),
            br#"{"notify_default": false, "notify": {"buildbox/a": true}, "show_background": false}"#,
        )
        .unwrap();
        (id, devices)
    }

    #[test]
    fn the_hub_folder_beside_the_hosts_file_becomes_reefs() {
        let root = scratch("dir");
        let hosts = root.join("hosts.json");
        fs::write(&hosts, "[]").unwrap();
        let (id, devices) = hub_files(&root.join("hub"));
        let args = args(&[hosts.to_str().unwrap()]);

        // Installed as the guide says: the binary beside the hosts file.
        fs::write(root.join("reef"), b"\x7fELF").unwrap();

        let lines = migrate(&args).unwrap();
        assert_eq!(lines.len(), 4, "{lines:?}");
        let data_dir = args.data_dir();
        assert_eq!(data_dir, root.join("reef-data"));
        assert!(!root.join("hub").exists());
        assert!(root.join("reef").is_file(), "the binary is left alone");
        // The same client id and tokens, so daemons and phones see no change.
        assert_eq!(config::client_id(&data_dir).unwrap(), id);
        let (kept, made) = config::devices(&data_dir).unwrap();
        assert_eq!(kept, devices);
        assert!(made.is_none(), "no new token");
        let saved = Settings::load(&data_dir.join(settings::FILE));
        assert!(!saved.notify_default);
        assert!(!saved.show_background);
        assert!(saved.notify_for("buildbox/a"));
        for name in [
            "reef-client-id.json",
            "reef-tokens.json",
            "reef-settings.json",
        ] {
            assert!(data_dir.join(name).is_file(), "{name}");
        }

        // Nothing more to do the next time.
        assert!(migrate(&args).unwrap().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_given_data_dir_keeps_its_place_and_its_files_are_renamed() {
        let root = scratch("given");
        let dir = root.join("state");
        let (id, _) = hub_files(&dir);
        // A hub folder elsewhere is not this one's business.
        hub_files(&root.join("hub"));
        let args = args(&["hosts.json", "--data-dir", dir.to_str().unwrap()]);
        let lines = migrate(&args).unwrap();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(root.join("hub").is_dir());
        assert_eq!(config::client_id(&dir).unwrap(), id);
        assert!(!dir.join("hub-tokens.json").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reef_folder_from_the_first_builds_moves_too() {
        let root = scratch("early");
        let hosts = root.join("hosts.json");
        fs::write(&hosts, "[]").unwrap();
        let early = root.join("reef");
        fs::create_dir_all(&early).unwrap();
        fs::write(early.join("reef-client-id.json"), br#""cid_early""#).unwrap();
        let args = args(&[hosts.to_str().unwrap()]);
        let lines = migrate(&args).unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(
            config::client_id(&root.join("reef-data")).unwrap(),
            "cid_early"
        );
        assert!(!early.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_data_dir_that_is_a_file_is_an_error() {
        let root = scratch("file");
        let hosts = root.join("hosts.json");
        fs::write(root.join("reef-data"), b"not a folder").unwrap();
        let err = migrate(&args(&[hosts.to_str().unwrap()])).unwrap_err();
        assert!(err.contains("is a file"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn new_files_win_over_old_ones() {
        let root = scratch("both");
        let data_dir = root.join("reef-data");
        hub_files(&data_dir);
        fs::write(data_dir.join("reef-client-id.json"), br#""cid_newer""#).unwrap();
        // And a reef folder beside a hub folder: the hub one is left alone.
        hub_files(&root.join("hub"));
        let hosts = root.join("hosts.json");
        let lines = migrate(&args(&[hosts.to_str().unwrap()])).unwrap();
        assert!(lines[0].contains("is not read"), "{lines:?}");
        assert_eq!(config::client_id(&data_dir).unwrap(), "cid_newer");
        assert!(data_dir.join("hub-client-id.json").is_file());
        assert!(data_dir.join("reef-tokens.json").is_file());
        assert!(root.join("hub").join("hub-tokens.json").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    fn get(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn the_old_page_variable_still_works() {
        assert_eq!(
            resolve(
                "REEF_PAGE",
                "CRUSTEO_HUB_PAGE",
                get(&[("CRUSTEO_HUB_PAGE", "old.html")])
            ),
            Some(("old.html".into(), true))
        );
        assert_eq!(
            resolve(
                "REEF_PAGE",
                "CRUSTEO_HUB_PAGE",
                get(&[("REEF_PAGE", "new.html"), ("CRUSTEO_HUB_PAGE", "old.html")])
            ),
            Some(("new.html".into(), false))
        );
        assert_eq!(resolve("REEF_PAGE", "CRUSTEO_HUB_PAGE", get(&[])), None);
        assert!(warn_deprecated("CRUSTEO_HUB_TEST", "REEF_TEST"));
        assert!(!warn_deprecated("CRUSTEO_HUB_TEST", "REEF_TEST"));
    }
}
