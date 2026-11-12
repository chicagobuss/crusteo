use std::process::Command;

fn main() {
    // The commit and the version shown with it, for --version and the startup line.
    let commit = commit();
    println!("cargo:rustc-env=CRUSTEO_VERSION={}", version(&commit));
    println!("cargo:rustc-env=CRUSTEO_COMMIT={commit}");
}

/// What --version and About show: `0.2.0 (<commit>)` for a release, and
/// `0.2.0-internal-<commit>` for a build of `next` between releases, whose
/// version ends in `-internal`. As in crusteo's build script.
fn version(commit: &str) -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    if version.ends_with("-internal") {
        format!("{version}-{commit}")
    } else {
        format!("{version} ({commit})")
    }
}

/// `CRUSTEO_COMMIT` when it is set (a build from a source tarball can pass
/// it), else git's short HEAD, else HEAD read from git's files, else
/// "unknown".
fn commit() -> String {
    println!("cargo:rerun-if-env-changed=CRUSTEO_COMMIT");
    if let Ok(commit) = std::env::var("CRUSTEO_COMMIT")
        && !commit.trim().is_empty()
    {
        return commit.trim().to_string();
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // A commit or a checkout moves HEAD and appends to its log. A worktree
    // has a git dir of its own, so ask git where it is.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/logs/HEAD");
    }
    git(&["rev-parse", "--short", "HEAD"])
        .or_else(head_from_files)
        .unwrap_or_else(|| "unknown".to_string())
}

/// HEAD's short commit read from git's files, for a git that cannot follow
/// the checkout: a worktree made from WSL names its git dir `/mnt/d/...`,
/// which Windows' git cannot open.
fn head_from_files() -> Option<String> {
    use std::path::{Path, PathBuf};
    let read = |path: &Path| std::fs::read_to_string(path).ok();
    // `/mnt/d/src/...` from WSL is `D:/src/...` here.
    let native = |path: &str| -> PathBuf {
        let mut parts = path.splitn(4, '/');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(""), Some("mnt"), Some(drive), Some(rest))
                if cfg!(windows) && drive.len() == 1 =>
            {
                PathBuf::from(format!("{}:/{rest}", drive.to_ascii_uppercase()))
            }
            _ => PathBuf::from(path),
        }
    };
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    let dot_git = manifest
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|p| p.exists())?;
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        native(read(&dot_git)?.trim().strip_prefix("gitdir:")?.trim())
    };
    let head_file = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head_file.display());
    let head = read(&head_file)?;
    let full = match head.trim().strip_prefix("ref:") {
        None => head.trim().to_string(),
        Some(name) => {
            // A worktree keeps its branches in the main git dir.
            let common = match read(&git_dir.join("commondir")) {
                Some(dir) => git_dir.join(dir.trim()),
                None => git_dir.clone(),
            };
            let branch = common.join(name.trim());
            println!("cargo:rerun-if-changed={}", branch.display());
            match read(&branch) {
                Some(commit) => commit.trim().to_string(),
                None => read(&common.join("packed-refs"))?
                    .lines()
                    .find_map(|line| line.strip_suffix(name.trim()))?
                    .trim()
                    .to_string(),
            }
        }
    };
    (full.len() >= 7 && full.bytes().all(|b| b.is_ascii_hexdigit())).then(|| full[..7].to_string())
}
