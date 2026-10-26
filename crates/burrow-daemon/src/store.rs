//! The in-memory directory: projects, workspaces, agents, timelines, and the
//! label catalog (`labels.rs`).
//!
//! Every change bumps the owning collection's `seq` and yields a
//! [`StoreEvent`]. All collections share one `generation`, a UUID per
//! process, so a client cursor from before a restart reads as
//! `generation_changed` and gets a fresh snapshot. With a home directory,
//! every change is also handed to [`Persist`] in order, and timelines load
//! lazily from disk with their epoch and seqs intact.

use std::collections::{BTreeMap, HashMap, VecDeque};

use burrow_protocol::codes::DIRECTORY_TOMBSTONES;
use burrow_protocol::directory::{
    AgentSnapshot, AgentStatus, AttentionReason, Checkout, ProjectDescriptor, ProjectKind,
    ProjectPlacement, StateBucket, SyncCursor, SyncMeta, SyncMode, SyncReason, SyncRemoval,
    WorkspaceDescriptor, WorkspaceKind,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::git::GitFacts;
use crate::ids;
use crate::labels::{LabelChange, Labels};
use crate::persist::{AgentRecord, Loaded, Persist};
use crate::timeline::{Row, Timeline};

/// One sequenced collection.
#[derive(Debug, Clone)]
pub struct Collection<T> {
    items: BTreeMap<String, Versioned<T>>,
    head_seq: u64,
    tombstones: VecDeque<SyncRemoval>,
    /// Removals with seq up to here were forgotten; older cursors expire.
    horizon: u64,
}

#[derive(Debug, Clone)]
struct Versioned<T> {
    value: T,
    seq: u64,
}

/// How a sequenced read answers a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    pub meta: SyncMeta,
    /// In `changes` mode, the cursor: only items with a later seq are sent.
    pub after: Option<u64>,
}

impl<T> Default for Collection<T> {
    fn default() -> Self {
        Self {
            items: BTreeMap::new(),
            head_seq: 0,
            tombstones: VecDeque::new(),
            horizon: 0,
        }
    }
}

