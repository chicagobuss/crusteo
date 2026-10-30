//! Agent CLIs in a process group of their own, ended with their process
//! tree, as Paseo ends them.
//!
//! An agent's CLI starts more processes: MCP servers, tool shells, dev
//! servers. Killing only the CLI leaves those running. On Unix the CLI leads
//! its own group, and [`Grouped::end`] (or dropping it) signals that group
//! and every process descended from the CLI, as Paseo's tree kill does:
//! `SIGTERM`, so the CLI can save its session, then `SIGKILL` [`KILL_AFTER`]
//! later for whatever is left. The tree matters as much as the group:
//! Claude Code runs each shell command in a session of its own, outside the
//! group. It has to be read while the CLI runs, since a CLI that has exited
//! has left its children to init, so the session's owner ends it as soon as
//! the session lets go, before the CLI sees its stdin close. Its own group
//! also keeps a Ctrl-C at Burrow's terminal from reaching the CLI. Windows
//! has no groups to signal; there the CLI is killed when it is let go of.

use std::ops::{Deref, DerefMut};
#[cfg(unix)]
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

/// How long the group has to exit after `SIGTERM` before `SIGKILL`.
pub const KILL_AFTER: Duration = Duration::from_secs(2);

/// Starts `command` as the leader of a new process group.
pub fn spawn(command: &mut Command) -> std::io::Result<Grouped> {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    command.kill_on_drop(true);
    let child = command.spawn()?;
    let group = child.id();
    Ok(Grouped { child, group })
}

/// A CLI started by [`spawn`]. Dropping it ends its process group.
pub struct Grouped {
    child: Child,
    #[cfg_attr(not(unix), allow(dead_code))]
    group: Option<u32>,
}

impl Deref for Grouped {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for Grouped {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Grouped {
    /// Ends the CLI with its process tree and group, now: what the session's
    /// owner calls as soon as the session is let go of. Later calls, and the
    /// drop, do nothing more.
    pub fn end(&mut self) {
        #[cfg(unix)]
        if let Some(leader) = self.group.take() {
            end(leader);
        }
        #[cfg(not(unix))]
        let _ = self.child.start_kill();
    }
}

impl Drop for Grouped {
    fn drop(&mut self) {
        self.end();
    }
}

/// `SIGTERM` to the leader's group and tree now, `SIGKILL` to the same after
/// [`KILL_AFTER`]. Processes already gone are skipped by `kill`.
#[cfg(unix)]
fn end(leader: u32) {
    let mut tree = descendants(leader);
    tree.push(leader);
    signal(leader, &tree, "TERM");
    std::thread::spawn(move || {
        std::thread::sleep(KILL_AFTER);
        signal(leader, &tree, "KILL");
    });
}

/// Every process descended from `root`, from `ps`.
#[cfg(unix)]
fn descendants(root: u32) -> Vec<u32> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().map(str::parse::<u32>);
            Some((fields.next()?.ok()?, fields.next()?.ok()?))
        })
        .collect();
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for &(pid, ppid) in &pairs {
            if ppid == parent && pid != root && !found.contains(&pid) {
                found.push(pid);
                frontier.push(pid);
            }
        }
    }
    found
}

/// `name` to the group `leader` leads and to each of `pids`.
#[cfg(unix)]
fn signal(leader: u32, pids: &[u32], name: &str) {
    let mut targets = vec![format!("-{leader}")];
    targets.extend(pids.iter().map(u32::to_string));
    let _ = std::process::Command::new("kill")
        .args(["-s", name, "--"])
        .args(&targets)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Running, and not a zombie waiting to be reaped.
    fn alive(pid: &str) -> bool {
        std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .is_ok_and(|out| {
                let stat = String::from_utf8_lossy(&out.stdout);
                !stat.trim().is_empty() && !stat.trim().starts_with('Z')
            })
    }

    /// The first line `cli` prints.
    async fn first_line(cli: &mut Grouped) -> String {
        use tokio::io::AsyncBufReadExt;
        let out = cli.stdout.take().unwrap();
        let mut lines = tokio::io::BufReader::new(out).lines();
        lines.next_line().await.unwrap().unwrap().trim().to_string()
    }

    /// A tool the CLI started in the background, which killing only the
    /// CLI would leave behind, goes with the group.
    #[tokio::test]
    async fn letting_go_ends_what_the_cli_started() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped());
        let mut cli = spawn(&mut command).unwrap();
        let tool = first_line(&mut cli).await;
        assert!(alive(&tool), "the tool runs: {tool:?}");
        drop(cli);
        for _ in 0..50 {
            if !alive(&tool) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the tool outlived its CLI");
    }

    /// A tool in a session of its own, as Claude Code runs shell commands,
    /// is outside the group but in the tree.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_tool_in_its_own_session_goes_with_the_tree() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "setsid sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped());
        let mut cli = spawn(&mut command).unwrap();
        let tool = first_line(&mut cli).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(alive(&tool), "the tool runs: {tool:?}");
        cli.end();
        for _ in 0..50 {
            if !alive(&tool) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the tool in its own session outlived its CLI");
    }

    /// One that ignores `SIGTERM` is killed after the grace period.
    #[tokio::test]
    async fn a_group_that_ignores_term_is_killed() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' TERM; echo $$; sleep 30"])
            .stdout(Stdio::piped());
        let mut cli = spawn(&mut command).unwrap();
        let shell = first_line(&mut cli).await;
        drop(cli);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(alive(&shell), "TERM is ignored");
        tokio::time::sleep(KILL_AFTER + Duration::from_secs(1)).await;
        assert!(!alive(&shell), "KILL is not");
    }
}
