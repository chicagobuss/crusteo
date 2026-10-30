//! A one-time import of a Paseo home (`burrow import-paseo`).
//!
//! Paseo's records are read, never written. Their layout, from Paseo 0.11
//! (`agent-storage.ts`, `workspace-registry.ts`):
//!
//! ```text
//! <paseo>/agents/<cwd slug>/<agentId>.json   one record per agent (also read directly under agents/)
//! <paseo>/projects/projects.json             the project registry
//! <paseo>/projects/workspaces.json           the workspace registry
//! <paseo>/projects/workspace-labels.json     the label catalog (0.5 and later)
//! ```
//!
//! They go into Burrow's home through the store, as if Burrow had
//! made them:
//! - **Agents of every provider both run** (Claude, Codex, Cursor, pi and
//!   Antigravity) keep their id, title, labels, timestamps, attention,
//!   archive state, mode, model and thinking level, and their provider
//!   session, which their next prompt resumes: Paseo's resume handle is
//!   Burrow's. They come back `closed`. Their timeline is read once from
//!   the provider's own records where it keeps them readable (Claude's
//!   transcripts, Codex's threads through its app-server, pi's session
//!   files), as a new epoch. Cursor and Antigravity replay nothing ahead of
//!   a session, so theirs start empty; the session itself carries on.
//! - **Projects and workspaces** keep their ids. A project whose root the
//!   Burrow already has maps to that one. Burrow cannot archive either,
//!   so archived ones are left out unless an imported active agent sits in
//!   one. Their label assignments come with them.
//! - **Labels** keep their names and colours; a label Burrow already has
//!   by that name keeps Burrow's colour.
//! - **Skipped:** providers Burrow does not have (OpenCode, custom ACP
//!   agents), internal agents, and records that do not parse.
//!
//! What Burrow's home already has wins: an id it knows is left alone,
//! so a second run adds nothing. Burrow's daemon must not be running on
//! that home, because it rewrites the registries from memory; there is no
//! lock to check that with.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use burrow_protocol::directory::{
    AgentSnapshot, AgentStatus, AttentionReason, ProjectKind, WorkspaceKind,
};
use burrow_protocol::labels::LabelDefinition;
use burrow_provider::ProviderInfo;
use burrow_provider_claude::history;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::agents::{capabilities, modes, placement};
use crate::clock;
use crate::git::{self, GitFacts};
use crate::persist::{self, Persist};
use crate::store::{ProjectRecord, Store, WorkspaceRecord};

/// What to import, from where, into where.
#[derive(Debug, Clone)]
pub struct Options {
    /// Paseo's home (`~/.paseo`).
    pub from: PathBuf,
    /// Burrow's home.
    pub home: PathBuf,
    /// Claude Code's config directory, which holds the transcripts.
    pub claude_dir: PathBuf,
    /// pi's sessions directory.
    pub pi_sessions: PathBuf,
    /// Codex's CLI, whose app-server reads its threads back; `None` leaves
    /// an imported Codex agent's timeline to be read at its first prompt.
    pub codex: Option<PathBuf>,
    /// Report what would happen; write nothing.
    pub dry_run: bool,
}

/// One line per decision, and the counts.
#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<String>,
    pub agents: usize,
    pub projects: usize,
    pub workspaces: usize,
    pub labels: usize,
    pub skipped: usize,
    /// Records Burrow's home already had.
    pub present: usize,
    pub warnings: usize,
}

impl Report {
    fn note(&mut self, line: String) {
        self.lines.push(line);
    }

    fn skip(&mut self, line: String) {
        self.skipped += 1;
        self.lines.push(format!("skip: {line}"));
    }

    fn warn(&mut self, line: String) {
        self.warnings += 1;
        self.lines.push(format!("warning: {line}"));
    }

    fn present(&mut self, line: String) {
        self.present += 1;
        self.lines.push(format!("already in burrow: {line}"));
    }

    pub fn summary(&self, dry_run: bool) -> String {
        format!(
            "{} {} agents, {} projects, {} workspaces and {} labels; {} skipped, {} already in \
             burrow, {} warnings.{}",
            if dry_run { "Would import" } else { "Imported" },
            self.agents,
            self.projects,
            self.workspaces,
            self.labels,
            self.skipped,
            self.present,
            self.warnings,
            if dry_run { " Nothing was written." } else { "" },
        )
    }
}

