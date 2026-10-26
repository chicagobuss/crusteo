use std::process::Command;

fn main() {
    // The commit, for --version and the startup line.
    println!("cargo:rustc-env=CRUSTEO_COMMIT={}", commit());
}

/// `CRUSTEO_COMMIT` when it is set (a build from a source tarball can pass
/// it), else git's short HEAD, else "unknown". As in crusteo's build script.
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
    git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string())
}
