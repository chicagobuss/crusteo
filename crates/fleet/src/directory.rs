//! One host's projects, workspaces, agents and labels, kept current from
//! directory-sync replies and subscription pushes.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use paseo_client::proto::{Agent, Bucket, Cursor, Project, SyncMeta, Workspace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelDef {
    pub name: String,
    pub color: String,
}

/// Subscriptions the daemon assigned us. They die with the socket.
#[derive(Debug, Clone, Default)]
pub struct Subscriptions {
    pub workspaces: Option<String>,
    pub agents: Option<String>,
    pub labels: Option<String>,
    pub events: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Directory {
    pub workspaces: BTreeMap<String, Workspace>,
    pub projects: BTreeMap<String, Project>,
    pub agents: HashMap<String, Agent>,
    pub labels: Vec<LabelDef>,
    /// Where the last fetch reply left each list. Pushes never move them:
    /// a daemon numbers changes for every client at once, and a change made
    /// by another client's fetch (an agent it no longer lists) is never
    /// pushed, so a cursor moved past it would skip that removal for good.
    pub workspace_cursor: Option<Cursor>,
    pub agent_cursor: Option<Cursor>,
    pub project_cursor: Option<Cursor>,
    pub subscriptions: Subscriptions,
    /// True once the first workspace snapshot landed.
    pub hydrated: bool,
    /// True once every page of the agent list landed. Tab reconciliation
    /// waits for this, or it would close tabs of agents not loaded yet.
    pub agents_hydrated: bool,
    /// Daemons without `projectList` get projects synthesized from workspaces.
    pub projects_from_workspaces: bool,
    /// What reducers changed since the owner last drained this, so views can
    /// patch what they derived instead of re-deriving it from scratch.
    pub journal: Vec<DirChange>,
}

/// One thing a reducer changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirChange {
    AgentAdded(String),
    /// An existing agent changed in place: status, title, attention.
    AgentUpdated(String),
    /// An existing agent changed workspace or parent, which moves it
    /// between lanes and changes which agents are a workspace's roots.
    AgentMoved(String),
    /// Removed or archived.
    AgentRemoved(String),
    /// A workspace appeared, changed (labels, name, project) or went away.
    Workspace(String),
    /// A workspace changed only in details nothing derives membership or
    /// titles from (status, git state, scripts).
    WorkspaceDetail(String),
    /// Anything broader: a snapshot page, projects, label definitions.
    Bulk,
}

/// True when two versions of a workspace differ only in live status: its
/// status, diff, git and GitHub state, and scripts.
fn same_shape(a: &Workspace, b: &Workspace) -> bool {
    a.id == b.id
        && a.project_id == b.project_id
        && a.project_display_name == b.project_display_name
        && a.project_custom_name == b.project_custom_name
        && a.project_root_path == b.project_root_path
        && a.workspace_directory == b.workspace_directory
        && a.worktree_slug == b.worktree_slug
        && a.project_kind == b.project_kind
        && a.workspace_kind == b.workspace_kind
        && a.name == b.name
        && a.title == b.title
        && a.pinned_at == b.pinned_at
        && a.labels == b.labels
        && a.archiving_at == b.archiving_at
        && a.background == b.background
        && a.project == b.project
}

/// What a reducer step touched, so views can skip work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Changed {
    pub workspaces: bool,
    pub agents: bool,
    pub projects: bool,
    pub labels: bool,
}

impl Changed {
    pub fn any(self) -> bool {
        self.workspaces || self.agents || self.projects || self.labels
    }

    fn merge(&mut self, other: Changed) {
        self.workspaces |= other.workspaces;
        self.agents |= other.agents;
        self.projects |= other.projects;
        self.labels |= other.labels;
    }
}

impl Directory {
    /// The connection dropped: subscriptions are gone, data stays on screen.
    pub fn disconnected(&mut self) {
        self.subscriptions = Subscriptions::default();
    }

    // ---- replies -------------------------------------------------------

