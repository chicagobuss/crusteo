//! `burrow status`: asks a running Burrow, over its own WebSocket, what it
//! is and what it runs. It does a real handshake and two requests, so a
//! daemon whose port answers but whose sessions fail reads as broken.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::client::{Client, WAIT};

/// What a running Burrow says about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// `host:port`, as asked.
    pub endpoint: String,
    pub server_id: String,
    pub hostname: Option<String>,
    pub version: String,
    pub pid: Option<u64>,
    pub started_at: Option<String>,
    /// Where its log goes, when that can be read (Linux, same user).
    pub log: Option<String>,
    /// Providers it offers.
    pub offered: Vec<String>,
    /// Providers it looked for and does not offer, with why.
    pub missing: Vec<(String, String)>,
    /// Agents by status (`idle`, `running`, …), archived ones left out.
    pub agents: BTreeMap<String, usize>,
    /// Agents waiting on a permission.
    pub asking: usize,
    /// Agents marked for review.
    pub attention: usize,
    /// For a Burrow on this machine (Linux): the desktop session bus its
    /// CLIs get, `Some(None)` when there is none. CLIs that keep their login
    /// in the desktop keyring (Antigravity's `agy`) need it.
    pub session_bus: Option<Option<String>>,
}

impl Report {
    pub fn total(&self) -> usize {
        self.agents.values().sum()
    }

    /// The report as `burrow status` prints it.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let host = self.hostname.as_deref().unwrap_or("this host");
        let at = if self.endpoint.starts_with("ssh://") {
            self.endpoint.clone()
        } else {
            format!("ws://{}/ws", self.endpoint)
        };
        out.push_str(&format!(
            "Burrow {} on {host}, answering at {at}\n",
            self.version
        ));
        let mut ids = vec![format!("server {}", self.server_id)];
        if let Some(pid) = self.pid {
            ids.push(format!("pid {pid}"));
        }
        if let Some(started) = &self.started_at {
            ids.push(format!("since {started}"));
        }
        out.push_str(&format!("  {}\n", ids.join(", ")));
        if let Some(log) = &self.log {
            out.push_str(&format!("  log: {log}\n"));
        }
        match &self.session_bus {
            Some(Some(bus)) => out.push_str(&format!("  desktop session bus for CLIs: {bus}\n")),
            Some(None) => out.push_str(
                "  no desktop session bus: CLIs that keep their login in the desktop keyring (agy) cannot read it until someone logs in to the desktop\n",
            ),
            None => {}
        }
        let offered = match self.offered.as_slice() {
            [] => "none".to_string(),
            some => some.join(", "),
        };
        out.push_str(&format!("Providers: {offered}\n"));
        for (id, why) in &self.missing {
            out.push_str(&format!("  not {id}: {why}\n"));
        }
        let mut counts: Vec<String> = (self.agents.iter())
            .map(|(status, n)| format!("{n} {status}"))
            .collect();
        if self.asking > 0 {
            counts.push(format!("{} asking permission", self.asking));
        }
        if self.attention > 0 {
            counts.push(format!("{} ready to review", self.attention));
        }
        out.push_str(&format!("Agents: {}", self.total()));
        if !counts.is_empty() {
            out.push_str(&format!(" ({})", counts.join(", ")));
        }
        out.push('\n');
        out
    }
}

/// Connects to `endpoint` (`host:port`), says hello as a CLI does, and asks
/// for the daemon's status and its agents.
pub async fn fetch(endpoint: &str) -> Result<Report, String> {
    let mut client = Client::connect(endpoint).await?;
    let url = client.url().to_string();
    let refused = |err: String| format!("{url} refused a request: {err}");
    let status = client
        .request(json!({"type": "daemon.get_status.request"}), Some(WAIT))
        .await
        .map_err(refused)?;
    let agents = client
        .request(json!({"type": "fetch_agents_request"}), Some(WAIT))
        .await
        .map_err(refused)?;
    let info = client.info.clone();
    client.close().await;
    let mut report = report(endpoint, &info, &status, &agents);
    // The same lookup Burrow makes for each CLI it starts, for a Burrow here.
    let here = ["127.", "localhost:", "[::1]:"]
        .iter()
        .any(|local| endpoint.starts_with(local));
    if cfg!(target_os = "linux") && here {
        report.session_bus = Some(burrow_provider::session_bus::address());
    }
    Ok(report)
}