/// `StoredAgentRecord` (`agent-storage.ts`), the fields the import uses.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaseoAgent {
    id: String,
    provider: String,
    cwd: String,
    #[serde(default)]
    workspace_id: Option<String>,
    created_at: String,
    updated_at: String,
    /// Diverges from `updatedAt`; Paseo shows the later of the two.
    #[serde(default)]
    last_activity_at: Option<String>,
    #[serde(default)]
    last_user_message_at: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    /// The last live mode; it wins over `config.modeId`.
    #[serde(default)]
    last_mode_id: Option<String>,
    #[serde(default)]
    config: Option<PaseoConfig>,
    #[serde(default)]
    runtime_info: Option<Value>,
    /// `{provider, sessionId, nativeHandle?, metadata?}`: the resume handle.
    #[serde(default)]
    persistence: Option<Value>,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    requires_attention: Option<bool>,
    #[serde(default)]
    attention_reason: Option<AttentionReason>,
    #[serde(default)]
    attention_timestamp: Option<String>,
    #[serde(default)]
    internal: Option<bool>,
    #[serde(default)]
    archived_at: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaseoConfig {
    #[serde(default)]
    mode_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    thinking_option_id: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
}

/// `PersistedProjectRecordSchema`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaseoProject {
    project_id: String,
    root_path: String,
    kind: ProjectKind,
    display_name: String,
    #[serde(default)]
    project_key: Option<String>,
    #[serde(default)]
    custom_name: Option<String>,
    #[serde(default)]
    archived_at: Option<String>,
}

/// `PersistedWorkspaceRecordSchema`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaseoWorkspace {
    workspace_id: String,
    project_id: String,
    cwd: String,
    kind: WorkspaceKind,
    display_name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    worktree_root: Option<String>,
    #[serde(default)]
    pinned_at: Option<String>,
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    archived_at: Option<String>,
    /// Paseo 0.12's background workspaces.
    #[serde(default)]
    background: bool,
    #[serde(default)]
    created_at: Option<String>,
}

struct Paseo {
    projects: Vec<PaseoProject>,
    workspaces: Vec<PaseoWorkspace>,
    /// The label catalog, `WorkspaceLabelDefinitionSchema[]`.
    labels: Vec<LabelDefinition>,
    agents: Vec<PaseoAgent>,
}

/// Runs the import. Errors are what stops it as a whole: no Paseo home, a
/// Burrow home inside it, or a Burrow home that cannot be opened or written.
pub fn run(options: &Options) -> io::Result<Report> {
    if !options.from.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no Paseo home at {}", options.from.display()),
        ));
    }
    if resolved(&options.home)?.starts_with(resolved(&options.from)?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is inside the Paseo home; the import never writes there",
                options.home.display()
            ),
        ));
    }
    let mut report = Report::default();
    let paseo = read_paseo(&options.from, &mut report)?;
    // A Burrow that has no identity yet takes Paseo's, so clients see the
    // same host after the switch. New projects are keyed by it, as the
    // daemon keys them.
    let read_id = |dir: &Path| {
        (std::fs::read_to_string(dir.join("server-id")).ok())
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
    };
    let server_id = match (read_id(&options.home), read_id(&options.from)) {
        (Some(id), _) => id,
        (None, Some(id)) => {
            if !options.dry_run {
                crate::home::write_server_id(&options.home, &id)?;
            }
            report.note(format!("server id {id}, Paseo's"));
            id
        }
        (None, None) => String::new(),
    };
    if options.dry_run {
        let mut store = Store::restore(persist::read(&options.home)?);
        store.server_id = server_id;
        import(&mut store, paseo, options, &mut report);
        return Ok(report);
    }
    let (persist, writer, loaded) = Persist::open(&options.home)?;
    let mut store = Store::open(persist.clone(), loaded);
    store.server_id = server_id;
    let imported = import(&mut store, paseo, options, &mut report);
    persist.flush();
    drop(store);
    drop(writer);
    // The writer only logs a failed write; check that the agents landed.
    let saved: HashSet<String> = persist::read(&options.home)?
        .agents
        .into_iter()
        .map(|record| record.snapshot.id)
        .collect();
    let missing = imported.iter().filter(|id| !saved.contains(*id)).count();
    if missing > 0 {
        return Err(io::Error::other(format!(
            "{missing} imported agents are not on disk in {}; see the log above",
            options.home.display()
        )));
    }
    Ok(report)
}