    /// `fetch_workspaces_response` payload. `first_page` replaces on snapshot.
    pub fn apply_workspaces_reply(&mut self, payload: &Value, first_page: bool) -> Changed {
        if first_page && let Some(id) = payload["subscriptionId"].as_str() {
            self.subscriptions.workspaces = Some(id.to_string());
        }
        let sync: Option<SyncMeta> = serde_json::from_value(payload["sync"].clone()).ok();
        let replace = first_page && sync.as_ref().is_none_or(SyncMeta::is_snapshot);
        if replace {
            self.workspaces.clear();
            if self.projects_from_workspaces {
                self.projects.clear();
            }
        }
        for entry in payload["entries"].as_array().into_iter().flatten() {
            if let Ok(workspace) = serde_json::from_value::<Workspace>(entry.clone()) {
                self.upsert_workspace(workspace);
            }
        }
        for project in payload["emptyProjects"].as_array().into_iter().flatten() {
            if let Ok(project) = serde_json::from_value::<Project>(project.clone()) {
                self.projects.insert(project.project_id.clone(), project);
            }
        }
        if let Some(sync) = sync {
            for removal in &sync.removals {
                self.workspaces.remove(&removal.id);
            }
            self.workspace_cursor = Some(Cursor {
                generation: sync.generation,
                after_seq: sync.head_seq,
            });
        }
        self.hydrated = true;
        self.journal.push(DirChange::Bulk);
        Changed {
            workspaces: true,
            projects: true,
            ..Default::default()
        }
    }

    /// `fetch_agents_response` payload.
    pub fn apply_agents_reply(&mut self, payload: &Value, first_page: bool) -> Changed {
        if first_page && let Some(id) = payload["subscriptionId"].as_str() {
            self.subscriptions.agents = Some(id.to_string());
        }
        let sync: Option<SyncMeta> = serde_json::from_value(payload["sync"].clone()).ok();
        if first_page && sync.as_ref().is_none_or(SyncMeta::is_snapshot) {
            self.agents.clear();
        }
        for entry in payload["entries"].as_array().into_iter().flatten() {
            if let Some(agent) = Agent::from_value(entry["agent"].clone()) {
                self.upsert_agent(agent);
            }
        }
        if let Some(sync) = sync {
            for removal in &sync.removals {
                self.agents.remove(&removal.id);
            }
            self.agent_cursor = Some(Cursor {
                generation: sync.generation,
                after_seq: sync.head_seq,
            });
        }
        self.journal.push(DirChange::Bulk);
        Changed {
            agents: true,
            ..Default::default()
        }
    }

    /// `project.list.response` payload.
    pub fn apply_projects_reply(&mut self, payload: &Value) -> Changed {
        self.projects_from_workspaces = false;
        let sync: Option<SyncMeta> = serde_json::from_value(payload["sync"].clone()).ok();
        if sync.as_ref().is_none_or(SyncMeta::is_snapshot) {
            self.projects.clear();
        }
        for project in payload["projects"].as_array().into_iter().flatten() {
            if let Ok(project) = serde_json::from_value::<Project>(project.clone()) {
                self.projects.insert(project.project_id.clone(), project);
            }
        }
        if let Some(sync) = sync {
            for removal in &sync.removals {
                self.projects.remove(&removal.id);
            }
            self.project_cursor = Some(Cursor {
                generation: sync.generation,
                after_seq: sync.head_seq,
            });
        }
        self.journal.push(DirChange::Bulk);
        Changed {
            projects: true,
            ..Default::default()
        }
    }

