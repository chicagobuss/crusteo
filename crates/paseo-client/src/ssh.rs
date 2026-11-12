//! Reaching a daemon through SSH, as Paseo's desktop app and CLI do. An
//! `ssh://[user@]host[:port][?daemonPort=N]` endpoint runs the system `ssh`
//! with `-W 127.0.0.1:<daemonPort>`, which makes ssh's standard input and
//! output a connection to the daemon's port on that host's loopback, and the
//! WebSocket runs over them. The user's keys, agent and `~/.ssh/config`
//! (aliases, `ProxyJump`, ports) all apply. `BatchMode` means it never asks
//! for a password, so a host needs a key that works without one. Nothing is
//! installed or started on the far side: the daemon must already run there.

use std::path::Path;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{oneshot, watch};

/// The daemon port an `ssh://` endpoint means when it names none: Paseo's.
pub const DEFAULT_DAEMON_PORT: u16 = 6767;

/// How much of ssh's standard error is kept, from the end.
const STDERR_LIMIT: usize = 8192;

/// How long a failed connection waits for ssh to exit and say why.
const EXIT_WAIT: Duration = Duration::from_secs(1);

/// Where an `ssh://` endpoint leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// What ssh is given: `host` or `user@host`, an alias from
    /// `~/.ssh/config` included.
    pub host: String,
    pub ssh_port: Option<u16>,
    /// The daemon's port on that host's loopback.
    pub daemon_port: u16,
}

impl Target {
    /// The target of an `ssh://` endpoint, or why it is not a valid one.
    /// `None` for an endpoint that is not `ssh://`. The rules are Paseo's: no
    /// password, no path, no fragment, and `daemonPort` the only option.
    pub fn parse(endpoint: &str) -> Option<Result<Target, String>> {
        let rest = endpoint.trim().strip_prefix("ssh://")?;
        Some(parse_rest(rest))
    }

    /// The arguments ssh runs with, Paseo's: no terminal, no prompts, no
    /// forwardings from the user's config, and give up when the daemon's
    /// port cannot be reached.
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ExitOnForwardFailure=yes",
        ]
        .map(String::from)
        .into();
        if let Some(port) = self.ssh_port {
            args.extend(["-p".to_string(), port.to_string()]);
        }
        args.extend([
            "-W".to_string(),
            format!("127.0.0.1:{}", self.daemon_port),
            self.host.clone(),
        ]);
        args
    }

    /// The URL the WebSocket handshake asks for: the daemon on the far
    /// host's loopback, which daemons accept as a Host.
    pub fn url(&self) -> String {
        format!("ws://127.0.0.1:{}/ws", self.daemon_port)
    }
}

