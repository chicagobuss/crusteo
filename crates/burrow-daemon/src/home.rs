//! The state directory (`BURROW_HOME`): the daemon's identity, so a restart
//! keeps its `serverId` and clients keep treating it as the same host, and
//! its records, timelines, prompt images and worktrees.
//!
//! Burrow was called carapace before 0.1.0-beta.1, and its default home was
//! `~/.carapace`. The default home moves from there to `~/.burrow` the first
//! time Burrow starts without one: see [`move_legacy`]. A home given with
//! `--home` or `BURROW_HOME` (or `CARAPACE_HOME`) is used as it is.

use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::{git, ids};

/// The default home's folder, in the user's home.
pub const DIR: &str = ".burrow";
/// The default home's folder before the rename.
pub const LEGACY_DIR: &str = ".carapace";

/// `$BURROW_HOME` (else `$CARAPACE_HOME`), used as it is; else `~/.burrow`,
/// moved there from `~/.carapace` first when only that exists.
pub fn default_home() -> Option<PathBuf> {
    if let Some(home) = burrow_provider::env::var_os("BURROW_HOME") {
        return Some(PathBuf::from(home));
    }
    let user = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(prepare_default(Path::new(&user)))
}

/// `<user>/.burrow`, after moving `<user>/.carapace` there if that is the
/// only one of the two. When the move fails, `<user>/.carapace` is used as
/// before, and the next start tries again.
pub fn prepare_default(user: &Path) -> PathBuf {
    let home = user.join(DIR);
    let legacy = user.join(LEGACY_DIR);
    match move_legacy(user) {
        Ok(Some(moved)) => {
            info!(
                from = %moved.from.display(),
                to = %moved.to.display(),
                rewritten = moved.rewritten,
                repaired = moved.repaired,
                "moved the state directory from its carapace name; \
                 rewrote the paths in its records and reconnected its git worktrees"
            );
            for problem in &moved.problems {
                warn!(%problem, "while moving the state directory");
            }
            home
        }
        Ok(None) => {
            if legacy.is_dir() {
                warn!(
                    using = %home.display(),
                    ignored = %legacy.display(),
                    "both state directories exist; the carapace one is not read"
                );
            }
            home
        }
        Err(err) => {
            warn!(
                from = %legacy.display(),
                to = %home.display(),
                %err,
                "could not move the state directory; using it where it is"
            );
            legacy
        }
    }
}

/// What [`move_legacy`] did.
#[derive(Debug)]
pub struct Moved {
    pub from: PathBuf,
    pub to: PathBuf,
    /// Record and timeline files whose paths were rewritten.
    pub rewritten: usize,
    /// Git worktrees reconnected to their repositories.
    pub repaired: usize,
    /// What went wrong after the move itself, which did not undo it.
    pub problems: Vec<String>,
}

/// Renames `<user>/.carapace` to `<user>/.burrow` when only the first
/// exists, then keeps what it holds working at the new place:
/// - absolute paths into the old folder, in the records (projects,
///   workspaces, agents) and timelines (prompt images), now name the new
///   one;
/// - each git worktree in `worktrees/<project>/<slug>` is reconnected to its
///   repository (`git worktree repair`), which still names the old place.
///
/// `None` when there was nothing to move. An `Err` means nothing moved.
pub fn move_legacy(user: &Path) -> io::Result<Option<Moved>> {
    let from = user.join(LEGACY_DIR);
    let to = user.join(DIR);
    if to.symlink_metadata().is_ok() || !from.is_dir() {
        return Ok(None);
    }
    // Records may hold the resolved form of a path (git reports that one),
    // so map it too. A symlinked folder stays where it points, and paths
    // through its real location keep working.
    let mut prefixes = vec![(from.clone(), to.clone())];
    let linked = from
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink());
    if !linked
        && let Ok(real) = std::fs::canonicalize(&from)
        && real != from
        && let Some(parent) = real.parent()
    {
        prefixes.push((real.clone(), parent.join(DIR)));
    }
    std::fs::rename(&from, &to)?;
    let mut moved = Moved {
        from,
        to,
        rewritten: 0,
        repaired: 0,
        problems: Vec::new(),
    };
    rewrite_records(&mut moved, &prefixes);
    repair_worktrees(&mut moved);
    Ok(Some(moved))
}

/// The record and timeline files: `*.json` in the home and `agents/`,
/// `*.jsonl` in `timelines/`. Never the worktrees or attachments.
fn record_files(home: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for (dir, extension) in [
        (home.to_path_buf(), "json"),
        (home.join("agents"), "json"),
        (home.join("timelines"), "jsonl"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some(extension) && path.is_file() {
                files.push(path);
            }
        }
    }
    files
}

fn rewrite_records(moved: &mut Moved, prefixes: &[(PathBuf, PathBuf)]) {
    let pairs: Vec<(String, String)> = prefixes
        .iter()
        .flat_map(|(from, to)| json_forms(from, to))
        .collect();
    for file in record_files(&moved.to) {
        match rewrite_file(&file, &pairs) {
            Ok(true) => moved.rewritten += 1,
            Ok(false) => {}
            Err(err) => moved.problems.push(format!("{}: {err}", file.display())),
        }
    }
}

fn rewrite_file(file: &Path, pairs: &[(String, String)]) -> io::Result<bool> {
    let Ok(mut text) = std::fs::read_to_string(file) else {
        return Ok(false);
    };
    let mut changed = false;
    for (from, to) in pairs {
        if let Some(new) = replace_prefix(&text, from, to) {
            text = new;
            changed = true;
        }
    }
    if changed {
        let temp = file.with_extension("migrating");
        std::fs::write(&temp, &text)?;
        std::fs::rename(&temp, file)?;
    }
    Ok(changed)
}

