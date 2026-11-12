//! The desktop's D-Bus session bus, for CLIs that keep their login in the
//! desktop keyring (the Secret Service): Antigravity's `agy` does, and
//! others may. A Burrow started at boot by a service manager has no
//! `DBUS_SESSION_BUS_ADDRESS`, and one started in a desktop session keeps
//! that session's address after a new login replaced it. Either way the CLI
//! cannot read its login and fails.
//!
//! So each CLI Burrow starts gets the bus as it is at that moment: Burrow's
//! own while its socket is still there, else the per-user bus systemd keeps
//! (`$XDG_RUNTIME_DIR/bus`), else the one the user's running keyring or
//! desktop session uses, read from its `/proc/<pid>/environ`. With no
//! desktop logged in there is none, and nothing is set. Linux only: macOS
//! keeps logins in the Keychain, and Windows in its credential store.

// The lookup's pieces are tested everywhere and used on Linux.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

/// The variable a D-Bus client reads.
pub const VAR: &str = "DBUS_SESSION_BUS_ADDRESS";

/// Processes whose environment holds the desktop's bus, best first: the
/// keyring serving secrets, any keyring, then a desktop session.
const HOLDERS: &[&str] = &[
    "gnome-keyring-daemon",
    "kwalletd6",
    "kwalletd5",
    "ksecretd",
    "gnome-session-binary",
    "gnome-session",
    "xfce4-session",
    "ksmserver",
    "plasma_session",
    "cinnamon-session",
    "mate-session",
    "lxqt-session",
    "dbus-daemon",
];

/// The session bus address a CLI started now should get, or `None` when
/// there is no desktop session to find.
#[cfg(target_os = "linux")]
pub fn address() -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let own = std::env::var(VAR).ok();
    if let Some(own) = own.as_deref().filter(|a| reachable(a)) {
        return Some(own.to_string());
    }
    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| format!("/run/user/{uid}"));
    let systemd = std::path::Path::new(&runtime).join("bus");
    if is_socket(&systemd) {
        return Some(format!("unix:path={}", systemd.display()));
    }
    from_processes(uid)
}

#[cfg(not(target_os = "linux"))]
pub fn address() -> Option<String> {
    None
}

/// Sets [`VAR`] on `command` to [`address`], unless the command sets it
/// itself, or the address is the one it inherits anyway.
pub fn apply(command: &mut std::process::Command) {
    if command.get_envs().any(|(key, _)| key == VAR) {
        return;
    }
    if let Some(address) = address()
        && std::env::var(VAR).ok().as_deref() != Some(address.as_str())
    {
        command.env(VAR, address);
    }
}

/// Whether `address` names a bus that may still be there: a `unix:path=`
/// socket that exists. Other kinds (abstract sockets, TCP) cannot be looked
/// at, and are taken as they are.
fn reachable(address: &str) -> bool {
    match socket_path(address) {
        Some(path) => is_socket(std::path::Path::new(path)),
        None => !address.trim().is_empty(),
    }
}

/// The socket a `unix:path=...` address names (the first, of several).
fn socket_path(address: &str) -> Option<&str> {
    let first = address.split(';').next()?;
    let rest = first.strip_prefix("unix:")?;
    rest.split(',').find_map(|part| part.strip_prefix("path="))
}

fn is_socket(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// The bus in the environment of the best of `uid`'s [`HOLDERS`], newest
/// first among equals, whose socket is there.
#[cfg(target_os = "linux")]
fn from_processes(uid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let mut found: Vec<(usize, u64, String)> = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let dir = entry.path();
        if std::fs::metadata(&dir).map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(dir.join("cmdline")) else {
            continue;
        };
        let Some(rank) = rank(&cmdline) else {
            continue;
        };
        let Ok(environ) = std::fs::read(dir.join("environ")) else {
            continue;
        };
        if let Some(address) = bus_in(&environ).filter(|a| reachable(a)) {
            found.push((rank, pid, address));
        }
    }
    // Best holder first; among the same kind, the newest process.
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    found.into_iter().next().map(|(_, _, address)| address)
}

/// Where a process's command line puts it among [`HOLDERS`] (lower is
/// better); a keyring that serves secrets comes before any other.
fn rank(cmdline: &[u8]) -> Option<usize> {
    let mut args = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty());
    let program = String::from_utf8_lossy(args.next()?);
    let program = program.rsplit('/').next().unwrap_or_default().to_string();
    let place = HOLDERS.iter().position(|h| *h == program)?;
    let secrets = args.any(|a| {
        let a = String::from_utf8_lossy(a);
        a.starts_with("--components=") && a.contains("secrets")
    });
    // Leave room before every holder for a keyring serving secrets.
    Some(if secrets { 0 } else { place + 1 })
}

/// [`VAR`]'s value in a NUL-separated environment.
fn bus_in(environ: &[u8]) -> Option<String> {
    let prefix = format!("{VAR}=");
    environ
        .split(|b| *b == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix(&prefix))
        .filter(|a| !a.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyring_serving_secrets_beats_any_other_holder() {
        let line = |args: &[&str]| args.join("\0").into_bytes();
        let secrets = rank(&line(&[
            "/usr/bin/gnome-keyring-daemon",
            "--start",
            "--foreground",
            "--components=secrets",
        ]));
        let login = rank(&line(&[
            "/usr/bin/gnome-keyring-daemon",
            "--daemonize",
            "--login",
        ]));
        let session = rank(&line(&["/usr/bin/xfce4-session"]));
        let bus = rank(&line(&["/usr/bin/dbus-daemon", "--session"]));
        assert_eq!(secrets, Some(0));
        assert!(secrets < login && login < session && session < bus);
        assert_eq!(
            rank(&line(&["/bin/bash", "-c", "gnome-keyring-daemon"])),
            None
        );
        assert_eq!(rank(b""), None);
    }

    #[test]
    fn the_bus_is_read_from_an_environment_and_its_socket_path_found() {
        let environ =
            b"HOME=/home/u\0DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/dbus-x,guid=1\0DISPLAY=:0\0";
        assert_eq!(
            bus_in(environ).as_deref(),
            Some("unix:path=/tmp/dbus-x,guid=1")
        );
        assert_eq!(bus_in(b"HOME=/home/u\0"), None);
        assert_eq!(bus_in(b"DBUS_SESSION_BUS_ADDRESS=\0"), None);
        assert_eq!(
            socket_path("unix:path=/tmp/dbus-x,guid=1"),
            Some("/tmp/dbus-x")
        );
        assert_eq!(
            socket_path("unix:guid=1,path=/run/user/1000/bus;tcp:host=x"),
            Some("/run/user/1000/bus")
        );
        assert_eq!(socket_path("unix:abstract=/tmp/dbus-y"), None);
        // A socket that is gone is not reachable; an abstract one is taken.
        assert!(!reachable("unix:path=/nonexistent/burrow-test-bus"));
        assert!(reachable("unix:abstract=/tmp/dbus-y"));
        assert!(!reachable(""));
    }

    #[test]
    fn a_command_that_sets_the_bus_itself_keeps_it() {
        let mut command = std::process::Command::new("true");
        command.env(VAR, "unix:path=/mine");
        apply(&mut command);
        let set: Vec<_> = command.get_envs().filter(|(k, _)| *k == VAR).collect();
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].1, Some(std::ffi::OsStr::new("unix:path=/mine")));
    }
}