impl<T> Collection<T> {
    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }

    pub fn get(&self, id: &str) -> Option<&T> {
        self.items.get(id).map(|item| &item.value)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut T> {
        self.items.get_mut(id).map(|item| &mut item.value)
    }

    /// Items with the seq of their latest change, in id order.
    pub fn iter(&self) -> impl Iterator<Item = (&T, u64)> {
        self.items.values().map(|item| (&item.value, item.seq))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn upsert(&mut self, id: &str, value: T) -> u64 {
        self.head_seq += 1;
        let seq = self.head_seq;
        self.items.insert(id.to_string(), Versioned { value, seq });
        seq
    }

    /// Re-sequences an item whose derived projection changed.
    pub fn touch(&mut self, id: &str) -> Option<u64> {
        let item = self.items.get_mut(id)?;
        self.head_seq += 1;
        item.seq = self.head_seq;
        Some(item.seq)
    }

    pub fn remove(&mut self, id: &str) -> Option<(T, u64)> {
        let item = self.items.remove(id)?;
        self.head_seq += 1;
        let seq = self.head_seq;
        self.tombstones.push_back(SyncRemoval {
            id: id.to_string(),
            seq,
        });
        while self.tombstones.len() > DIRECTORY_TOMBSTONES {
            if let Some(dropped) = self.tombstones.pop_front() {
                self.horizon = dropped.seq;
            }
        }
        Some((item.value, seq))
    }

    /// Answers a directory-sync cursor (`sync` on a directory read).
    pub fn plan(&self, generation: &str, cursor: &SyncCursor) -> SyncPlan {
        let snapshot = |reason| SyncPlan {
            meta: SyncMeta {
                generation: generation.to_string(),
                head_seq: self.head_seq,
                mode: SyncMode::Snapshot,
                reason: Some(reason),
                removals: Vec::new(),
            },
            after: None,
        };
        let (Some(their_generation), Some(after)) = (&cursor.generation, cursor.after_seq) else {
            return snapshot(SyncReason::NoCursor);
        };
        if their_generation != generation {
            return snapshot(SyncReason::GenerationChanged);
        }
        if after > self.head_seq || after < self.horizon {
            return snapshot(SyncReason::CursorExpired);
        }
        SyncPlan {
            meta: SyncMeta {
                generation: generation.to_string(),
                head_seq: self.head_seq,
                mode: SyncMode::Changes,
                reason: None,
                removals: self
                    .tombstones
                    .iter()
                    .filter(|removal| removal.seq > after)
                    .cloned()
                    .collect(),
            },
            after: Some(after),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub project_id: String,
    pub project_key: String,
    pub root_path: String,
    pub kind: ProjectKind,
    pub display_name: String,
    pub custom_name: Option<String>,
}

impl ProjectRecord {
    pub fn name(&self) -> &str {
        self.custom_name.as_deref().unwrap_or(&self.display_name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub workspace_id: String,
    pub project_id: String,
    pub cwd: String,
    pub kind: WorkspaceKind,
    pub display_name: String,
    pub title: Option<String>,
    pub pinned_at: Option<String>,
    pub labels: Vec<String>,
    /// The checkout, when the workspace is a git work tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitFacts>,
    /// A background workspace: made so by its creator, and listed only to
    /// clients that ask for them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
    /// The exact ref a worktree Burrow made was cut from (`baseBranch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// When it was archived; an archived workspace is out of the directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

impl WorkspaceRecord {
    /// `ProjectCheckoutLite` for an agent working in `cwd` here.
    pub fn checkout(&self, cwd: &str) -> Checkout {
        match &self.git {
            Some(git) => Checkout {
                cwd: cwd.to_string(),
                is_git: true,
                current_branch: git.branch.clone(),
                remote_url: git.remote_url.clone(),
                worktree_root: Some(git.worktree_root.clone()),
                is_paseo_owned_worktree: git.paseo_owned,
                main_repo_root: git.main_repo_root.clone(),
            },
            None => Checkout::not_git(cwd),
        }
    }

    /// The folder name of a worktree Burrow made (`worktreeSlug`).
    pub fn worktree_slug(&self) -> Option<String> {
        let git = self.git.as_ref().filter(|git| git.paseo_owned)?;
        std::path::Path::new(&git.worktree_root)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }

    pub fn name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.display_name)
    }
}

/// A change, published to every connection after the store applied it.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreEvent {
    Agent {
        seq: u64,
        change: AgentChange,
    },
    Workspace {
        seq: u64,
        change: WorkspaceChange,
    },
    Project {
        seq: u64,
        change: ProjectChange,
    },
    /// A label catalog change, sequenced by the label journal.
    Label {
        seq: u64,
        change: LabelChange,
    },
    Timeline {
        agent_id: String,
        epoch: String,
        row: Row,
    },
    /// A live `agent_stream` event without a seq: `turn_started`,
    /// `turn_completed`, `thread_started`, `permission_requested`, ...
    Stream {
        agent_id: String,
        event: Value,
        timestamp: String,
    },
    /// A session event for `session.events.set_subscription.request`
    /// subscribers: `kind` is its name (`agent_permission_request`,
    /// `agent_permission_resolved`, `agent_attention_required`, ...) and
    /// `payload` the message payload.
    Session {
        kind: &'static str,
        payload: Value,
    },
    /// The providers snapshot changed (a provider finished loading): the
    /// `providers_snapshot_update` payload in the plain and the compact
    /// encoding, for each socket to pick its own.
    Providers {
        plain: Value,
        compact: Value,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentChange {
    Upsert {
        agent: Box<AgentSnapshot>,
        project: ProjectPlacement,
        /// The agent is in a background workspace.
        background: bool,
    },
    Remove {
        agent_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceChange {
    Upsert(Box<WorkspaceDescriptor>),
    Remove {
        id: String,
        /// The workspace's project when it has no other workspace left, as a
        /// client that sees background workspaces counts them, and as one
        /// that does not.
        empty_project: Option<Box<ProjectDescriptor>>,
        empty_project_without_background: Option<Box<ProjectDescriptor>>,
        /// The project went with it (`project.remove`); then there is no
        /// `emptyProject`.
        removed_project_id: Option<String>,
    },
    /// A project removed while none of its workspaces was in the directory.
    /// Paseo still pushes a removal, unsequenced, for its first archived
    /// workspace or else the project's own id, so clients drop the project.
    /// `seq` is the collection head, not a new one.
    ProjectGone {
        id: String,
        project_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectChange {
    Upsert(ProjectDescriptor),
    Remove { project_id: String },
}

#[derive(Debug, Clone)]
pub struct Store {
    pub generation: String,
    /// The daemon's identity, for the project keys of local folders.
    pub server_id: String,
    pub projects: Collection<ProjectRecord>,
    pub workspaces: Collection<WorkspaceRecord>,
    /// Archived workspaces: out of the directory, kept on disk.
    pub archived_workspaces: BTreeMap<String, WorkspaceRecord>,
    /// The active (unarchived) agent directory.
    pub agents: Collection<AgentSnapshot>,
    /// The workspace label catalog (`labels.rs`).
    pub labels: Labels,
    /// Archived agents: out of the active directory, still readable.
    pub archived: BTreeMap<String, AgentSnapshot>,
    /// Provider settings per agent, kept with its record.
    provider_configs: HashMap<String, Value>,
    timelines: HashMap<String, Timeline>,
    persist: Option<Persist>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        Self {
            generation: ids::generation(),
            server_id: String::new(),
            projects: Collection::default(),
            workspaces: Collection::default(),
            archived_workspaces: BTreeMap::new(),
            agents: Collection::default(),
            labels: Labels::default(),
            archived: BTreeMap::new(),
            provider_configs: HashMap::new(),
            timelines: HashMap::new(),
            persist: None,
        }
    }

    /// A store backed by a home directory, with what was saved there.
    ///
    /// Agents come back `closed` (resumable, nothing running) until they are
    /// used again; the agent manager starts their provider lazily.
    pub fn open(persist: Persist, loaded: Loaded) -> Self {
        let mut store = Self::restore(loaded);
        store.persist = Some(persist);
        store
    }

    /// What was saved, held in memory only: changes are not written back.
    pub fn restore(loaded: Loaded) -> Self {
        let mut store = Self::new();
        store.labels = Labels::restore(loaded.labels);
        for project in loaded.projects {
            let id = project.project_id.clone();
            store.projects.upsert(&id, project);
        }
        for workspace in loaded.workspaces {
            let id = workspace.workspace_id.clone();
            if workspace.archived_at.is_some() {
                store.archived_workspaces.insert(id, workspace);
            } else {
                store.workspaces.upsert(&id, workspace);
            }
        }
        for record in loaded.agents {
            let AgentRecord {
                mut snapshot,
                provider_config,
                ..
            } = record;
            if let Some(config) = provider_config {
                store.provider_configs.insert(snapshot.id.clone(), config);
            }
            if snapshot.archived_at.is_some() {
                store.archived.insert(snapshot.id.clone(), snapshot);
                continue;
            }
            if snapshot.status != AgentStatus::Closed {
                snapshot.status = AgentStatus::Closed;
                snapshot.active_turn = None;
            }
            let id = snapshot.id.clone();
            store.agents.upsert(&id, snapshot);
        }
        store
    }

    pub fn persist(&self) -> Option<&Persist> {
        self.persist.as_ref()
    }

    pub fn provider_config(&self, agent_id: &str) -> Option<&Value> {
        self.provider_configs.get(agent_id)
    }

    pub fn set_provider_config(&mut self, agent_id: &str, config: Value) {
        self.provider_configs.insert(agent_id.to_string(), config);
    }

    fn save_projects(&self) {
        if let Some(persist) = &self.persist {
            persist.projects(self.projects.iter().map(|(p, _)| p.clone()).collect());
        }
    }

    pub(crate) fn save_workspaces(&self) {
        if let Some(persist) = &self.persist {
            let active = self.workspaces.iter().map(|(w, _)| w.clone());
            let archived = self.archived_workspaces.values().cloned();
            persist.workspaces(active.chain(archived).collect());
        }
    }

    fn save_agent(&self, snapshot: &AgentSnapshot) {
        if let Some(persist) = &self.persist {
            persist.agent(AgentRecord {
                snapshot: snapshot.clone(),
                last_status: None,
                provider_config: self.provider_configs.get(&snapshot.id).cloned(),
            });
        }
    }

    /// Any agent, active or archived.
    pub fn agent(&self, agent_id: &str) -> Option<&AgentSnapshot> {
        self.agents
            .get(agent_id)
            .or_else(|| self.archived.get(agent_id))
    }

    pub fn project_descriptor(&self, project: &ProjectRecord) -> ProjectDescriptor {
        ProjectDescriptor {
            project_id: project.project_id.clone(),
            project_key: project.project_key.clone(),
            project_display_name: project.name().to_string(),
            project_custom_name: project.custom_name.clone(),
            project_custom_icon_revision: None,
            project_root_path: project.root_path.clone(),
            project_kind: project.kind,
            sync_seq: None,
        }
    }

    /// Projects with no workspace (`emptyProjects`). For a client that does
    /// not see background workspaces, as for Paseo, a project with only
    /// those is empty.
    pub fn empty_projects(&self, include_background: bool) -> Vec<ProjectDescriptor> {
        self.projects
            .iter()
            .filter(|(project, _)| {
                !self.workspaces.iter().any(|(workspace, _)| {
                    workspace.project_id == project.project_id
                        && (include_background || !workspace.background)
                })
            })
            .map(|(project, _)| self.project_descriptor(project))
            .collect()
    }

    /// The sync generation a directory read is answered with. Paseo gives a
    /// read that includes background workspaces its own, so a cursor taken
    /// without them never resumes one that has them, or the other way round.
    pub fn generation_for(&self, include_background: bool) -> String {
        background_generation(&self.generation, include_background)
    }

    /// Whether an agent is in a background workspace.
    pub fn in_background(&self, agent: &AgentSnapshot) -> bool {
        agent
            .workspace_id
            .as_deref()
            .and_then(|id| self.workspaces.get(id))
            .is_some_and(|workspace| workspace.background)
    }

    pub fn workspace_descriptor(&self, workspace: &WorkspaceRecord) -> WorkspaceDescriptor {
        let project = self.projects.get(&workspace.project_id);
        let members = || {
            self.agents
                .iter()
                .map(|(agent, _)| agent)
                .filter(|agent| agent.workspace_id.as_deref() == Some(&workspace.workspace_id))
        };
        let status = members()
            .map(agent_bucket)
            .min_by_key(|bucket| bucket_rank(*bucket))
            .unwrap_or(StateBucket::Done);
        let activity_at = members().map(|agent| agent.updated_at.clone()).max();
        WorkspaceDescriptor {
            id: workspace.workspace_id.clone(),
            project_id: workspace.project_id.clone(),
            project_display_name: project.map(|p| p.name().to_string()).unwrap_or_default(),
            project_custom_name: project.and_then(|p| p.custom_name.clone()),
            project_custom_icon_revision: None,
            project_root_path: project.map(|p| p.root_path.clone()).unwrap_or_default(),
            workspace_directory: workspace.cwd.clone(),
            worktree_slug: workspace.worktree_slug(),
            project_kind: project.map_or(ProjectKind::NonGit, |p| p.kind),
            workspace_kind: workspace.kind,
            name: workspace.name().to_string(),
            title: workspace.title.clone(),
            pinned_at: workspace.pinned_at.clone(),
            labels: workspace.labels.clone(),
            background: workspace.background,
            archiving_at: None,
            status,
            status_entered_at: None,
            activity_at,
            diff_stat: None,
            scripts: Vec::new(),
            git_runtime: workspace.git.as_ref().map(|git| {
                serde_json::json!({
                    "currentBranch": git.branch,
                    "remoteUrl": git.remote_url,
                    "isPaseoOwnedWorktree": git.paseo_owned,
                })
            }),
            github_runtime: None,
            project: Some(self.workspace_placement(workspace)),
            sync_seq: None,
        }
    }

    fn workspace_placement(&self, workspace: &WorkspaceRecord) -> ProjectPlacement {
        let project = self.projects.get(&workspace.project_id);
        ProjectPlacement {
            project_key: project.map_or_else(|| workspace.cwd.clone(), |p| p.project_key.clone()),
            project_name: project.map_or_else(|| basename(&workspace.cwd), |p| p.name().into()),
            workspace_name: Some(workspace.name().to_string()),
            checkout: workspace.checkout(&workspace.cwd),
        }
    }

    /// Where an agent sits in the project tree.
    pub fn agent_placement(&self, agent: &AgentSnapshot) -> ProjectPlacement {
        match agent
            .workspace_id
            .as_deref()
            .and_then(|id| self.workspaces.get(id))
        {
            Some(workspace) => ProjectPlacement {
                checkout: workspace.checkout(&agent.cwd),
                ..self.workspace_placement(workspace)
            },
            None => ProjectPlacement {
                project_key: agent.cwd.clone(),
                project_name: basename(&agent.cwd),
                workspace_name: None,
                checkout: Checkout::not_git(&agent.cwd),
            },
        }
    }

    pub fn upsert_project(&mut self, project: ProjectRecord) -> Vec<StoreEvent> {
        let id = project.project_id.clone();
        let descriptor = self.project_descriptor(&project);
        let seq = self.projects.upsert(&id, project);
        self.save_projects();
        vec![StoreEvent::Project {
            seq,
            change: ProjectChange::Upsert(ProjectDescriptor {
                sync_seq: Some(seq),
                ..descriptor
            }),
        }]
    }

    pub fn upsert_workspace(&mut self, workspace: WorkspaceRecord) -> Vec<StoreEvent> {
        let id = workspace.workspace_id.clone();
        self.workspaces.upsert(&id, workspace);
        self.save_workspaces();
        self.workspace_changed(&id).into_iter().collect()
    }

    /// Takes a workspace out of the directory and keeps it as archived. Its
    /// agents are the caller's to archive first. The removal names the
    /// project when no workspace is left in it.
    pub fn archive_workspace(&mut self, id: &str, archived_at: &str) -> Vec<StoreEvent> {
        let Some((mut workspace, seq)) = self.workspaces.remove(id) else {
            return Vec::new();
        };
        let project_id = workspace.project_id.clone();
        workspace.archived_at = Some(archived_at.to_string());
        self.archived_workspaces.insert(id.to_string(), workspace);
        self.save_workspaces();
        let remaining = |include_background: bool| {
            self.workspaces
                .iter()
                .any(|(w, _)| w.project_id == project_id && (include_background || !w.background))
        };
        let empty = |include_background: bool| {
            (!remaining(include_background))
                .then(|| self.projects.get(&project_id))
                .flatten()
                .map(|project| Box::new(self.project_descriptor(project)))
        };
        vec![StoreEvent::Workspace {
            seq,
            change: WorkspaceChange::Remove {
                id: id.to_string(),
                empty_project: empty(true),
                empty_project_without_background: empty(false),
                removed_project_id: None,
            },
        }]
    }

    /// `project.remove`: archives the project's workspaces still in the
    /// directory (their agents are the caller's to archive first) and
    /// forgets the project. Archived workspaces keep their records, project
    /// id included, as in Paseo. Returns the workspaces archived.
    pub fn remove_project(
        &mut self,
        project_id: &str,
        archived_at: &str,
    ) -> (Vec<String>, Vec<StoreEvent>) {
        let ids: Vec<String> = (self.workspaces.iter())
            .filter(|(w, _)| w.project_id == project_id)
            .map(|(w, _)| w.workspace_id.clone())
            .collect();
        let mut events = Vec::new();
        for id in &ids {
            let Some((mut workspace, seq)) = self.workspaces.remove(id) else {
                continue;
            };
            workspace.archived_at = Some(archived_at.to_string());
            self.archived_workspaces.insert(id.clone(), workspace);
            events.push(StoreEvent::Workspace {
                seq,
                change: WorkspaceChange::Remove {
                    id: id.clone(),
                    empty_project: None,
                    empty_project_without_background: None,
                    removed_project_id: Some(project_id.to_string()),
                },
            });
        }
        if !ids.is_empty() {
            self.save_workspaces();
        } else {
            let id = (self.archived_workspaces.values())
                .find(|w| w.project_id == project_id)
                .map_or_else(|| project_id.to_string(), |w| w.workspace_id.clone());
            events.push(StoreEvent::Workspace {
                seq: self.workspaces.head_seq(),
                change: WorkspaceChange::ProjectGone {
                    id,
                    project_id: project_id.to_string(),
                },
            });
        }
        if let Some((_, seq)) = self.projects.remove(project_id) {
            self.save_projects();
            events.push(StoreEvent::Project {
                seq,
                change: ProjectChange::Remove {
                    project_id: project_id.to_string(),
                },
            });
        }
        (ids, events)
    }

    /// Re-derives a workspace's descriptor after its agents changed.
    pub(crate) fn workspace_changed(&mut self, id: &str) -> Option<StoreEvent> {
        let descriptor = self.workspace_descriptor(self.workspaces.get(id)?);
        let seq = self.workspaces.touch(id)?;
        Some(StoreEvent::Workspace {
            seq,
            change: WorkspaceChange::Upsert(Box::new(WorkspaceDescriptor {
                sync_seq: Some(seq),
                ..descriptor
            })),
        })
    }

    pub fn upsert_agent(&mut self, agent: AgentSnapshot) -> Vec<StoreEvent> {
        let id = agent.id.clone();
        let workspace_id = agent.workspace_id.clone();
        let project = self.agent_placement(&agent);
        let background = self.in_background(&agent);
        self.archived.remove(&id);
        self.save_agent(&agent);
        let seq = self.agents.upsert(&id, agent.clone());
        self.ensure_timeline(&id);
        let mut events = vec![StoreEvent::Agent {
            seq,
            change: AgentChange::Upsert {
                agent: Box::new(agent),
                project,
                background,
            },
        }];
        events.extend(workspace_id.and_then(|id| self.workspace_changed(&id)));
        events
    }

    /// Takes an agent out of the active directory.
    pub fn remove_agent(&mut self, agent_id: &str) -> Vec<StoreEvent> {
        let Some((agent, seq)) = self.agents.remove(agent_id) else {
            return Vec::new();
        };
        let mut events = vec![StoreEvent::Agent {
            seq,
            change: AgentChange::Remove {
                agent_id: agent_id.to_string(),
            },
        }];
        events.extend(
            agent
                .workspace_id
                .and_then(|id| self.workspace_changed(&id)),
        );
        events
    }

    /// Moves an agent to the archive; it stays readable by id.
    pub fn archive_agent(&mut self, agent_id: &str, archived_at: &str) -> Vec<StoreEvent> {
        let Some(mut agent) = self.agents.get(agent_id).cloned() else {
            return Vec::new();
        };
        agent.archived_at = Some(archived_at.to_string());
        agent.updated_at = archived_at.to_string();
        agent.requires_attention = false;
        agent.attention_reason = None;
        agent.attention_timestamp = None;
        agent.active_turn = None;
        if agent.status == AgentStatus::Running || agent.status == AgentStatus::Initializing {
            agent.status = AgentStatus::Closed;
        }
        let events = self.remove_agent(agent_id);
        self.save_agent(&agent);
        self.archived.insert(agent_id.to_string(), agent);
        events
    }

    /// Forgets an agent, active or archived, with its timeline and its
    /// files. Returns whether it was there, and the directory events.
    pub fn delete_agent(&mut self, agent_id: &str) -> (bool, Vec<StoreEvent>) {
        let archived = self.archived.remove(agent_id).is_some();
        let events = self.remove_agent(agent_id);
        let found = archived || !events.is_empty();
        if found {
            self.timelines.remove(agent_id);
            self.provider_configs.remove(agent_id);
            if let Some(persist) = &self.persist {
                persist.delete_agent(agent_id);
            }
        }
        (found, events)
    }

    /// Saves an agent that is already archived (an imported one). It goes
    /// straight to the archive, so no directory event is published.
    pub fn insert_archived_agent(&mut self, agent: AgentSnapshot) {
        let id = agent.id.clone();
        self.save_agent(&agent);
        self.archived.insert(id.clone(), agent);
        self.ensure_timeline(&id);
    }

    pub fn timeline(&self, agent_id: &str) -> Option<&Timeline> {
        self.timelines.get(agent_id)
    }

    /// Makes an agent's timeline resident: read from disk, or started empty.
    pub fn ensure_timeline(&mut self, agent_id: &str) -> Option<&Timeline> {
        self.agent(agent_id)?;
        if !self.timelines.contains_key(agent_id) {
            let loaded = match &self.persist {
                Some(persist) => persist.load_timeline(agent_id).unwrap_or_else(|err| {
                    tracing::warn!(agent_id, %err, "unreadable timeline; starting a new epoch");
                    None
                }),
                None => None,
            };
            let timeline = match loaded {
                Some((epoch, rows)) => Timeline::restore(epoch, rows),
                None => {
                    let timeline = Timeline::new();
                    if let Some(persist) = &self.persist {
                        persist.new_timeline(agent_id, &timeline.epoch);
                    }
                    timeline
                }
            };
            self.timelines.insert(agent_id.to_string(), timeline);
        }
        self.timelines.get(agent_id)
    }

    pub fn append_timeline(
        &mut self,
        agent_id: &str,
        item: Value,
        turn_id: Option<String>,
        timestamp: String,
    ) -> Option<StoreEvent> {
        let provider = self.agent(agent_id)?.provider.clone();
        self.ensure_timeline(agent_id)?;
        let timeline = self.timelines.get_mut(agent_id)?;
        let row = timeline.append(&provider, item, turn_id, timestamp);
        if let Some(persist) = &self.persist {
            persist.rows(agent_id, vec![row.clone()]);
        }
        Some(StoreEvent::Timeline {
            agent_id: agent_id.to_string(),
            epoch: timeline.epoch.clone(),
            row,
        })
    }

    /// `fetch_agent_request` lookup: a full id, a unique id prefix, or an
    /// exact title, among active agents first, then archived ones.
    pub fn find_agent(&self, key: &str) -> Option<&AgentSnapshot> {
        if let Some(agent) = self.agent(key) {
            return Some(agent);
        }
        let all = || {
            self.agents
                .iter()
                .map(|(agent, _)| agent)
                .chain(self.archived.values())
        };
        let mut by_prefix = all().filter(|agent| agent.id.starts_with(key));
        if let (Some(agent), None) = (by_prefix.next(), by_prefix.next()) {
            return Some(agent);
        }
        all().find(|agent| agent.title.as_deref() == Some(key))
    }
}

/// `deriveAgentStateBucket`.
pub fn agent_bucket(agent: &AgentSnapshot) -> StateBucket {
    let reason = agent.attention_reason;
    if !agent.pending_permissions.is_empty() || reason == Some(AttentionReason::Permission) {
        StateBucket::NeedsInput
    } else if agent.status == AgentStatus::Error || reason == Some(AttentionReason::Error) {
        StateBucket::Failed
    } else if agent.status == AgentStatus::Running {
        StateBucket::Running
    } else if agent.requires_attention {
        StateBucket::Attention
    } else {
        StateBucket::Done
    }
}

fn bucket_rank(bucket: StateBucket) -> u8 {
    match bucket {
        StateBucket::NeedsInput => 0,
        StateBucket::Failed => 1,
        StateBucket::Running => 2,
        StateBucket::Attention => 3,
        StateBucket::Done => 4,
    }
}

/// `generationFor` in Paseo's directory sync: the plain generation, or with
/// `:background` for reads and pushes that include background workspaces.
pub fn background_generation(generation: &str, include_background: bool) -> String {
    if include_background {
        format!("{generation}:background")
    } else {
        generation.to_string()
    }
}

fn basename(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(generation: &str, after: u64) -> SyncCursor {
        SyncCursor {
            generation: Some(generation.into()),
            after_seq: Some(after),
        }
    }

    #[test]
    fn sync_plans_match_paseo_modes() {
        let mut items: Collection<u32> = Collection::default();
        for n in 0..5 {
            items.upsert(&format!("i{n}"), n);
        }
        items.remove("i0");
        let head = items.head_seq();
        assert_eq!(head, 6);

        let none = items.plan("g", &SyncCursor::default());
        assert_eq!(none.meta.reason, Some(SyncReason::NoCursor));
        let current = items.plan("g", &cursor("g", head));
        assert_eq!(current.meta.mode, SyncMode::Changes);
        assert!(current.meta.removals.is_empty());
        let behind = items.plan("g", &cursor("g", 4));
        assert_eq!(behind.after, Some(4));
        assert_eq!(behind.meta.removals.len(), 1);
        let restarted = items.plan("g", &cursor("other", 1));
        assert_eq!(restarted.meta.reason, Some(SyncReason::GenerationChanged));
        let future = items.plan("g", &cursor("g", head + 1000));
        assert_eq!(future.meta.reason, Some(SyncReason::CursorExpired));
    }

    fn project(id: &str) -> ProjectRecord {
        ProjectRecord {
            project_id: id.into(),
            project_key: format!("host:srv:/{id}"),
            root_path: format!("/{id}"),
            kind: ProjectKind::NonGit,
            display_name: id.into(),
            custom_name: None,
        }
    }

    fn workspace(id: &str, project_id: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            workspace_id: id.into(),
            project_id: project_id.into(),
            cwd: format!("/{project_id}"),
            kind: WorkspaceKind::Directory,
            display_name: id.into(),
            title: None,
            pinned_at: None,
            labels: Vec::new(),
            git: None,
            background: false,
            base_branch: None,
            archived_at: None,
        }
    }

    #[test]
    fn removed_projects_push_sequenced_and_unsequenced_removals() {
        use crate::subscriptions::{Subscription, Subscriptions};
        let mut store = Store::new();
        store.upsert_project(project("p1"));
        store.upsert_project(project("p2"));
        store.upsert_workspace(workspace("w1", "p1"));
        let mut subs = Subscriptions::default();
        subs.open(Subscription::Workspaces {
            project_id: None,
            from_seq: store.workspaces.head_seq(),
            sync: true,
            background: false,
        });

        // An active workspace leaves sequenced, naming the project.
        let (removed, events) = store.remove_project("p1", "2026-10-20T00:00:00Z");
        assert_eq!(removed, ["w1"]);
        let frames: Vec<Value> = (events.iter())
            .flat_map(|e| subs.route(e, &store.generation))
            .collect();
        let payload = &frames[0]["message"]["payload"];
        assert_eq!(payload["removedProjectId"], "p1");
        assert!(payload["seq"].is_u64());
        assert!(payload.get("emptyProject").is_none());
        assert!(store.archived_workspaces["w1"].archived_at.is_some());
        assert!(store.projects.get("p1").is_none());

        // An empty one is named by its own id, unsequenced, and reaches a
        // subscription opened at the current head.
        let (removed, events) = store.remove_project("p2", "2026-10-20T00:00:00Z");
        assert!(removed.is_empty());
        let frames: Vec<Value> = (events.iter())
            .flat_map(|e| subs.route(e, &store.generation))
            .collect();
        assert_eq!(frames.len(), 1, "{frames:#?}");
        let payload = &frames[0]["message"]["payload"];
        assert_eq!(payload["id"], "p2");
        assert_eq!(payload["removedProjectId"], "p2");
        assert!(payload.get("seq").is_none());
        assert!(store.projects.is_empty());
    }

    #[test]
    fn basename_handles_both_separators() {
        assert_eq!(basename("/tmp/demo/"), "demo");
        assert_eq!(basename(r"C:\work\demo"), "demo");
    }
}