/// `path` made absolute with its links resolved, as far as it exists.
fn resolved(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let mut rest = Vec::new();
    let mut base = path.as_path();
    loop {
        if let Ok(real) = fs::canonicalize(base) {
            return Ok(rest.iter().rev().fold(real, |path, part| path.join(part)));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                base = parent;
            }
            _ => return Ok(path),
        }
    }
}

// --- reading Paseo's home ---------------------------------------------------

fn read_paseo(from: &Path, report: &mut Report) -> io::Result<Paseo> {
    let projects = registry(from, "projects/projects.json", "project", report)?;
    let workspaces = registry(from, "projects/workspaces.json", "workspace", report)?;
    let labels = registry(from, "projects/workspace-labels.json", "label", report)?;
    let mut by_id: BTreeMap<String, (String, PaseoAgent)> = BTreeMap::new();
    for path in agent_files(&from.join("agents"))? {
        let shown = shown_path(from, &path);
        let parsed = fs::read(&path)
            .map_err(|err| err.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<PaseoAgent>(&bytes).map_err(|err| err.to_string())
            });
        let agent = match parsed {
            Ok(agent) => agent,
            Err(err) => {
                report.skip(format!("{shown}: not a Paseo agent record ({err})"));
                continue;
            }
        };
        // A record that moved with its cwd can leave a stale copy behind;
        // the newer one is the agent.
        let (kept_path, kept) = match by_id.remove(&agent.id) {
            Some((seen_path, seen)) if updated_at(&seen) > updated_at(&agent) => {
                report.skip(format!("{shown}: an older copy of agent {}", agent.id));
                (seen_path, seen)
            }
            Some((seen_path, _)) => {
                report.skip(format!("{seen_path}: an older copy of agent {}", agent.id));
                (shown, agent)
            }
            None => (shown, agent),
        };
        by_id.insert(kept.id.clone(), (kept_path, kept));
    }
    let mut agents: Vec<PaseoAgent> = by_id.into_values().map(|(_, agent)| agent).collect();
    agents.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    Ok(Paseo {
        projects,
        workspaces,
        labels,
        agents,
    })
}

/// A registry file: a JSON array of records. A record that does not parse
/// is skipped on its own; Paseo would drop the whole file.
fn registry<T: DeserializeOwned>(
    from: &Path,
    name: &str,
    what: &str,
    report: &mut Report,
) -> io::Result<Vec<T>> {
    let bytes = match fs::read(from.join(name)) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let items = match serde_json::from_slice::<Vec<Value>>(&bytes) {
        Ok(items) => items,
        Err(err) => {
            report.skip(format!("{name}: not a list of records ({err})"));
            return Ok(Vec::new());
        }
    };
    let mut records = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        match serde_json::from_value(item) {
            Ok(record) => records.push(record),
            Err(err) => report.skip(format!("{what} #{} in {name}: {err}", index + 1)),
        }
    }
    Ok(records)
}

