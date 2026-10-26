//! Removing one host's project (`project.remove.request`, `projectRemove`).
//! The daemon archives the project's open workspaces, their agents too,
//! and forgets the project; nothing on disk changes. Crusteo refuses while
//! an agent in those workspaces is working or waiting for input, which
//! Paseo does not.

use serde_json::Value;

use crate::{Bucket, Host};

/// `server_info.features` flag for `project.remove.request` (Paseo 0.1.97+).
pub const FEATURE: &str = "projectRemove";

pub fn supported(host: &Host) -> bool {
    host.has(FEATURE)
}

/// Whether a project can be removed from its host now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// It can: this many open workspaces are archived with it.
    Ready { name: String, workspaces: usize },
    /// Agents in its open workspaces are working or waiting for input.
    Busy { name: String, working: Vec<String> },
    /// Not known yet: the host is offline, or its workspace or agent list
    /// is still loading, so an agent at work there could be missed.
    Waiting { name: String, reason: &'static str },
}

/// What removing `project_id` would do on `host`; None if the host does
/// not list it.
pub fn check(host: &Host, project_id: &str) -> Option<Check> {
    let directory = &host.directory;
    let name = directory.projects.get(project_id)?.name();
    if !host.connection.is_online() {
        let reason = "Connect to this host first.";
        return Some(Check::Waiting { name, reason });
    }
    if !directory.hydrated || !directory.agents_hydrated {
        let reason = "Wait for the workspace and agent lists to finish loading.";
        return Some(Check::Waiting { name, reason });
    }
    let workspaces: Vec<_> = (directory.workspaces.values())
        .filter(|w| w.project_id == project_id)
        .collect();
    let working: Vec<String> = (workspaces.iter())
        .flat_map(|w| directory.workspace_agents(&w.id))
        .filter(|a| a.archived_at.is_none())
        .filter(|a| matches!(a.bucket(), Bucket::Running | Bucket::NeedsInput))
        .map(|a| format!("\"{}\"", a.display_title()))
        .collect();
    Some(if working.is_empty() {
        Check::Ready {
            name,
            workspaces: workspaces.len(),
        }
    } else {
        Check::Busy { name, working }
    })
}

impl Check {
    pub fn name(&self) -> &str {
        match self {
            Check::Ready { name, .. } | Check::Busy { name, .. } | Check::Waiting { name, .. } => {
                name
            }
        }
    }

    /// The confirmation's text, for removing it from `host`.
    pub fn message(&self, host: &str) -> String {
        match self {
            Check::Ready { name, workspaces } => {
                let archived = match workspaces {
                    0 => String::new(),
                    1 => " Its open workspace is archived (its agents too).".into(),
                    n => format!(" Its {n} open workspaces are archived (their agents too)."),
                };
                format!(
                    "Removes {name} from {host}'s project list.{archived} \
                     Files, worktrees and branches stay on disk."
                )
            }
            Check::Busy { working, .. } => {
                let count = match working.len() {
                    1 => "1 agent is".to_string(),
                    n => format!("{n} agents are"),
                };
                format!(
                    "{count} still working: {}; stop or archive them first.",
                    working.join(", ")
                )
            }
            Check::Waiting { reason, .. } => reason.to_string(),
        }
    }
}

