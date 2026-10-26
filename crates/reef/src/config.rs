//! What Reef reads at start: a hosts file in crusteo's `hosts.json`
//! format, and its own data dir, which holds its client id and the device
//! tokens phones sign in with. Reef never reads crusteo's own
//! `client-id.json`: its files have names of their own. Those crusteo-hub
//! left are moved into place by [`crate::legacy`].

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use fleet::HostConfig;
use serde::{Deserialize, Serialize};

use crate::proto::Limits;

pub const DEFAULT_PORT: u16 = 6798;
/// The data dir's name beside the hosts file, when `--data-dir` is not given.
pub const DATA_DIR: &str = "reef-data";
const CLIENT_ID_FILE: &str = "reef-client-id.json";
const TOKENS_FILE: &str = "reef-tokens.json";

/// One phone, or any other device, allowed to connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    pub token: String,
}

/// Everything [`crate::start`] needs. The binary builds it from the command
/// line; tests build it directly.
#[derive(Debug, Clone)]
pub struct ReefConfig {
    pub hosts: Vec<HostConfig>,
    /// Sent to every daemon as this Reef's identity.
    pub client_id: String,
    pub devices: Vec<Device>,
    /// Any phone that can reach Reef may use it, token or not: for
    /// networks where reaching Reef at all is the lock.
    pub open: bool,
    pub bind: SocketAddr,
    pub limits: Limits,
    /// Where settings are saved; None keeps them in memory.
    pub data_dir: Option<PathBuf>,
    /// Where hosts added or removed from a phone are saved; None keeps
    /// them in memory.
    pub hosts_file: Option<PathBuf>,
    /// DNS names Reef answers to, besides IP addresses and localhost.
    /// Any other Host header is refused, so a page on another site that
    /// rebinds its name to Reef's address cannot reach it.
    pub host_names: Vec<String>,
    /// A usage feed's `http://` base URL, read for the `usage` view
    /// besides the daemons' own reports. None reads only the daemons.
    pub usage: Option<String>,
}

impl ReefConfig {
    pub fn new(hosts: Vec<HostConfig>, client_id: impl Into<String>, devices: Vec<Device>) -> Self {
        Self {
            hosts,
            client_id: client_id.into(),
            devices,
            open: false,
            bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            limits: Limits::default(),
            data_dir: None,
            hosts_file: None,
            host_names: Vec::new(),
            usage: None,
        }
    }
}

/// The command line.
#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub hosts_file: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub bind: SocketAddr,
    /// A device token given on the command line, used instead of the
    /// tokens file.
    pub token: Option<String>,
    /// No device tokens; see [`ReefConfig::open`].
    pub open: bool,
    /// Serve a made-up fleet from in-process fake daemons.
    pub demo: bool,
    /// See [`ReefConfig::host_names`].
    pub host_names: Vec<String>,
    /// See [`ReefConfig::usage`].
    pub usage: Option<String>,
    /// Print the version and stop.
    pub version: bool,
}

pub const USAGE: &str = "\
usage: reef <hosts.json> [options]
       reef --demo [options]

options:
  --port <n>        port to listen on (default 6798)
  --bind <addr>     address to listen on (default 127.0.0.1)
  --data-dir <dir>  where Reef keeps its client id and device tokens
                    (default: a `reef-data` folder next to the hosts file;
                    a `hub` folder there, from crusteo-hub, becomes it)
  --token <token>   accept this device token instead of the tokens file
  --open            no device tokens: anyone who can reach Reef can use it
  --host-name <name>  a DNS name Reef is reached by, such as the one
                    `tailscale serve` gives it; repeat for more. IP
                    addresses and localhost always work.
  --usage <url>     also show account usage from this usage feed (an
                    http:// base URL serving /v1/usage), besides what the
                    daemons report
  --demo           serve a made-up fleet; no daemon is contacted
  -V, --version     print the version and commit, and stop
";

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        hosts_file: None,
        data_dir: None,
        bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
        token: None,
        open: false,
        demo: false,
        host_names: Vec::new(),
        usage: None,
        version: false,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--port" => {
                let port = value("--port")?;
                let port = port.parse().map_err(|_| format!("bad port: {port}"))?;
                parsed.bind.set_port(port);
            }
            "--bind" => {
                let ip = value("--bind")?;
                let ip = ip.parse().map_err(|_| format!("bad address: {ip}"))?;
                parsed.bind.set_ip(ip);
            }
            "--data-dir" => parsed.data_dir = Some(PathBuf::from(value("--data-dir")?)),
            "--token" => parsed.token = Some(value("--token")?),
            "--open" => parsed.open = true,
            "--host-name" => parsed
                .host_names
                .push(value("--host-name")?.to_ascii_lowercase()),
            "--demo" => parsed.demo = true,
            "--usage" => {
                let url = value("--usage")?;
                if !url.starts_with("http://") {
                    return Err(format!("--usage needs an http:// URL, not {url}"));
                }
                parsed.usage = Some(url.trim_end_matches('/').to_string());
            }
            "-V" | "--version" => parsed.version = true,
            "-h" | "--help" => return Err(String::new()),
            other if other.starts_with("--") => return Err(format!("unknown option {other}")),
            path => {
                if parsed.hosts_file.is_some() {
                    return Err(format!("one hosts file only, not also {path}"));
                }
                parsed.hosts_file = Some(PathBuf::from(path));
            }
        }
    }
    if parsed.hosts_file.is_none() && !parsed.demo && !parsed.version {
        return Err("no hosts file given".into());
    }
    Ok(parsed)
}

