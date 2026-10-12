#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/crusteo.rc");
    println!("cargo:rerun-if-changed=assets/icon/crusteo.ico");
    // The commit, for About: every build is 0.1.0. Absent outside a checkout.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/logs/HEAD");
    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        && out.status.success()
    {
        let commit = String::from_utf8_lossy(&out.stdout);
        println!("cargo:rustc-env=CRUSTEO_COMMIT={}", commit.trim());
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // The app icon, for the exe, its windows and the taskbar.
        embed_resource::compile("assets/icon/crusteo.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