fn parse_rest(rest: &str) -> Result<Target, String> {
    let invalid = || "Invalid SSH host URI".to_string();
    if rest.contains('#') {
        return Err("SSH host URI does not support fragments".into());
    }
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !path.is_empty() {
        return Err(invalid());
    }
    let (user, host_port) = match authority.rsplit_once('@') {
        Some((user, host_port)) => (Some(user), host_port),
        None => (None, authority),
    };
    let (host, ssh_port) = split_host_port(host_port).ok_or_else(invalid)?;
    let host = valid_host(host)?;
    let host = match user {
        // `user:password@` is refused: ssh takes no password this way.
        Some(user) if user.contains(':') => return Err(invalid()),
        Some("") | None => host,
        Some(user) => {
            let user = percent_decode(user).ok_or_else(invalid)?;
            valid_host(&format!("{user}@{host}"))?
        }
    };
    let ssh_port = ssh_port
        .map(|port| port_number(port, "SSH port"))
        .transpose()?;
    let mut daemon_port = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "daemonPort" {
            return Err(format!("Unsupported SSH host option: {key}"));
        }
        if daemon_port.is_some() {
            return Err("daemonPort may only be specified once".into());
        }
        daemon_port = Some(port_number(value, "Daemon port")?);
    }
    Ok(Target {
        host,
        ssh_port,
        daemon_port: daemon_port.unwrap_or(DEFAULT_DAEMON_PORT),
    })
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`, the brackets taken off.
fn split_host_port(text: &str) -> Option<(&str, Option<&str>)> {
    if let Some(inner) = text.strip_prefix('[') {
        let (host, after) = inner.split_once(']')?;
        return match after {
            "" => Some((host, None)),
            after => Some((host, Some(after.strip_prefix(':')?))),
        };
    }
    Some(match text.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (text, None),
    })
}

/// A host ssh can be given: present, one word, and not an option.
fn valid_host(host: &str) -> Result<String, String> {
    let host = host.trim();
    if host.is_empty() || host.starts_with('@') {
        return Err("SSH host is required".into());
    }
    if host.contains(char::is_whitespace) || host.starts_with('-') {
        return Err("SSH host is invalid".into());
    }
    Ok(host.to_string())
}

fn port_number(text: &str, label: &str) -> Result<u16, String> {
    match text.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(format!("{label} must be between 1 and 65535")),
    }
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// ssh's standard input and output, as one connection to the daemon. Dropping
/// it ends ssh.
pub struct Stream {
    stdin: ChildStdin,
    stdout: ChildStdout,
    _stop: oneshot::Sender<()>,
}

/// What ssh said and how it ended, kept after its [`Stream`] is gone.
#[derive(Clone)]
pub struct Watch {
    stderr: Arc<Mutex<String>>,
    /// `Some` once ssh has ended: its status, or `None` when it was stopped.
    ended: watch::Receiver<Option<Option<ExitStatus>>>,
}

/// Starts ssh to `target`: `program`, or `ssh` on PATH.
pub fn open(target: &Target, program: Option<&Path>) -> Result<(Stream, Watch), String> {
    let program = program.unwrap_or(Path::new("ssh"));
    let mut command = tokio::process::Command::new(program);
    command
        .args(target.args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // No console window for each connection.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            format!(
                "{} was not found: an ssh:// host needs an SSH client (OpenSSH) on PATH",
                program.display()
            )
        } else {
            format!("could not start {}: {err}", program.display())
        }
    })?;
    let (Some(stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("ssh started without its standard streams".into());
    };
    let said = Arc::new(Mutex::new(String::new()));
    let reader = tokio::spawn({
        let said = said.clone();
        async move {
            let mut chunk = [0u8; 1024];
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                let mut text = said.lock().unwrap_or_else(|p| p.into_inner());
                text.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if text.len() > STDERR_LIMIT {
                    let mut cut = text.len() - STDERR_LIMIT;
                    while !text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    text.drain(..cut);
                }
            }
        }
    });
    let (stop, stopped) = oneshot::channel::<()>();
    let (ended_tx, ended) = watch::channel(None);
    tokio::spawn(async move {
        let status = tokio::select! {
            status = child.wait() => status.ok(),
            // The stream was dropped.
            _ = stopped => {
                let _ = child.kill().await;
                None
            }
        };
        // What it said before it ended, all of it.
        let _ = tokio::time::timeout(EXIT_WAIT, reader).await;
        let _ = ended_tx.send(Some(status));
    });
    Ok((
        Stream {
            stdin,
            stdout,
            _stop: stop,
        },
        Watch {
            stderr: said,
            ended,
        },
    ))
}

impl Watch {
    /// Why ssh failed, once it has ended: what it said, else how it exited.
    /// `None` when it still runs after a moment, exited cleanly, or was
    /// stopped without a word.
    pub async fn failure(&self) -> Option<String> {
        let mut ended = self.ended.clone();
        let status = tokio::time::timeout(EXIT_WAIT, ended.wait_for(Option::is_some))
            .await
            .ok()?
            .ok()?
            .flatten();
        let said = self
            .stderr
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let said = summary(&said);
        if !said.is_empty() {
            return Some(said);
        }
        status
            .filter(|status| !status.success())
            .map(|status| format!("ssh exited with {status}"))
    }
}

/// ssh's standard error as one line: its lines joined, without the notice of
/// a host key it has just learned.
fn summary(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("Warning: Permanently added"))
        .collect::<Vec<_>>()
        .join("; ")
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(endpoint: &str) -> Result<Target, String> {
        Target::parse(endpoint).expect("an ssh:// endpoint")
    }

    #[test]
    fn only_ssh_endpoints_are_parsed() {
        assert!(Target::parse("192.0.2.20:6767").is_none());
        assert!(Target::parse("ws://box:6767/ws").is_none());
    }

    #[test]
    fn an_endpoint_names_user_host_and_ports() {
        assert_eq!(
            target("ssh://box"),
            Ok(Target {
                host: "box".into(),
                ssh_port: None,
                daemon_port: 6767
            })
        );
        assert_eq!(
            target("ssh://dev@box.example:2222?daemonPort=6797"),
            Ok(Target {
                host: "dev@box.example".into(),
                ssh_port: Some(2222),
                daemon_port: 6797
            })
        );
        assert_eq!(target("ssh://box/").unwrap().host, "box");
        assert_eq!(
            target("ssh://[2001:db8::1]:22").unwrap().host,
            "2001:db8::1"
        );
        assert_eq!(target("ssh://a%2Eb@box").unwrap().host, "a.b@box");
    }

    #[test]
    fn malformed_endpoints_say_why() {
        assert_eq!(target("ssh://"), Err("SSH host is required".into()));
        assert_eq!(
            target("ssh://-oProxyCommand=x"),
            Err("SSH host is invalid".into())
        );
        assert_eq!(
            target("ssh://dev:secret@box"),
            Err("Invalid SSH host URI".into())
        );
        assert_eq!(target("ssh://box/path"), Err("Invalid SSH host URI".into()));
        assert_eq!(
            target("ssh://box#x"),
            Err("SSH host URI does not support fragments".into())
        );
        assert_eq!(
            target("ssh://box?user=x"),
            Err("Unsupported SSH host option: user".into())
        );
        assert_eq!(
            target("ssh://box?daemonPort=1&daemonPort=2"),
            Err("daemonPort may only be specified once".into())
        );
        assert_eq!(
            target("ssh://box:0"),
            Err("SSH port must be between 1 and 65535".into())
        );
        assert_eq!(
            target("ssh://box?daemonPort=70000"),
            Err("Daemon port must be between 1 and 65535".into())
        );
    }

    #[test]
    fn ssh_runs_with_paseos_arguments() {
        let args = target("ssh://dev@box:2222?daemonPort=6797").unwrap().args();
        assert_eq!(
            args.join(" "),
            "-T -o BatchMode=yes -o ConnectTimeout=10 -o ClearAllForwardings=yes \
             -o ExitOnForwardFailure=yes -p 2222 -W 127.0.0.1:6797 dev@box"
        );
        assert_eq!(target("ssh://box").unwrap().url(), "ws://127.0.0.1:6767/ws");
    }

    #[test]
    fn stderr_is_summed_up_without_host_key_notices() {
        let said = "Warning: Permanently added 'box' (ED25519) to the list of known hosts.\r\n\
                    dev@box: Permission denied (publickey).\n";
        assert_eq!(summary(said), "dev@box: Permission denied (publickey).");
    }
}