/// The report from the three replies.
pub fn report(endpoint: &str, info: &Value, status: &Value, agents: &Value) -> Report {
    let text = |value: &Value| value.as_str().map(str::to_string);
    let mut offered = Vec::new();
    let mut missing = Vec::new();
    for entry in status["providers"].as_array().into_iter().flatten() {
        let Some(id) = text(&entry["provider"]) else {
            continue;
        };
        if entry["available"] == true {
            offered.push(id);
        } else {
            let why = text(&entry["error"]).unwrap_or_else(|| "not available".into());
            missing.push((id, why));
        }
    }
    let mut counts = BTreeMap::new();
    let (mut asking, mut attention) = (0, 0);
    for entry in agents["entries"].as_array().into_iter().flatten() {
        let agent = &entry["agent"];
        if !agent["archivedAt"].is_null() {
            continue;
        }
        let status = text(&agent["status"]).unwrap_or_else(|| "unknown".into());
        *counts.entry(status).or_insert(0) += 1;
        if agent["pendingPermissions"]
            .as_array()
            .is_some_and(|p| !p.is_empty())
        {
            asking += 1;
        } else if agent["requiresAttention"] == true {
            attention += 1;
        }
    }
    let pid = status["pid"].as_u64();
    Report {
        endpoint: endpoint.to_string(),
        server_id: text(&info["serverId"])
            .or_else(|| text(&status["serverId"]))
            .unwrap_or_default(),
        hostname: text(&info["hostname"]),
        version: text(&status["version"])
            .or_else(|| text(&info["version"]))
            .unwrap_or_default(),
        pid,
        started_at: text(&status["startedAt"]),
        log: pid.and_then(log_of),
        offered,
        missing,
        agents: counts,
        asking,
        attention,
        session_bus: None,
    }
}

/// Where process `pid`'s standard output goes: Burrow logs there.
#[cfg(target_os = "linux")]
fn log_of(pid: u64) -> Option<String> {
    let target = std::fs::read_link(format!("/proc/{pid}/fd/1")).ok()?;
    let shown = target.display().to_string();
    Some(if shown.starts_with("socket:") {
        // A systemd unit's output goes to the journal over a socket.
        "the service manager's journal (for a systemd unit: journalctl --user -u burrow)".into()
    } else if shown.ends_with(" (deleted)") {
        format!("{shown}: gone, so its log is lost; restart it under its service")
    } else {
        shown
    })
}

#[cfg(not(target_os = "linux"))]
fn log_of(_pid: u64) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_counts_agents_and_names_missing_providers() {
        let info = json!({"status": "server_info", "serverId": "srv_a", "hostname": "box",
                          "version": "0.13.0"});
        let status = json!({"serverId": "srv_a", "version": "0.1.0-burrow", "pid": null,
            "startedAt": "2026-11-08T00:00:00Z",
            "providers": [{"provider": "claude", "available": true, "error": null},
                          {"provider": "pi", "available": false, "error": "its CLI was not found"}]});
        let agents = json!({"entries": [
            {"agent": {"status": "idle", "requiresAttention": true, "pendingPermissions": []}},
            {"agent": {"status": "idle", "requiresAttention": false, "pendingPermissions": []}},
            {"agent": {"status": "running", "pendingPermissions": [{"id": "p"}]}},
            {"agent": {"status": "idle", "archivedAt": "2026-11-01T00:00:00Z"}},
        ]});
        let report = report("127.0.0.1:6767", &info, &status, &agents);
        assert_eq!(
            report.version, "0.1.0-burrow",
            "the daemon's own version wins"
        );
        assert_eq!(report.offered, ["claude"]);
        assert_eq!(
            report.missing,
            [("pi".to_string(), "its CLI was not found".to_string())]
        );
        assert_eq!(report.total(), 3, "archived agents are left out");
        assert_eq!((report.asking, report.attention), (1, 1));
        let text = report.render();
        assert!(
            text.starts_with("Burrow 0.1.0-burrow on box, answering at ws://127.0.0.1:6767/ws\n"),
            "{text}"
        );
        assert!(text.contains("  not pi: its CLI was not found\n"), "{text}");
        assert!(
            text.contains(
                "Agents: 3 (2 idle, 1 running, 1 asking permission, 1 ready to review)\n"
            ),
            "{text}"
        );
        assert!(
            !text.contains("session bus"),
            "only for a Burrow here: {text}"
        );
        let mut here = report.clone();
        here.session_bus = Some(Some("unix:path=/run/user/1000/bus".into()));
        assert!(
            (here.render())
                .contains("  desktop session bus for CLIs: unix:path=/run/user/1000/bus\n")
        );
        here.session_bus = Some(None);
        assert!(
            here.render()
                .contains("  no desktop session bus: CLIs that keep their login")
        );
    }
}