/// A path as it is written inside a JSON string, mapped to the new one; on
/// Windows also with forward slashes, as git writes paths there.
fn json_forms(from: &Path, to: &Path) -> Vec<(String, String)> {
    let (Some(from), Some(to)) = (from.to_str(), to.to_str()) else {
        return Vec::new();
    };
    let escape = |path: &str| {
        let quoted = serde_json::to_string(path).unwrap_or_default();
        quoted
            .get(1..quoted.len().saturating_sub(1))
            .unwrap_or_default()
            .to_string()
    };
    let mut forms = vec![(escape(from), escape(to))];
    if from.contains('\\') {
        forms.push((
            escape(&from.replace('\\', "/")),
            escape(&to.replace('\\', "/")),
        ));
    }
    forms
}

/// Replaces `from` wherever it is a whole path, or the start of one: not
/// part of a longer name on either side. `None` when nothing was replaced.
pub fn replace_prefix(text: &str, from: &str, to: &str) -> Option<String> {
    let part_of_name = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while let Some(at) = rest.find(from) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + from.len()..].chars().next();
        let whole = !before.is_some_and(|c| part_of_name(c) || c == '/')
            && !after.is_some_and(part_of_name);
        out.push_str(&rest[..at]);
        out.push_str(if whole { to } else { from });
        changed |= whole;
        rest = &rest[at + from.len()..];
    }
    out.push_str(rest);
    changed.then_some(out)
}

/// Each worktree in `worktrees/<project>/<slug>` has a `.git` file naming
/// its repository, whose own record of it still names the old place.
/// `git worktree repair`, run inside it, points that record here.
fn repair_worktrees(moved: &mut Moved) {
    let Ok(projects) = std::fs::read_dir(moved.to.join("worktrees")) else {
        return;
    };
    for project in projects.flatten() {
        let Ok(slugs) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for slug in slugs.flatten() {
            let dir = slug.path();
            if !dir.join(".git").is_file() {
                continue;
            }
            match git::run(&dir, &["worktree", "repair"], &[], git::DEFAULT_TIMEOUT) {
                Ok(output) if output.ok() => moved.repaired += 1,
                Ok(output) => moved.problems.push(format!(
                    "git worktree repair in {}: {}",
                    dir.display(),
                    output.stderr.trim()
                )),
                Err(err) => moved.problems.push(format!("{}: {err}", dir.display())),
            }
        }
    }
}

/// Reads `<home>/server-id`, creating it on first run.
pub fn server_id(home: &Path) -> std::io::Result<String> {
    let path = home.join("server-id");
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => return Ok(id.trim().to_string()),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let id = ids::server_id();
    write_server_id(home, &id)?;
    Ok(id)
}

/// Writes `<home>/server-id`. Write then rename, so a crash never leaves a
/// half-written identity.
pub fn write_server_id(home: &Path, id: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    let temp = home.join("server-id.tmp");
    std::fs::write(&temp, id)?;
    std::fs::rename(&temp, home.join("server-id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("burrow-{name}-{}", ids::agent_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn server_id_is_stable() {
        let dir = std::env::temp_dir().join(format!("burrow-home-{}", ids::agent_id()));
        let first = server_id(&dir).unwrap();
        assert!(first.starts_with("srv_"));
        assert_eq!(server_id(&dir).unwrap(), first);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_whole_paths_are_replaced() {
        let text = r#"{"cwd":"/home/user/.carapace/worktrees/a/b","other":"/home/user/.carapace-old/x","home":"/home/user/.carapace","deep":"/x/home/user/.carapace/y","said":"see /home/user/.carapace/attachments/i.png now"}"#;
        let out = replace_prefix(text, "/home/user/.carapace", "/home/user/.burrow").unwrap();
        assert_eq!(
            out,
            r#"{"cwd":"/home/user/.burrow/worktrees/a/b","other":"/home/user/.carapace-old/x","home":"/home/user/.burrow","deep":"/x/home/user/.carapace/y","said":"see /home/user/.burrow/attachments/i.png now"}"#
        );
        assert_eq!(replace_prefix("nothing here", "/a", "/b"), None);
        assert_eq!(replace_prefix("/a.b/c", "/a", "/b"), None);
        // Windows paths, as JSON writes them.
        let pairs = json_forms(
            Path::new(r"C:\Users\user\.carapace"),
            Path::new(r"C:\Users\user\.burrow"),
        );
        assert_eq!(pairs[0].0, r"C:\\Users\\user\\.carapace");
        assert_eq!(pairs[0].1, r"C:\\Users\\user\\.burrow");
        assert_eq!(pairs[1].0, "C:/Users/user/.carapace");
    }

    #[test]
    fn nothing_moves_when_there_is_nothing_to_move_or_both_exist() {
        let user = scratch("nomove");
        assert!(move_legacy(&user).unwrap().is_none());
        assert_eq!(prepare_default(&user), user.join(DIR));
        assert!(!user.join(DIR).exists(), "made when first written");

        std::fs::create_dir_all(user.join(DIR)).unwrap();
        std::fs::create_dir_all(user.join(LEGACY_DIR)).unwrap();
        std::fs::write(user.join(LEGACY_DIR).join("server-id"), "srv_old").unwrap();
        assert!(move_legacy(&user).unwrap().is_none());
        assert_eq!(prepare_default(&user), user.join(DIR));
        assert!(
            user.join(LEGACY_DIR).join("server-id").is_file(),
            "left alone"
        );
        std::fs::remove_dir_all(&user).unwrap();
    }
}
