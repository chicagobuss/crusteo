//! Paseo's daemon lock, `$PASEO_HOME/paseo.pid`, written for Burrow.
//!
//! Paseo's CLI (0.13 on) finds the daemon on its host through this file: a
//! command without `--host` reads its `listen` address, and with no live
//! lock says "Daemon is not running … Start with: paseo daemon start". On a
//! host that moved from Paseo to Burrow, every bare `paseo` command, from a
//! person's shell, a cron job or an agent's own watcher, failed that way,
//! and the hint started Paseo's daemon beside Burrow. While Burrow runs it
//! holds the lock as Paseo's supervisor would:
//!
//! - In Paseo's format (`pid-lock.ts`): `pid`, `startedAt`, `hostname`,
//!   `uid`, `listen`, `serverId`, and on Linux `bootId`, which a reader
//!   checks so a lock from before a reboot (or a VM's sleep) reads as dead.
//!   No `heartbeat`: nothing reads it.
//! - Never over a live one: a Paseo daemon (or the desktop app's) that holds
//!   it keeps it, and Burrow runs without.
//! - A dead one is cleared only after reading it again, as Paseo clears an
//!   abandoned lock, and the new one appears whole: written to a temporary
//!   file and linked into place, which fails if another got there first.
//! - Removed when Burrow stops, only if it is still Burrow's.
//!
//! With the lock held, `paseo daemon start` finds a daemon running and does
//! not start another, and `paseo daemon stop` sends Burrow `SIGTERM`.
//!
//! Only a Burrow that has taken Paseo's place holds it: one listening on the
//! port Paseo's own config gives it ([`paseo_port`]). Next to a Paseo on its
//! own port, holding it would keep that Paseo from starting.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const FILE: &str = "paseo.pid";
/// Slack for a boot instant derived from whole seconds of uptime (Paseo's).
const BOOT_TOLERANCE_SECS: i64 = 5;

/// `pidLockInfoSchema`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockInfo {
    pub pid: u32,
    pub started_at: String,
    pub hostname: String,
    pub uid: u32,
    pub listen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_managed: Option<bool>,
}

/// Paseo's home: `$PASEO_HOME` (`~` expanded), else `~/.paseo`.
pub fn paseo_home(env_home: Option<std::ffi::OsString>, user_home: &Path) -> PathBuf {
    match env_home.and_then(|home| home.into_string().ok()) {
        Some(home) if home == "~" => user_home.to_path_buf(),
        Some(home) if home.starts_with("~/") => user_home.join(&home[2..]),
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => user_home.join(".paseo"),
    }
}

/// Paseo's default port.
pub const PASEO_PORT: u16 = 6767;