/// `project.remove.response`: the workspaces it archived, or why it did not.
pub fn parse_reply(payload: &Value) -> Result<Vec<String>, String> {
    if payload["accepted"] == true {
        return Ok((payload["removedWorkspaceIds"].as_array().into_iter())
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect());
    }
    Err(payload["error"]
        .as_str()
        .filter(|e| !e.is_empty())
        .unwrap_or("The host did not remove the project")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Connection, HostConfig};
    use serde_json::json;

    /// An online host whose lists have loaded: one project with one
    /// workspace, holding agents with these titles and fields.
    fn host(statuses: &[(&str, Value)]) -> Host {
        let mut host = Host::new(HostConfig {
            name: "devbox".into(),
            ..Default::default()
        });
        host.connection = Connection::Online;
        host.directory.hydrated = true;
        host.directory.agents_hydrated = true;
        let dir = &mut host.directory;
        dir.apply_push(
            &json!({"type": "project.update", "payload": {"kind": "upsert",
            "project": {"projectId": "p1", "projectDisplayName": "app"}}}),
        );
        dir.apply_push(
            &json!({"type": "workspace_update", "payload": {"kind": "upsert",
            "workspace": {"id": "w1", "projectId": "p1", "name": "main", "status": "done"}}}),
        );
        for (i, (title, extra)) in statuses.iter().enumerate() {
            let mut agent = json!({"id": format!("a{i}"), "workspaceId": "w1", "title": title});
            agent
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            dir.apply_push(
                &json!({"type": "agent_update", "payload": {"kind": "upsert",
                "agent": agent}}),
            );
        }
        host
    }

    #[test]
    fn idle_agents_let_it_go_and_say_what_happens() {
        let check = check(&host(&[("tidy", json!({"status": "idle"}))]), "p1").unwrap();
        assert_eq!(
            check,
            Check::Ready {
                name: "app".into(),
                workspaces: 1
            }
        );
        assert_eq!(
            check.message("devbox"),
            "Removes app from devbox's project list. Its open workspace is archived (its \
             agents too). Files, worktrees and branches stay on disk."
        );
        assert!(super::check(&host(&[]), "nope").is_none());
    }

    #[test]
    fn running_or_waiting_agents_refuse() {
        let running = host(&[
            ("fix login", json!({"status": "running"})),
            (
                "asks",
                json!({"status": "idle", "requiresAttention": true, "attentionReason": "permission"}),
            ),
            ("done", json!({"status": "idle"})),
        ]);
        let check = check(&running, "p1").unwrap();
        let Check::Busy { working, .. } = &check else {
            panic!("{check:?}");
        };
        let mut working = working.clone();
        working.sort();
        assert_eq!(working, ["\"asks\"", "\"fix login\""]);
        assert!(
            check
                .message("devbox")
                .starts_with("2 agents are still working: ")
        );
        assert!(
            check
                .message("devbox")
                .ends_with("; stop or archive them first.")
        );
    }

    #[test]
    fn it_waits_until_the_host_is_online_and_fully_loaded() {
        // A working agent on a page not loaded yet must not read as Ready.
        let mut loading = host(&[("tidy", json!({"status": "idle"}))]);
        loading.directory.agents_hydrated = false;
        let check = check(&loading, "p1").unwrap();
        assert!(matches!(check, Check::Waiting { .. }), "{check:?}");
        assert!(check.message("devbox").contains("finish loading"));

        let mut workspaces_loading = host(&[]);
        workspaces_loading.directory.hydrated = false;
        assert!(matches!(
            super::check(&workspaces_loading, "p1"),
            Some(Check::Waiting { .. })
        ));

        let mut offline = host(&[]);
        offline.connection = Connection::Offline("gone".into());
        let check = super::check(&offline, "p1").unwrap();
        assert_eq!(check.message("devbox"), "Connect to this host first.");
        assert_eq!(check.name(), "app");
    }

    #[test]
    fn replies_are_read_as_paseo_sends_them() {
        let ok = json!({"requestId": "r", "projectId": "p1", "accepted": true,
                        "removedWorkspaceIds": ["w1", "w2"], "error": null});
        assert_eq!(parse_reply(&ok).unwrap(), ["w1", "w2"]);
        let refused = json!({"accepted": false, "removedWorkspaceIds": [],
                             "error": "Failed to remove project"});
        assert_eq!(
            parse_reply(&refused).unwrap_err(),
            "Failed to remove project"
        );
        assert!(parse_reply(&json!({})).is_err());
    }
}