impl Args {
    /// The data dir: given, else a `reef-data` folder beside the hosts file.
    /// Not `reef`: the binary is usually installed beside the hosts file too.
    pub fn data_dir(&self) -> PathBuf {
        if let Some(dir) = &self.data_dir {
            return dir.clone();
        }
        match self.hosts_file.as_deref().and_then(Path::parent) {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join(DATA_DIR),
            _ => PathBuf::from(DATA_DIR),
        }
    }
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    // Windows editors and PowerShell often save UTF-8 with a byte-order
    // mark, which JSON parsers refuse.
    let json = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    serde_json::from_slice(json).map_err(|err| format!("{}: {err}", path.display()))
}

pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|err| format!("{}: {err}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|err| format!("{}: {err}", path.display()))
}

/// Saves the hosts, in crusteo's format, replacing the file whole.
pub fn save_hosts(path: &Path, hosts: &[HostConfig]) -> Result<(), String> {
    write_json(path, &hosts)
}

/// The saved hosts, in crusteo's format. Each name must pass
/// [`check_host_name`] and be used once, since Reef keys agents by
/// `host/agent` and finds hosts by name. Each host's id becomes its name
/// (see [`host_id`]).
pub fn read_hosts(path: &Path) -> Result<Vec<HostConfig>, String> {
    let mut hosts: Vec<HostConfig> = read_json(path)?;
    let mut seen = std::collections::HashSet::new();
    for (index, host) in hosts.iter().enumerate() {
        let entry = format!("{}: host {} ({:?})", path.display(), index + 1, host.name);
        check_host_name(&host.name).map_err(|problem| format!("{entry}: {problem}"))?;
        if !seen.insert(host.name.as_str()) {
            return Err(format!("{entry}: another host has this name"));
        }
    }
    for host in &mut hosts {
        host.id = host_id(&host.name);
    }
    Ok(hosts)
}

/// The id of Reef's host called `name`. Until Skiff's protocol v1 (stage 8
/// of the beta plan), Reef and its phones know hosts by name, which Reef
/// cannot change and keeps unique, so every Reef host's id is its name and
/// this is how Reef turns a name into one.
pub fn host_id(name: &str) -> fleet::HostId {
    fleet::HostId::new(name)
}

/// A host name Reef can use: not empty, up to 64 characters, and without a
/// slash, which would make `host/agent` keys ambiguous.
pub fn check_host_name(name: &str) -> Result<(), &'static str> {
    if name.trim().is_empty() || name.chars().count() > 64 || name.contains('/') {
        return Err("A host needs a name, up to 64 characters, without a slash");
    }
    Ok(())
}

/// Reef's own client id, made on first start.
pub fn client_id(data_dir: &Path) -> Result<String, String> {
    let path = data_dir.join(CLIENT_ID_FILE);
    if let Ok(id) = read_json::<String>(&path)
        && !id.trim().is_empty()
    {
        return Ok(id);
    }
    let id = paseo_client::generate_client_id();
    write_json(&path, &id)?;
    Ok(id)
}