/// The port Paseo listens on by `home`'s `config.json` (`daemon.listen`, as
/// `host:port`, `[v6]:port` or a bare port), else [`PASEO_PORT`].
pub fn paseo_port(home: &Path) -> u16 {
    let listen = fs::read_to_string(home.join("config.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|config| config["daemon"]["listen"].as_str().map(str::to_string));
    listen
        .and_then(|listen| listen.rsplit(':').next().and_then(|port| port.parse().ok()))
        .unwrap_or(PASEO_PORT)
}

/// What came of trying to take the lock.
#[derive(Debug)]
pub enum Taken {
    Held(Lock),
    /// A live daemon holds it: its pid.
    Busy(u32),
}

/// The lock, while Burrow holds it. Dropping it removes the file if it is
/// still this one.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    pid: u32,
    started_at: String,
}

impl Lock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let ours = read(&self.path)
            .ok()
            .flatten()
            .and_then(|text| parse(&text))
            .is_some_and(|lock| lock.pid == self.pid && lock.started_at == self.started_at);
        if ours {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Takes the lock in `home` for this process, listening on `listen`.
pub fn take(
    home: &Path,
    listen: &str,
    server_id: &str,
    hostname: &str,
    started_at: &str,
) -> io::Result<Taken> {
    private_dir(home)?;
    let path = home.join(FILE);
    let pid = std::process::id();
    for _ in 0..3 {
        if let Some(text) = read(&path)? {
            match parse(&text) {
                Some(lock) if lock.pid != pid && owner_running(&lock) => {
                    return Ok(Taken::Busy(lock.pid));
                }
                // Abandoned, unreadable or ours from before: cleared if it is
                // still what was read.
                _ => {
                    if read(&path)?.as_deref() == Some(text.as_str()) {
                        match fs::remove_file(&path) {
                            Ok(()) => {}
                            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                            Err(err) => return Err(err),
                        }
                    }
                    continue;
                }
            }
        }
        let lock = LockInfo {
            pid,
            started_at: started_at.to_string(),
            hostname: hostname.to_string(),
            uid: 0,
            listen: Some(listen.to_string()),
            server_id: Some(server_id.to_string()),
            boot_id: boot_id(),
            desktop_managed: None,
        };
        match publish(&path, lock) {
            Ok(()) => {
                info!(path = %path.display(), listen, "holding Paseo's daemon lock, so its CLI finds Burrow");
                return Ok(Taken::Held(Lock {
                    path,
                    pid,
                    started_at: started_at.to_string(),
                }));
            }
            // Another took it between the read and the link: look again.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    match read(&path)?.as_deref().and_then(parse) {
        Some(lock) => Ok(Taken::Busy(lock.pid)),
        None => Err(io::Error::other("the lock kept changing")),
    }
}

/// Writes `lock` beside `path` and links it into place: `AlreadyExists`
/// when there is a lock there now. The owner's uid is the temporary file's.
fn publish(path: &Path, mut lock: LockInfo) -> io::Result<()> {
    let temp = path.with_file_name(format!(".{FILE}.burrow-{}", lock.pid));
    let _ = fs::remove_file(&temp);
    let mut file = private_file(&temp)?;
    lock.uid = owner_uid(&temp);
    let mut text = serde_json::to_string_pretty(&lock).map_err(io::Error::other)?;
    text.push('\n');
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let linked = written.and_then(|()| fs::hard_link(&temp, path));
    let _ = fs::remove_file(&temp);
    linked
}

fn read(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// A lock Paseo's readers accept: valid fields, and an empty file is none.
fn parse(text: &str) -> Option<LockInfo> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value
        .get("heartbeat")
        .is_some_and(|h| *h != serde_json::Value::Bool(true))
    {
        return None;
    }
    serde_json::from_value(value)
        .ok()
        .filter(|lock: &LockInfo| lock.pid > 0)
}

/// `isPidLockOwnerRunning`: written during this boot, and its pid runs.
pub fn owner_running(lock: &LockInfo) -> bool {
    written_this_boot(lock) && pid_running(lock.pid)
}

fn written_this_boot(lock: &LockInfo) -> bool {
    if let (Some(theirs), Some(ours)) = (&lock.boot_id, boot_id()) {
        return *theirs == ours;
    }
    let (Some(started), Some(booted)) = (crate::clock::parse_iso(&lock.started_at), boot_instant())
    else {
        return true;
    };
    started >= booted - BOOT_TOLERANCE_SECS
}

/// The kernel's name for this boot, on Linux.
fn boot_id() -> Option<String> {
    let id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(id.trim().to_string()).filter(|id| !id.is_empty())
}

/// When this host booted, in unix seconds.
fn boot_instant() -> Option<i64> {
    let now = crate::clock::now_unix();
    if let Ok(uptime) = fs::read_to_string("/proc/uptime") {
        let secs: f64 = uptime.split_whitespace().next()?.parse().ok()?;
        return Some(now - secs as i64);
    }
    // macOS: `{ sec = 1793000000, usec = 0 } ...`.
    let output = std::process::Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let sec = text
        .split("sec = ")
        .nth(1)?
        .split(|c: char| !c.is_ascii_digit())
        .next()?;
    sec.parse().ok()
}

/// Whether a process with this pid exists, whoever owns it (Paseo counts
/// `EPERM` as running).
#[cfg(unix)]
fn pid_running(pid: u32) -> bool {
    if Path::new("/proc/self").exists() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Without a way to tell, a lock is taken to be live: Burrow does not take
/// it over.
#[cfg(not(unix))]
fn pid_running(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
fn private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(not(unix))]
fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
fn owner_uid(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).map(|m| m.uid()).unwrap_or(0)
}

#[cfg(not(unix))]
fn owner_uid(_path: &Path) -> u32 {
    0
}

/// Takes the lock for the daemon when it listens where Paseo would, logging
/// what came of it. `None` when it was not taken.
pub fn take_for(home: &Path, daemon: &crate::Daemon, bound_port: u16) -> Option<Lock> {
    let port = paseo_port(home);
    let mut ports =
        std::iter::once(bound_port).chain(daemon.config.also_listen.iter().map(|a| a.port()));
    if !ports.any(|p| p == port) {
        info!(
            paseo_port = port,
            "not on Paseo's port: leaving Paseo's daemon lock to Paseo"
        );
        return None;
    }
    let listen = daemon.local_endpoint.get()?.clone();
    let hostname = daemon.config.hostname.clone().unwrap_or_default();
    match take(
        home,
        &listen,
        &daemon.config.server_id,
        &hostname,
        &daemon.started_at,
    ) {
        Ok(Taken::Held(lock)) => Some(lock),
        Ok(Taken::Busy(pid)) => {
            warn!(
                path = %home.join(FILE).display(),
                pid,
                "a running Paseo daemon holds its lock; Paseo's CLI without --host reaches it, not Burrow"
            );
            None
        }
        Err(err) => {
            warn!(path = %home.join(FILE).display(), %err, "could not write Paseo's daemon lock");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "burrow-paseo-lock-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn held(taken: Taken) -> Lock {
        match taken {
            Taken::Held(lock) => lock,
            Taken::Busy(pid) => panic!("busy: {pid}"),
        }
    }

    #[test]
    fn the_lock_is_paseos_shape_and_goes_when_burrow_stops() {
        let home = scratch("shape");
        let started = crate::clock::now_iso();
        let lock = held(
            take(
                &home,
                "127.0.0.1:6767",
                "srv_test",
                "example-host",
                &started,
            )
            .unwrap(),
        );
        let text = fs::read_to_string(home.join(FILE)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["pid"], std::process::id());
        assert_eq!(value["listen"], "127.0.0.1:6767");
        assert_eq!(value["serverId"], "srv_test");
        assert_eq!(value["startedAt"], started.as_str());
        assert!(value["uid"].is_u64());
        assert!(value.get("heartbeat").is_none() && value.get("desktopManaged").is_none());
        assert!(owner_running(&parse(&text).unwrap()), "it reads as live");
        let leftovers: Vec<_> = (fs::read_dir(&home).unwrap())
            .map(|e| e.unwrap().file_name())
            .filter(|name| name != FILE)
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temporary file is left: {leftovers:?}"
        );
        drop(lock);
        assert!(!home.join(FILE).exists());
        fs::remove_dir_all(&home).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_live_daemons_lock_is_left_alone() {
        let home = scratch("live");
        // Another live process: this test's parent shell, or init.
        let other = LockInfo {
            pid: 1,
            started_at: crate::clock::now_iso(),
            hostname: "h".into(),
            uid: 0,
            listen: Some("127.0.0.1:6767".into()),
            server_id: Some("srv_paseo".into()),
            boot_id: boot_id(),
            desktop_managed: None,
        };
        let text = serde_json::to_string(&other).unwrap();
        fs::write(home.join(FILE), &text).unwrap();
        let taken = take(
            &home,
            "127.0.0.1:6767",
            "srv_test",
            "h",
            &crate::clock::now_iso(),
        )
        .unwrap();
        assert!(matches!(taken, Taken::Busy(1)));
        assert_eq!(
            fs::read_to_string(home.join(FILE)).unwrap(),
            text,
            "untouched"
        );
        fs::remove_dir_all(&home).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_dead_or_broken_lock_is_replaced() {
        let home = scratch("dead");
        let gone = LockInfo {
            pid: u32::MAX - 7,
            started_at: "2020-01-01T00:00:00.000Z".into(),
            hostname: "h".into(),
            uid: 0,
            listen: Some("127.0.0.1:6767".into()),
            server_id: None,
            boot_id: Some("an-earlier-boot".into()),
            desktop_managed: None,
        };
        for stale in [
            serde_json::to_string(&gone).unwrap(),
            String::new(),
            "{not json".into(),
        ] {
            fs::write(home.join(FILE), &stale).unwrap();
            let lock = held(
                take(
                    &home,
                    "127.0.0.1:6767",
                    "srv_test",
                    "h",
                    &crate::clock::now_iso(),
                )
                .unwrap(),
            );
            let now = parse(&fs::read_to_string(home.join(FILE)).unwrap()).unwrap();
            assert_eq!(now.pid, std::process::id());
            drop(lock);
        }
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn someone_elses_lock_is_not_removed_on_exit() {
        let home = scratch("replaced");
        let lock = held(
            take(
                &home,
                "127.0.0.1:6767",
                "srv_test",
                "h",
                &crate::clock::now_iso(),
            )
            .unwrap(),
        );
        fs::write(
            home.join(FILE),
            r#"{"pid": 1, "startedAt": "x", "hostname": "h", "uid": 0, "listen": null}"#,
        )
        .unwrap();
        drop(lock);
        assert!(home.join(FILE).exists());
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn paseos_port_is_its_configs_or_the_default() {
        let home = scratch("port");
        assert_eq!(paseo_port(&home), PASEO_PORT);
        for (listen, port) in [
            ("10.0.0.5:6800", 6800),
            ("[::1]:6801", 6801),
            ("6802", 6802),
        ] {
            fs::write(
                home.join("config.json"),
                format!(r#"{{"daemon": {{"listen": "{listen}"}}}}"#),
            )
            .unwrap();
            assert_eq!(paseo_port(&home), port, "{listen}");
        }
        fs::write(home.join("config.json"), "not json").unwrap();
        assert_eq!(paseo_port(&home), PASEO_PORT);
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn paseo_home_follows_the_variable() {
        let user = Path::new("/home/u");
        assert_eq!(paseo_home(None, user), user.join(".paseo"));
        assert_eq!(paseo_home(Some("~/.p".into()), user), user.join(".p"));
        assert_eq!(
            paseo_home(Some("/srv/p".into()), user),
            PathBuf::from("/srv/p")
        );
        assert_eq!(paseo_home(Some("".into()), user), user.join(".paseo"));
    }
}