    /// `workspace.label.list.response` payload.
    pub fn apply_labels_reply(&mut self, payload: &Value) -> Changed {
        if let Some(id) = payload["subscriptionId"].as_str() {
            self.subscriptions.labels = Some(id.to_string());
        }
        self.labels = payload["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(label_def)
            .collect();
        self.journal.push(DirChange::Bulk);
        Changed {
            labels: true,
            ..Default::default()
        }
    }

    // ---- pushes --------------------------------------------------------

    /// A session message that is not a reply. Returns what changed.
    pub fn apply_push(&mut self, message: &Value) -> Changed {
        let payload = &message["payload"];
        match message["type"].as_str() {
            Some("workspace_update") => self.apply_workspace_update(payload),
            Some("agent_update") => self.apply_agent_update(payload),
            Some("project.update") => self.apply_project_update(payload),
            Some("script_status_update") => {
                let id = payload["workspaceId"].as_str().unwrap_or_default();
                if let Some(workspace) = self.workspaces.get_mut(id) {
                    workspace.scripts =
                        serde_json::from_value(payload["scripts"].clone()).unwrap_or_default();
                    self.journal
                        .push(DirChange::WorkspaceDetail(id.to_string()));
                    Changed {
                        workspaces: true,
                        ..Default::default()
                    }
                } else {
                    Changed::default()
                }
            }
            Some("workspace.label.update") => self.apply_label_update(payload),
            _ => Changed::default(),
        }
    }

    fn apply_workspace_update(&mut self, payload: &Value) -> Changed {
        let mut changed = Changed {
            workspaces: true,
            ..Default::default()
        };
        match payload["kind"].as_str() {
            Some("upsert") => {
                if let Ok(workspace) =
                    serde_json::from_value::<Workspace>(payload["workspace"].clone())
                {
                    // Status, git and script churn is most of what a daemon
                    // pushes; nothing derives membership or titles from it.
                    let detail = self.projects.contains_key(&workspace.project_id)
                        && self
                            .workspaces
                            .get(&workspace.id)
                            .is_some_and(|old| same_shape(old, &workspace));
                    self.journal.push(if detail {
                        DirChange::WorkspaceDetail(workspace.id.clone())
                    } else {
                        DirChange::Workspace(workspace.id.clone())
                    });
                    self.upsert_workspace(workspace);
                    changed.projects = true;
                }
            }
            Some("remove") => {
                if let Some(id) = payload["id"].as_str() {
                    self.workspaces.remove(id);
                    self.journal.push(DirChange::Workspace(id.to_string()));
                }
                if let Ok(project) =
                    serde_json::from_value::<Project>(payload["emptyProject"].clone())
                {
                    self.projects.insert(project.project_id.clone(), project);
                    changed.projects = true;
                }
                if let Some(project_id) = payload["removedProjectId"].as_str() {
                    self.projects.remove(project_id);
                    changed.projects = true;
                }
            }
            _ => return Changed::default(),
        }
        changed
    }

    fn apply_agent_update(&mut self, payload: &Value) -> Changed {
        match payload["kind"].as_str() {
            Some("upsert") => {
                if let Some(agent) = Agent::from_value(payload["agent"].clone()) {
                    if agent.archived_at.is_some() {
                        if self.agents.remove(&agent.id).is_some() {
                            self.journal.push(DirChange::AgentRemoved(agent.id));
                        }
                    } else {
                        let id = agent.id.clone();
                        let before = self
                            .agents
                            .get(&id)
                            .map(|old| (old.workspace_id.clone(), old.parent_agent_id.clone()));
                        self.upsert_agent(agent);
                        let after = self
                            .agents
                            .get(&id)
                            .map(|new| (new.workspace_id.clone(), new.parent_agent_id.clone()));
                        self.journal.push(if before.is_none() {
                            DirChange::AgentAdded(id)
                        } else if before != after {
                            DirChange::AgentMoved(id)
                        } else {
                            DirChange::AgentUpdated(id)
                        });
                    }
                }
            }
            Some("remove") => {
                if let Some(id) = payload["agentId"].as_str()
                    && self.agents.remove(id).is_some()
                {
                    self.journal.push(DirChange::AgentRemoved(id.to_string()));
                }
            }
            _ => return Changed::default(),
        }
        Changed {
            agents: true,
            workspaces: true,
            ..Default::default()
        }
    }

    fn apply_project_update(&mut self, payload: &Value) -> Changed {
        match payload["kind"].as_str() {
            Some("upsert") => {
                let Ok(project) = serde_json::from_value::<Project>(payload["project"].clone())
                else {
                    return Changed::default();
                };
                for workspace in self.workspaces.values_mut() {
                    if workspace.project_id == project.project_id {
                        workspace.project_display_name = project.project_display_name.clone();
                        workspace.project_custom_name = project.project_custom_name.clone();
                        workspace.project_root_path = project.project_root_path.clone();
                        workspace.project_kind = project.project_kind.clone();
                    }
                }
                self.projects.insert(project.project_id.clone(), project);
            }
            Some("remove") => {
                let Some(id) = payload["projectId"].as_str() else {
                    return Changed::default();
                };
                self.projects.remove(id);
                self.workspaces
                    .retain(|_, workspace| workspace.project_id != id);
            }
            _ => return Changed::default(),
        }
        self.journal.push(DirChange::Bulk);
        Changed {
            projects: true,
            workspaces: true,
            ..Default::default()
        }
    }

    fn apply_label_update(&mut self, payload: &Value) -> Changed {
        match payload["kind"].as_str() {
            Some("upsert") => {
                let Some(label) = label_def(&payload["label"]) else {
                    return Changed::default();
                };
                let previous = payload["previousName"].as_str().unwrap_or(&label.name);
                let key = label_key(previous);
                if let Some(existing) = self.labels.iter_mut().find(|l| label_key(&l.name) == key) {
                    *existing = label.clone();
                } else {
                    self.labels.push(label.clone());
                }
                if previous != label.name {
                    for workspace in self.workspaces.values_mut() {
                        for name in &mut workspace.labels {
                            if label_key(name) == key {
                                *name = label.name.clone();
                            }
                        }
                    }
                }
            }
            Some("remove") => {
                let key = label_key(payload["name"].as_str().unwrap_or_default());
                self.labels.retain(|label| label_key(&label.name) != key);
                for workspace in self.workspaces.values_mut() {
                    workspace.labels.retain(|name| label_key(name) != key);
                }
            }
            _ => return Changed::default(),
        }
        self.journal.push(DirChange::Bulk);
        Changed {
            labels: true,
            workspaces: true,
            ..Default::default()
        }
    }

    /// Snapshots arrive out of order; never let an older one replace a newer
    /// (spec: accept only if `updatedAt` is not older), but keep its usage.
    fn upsert_agent(&mut self, agent: Agent) {
        if let Some(current) = self.agents.get_mut(&agent.id)
            && let (Some(have), Some(incoming)) = (&current.updated_at, &agent.updated_at)
            && incoming < have
        {
            if agent.raw["lastUsage"] != current.raw["lastUsage"] {
                current.raw["lastUsage"] = agent.raw["lastUsage"].clone();
            }
            return;
        }
        self.agents.insert(agent.id.clone(), agent);
    }

    fn upsert_workspace(&mut self, mut workspace: Workspace) {
        workspace.labels = named_once(std::mem::take(&mut workspace.labels));
        if self.projects_from_workspaces || !self.projects.contains_key(&workspace.project_id) {
            self.projects
                .entry(workspace.project_id.clone())
                .or_insert_with(|| Project::from_workspace(&workspace));
        }
        self.workspaces.insert(workspace.id.clone(), workspace);
    }

    // ---- derived -------------------------------------------------------

    /// Whether an agent lives in a background workspace. An agent whose
    /// workspace has not arrived yet counts as not background.
    pub fn in_background(&self, agent: &Agent) -> bool {
        agent
            .workspace_id
            .as_deref()
            .and_then(|id| self.workspaces.get(id))
            .is_some_and(|workspace| workspace.background)
    }

    pub fn label(&self, name: &str) -> Option<&LabelDef> {
        let key = label_key(name);
        self.labels
            .iter()
            .find(|label| label_key(&label.name) == key)
    }

    /// Agents belonging to a workspace, newest activity first.
    pub fn workspace_agents(&self, workspace_id: &str) -> Vec<&Agent> {
        let mut agents: Vec<&Agent> = self
            .agents
            .values()
            .filter(|agent| agent.workspace_id.as_deref() == Some(workspace_id))
            .collect();
        agents.sort_by(|a, b| activity_at(b).cmp(activity_at(a)).then(a.id.cmp(&b.id)));
        agents
    }

    /// Root agents of a workspace: no parent, or a parent outside it.
    pub fn root_agents(&self, workspace_id: &str) -> Vec<&Agent> {
        self.workspace_agents(workspace_id)
            .into_iter()
            .filter(|agent| self.is_root_agent(agent))
            .collect()
    }

    fn is_root_agent(&self, agent: &Agent) -> bool {
        match agent.parent_agent_id.as_deref() {
            None => true,
            Some(parent) => self
                .agents
                .get(parent)
                .is_none_or(|p| p.workspace_id.as_deref() != agent.workspace_id.as_deref()),
        }
    }

    /// Root agents grouped in one pass, with the same root rule as root_agents.
    /// Callers choose their own ordering within each workspace.
    pub fn roots_by_workspace(&self) -> HashMap<&str, Vec<&Agent>> {
        let mut roots: HashMap<&str, Vec<&Agent>> = HashMap::new();
        for agent in self
            .agents
            .values()
            .filter(|agent| self.is_root_agent(agent))
        {
            if let Some(workspace) = agent.workspace_id.as_deref() {
                roots.entry(workspace).or_default().push(agent);
            }
        }
        roots
    }

    /// The bucket the sidebar shows: the daemon's status, unless it says done
    /// while the newest root agent is visibly busy.
    pub fn effective_bucket(&self, workspace: &Workspace) -> Bucket {
        let reported = Bucket::parse(&workspace.status);
        if reported != Bucket::Done {
            return reported;
        }
        self.root_agents(&workspace.id)
            .first()
            .map(|agent| agent.bucket())
            .unwrap_or(Bucket::Done)
    }

    /// Each workspace's newest root agent, the one `root_agents(..).first()`
    /// returns, for every workspace in one pass over the agents. Use with
    /// [`Self::effective_bucket_with`] when looping over all workspaces;
    /// `effective_bucket` scans every agent per call.
    pub fn newest_roots(&self) -> HashMap<&str, &Agent> {
        let mut newest: HashMap<&str, &Agent> = HashMap::new();
        for agent in self.agents.values() {
            let Some(workspace) = agent.workspace_id.as_deref() else {
                continue;
            };
            if !self.is_root_agent(agent) {
                continue;
            }
            newest
                .entry(workspace)
                .and_modify(|best| {
                    // Same order as `workspace_agents`: newest activity, then id.
                    let newer = activity_at(agent)
                        .cmp(activity_at(best))
                        .then(best.id.cmp(&agent.id));
                    if newer.is_gt() {
                        *best = agent;
                    }
                })
                .or_insert(agent);
        }
        newest
    }

    /// [`Self::effective_bucket`] using a precomputed [`Self::newest_roots`].
    pub fn effective_bucket_with(
        &self,
        workspace: &Workspace,
        newest_roots: &HashMap<&str, &Agent>,
    ) -> Bucket {
        let reported = Bucket::parse(&workspace.status);
        if reported != Bucket::Done {
            return reported;
        }
        newest_roots
            .get(workspace.id.as_str())
            .map(|agent| agent.bucket())
            .unwrap_or(Bucket::Done)
    }

    /// Picks the agent a workspace click should reveal: one needing a
    /// permission, then an error, then a finished one, else the newest.
    pub fn attention_agent(&self, workspace_id: &str) -> Option<&Agent> {
        let agents = self.root_agents(workspace_id);
        let rank = |agent: &&Agent| match (
            agent.pending_permissions.is_empty(),
            agent.attention_reason.as_deref(),
        ) {
            (false, _) | (_, Some("permission")) => 0,
            (_, Some("error")) => 1,
            _ if agent.requires_attention => 2,
            _ => 3,
        };
        agents.iter().min_by_key(|agent| rank(agent)).copied()
    }
}

/// A daemon's labels as the views may group by them: blank names dropped (their
/// key would be no label's), and each name once as labels match.
fn named_once(labels: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    (labels.into_iter())
        .filter(|name| {
            let key = label_key(name);
            !key.is_empty() && seen.insert(key)
        })
        .collect()
}

fn label_def(value: &Value) -> Option<LabelDef> {
    Some(LabelDef {
        name: value["name"]
            .as_str()
            .filter(|name| !label_key(name).is_empty())?
            .to_string(),
        color: value["color"].as_str().unwrap_or("violet").to_string(),
    })
}

/// Labels match on collapsed whitespace, case-insensitively.
pub fn label_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn activity_at(agent: &Agent) -> &str {
    agent
        .raw
        .get("attentionTimestamp")
        .and_then(Value::as_str)
        .or(agent.updated_at.as_deref())
        .unwrap_or("")
}

impl Changed {
    pub fn union(mut self, other: Changed) -> Changed {
        self.merge(other);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ws(id: &str, project: &str, status: &str) -> Value {
        json!({"id": id, "projectId": project, "projectDisplayName": project, "projectRootPath": format!("/p/{project}"),
               "projectKind": "git", "workspaceKind": "worktree", "name": id, "status": status})
    }

    #[test]
    fn snapshot_then_changes() {
        let mut dir = Directory {
            projects_from_workspaces: true,
            ..Default::default()
        };
        dir.apply_workspaces_reply(
            &json!({"subscriptionId": "s1", "entries": [ws("w1", "p", "done"), ws("w2", "p", "running")],
                    "sync": {"generation": "g", "headSeq": 5, "mode": "snapshot", "removals": []}}),
            true,
        );
        assert_eq!(dir.workspaces.len(), 2);
        assert_eq!(dir.subscriptions.workspaces.as_deref(), Some("s1"));
        assert_eq!(dir.projects.len(), 1);

        dir.apply_workspaces_reply(
            &json!({"entries": [ws("w3", "p", "done")],
                    "sync": {"generation": "g", "headSeq": 7, "mode": "changes", "removals": [{"id": "w1", "seq": 6}]}}),
            true,
        );
        let ids: Vec<_> = dir.workspaces.keys().cloned().collect();
        assert_eq!(ids, vec!["w2", "w3"]);
        assert_eq!(dir.workspace_cursor.as_ref().unwrap().after_seq, 7);
    }

    #[test]
    fn pushes_update_and_remove() {
        let mut dir = Directory::default();
        dir.apply_push(&json!({"type": "workspace_update", "payload": {"kind": "upsert", "workspace": ws("w1", "p", "running"), "generation": "g", "seq": 3}}));
        assert_eq!(dir.workspaces["w1"].status, "running");
        dir.apply_push(&json!({"type": "workspace_update", "payload": {"kind": "remove", "id": "w1", "removedProjectId": "p"}}));
        assert!(dir.workspaces.is_empty());
        assert!(dir.projects.is_empty());
    }

    /// The daemon dropped `gone` at seq 6 while answering another client's
    /// fetch, so no push said so; a push at seq 7 must not carry the cursor
    /// past it, or the next delta would leave `gone` here for good.
    #[test]
    fn an_agent_removed_without_a_push_goes_at_the_next_delta() {
        let agent = |id: &str| json!({"agent": {"id": id, "workspaceId": "w1", "status": "idle", "updatedAt": "2026-10-01T00:00:00Z"}});
        let mut dir = Directory::default();
        dir.apply_agents_reply(
            &json!({"entries": [agent("gone"), agent("kept")],
                    "sync": {"generation": "g", "headSeq": 5, "mode": "snapshot", "removals": []}}),
            true,
        );
        dir.apply_push(&json!({"type": "agent_update", "payload": {"kind": "upsert", "generation": "g", "seq": 7,
                                "agent": {"id": "kept", "workspaceId": "w1", "status": "running", "updatedAt": "2026-10-01T00:01:00Z"}}}));
        let cursor = dir.agent_cursor.clone().unwrap();
        assert_eq!((cursor.generation.as_str(), cursor.after_seq), ("g", 5));

        dir.apply_agents_reply(
            &json!({"entries": [agent("kept")],
                    "sync": {"generation": "g", "headSeq": 7, "mode": "changes", "removals": [{"id": "gone", "seq": 6}]}}),
            true,
        );
        let ids: Vec<_> = dir.agents.keys().cloned().collect();
        assert_eq!(ids, vec!["kept"]);
    }

    #[test]
    fn effective_bucket_uses_root_agent_when_done() {
        let mut dir = Directory::default();
        dir.apply_push(&json!({"type": "workspace_update", "payload": {"kind": "upsert", "workspace": ws("w1", "p", "done")}}));
        dir.apply_push(&json!({"type": "agent_update", "payload": {"kind": "upsert", "agent": {"id": "a1", "workspaceId": "w1", "status": "idle", "requiresAttention": true, "updatedAt": "2026-10-01T00:00:00Z"}}}));
        let workspace = dir.workspaces["w1"].clone();
        assert_eq!(dir.effective_bucket(&workspace), Bucket::Attention);
    }

    #[test]
    fn newest_roots_matches_root_agents() {
        let mut dir = Directory::default();
        for w in ["w1", "w2", "w3"] {
            dir.apply_push(&json!({"type": "workspace_update", "payload": {"kind": "upsert", "workspace": ws(w, "p", "done")}}));
        }
        let agent = |id: &str, w: &str, at: &str, parent: Option<&str>| {
            let mut a = json!({"id": id, "workspaceId": w, "status": "idle", "updatedAt": at});
            if let Some(parent) = parent {
                a["parentAgentId"] = json!(parent);
            }
            json!({"type": "agent_update", "payload": {"kind": "upsert", "agent": a}})
        };
        dir.apply_push(&agent("a", "w1", "2026-10-01T00:00:01Z", None));
        dir.apply_push(&agent("b", "w1", "2026-10-01T00:00:03Z", None));
        // A child newer than its parent in the same workspace is not a root.
        dir.apply_push(&agent("c", "w1", "2026-10-01T00:00:09Z", Some("b")));
        // Ties go to the smaller id.
        dir.apply_push(&agent("e", "w2", "2026-10-01T00:00:05Z", None));
        dir.apply_push(&agent("d", "w2", "2026-10-01T00:00:05Z", None));
        // A parent in another workspace still makes a root.
        dir.apply_push(&agent("f", "w3", "2026-10-01T00:00:01Z", Some("a")));
        let mut archived = agent("archived", "w1", "2026-10-01T00:00:10Z", None);
        archived["payload"]["agent"]["archivedAt"] = json!("2026-10-01T00:00:11Z");
        dir.apply_push(&archived);
        let newest = dir.newest_roots();
        let grouped = dir.roots_by_workspace();
        for w in ["w1", "w2", "w3"] {
            let mut expected_ids: Vec<_> =
                dir.root_agents(w).iter().map(|a| a.id.as_str()).collect();
            let mut grouped_ids: Vec<_> = grouped[w].iter().map(|a| a.id.as_str()).collect();
            expected_ids.sort();
            grouped_ids.sort();
            assert_eq!(grouped_ids, expected_ids);
            let expected = dir.root_agents(w).first().map(|a| a.id.clone());
            assert_eq!(newest.get(w).map(|a| a.id.clone()), expected, "{w}");
            let workspace = dir.workspaces[w].clone();
            assert_eq!(
                dir.effective_bucket_with(&workspace, &newest),
                dir.effective_bucket(&workspace)
            );
        }
    }

    #[test]
    fn older_agent_snapshot_does_not_win() {
        let mut dir = Directory::default();
        let up = |status: &str, at: &str| {
            json!({"type": "agent_update", "payload": {"kind": "upsert",
            "agent": {"id": "a", "status": status, "updatedAt": at}}})
        };
        dir.apply_push(&up("running", "2026-10-02T07:00:02.000Z"));
        dir.apply_push(&up("idle", "2026-10-02T07:00:01.000Z"));
        assert_eq!(dir.agents["a"].status, "running");
        dir.apply_push(&up("idle", "2026-10-02T07:00:03.000Z"));
        assert_eq!(dir.agents["a"].status, "idle");
    }

    #[test]
    fn label_rename_rewrites_assignments() {
        let mut dir = Directory::default();
        let mut w = ws("w1", "p", "done");
        w["labels"] = json!(["Blocked"]);
        dir.apply_push(
            &json!({"type": "workspace_update", "payload": {"kind": "upsert", "workspace": w}}),
        );
        dir.apply_labels_reply(&json!({"labels": [{"name": "Blocked", "color": "red"}]}));
        dir.apply_push(&json!({"type": "workspace.label.update", "payload": {"kind": "upsert", "label": {"name": "Stuck", "color": "red"}, "previousName": "blocked", "generation": "g", "seq": 1}}));
        assert_eq!(dir.workspaces["w1"].labels, vec!["Stuck"]);
        assert_eq!(dir.labels.len(), 1);
    }

    #[test]
    fn blank_and_repeated_labels_from_a_daemon_are_dropped() {
        let mut dir = Directory::default();
        let mut w = ws("w1", "p", "done");
        // A blank name's key is no label's; "UX" and " ux " are one label.
        w["labels"] = json!(["UX", "  ", "", " ux ", "Blocked"]);
        dir.apply_push(
            &json!({"type": "workspace_update", "payload": {"kind": "upsert", "workspace": w}}),
        );
        assert_eq!(dir.workspaces["w1"].labels, vec!["UX", "Blocked"]);
        dir.apply_labels_reply(&json!({"labels": [{"name": " ", "color": "red"}, {"name": "UX"}]}));
        let names: Vec<&str> = dir.labels.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["UX"]);
    }
}