/// `agents/*.json` and `agents/*/*.json`, as Paseo scans them, in path order.
fn agent_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let is_json = |path: &Path| path.is_file() && path.extension().is_some_and(|e| e == "json");
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            for inner in fs::read_dir(&path)? {
                let inner = inner?.path();
                if is_json(&inner) {
                    files.push(inner);
                }
            }
        } else if is_json(&path) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn shown_path(from: &Path, path: &Path) -> String {
    path.strip_prefix(from)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The later of `updatedAt` and `lastActivityAt`. Both are JavaScript
/// `toISOString()` output, so they compare as strings.
fn updated_at(agent: &PaseoAgent) -> &str {
    match agent.last_activity_at.as_deref() {
        Some(activity) if activity > agent.updated_at.as_str() => activity,
        _ => &agent.updated_at,
    }
}

// --- writing Burrow's store ---------------------------------------------

/// Applies the import to `store`. Returns the ids of the agents it added.
fn import(store: &mut Store, paseo: Paseo, options: &Options, report: &mut Report) -> Vec<String> {
    let mut agents = Vec::new();
    for agent in paseo.agents {
        if provider_info(&agent.provider).is_none() {
            report.skip(format!(
                "agent {}: provider \"{}\" (Burrow has no such provider)",
                agent.id, agent.provider
            ));
        } else if agent.internal == Some(true) {
            report.skip(format!("agent {}: an internal agent", agent.id));
        } else if store.agent(&agent.id).is_some() {
            report.present(format!("agent {}", agent.id));
        } else {
            agents.push(agent);
        }
    }

    // Archived workspaces and projects come along only for an active agent.
    let used: HashSet<&str> = agents
        .iter()
        .filter(|agent| agent.archived_at.is_none())
        .filter_map(|agent| agent.workspace_id.as_deref())
        .collect();
    let workspaces: Vec<&PaseoWorkspace> = paseo
        .workspaces
        .iter()
        .filter(|w| w.archived_at.is_none() || used.contains(w.workspace_id.as_str()))
        .collect();
    for workspace in &paseo.workspaces {
        if !workspaces
            .iter()
            .any(|w| w.workspace_id == workspace.workspace_id)
        {
            report.skip(format!(
                "workspace {} {}: archived",
                workspace.workspace_id, workspace.cwd
            ));
        }
    }
    let used: HashSet<&str> = workspaces.iter().map(|w| w.project_id.as_str()).collect();

    // Paseo's project id to Burrow's.
    let mut project_ids: HashMap<String, String> = HashMap::new();
    for project in &paseo.projects {
        let id = &project.project_id;
        if project.archived_at.is_some() && !used.contains(id.as_str()) {
            report.skip(format!("project {id} {}: archived", project.root_path));
        } else if store.projects.get(id).is_some() {
            report.present(format!("project {id} {}", project.root_path));
            project_ids.insert(id.clone(), id.clone());
        } else if let Some((existing, _)) = store
            .projects
            .iter()
            .find(|(p, _)| p.root_path == project.root_path)
        {
            report.present(format!(
                "project {id} {}, as {}",
                project.root_path, existing.project_id
            ));
            project_ids.insert(id.clone(), existing.project_id.clone());
        } else {
            store.upsert_project(ProjectRecord {
                project_id: id.clone(),
                project_key: project
                    .project_key
                    .clone()
                    .unwrap_or_else(|| project.root_path.clone()),
                root_path: project.root_path.clone(),
                kind: project.kind,
                display_name: project.display_name.clone(),
                custom_name: project.custom_name.clone(),
            });
            report.projects += 1;
            report.note(format!("project {id} {}", project.root_path));
            project_ids.insert(id.clone(), id.clone());
        }
    }

    for workspace in workspaces {
        let id = &workspace.workspace_id;
        if store.workspaces.get(id).is_some() {
            report.present(format!("workspace {id} {}", workspace.cwd));
            continue;
        }
        let Some(project_id) = project_ids.get(&workspace.project_id) else {
            report.skip(format!(
                "workspace {id} {}: its project {} is not in projects.json",
                workspace.cwd, workspace.project_id
            ));
            continue;
        };
        store.upsert_workspace(WorkspaceRecord {
            workspace_id: id.clone(),
            project_id: project_id.clone(),
            cwd: workspace.cwd.clone(),
            kind: workspace.kind,
            display_name: workspace.display_name.clone(),
            title: workspace.title.clone(),
            pinned_at: workspace.pinned_at.clone(),
            labels: workspace.labels.clone().unwrap_or_default(),
            git: checkout(workspace),
            background: workspace.background,
            base_branch: None,
            archived_at: None,
            created_at: workspace.created_at.clone(),
            setup_status: None,
        });
        report.workspaces += 1;
        report.note(format!("workspace {id} {}", workspace.cwd));
    }

    // The catalog's names and colours; workspaces already carry their
    // assignments by name. A label Burrow has keeps its own colour.
    for label in paseo.labels {
        let name = label.name.clone();
        if store.labels.find(&name).is_some() {
            report.present(format!("label \"{name}\""));
        } else if store.import_label(label).is_some() {
            report.labels += 1;
            report.note(format!("label \"{name}\""));
        } else {
            report.skip(format!("label \"{name}\": no name"));
        }
    }

    agents
        .into_iter()
        .map(|agent| import_agent(store, agent, options, report))
        .collect()
}

/// What Burrow offers for one of Paseo's provider ids, before its CLI is
/// probed (the session reports its own models and modes); `None` for one
/// Burrow does not have.
fn provider_info(provider: &str) -> Option<ProviderInfo> {
    match provider {
        "claude" => Some(burrow_provider_claude::info(None)),
        "codex" => Some(burrow_provider_codex::info(None, &Default::default())),
        "pi" => Some(burrow_provider_pi::info(Vec::new())),
        burrow_provider_antigravity::catalog::PROVIDER_ID => {
            Some(burrow_provider_antigravity::info(Vec::new()))
        }
        "cursor" => Some(burrow_provider_acp::info(
            &burrow_provider_acp::cursor(None),
            &Default::default(),
        )),
        _ => None,
    }
}

/// The conversation behind an imported session, from the provider's own
/// records. `Err` says why there is none, and what becomes of the timeline.
fn history(
    agent: &PaseoAgent,
    label: &str,
    session: &str,
    options: &Options,
) -> Result<Vec<burrow_provider::HistoryItem>, String> {
    match agent.provider.as_str() {
        "claude" => {
            let path =
                history::locate(&options.claude_dir, &agent.cwd, session).ok_or_else(|| {
                    format!(
                        "no transcript for Claude session {session} under {}; its timeline starts \
                     empty",
                        options.claude_dir.join("projects").display()
                    )
                })?;
            let text = fs::read_to_string(&path).map_err(|err| {
                format!(
                    "cannot read {} ({err}); its timeline starts empty",
                    path.display()
                )
            })?;
            Ok(history::parse(&text))
        }
        "codex" => {
            let Some(binary) = &options.codex else {
                return Err(format!(
                    "Codex is not here to read thread {session}; its timeline is read at the \
                     agent's first prompt"
                ));
            };
            burrow_provider_codex::read_thread(binary, &agent.cwd, session).map_err(|err| {
                format!(
                    "cannot read Codex thread {session} ({err}); its timeline is read at the \
                     agent's first prompt"
                )
            })
        }
        "pi" => {
            burrow_provider_pi::history::locate(&options.pi_sessions, &agent.cwd, session)
                .ok_or_else(|| {
                    format!(
                        "no pi session {session} under {}; its timeline starts empty",
                        options.pi_sessions.display()
                    )
                })?;
            Ok(burrow_provider_pi::history::read(
                &options.pi_sessions,
                &agent.cwd,
                session,
            ))
        }
        _ => Err(format!(
            "{label} replays nothing ahead of a session, so its timeline starts empty; the \
             session itself resumes"
        )),
    }
}

/// The checkout as git reports it now, else as Paseo recorded it (the
/// directory may be gone).
fn checkout(workspace: &PaseoWorkspace) -> Option<GitFacts> {
    if workspace.kind == WorkspaceKind::Directory {
        return None;
    }
    git::probe(Path::new(&workspace.cwd)).or_else(|| {
        workspace.worktree_root.clone().map(|root| GitFacts {
            worktree_root: root,
            branch: workspace.branch.clone(),
            remote_url: None,
            main_repo_root: None,
            paseo_owned: false,
        })
    })
}

/// The provider session to resume: the persistence handle, else the
/// runtime info's session id.
fn session_id(agent: &PaseoAgent) -> Option<String> {
    let handle = agent.persistence.as_ref().map(|p| &p["sessionId"]);
    let runtime = agent.runtime_info.as_ref().map(|r| &r["sessionId"]);
    handle
        .and_then(Value::as_str)
        .or_else(|| runtime.and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn import_agent(
    store: &mut Store,
    agent: PaseoAgent,
    options: &Options,
    report: &mut Report,
) -> String {
    let id = agent.id.clone();
    let info = provider_info(&agent.provider).expect("only known providers are imported");
    let info = &info;
    let config = agent.config.as_ref();
    let session = session_id(&agent);
    let persistence = session.as_ref().map(|session| match &agent.persistence {
        // Paseo's handle has Burrow's shape already; keep its metadata.
        Some(handle) if handle["sessionId"] == session.as_str() => handle.clone(),
        _ => json!({"provider": agent.provider, "sessionId": session, "nativeHandle": session}),
    });
    // Paseo only shows attention that has a reason and a time.
    let attention = agent.requires_attention == Some(true)
        && agent.attention_reason.is_some()
        && agent.attention_timestamp.is_some();
    let thinking = config.and_then(|c| c.thinking_option_id.clone());
    let effective_thinking = match agent.runtime_info.as_ref().and_then(Value::as_object) {
        Some(runtime) if runtime.contains_key("thinkingOptionId") => {
            runtime["thinkingOptionId"].as_str().map(str::to_string)
        }
        _ => thinking.clone(),
    };
    let archived = agent.archived_at.is_some();
    // Said after the agent's own line.
    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    let workspace_id = match agent.workspace_id.clone() {
        Some(id) if store.workspaces.get(&id).is_some() => Some(id),
        _ if archived => None,
        _ => {
            // An active agent needs a workspace; find or make one for its cwd,
            // as a new agent would get.
            let git = git::probe(Path::new(&agent.cwd));
            let (workspace_id, _) = placement(store, &agent.cwd, git, false);
            notes.push(format!(
                "agent {id}: not in a Paseo workspace; placed in {workspace_id} for {}",
                agent.cwd
            ));
            Some(workspace_id)
        }
    };
    let snapshot = AgentSnapshot {
        id: id.clone(),
        provider: agent.provider.clone(),
        cwd: agent.cwd.clone(),
        workspace_id,
        model: config.and_then(|c| c.model.clone()),
        features: Some(Vec::new()),
        thinking_option_id: thinking,
        effective_thinking_option_id: effective_thinking,
        created_at: agent.created_at.clone(),
        updated_at: updated_at(&agent).to_string(),
        last_user_message_at: agent.last_user_message_at.clone(),
        status: AgentStatus::Closed,
        active_turn: None,
        capabilities: capabilities(info),
        current_mode_id: agent
            .last_mode_id
            .clone()
            .or_else(|| config.and_then(|c| c.mode_id.clone()))
            .or_else(|| info.default_mode_id.clone()),
        available_modes: modes(info),
        pending_permissions: Vec::new(),
        persistence,
        runtime_info: agent.runtime_info.clone(),
        last_usage: None,
        last_error: agent.last_error.clone(),
        title: agent.title.clone(),
        labels: agent.labels.clone(),
        requires_attention: attention,
        attention_reason: agent.attention_reason.filter(|_| attention),
        attention_timestamp: agent.attention_timestamp.clone().filter(|_| attention),
        archived_at: agent.archived_at.clone(),
    };
    if let Some(prompt) = config.and_then(|c| c.system_prompt.as_deref()) {
        store.set_provider_config(&id, json!({"systemPrompt": prompt}));
    }
    if archived {
        store.insert_archived_agent(snapshot);
    } else {
        store.upsert_agent(snapshot);
    }

    let mut rows = 0;
    match &session {
        None => warnings.push(format!(
            "agent {id}: no {} session id; its timeline starts empty and its next prompt \
             starts a new session",
            info.label
        )),
        Some(session) => match history(&agent, &info.label, session, options) {
            Err(why) => warnings.push(format!("agent {id}: {why}")),
            Ok(entries) => {
                for entry in entries {
                    let at = entry.timestamp.unwrap_or_else(clock::now_iso);
                    if store.append_timeline(&id, entry.item, None, at).is_some() {
                        rows += 1;
                    }
                }
            }
        },
    }
    if !archived && !Path::new(&agent.cwd).is_dir() {
        warnings.push(format!(
            "agent {id}: {} does not exist here, so its next prompt will fail",
            agent.cwd
        ));
    }
    report.agents += 1;
    report.note(format!(
        "agent {id} {}{}: {rows} timeline items{}",
        agent
            .title
            .as_deref()
            .map_or_else(|| "(untitled)".to_string(), |t| format!("\"{t}\"")),
        if archived { ", archived" } else { "" },
        session.as_deref().map_or_else(String::new, |s| format!(
            ", resumes {} session {s}",
            info.label
        )),
    ));
    notes.into_iter().for_each(|note| report.note(note));
    warnings
        .into_iter()
        .for_each(|warning| report.warn(warning));
    id
}
