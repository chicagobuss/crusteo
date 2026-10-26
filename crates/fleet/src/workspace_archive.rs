//! The affected live agents and worktree risks for a workspace archive.
use crate::{Bucket, Fleet, Host, HostId, Workspace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveAgent {
    pub id: String,
    pub title: String,
    pub activity: Option<ArchiveActivity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveActivity {
    Running,
    NeedsInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorktreeRisks {
    pub dirty: bool,
    pub diff: Option<(u64, u64)>,
    pub unpushed: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePlan {
    pub host: HostId,
    /// The host's name, for the confirmation.
    pub host_name: String,
    pub workspace: String,
    pub name: String,
    pub source_agent: Option<String>,
    pub agents: Vec<ArchiveAgent>,
    pub deletes_worktree: bool,
    pub worktree_risks: WorktreeRisks,
}

/// Local deduplication state. Attempts distinguish late failures/deadlines
/// from a newer request for the same workspace.
#[derive(Default)]
pub struct PendingArchives {
    attempts: std::collections::BTreeMap<(HostId, String), u64>,
    next: u64,
}

impl PendingArchives {
    pub fn contains(&self, host: &HostId, workspace: &str) -> bool {
        self.attempts
            .keys()
            .any(|(h, w)| h == host && w == workspace)
    }

    pub fn begin(&mut self, host: &HostId, workspace: &str) -> Option<u64> {
        if self.contains(host, workspace) {
            return None;
        }
        self.next += 1;
        self.attempts
            .insert((host.clone(), workspace.into()), self.next);
        Some(self.next)
    }

    /// Used by both failure and deadline; only the owning attempt can release.
    pub fn release(&mut self, host: &HostId, workspace: &str, attempt: u64) -> bool {
        let key = (host.clone(), workspace.to_string());
        if self.attempts.get(&key) != Some(&attempt) {
            return false;
        }
        self.attempts.remove(&key);
        true
    }

    pub fn retain_unsettled(&mut self, fleet: &Fleet) {
        self.attempts
            .retain(|(host, workspace), _| !archive_settled(fleet, host, workspace));
    }
}

/// Whether a configured host's authoritative directory has observed the archive.
/// Removing the host also releases client-local bookkeeping.
pub fn archive_settled(fleet: &Fleet, host: &HostId, workspace: &str) -> bool {
    fleet.host(host).is_none_or(|host| {
        host.connection.is_online()
            && host.directory.hydrated
            && host
                .directory
                .workspaces
                .get(workspace)
                .is_none_or(|workspace| workspace.archiving_at.is_some())
    })
}

/// Validate without scanning members; safe to use while rendering an action.
pub fn archive_target<'a>(
    fleet: &'a Fleet,
    host: &HostId,
    workspace: &str,
    source_agent: Option<&str>,
) -> Result<(&'a Host, &'a Workspace), &'static str> {
    let host = fleet
        .host(host)
        .ok_or("This host is no longer available.")?;
    if !host.connection.is_online() {
        return Err("Connect to this host before archiving.");
    }
    if !host.directory.hydrated || !host.directory.agents_hydrated {
        return Err("Wait for the workspace and agent lists to finish loading.");
    }
    let workspace = host
        .directory
        .workspaces
        .get(workspace)
        .ok_or("This workspace is no longer available.")?;
    if workspace.archiving_at.is_some() {
        return Err("This workspace is already being archived.");
    }
    if let Some(id) = source_agent
        && !host.directory.agents.get(id).is_some_and(|agent| {
            agent.archived_at.is_none() && agent.workspace_id.as_deref() == Some(&workspace.id)
        })
    {
        return Err("This agent no longer belongs to this workspace. Open the action again.");
    }
    Ok((host, workspace))
}

impl ArchivePlan {
    pub fn build(
        fleet: &Fleet,
        host: &HostId,
        workspace: &str,
        source_agent: Option<&str>,
    ) -> Result<Self, &'static str> {
        let (target_host, workspace) = archive_target(fleet, host, workspace, source_agent)?;
        let directory = &target_host.directory;
        let mut agents: Vec<_> = directory
            .workspace_agents(&workspace.id)
            .into_iter()
            .filter(|agent| agent.archived_at.is_none())
            .map(|agent| ArchiveAgent {
                id: agent.id.clone(),
                title: agent.display_title(),
                activity: match agent.bucket() {
                    Bucket::Running => Some(ArchiveActivity::Running),
                    Bucket::NeedsInput => Some(ArchiveActivity::NeedsInput),
                    _ => None,
                },
            })
            .collect();
        // Confirmation equality ignores timestamps and directory iteration order.
        agents.sort_by(|a, b| a.id.cmp(&b.id));
        let deletes_worktree = workspace.workspace_kind == "worktree";
        let mut worktree_risks = WorktreeRisks::default();
        if deletes_worktree && let Some(git) = &workspace.git_runtime {
            worktree_risks.dirty = git.is_dirty == Some(true);
            if worktree_risks.dirty {
                worktree_risks.diff = workspace
                    .diff_stat
                    .as_ref()
                    .map(|diff| (diff.additions, diff.deletions));
            }
            worktree_risks.unpushed = git.ahead_of_origin.unwrap_or(0).max(0);
        }
        let name = workspace
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&workspace.name);
        Ok(Self {
            host: host.clone(),
            host_name: target_host.name().to_string(),
            workspace: workspace.id.clone(),
            name: if name.trim().is_empty() {
                workspace.id.clone()
            } else {
                name.into()
            },
            source_agent: source_agent.map(str::to_string),
            agents,
            deletes_worktree,
            worktree_risks,
        })
    }

    /// Ask again only for newly affected agents or increased destructive risk.
    /// Received timestamp, rename, and safer activity changes need no new consent.
    pub fn needs_reconfirm(&self, latest: &Self) -> bool {
        if latest.deletes_worktree && !self.deletes_worktree {
            return true;
        }
        if latest.agents.iter().any(|agent| {
            self.agents
                .iter()
                .find(|old| old.id == agent.id)
                .is_none_or(|old| old.activity.is_none() && agent.activity.is_some())
        }) {
            return true;
        }
        let (old, new) = (&self.worktree_risks, &latest.worktree_risks);
        if new.unpushed > old.unpushed || (new.dirty && !old.dirty) {
            return true;
        }
        new.dirty
            && match (old.diff, new.diff) {
                (Some((a, d)), Some((na, nd))) => na > a || nd > d,
                (Some(_), None) => true, // The extent of the remaining changes is unknown.
                _ => false,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Agent, Connection, Directory, Host, HostConfig};
    use serde_json::json;
    use std::collections::{BTreeMap, HashMap};

    fn h(id: &str) -> HostId {
        HostId::new(id)
    }

    fn agent(id: &str, workspace: &str, running: bool) -> Agent {
        Agent::from_value(json!({
            "id": id, "workspaceId": workspace, "title": id,
            "status": if running { "running" } else { "done" },
            "updatedAt": "2026-10-04T00:00:00Z",
            "activeTurn": if running { json!({"phase":"open"}) } else { json!(null) }
        }))
        .unwrap()
    }
    fn fixture() -> Fleet {
        let workspace: Workspace = serde_json::from_value(json!({
            "id":"w", "name":"w", "title":"Workspace title", "workspaceKind":"worktree",
            "gitRuntime":{"isDirty":true,"aheadOfOrigin":2},
            "diffStat":{"additions":12,"deletions":3}
        }))
        .unwrap();
        let mut archived = agent("archived", "w", false);
        archived.archived_at = Some("2026-10-04T01:00:00Z".into());
        let mut child = agent("child", "w", true);
        child.parent_agent_id = Some("current".into());
        let directory = Directory {
            workspaces: BTreeMap::from([("w".into(), workspace)]),
            agents: HashMap::from([
                ("current".into(), agent("current", "w", false)),
                ("other".into(), agent("other", "w", true)),
                ("child".into(), child),
                ("foreign".into(), agent("foreign", "different", true)),
                ("archived".into(), archived),
            ]),
            hydrated: true,
            agents_hydrated: true,
            ..Default::default()
        };
        let host = Host {
            config: HostConfig {
                id: h("mac"),
                name: "mac".into(),
                ..Default::default()
            },
            connection: Connection::Online,
            directory,
            ..Default::default()
        };
        let mut foreign_host = host.clone();
        foreign_host.config.id = h("windows");
        foreign_host.config.name = "windows".into();
        foreign_host.directory.agents.insert(
            "unique-foreign-host".into(),
            agent("unique-foreign-host", "w", true),
        );
        Fleet {
            labels: None,
            hosts: vec![host, foreign_host],
        }
    }

    #[test]
    fn includes_every_live_member_on_only_the_target_host_and_preserves_worktree_risks() {
        let fleet = fixture();
        let plan = ArchivePlan::build(&fleet, &h("mac"), "w", Some("current")).unwrap();
        assert_eq!(
            plan.agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec!["child", "current", "other"]
        );
        assert_eq!(
            plan.agents
                .iter()
                .filter(|agent| agent.activity.is_some())
                .count(),
            2
        );
        assert_eq!(plan.name, "Workspace title");
        assert_eq!(
            plan.worktree_risks,
            WorktreeRisks {
                dirty: true,
                diff: Some((12, 3)),
                unpushed: 2
            }
        );
        assert_eq!(plan.host, h("mac"));
        assert_eq!(plan.workspace, "w");
        assert_eq!(plan.source_agent.as_deref(), Some("current"));
        let mut plain = fleet;
        plain.hosts[0]
            .directory
            .workspaces
            .get_mut("w")
            .unwrap()
            .workspace_kind = "directory".into();
        let plan = ArchivePlan::build(&plain, &h("mac"), "w", None).unwrap();
        assert!(!plan.deletes_worktree);
        assert_eq!(plan.worktree_risks, WorktreeRisks::default());
        assert!(plan.source_agent.is_none());
    }

    #[test]
    fn confirmation_changes_for_new_members_and_git_risks_not_timestamp_noise() {
        let mut fleet = fixture();
        let original = ArchivePlan::build(&fleet, &h("mac"), "w", Some("current")).unwrap();
        fleet.hosts[0]
            .directory
            .agents
            .get_mut("other")
            .unwrap()
            .updated_at = Some("2026-10-04T12:00:00Z".into());
        assert_eq!(
            ArchivePlan::build(&fleet, &h("mac"), "w", Some("current")).unwrap(),
            original
        );
        fleet.hosts[0]
            .directory
            .agents
            .insert("new".into(), agent("new", "w", false));
        assert!(original.needs_reconfirm(
            &ArchivePlan::build(&fleet, &h("mac"), "w", Some("current")).unwrap()
        ));
        fleet.hosts[0].directory.agents.remove("new");
        fleet.hosts[0]
            .directory
            .workspaces
            .get_mut("w")
            .unwrap()
            .git_runtime
            .as_mut()
            .unwrap()
            .ahead_of_origin = Some(3);
        assert!(original.needs_reconfirm(
            &ArchivePlan::build(&fleet, &h("mac"), "w", Some("current")).unwrap()
        ));
    }

    #[test]
    fn safer_changes_do_not_repeat_confirmation_but_new_active_risks_do() {
        let original = ArchivePlan::build(&fixture(), &h("mac"), "w", Some("current")).unwrap();
        let mut safer = original.clone();
        safer.agents.retain(|agent| agent.id != "child");
        for agent in &mut safer.agents {
            agent.activity = None;
            agent.title = "Renamed".into();
        }
        safer.worktree_risks = WorktreeRisks::default();
        safer.deletes_worktree = false;
        assert!(!original.needs_reconfirm(&safer));
        assert!(safer.needs_reconfirm(&original));
        let mut active = safer.clone();
        active.agents[0].activity = Some(ArchiveActivity::NeedsInput);
        assert!(safer.needs_reconfirm(&active));
        let mut larger = original.clone();
        larger.worktree_risks.diff = Some((12, 4));
        assert!(original.needs_reconfirm(&larger));
    }

    #[test]
    fn pending_requests_deduplicate_and_old_failure_or_deadline_cannot_release_new_attempt() {
        let mut pending = PendingArchives::default();
        let first = pending.begin(&h("mac"), "w").unwrap();
        assert!(pending.begin(&h("mac"), "w").is_none());
        assert!(pending.begin(&h("windows"), "w").is_some());
        pending.retain_unsettled(&fixture());
        assert!(pending.contains(&h("mac"), "w"));
        assert!(pending.release(&h("mac"), "w", first));
        assert!(!pending.release(&h("mac"), "w", first));
        let second = pending.begin(&h("mac"), "w").unwrap();
        assert_ne!(first, second);
        assert!(!pending.release(&h("mac"), "w", first));
        assert!(pending.contains(&h("mac"), "w"));
        let mut fleet = fixture();
        fleet.hosts[0].directory.workspaces.remove("w");
        pending.retain_unsettled(&fleet);
        assert!(!pending.contains(&h("mac"), "w"));
        assert!(pending.contains(&h("windows"), "w"));
        fleet.hosts.clear();
        pending.retain_unsettled(&fleet);
        assert!(!pending.contains(&h("windows"), "w"));
    }

    #[test]
    fn guard_settles_only_on_authoritative_archive_or_removed_host() {
        let mut fleet = fixture();
        assert!(!archive_settled(&fleet, &h("mac"), "w"));
        assert!(archive_settled(&fleet, &h("removed"), "w"));
        fleet.hosts[0]
            .directory
            .workspaces
            .get_mut("w")
            .unwrap()
            .archiving_at = Some("now".into());
        assert!(archive_settled(&fleet, &h("mac"), "w"));
        fleet.hosts[0].directory.workspaces.remove("w");
        assert!(archive_settled(&fleet, &h("mac"), "w"));
        fleet.hosts[0].directory.hydrated = false;
        assert!(!archive_settled(&fleet, &h("mac"), "w"));
        fleet.hosts[0].directory.hydrated = true;
        fleet.hosts[0].connection = Connection::Offline("offline".into());
        assert!(!archive_settled(&fleet, &h("mac"), "w"));
    }

    #[test]
    fn refuses_stale_source_incomplete_directory_disconnected_and_archiving_targets() {
        let fleet = fixture();
        assert!(archive_target(&fleet, &h("mac"), "w", Some("foreign")).is_err());
        assert!(archive_target(&fleet, &h("mac"), "w", Some("archived")).is_err());
        assert!(archive_target(&fleet, &h("missing"), "w", None).is_err());
        assert!(archive_target(&fleet, &h("mac"), "missing", None).is_err());
        let mut changed = fleet.clone();
        changed.hosts[0].directory.agents_hydrated = false;
        assert!(archive_target(&changed, &h("mac"), "w", None).is_err());
        changed.hosts[0].directory.agents_hydrated = true;
        changed.hosts[0].directory.hydrated = false;
        assert!(archive_target(&changed, &h("mac"), "w", None).is_err());
        changed.hosts[0].directory.hydrated = true;
        changed.hosts[0].connection = Connection::Offline("disconnected".into());
        assert!(archive_target(&changed, &h("mac"), "w", None).is_err());
        changed.hosts[0].connection = Connection::Online;
        changed.hosts[0]
            .directory
            .workspaces
            .get_mut("w")
            .unwrap()
            .archiving_at = Some("now".into());
        assert!(archive_target(&changed, &h("mac"), "w", None).is_err());
    }
}