/// The device tokens, and the device made now if there was none yet, so
/// its token can be shown once.
pub fn devices(data_dir: &Path) -> Result<(Vec<Device>, Option<Device>), String> {
    let path = data_dir.join(TOKENS_FILE);
    if path.exists() {
        let devices: Vec<Device> = read_json(&path)?;
        let devices: Vec<Device> = devices
            .into_iter()
            .filter(|device| !device.token.trim().is_empty())
            .collect();
        if !devices.is_empty() {
            return Ok((devices, None));
        }
    }
    let device = Device {
        name: "phone".into(),
        token: new_token()?,
    };
    write_json(&path, &vec![device.clone()])?;
    Ok((vec![device.clone()], Some(device)))
}

pub fn tokens_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKENS_FILE)
}

/// 32 random bytes from the operating system, as hex.
pub fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| format!("no random source: {err}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn the_command_line_reads_a_hosts_file_and_options() {
        let parsed = args(&["hosts.json", "--port", "7000", "--bind", "0.0.0.0"]).unwrap();
        let named = args(&[
            "h.json",
            "--host-name",
            "Reef.Example.test",
            "--host-name",
            "b.test",
        ]);
        assert_eq!(named.unwrap().host_names, ["reef.example.test", "b.test"]);
        assert_eq!(parsed.hosts_file.as_deref(), Some(Path::new("hosts.json")));
        assert_eq!(parsed.bind, "0.0.0.0:7000".parse().unwrap());
        assert!(args(&[]).is_err(), "a hosts file is needed");
        assert!(args(&["--demo"]).unwrap().demo);
        assert!(
            args(&["--version"]).unwrap().version,
            "no hosts file needed"
        );
        assert!(args(&["-V"]).unwrap().version);
        assert!(!parsed.version);
        assert!(!parsed.open, "tokens unless asked otherwise");
        assert!(args(&["hosts.json", "--open"]).unwrap().open);
        assert!(args(&["a.json", "b.json"]).is_err());
        assert!(args(&["a.json", "--port"]).is_err());
        let usage = args(&["a.json", "--usage", "http://feed.example.test:8080/"]).unwrap();
        assert_eq!(
            usage.usage.as_deref(),
            Some("http://feed.example.test:8080")
        );
        assert!(args(&["a.json", "--usage", "https://feed.example.test"]).is_err());
        assert_eq!(parsed.usage, None);
    }

    #[test]
    fn the_data_dir_defaults_beside_the_hosts_file() {
        let parsed = args(&["conf/hosts.json"]).unwrap();
        assert_eq!(parsed.data_dir(), Path::new("conf").join("reef-data"));
        let parsed = args(&["hosts.json", "--data-dir", "elsewhere"]).unwrap();
        assert_eq!(parsed.data_dir(), Path::new("elsewhere"));
    }

    #[test]
    fn hosts_files_with_a_byte_order_mark_still_read() {
        let dir = std::env::temp_dir().join(format!("reef-test-{}", new_token().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hosts.json");
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend_from_slice(br#"[{"name": "example-host", "endpoint": "127.0.0.1:6767"}]"#);
        fs::write(&path, bytes).unwrap();
        let hosts = read_hosts(&path).unwrap();
        assert_eq!(hosts[0].name, "example-host");
        assert_eq!(hosts[0].endpoint.as_deref(), Some("127.0.0.1:6767"));

        // Names Reef could not key agents by stop it at start, saying which.
        for (hosts, problem) in [
            (
                r#"[{"name": "a/b"}]"#,
                "host 1 (\"a/b\"): A host needs a name",
            ),
            (r#"[{"name": " "}]"#, "host 1 (\" \"): A host needs a name"),
            (
                r#"[{"name": "a"}, {"name": "a"}]"#,
                "host 2 (\"a\"): another host",
            ),
        ] {
            fs::write(&path, hosts).unwrap();
            let err = read_hosts(&path).unwrap_err();
            assert!(err.contains(problem), "{err}");
        }

        // The client id and tokens are made once and then kept.
        let id = client_id(&dir).unwrap();
        assert!(id.starts_with("cid_"));
        assert_eq!(client_id(&dir).unwrap(), id);
        let (first, made) = devices(&dir).unwrap();
        assert!(made.is_some());
        assert_eq!(first[0].token.len(), 64);
        let (again, made) = devices(&dir).unwrap();
        assert!(made.is_none());
        assert_eq!(again, first);
        let _ = fs::remove_dir_all(&dir);
    }
}
