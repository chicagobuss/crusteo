//! What a workspace's checkout is, and the `git` CLI that reads and changes
//! it, as Paseo shells out to it.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How long a git command that has no timeout of its own may run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Paseo's read-only git environment: no optional locks.
const READ_ONLY: &[(&str, &str)] = &[("GIT_OPTIONAL_LOCKS", "0")];

/// The facts clients show for a git checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitFacts {
    pub worktree_root: String,
    pub branch: Option<String>,
    pub remote_url: Option<String>,
    /// The main checkout when this is a linked worktree (`mainRepoRoot`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_repo_root: Option<String>,
    /// A worktree Burrow made under its worktrees root
    /// (`isPaseoOwnedWorktree`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paseo_owned: bool,
}

/// One finished git command.
#[derive(Debug, Clone)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// stdout, trimmed.
    pub fn text(&self) -> String {
        self.stdout.trim().to_string()
    }
}

/// Runs `git -C <cwd> <args>` with extra environment and a timeout. Blocking:
/// run it off the async threads. A git that cannot start is an `Err`; one that
/// fails is an `Ok` with its exit code.
pub fn run(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    timeout: Duration,
) -> Result<Output, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0");
    for (key, value) in env {
        command.env(key, value);
    }
    run_command(command, timeout).map_err(|err| format!("git {}: {err}", args.join(" ")))
}

/// Runs a prepared command, killing it after `timeout`. Its output is read on
/// threads of its own, so a chatty child cannot fill a pipe and stall.
pub fn run_command(mut command: Command, timeout: Duration) -> std::io::Result<Output> {
    let mut child = command.spawn()?;
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let stdout = reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout.join().unwrap_or_default();
    let mut stderr = stderr.join().unwrap_or_default();
    if status.is_none() {
        stderr.push_str(&format!("\ntimed out after {}s", timeout.as_secs()));
    }
    Ok(Output {
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
    })
}

/// A read: trimmed stdout when git succeeds with some, else `None`.
pub fn read(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = run(cwd, args, READ_ONLY, DEFAULT_TIMEOUT).ok()?;
    let text = output.text();
    (output.ok() && !text.is_empty()).then_some(text)
}

/// A command that must succeed: its stdout, or its stderr as the error.
pub fn must(cwd: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let output = run(cwd, args, &[], timeout)?;
    if output.ok() {
        Ok(output.stdout)
    } else {
        let stderr = output.stderr.trim();
        Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr.to_string()
        })
    }
}

/// Whether a git command exits 0.
pub fn succeeds(cwd: &Path, args: &[&str]) -> bool {
    run(cwd, args, READ_ONLY, DEFAULT_TIMEOUT).is_ok_and(|o| o.ok())
}

/// `None` when `cwd` is not inside a git work tree (or git is missing).
/// Blocking: run it off the async threads.
pub fn probe(cwd: &Path) -> Option<GitFacts> {
    let root = native(&read(cwd, &["rev-parse", "--show-toplevel"])?);
    // Names the branch even before the first commit; none when detached.
    let branch = read(cwd, &["symbolic-ref", "--short", "-q", "HEAD"]);
    let remote_url = read(cwd, &["config", "--get", "remote.origin.url"]);
    let main_repo_root = main_repo_root(cwd).filter(|main| !same_path(main, &root));
    Some(GitFacts {
        worktree_root: root,
        branch,
        remote_url,
        main_repo_root,
        paseo_owned: false,
    })
}

/// The repository's main checkout: the parent of the shared git directory
/// (`--git-common-dir`), or that directory itself for a bare repository.
pub fn main_repo_root(cwd: &Path) -> Option<String> {
    let common = read(cwd, &["rev-parse", "--git-common-dir"])?;
    let common = resolve_against(cwd, &native(&common));
    let common = canonical(&common);
    let root = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent().map(Path::to_path_buf).unwrap_or(common)
    } else {
        common
    };
    Some(root.to_string_lossy().into_owned())
}

/// Git prints `C:/x` on Windows; Burrow's paths use the native separator.
pub fn native(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_string()
    }
}

/// Node's `path.resolve`: absolute, with `.` and `..` folded, no symlinks read.
pub fn resolve(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `path` resolved against `base` when it is relative.
pub fn resolve_against(base: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        resolve(path)
    } else {
        resolve(&base.join(path))
    }
}

/// `realpath`, falling back to [`resolve`]; without Windows' `\\?\` prefix.
pub fn canonical(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(real) => strip_verbatim(real),
        Err(_) => resolve(path),
    }
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

/// Paseo's `expandTilde`: `~` and `~/…` name the user's home.
pub fn expand_tilde(path: &str) -> String {
    let home = || {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default()
    };
    if path == "~" {
        home()
    } else if let Some(rest) = path.strip_prefix("~/") {
        Path::new(&home()).join(rest).to_string_lossy().into_owned()
    } else {
        path.to_string()
    }
}

/// Whether two paths name the same place, symlinks resolved where they exist.
pub fn same_path(a: &str, b: &str) -> bool {
    canonical(Path::new(a)) == canonical(Path::new(b))
}

/// `candidate` relative to `root` when it is inside it (`""` for the root
/// itself), comparing both as given and with symlinks resolved, as Paseo's
/// `getRealpathAwareRelativePath` does.
pub fn relative_inside(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let pairs = [
        (resolve(root), resolve(candidate)),
        (canonical(root), canonical(candidate)),
    ];
    pairs
        .into_iter()
        .find_map(|(root, candidate)| candidate.strip_prefix(&root).ok().map(Path::to_path_buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_directory_is_not_a_checkout() {
        let dir = std::env::temp_dir().join(format!("burrow-git-{}", crate::ids::agent_id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(probe(&dir), None);
        let initialized = Command::new("git")
            .args(["init", "-q", "-b", "trunk"])
            .current_dir(&dir)
            .status()
            .is_ok_and(|s| s.success());
        if initialized {
            let facts = probe(&dir).expect("a checkout");
            assert!(
                facts
                    .worktree_root
                    .ends_with(dir.file_name().unwrap().to_str().unwrap())
            );
            assert_eq!(facts.remote_url, None);
            assert_eq!(facts.main_repo_root, None);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn paths_resolve_like_node() {
        let base = std::env::temp_dir();
        let folded = resolve(&base.join("a").join("..").join("b").join("."));
        assert_eq!(folded, resolve(&base.join("b")));
        let inside = relative_inside(&base, &base.join("x").join("y")).unwrap();
        assert_eq!(inside, Path::new("x").join("y"));
        assert_eq!(relative_inside(&base.join("x"), &base), None);
        assert_eq!(expand_tilde("/abs"), "/abs");
    }
}
