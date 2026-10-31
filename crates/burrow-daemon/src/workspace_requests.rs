//! Adding and removing projects and making workspaces, git worktrees
//! included, as Paseo 0.12 does (`session.ts` `handleProjectAddRequest`,
//! `handleProjectRemoveRequest`, `handleWorkspaceCreation` and its helpers, `workspace-provisioning-service.ts`, `worktree-session.ts`,
//! `paseo-worktree-service.ts`, `workspace-archive-service.ts`), and the
//! reads a new-project or new-workspace form makes: the folder search and
//! the starting branches.
//!
//! Every request here runs as a task, since git and the filesystem block;
//! replies go through the connection's `late` channel.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use burrow_protocol::directory::{ProjectKind, WorkspaceDescriptor};
use burrow_protocol::envelope::session;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::agent_requests::{AgentConfig, ImagePayload, Late, images};
use crate::agents::{self, CreateAgent, NewWorkspace};
use crate::daemon::Daemon;
use crate::git::{self, GitFacts};
use crate::setup;
use crate::store::{StoreEvent, WorkspaceRecord};
use crate::suggest;
use crate::worktrees::{self, Intent, WorktreeError};

/// The kinds this module answers.
pub const KINDS: &[&str] = &[
    "workspace.create.request",
    "project.add.request",
    "project.create_directory.request",
    "open_project_request",
    "directory_suggestions_request",
    "branch_suggestions_request",
    "validate_branch_request",
    "paseo_worktree_list_request",
    "workspace_setup_status_request",
    "archive_workspace_request",
    "project.remove.request",
];

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(work)
        .await
        .expect("a blocking task panicked")
}

fn is_dir(path: &str) -> bool {
    Path::new(path).is_dir()
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `None`: the message did not validate.
pub fn handle(
    daemon: &Arc<Daemon>,
    kind: &str,
    message: &Value,
    late: &Late,
) -> Option<Vec<Value>> {
    fn parse<T: for<'de> Deserialize<'de>>(message: &Value) -> Option<T> {
        T::deserialize(message).ok()
    }
    let daemon = daemon.clone();
    let late = late.clone();
    let send = move |frames: Vec<Value>| {
        for frame in frames {
            let _ = late.send(frame);
        }
    };
    let frames: std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Value>> + Send>> = match kind
    {
        "workspace.create.request" => {
            let request: CreateWorkspace = parse(message)?;
            // Paseo's schema: `wks_` and 16 lowercase hex digits.
            let id_ok = |id: &str| {
                id.strip_prefix("wks_").is_some_and(|hex| {
                    hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                })
            };
            if request.workspace_id.as_deref().is_some_and(|id| !id_ok(id)) {
                return None;
            }
            let message = message.clone();
            let updates = send.clone();
            Box::pin(async move { create_workspace(&daemon, request, &message, updates).await })
        }
        "project.add.request" => {
            let request: CwdRequest = parse(message)?;
            Box::pin(async move { project_add(&daemon, request).await })
        }
        "project.create_directory.request" => {
            let request: CreateDirectory = parse(message)?;
            Box::pin(async move { create_directory(&daemon, request).await })
        }
        "open_project_request" => {
            let request: CwdRequest = parse(message)?;
            Box::pin(async move { open_project(&daemon, request).await })
        }
        "directory_suggestions_request" => {
            let request: DirectorySuggestions = parse(message)?;
            Box::pin(async move { directory_suggestions(request).await })
        }
        "branch_suggestions_request" => {
            let request: BranchSuggestions = parse(message)?;
            Box::pin(async move { branch_suggestions(request).await })
        }
        "validate_branch_request" => {
            let request: ValidateBranch = parse(message)?;
            Box::pin(async move { validate_branch(request).await })
        }
        "paseo_worktree_list_request" => {
            let request: WorktreeList = parse(message)?;
            Box::pin(async move { worktree_list(&daemon, request).await })
        }
        "workspace_setup_status_request" => {
            let request: WorkspaceIdRequest = parse(message)?;
            Box::pin(async move { setup_status(&daemon, request) })
        }
        "archive_workspace_request" => {
            let request: WorkspaceIdRequest = parse(message)?;
            Box::pin(async move { archive_workspace_request(&daemon, request).await })
        }
        "project.remove.request" => {
            let request: ProjectIdRequest = parse(message)?;
            Box::pin(async move { project_remove(&daemon, request).await })
        }
        _ => return None,
    };
    tokio::spawn(async move { send(frames.await) });
    Some(Vec::new())
}

// --- projects ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CwdRequest {
    request_id: String,
    cwd: String,
}

/// `project.add.request`: registers a folder as a project, git or not,
/// without making a workspace. A folder already registered is answered with
/// its project, its kind and key brought up to date.
async fn project_add(daemon: &Arc<Daemon>, request: CwdRequest) -> Vec<Value> {
    let cwd = git::expand_tilde(&request.cwd);
    let reply = |project: Value, error: Value, code: Option<&str>| {
        let mut payload =
            json!({"requestId": request.request_id, "project": project, "error": error});
        if let Some(code) = code {
            payload["errorCode"] = code.into();
        }
        vec![session("project.add.response", payload)]
    };
    if !is_dir(&cwd) {
        return reply(
            Value::Null,
            format!("Directory not found: {cwd}").into(),
            Some("directory_not_found"),
        );
    }
    let project = register_project(daemon, text(&git::resolve(Path::new(&cwd)))).await;
    reply(project, Value::Null, None)
}

/// The project descriptor for `root`, registered if it is new.
async fn register_project(daemon: &Arc<Daemon>, root: String) -> Value {
    let probe_root = PathBuf::from(&root);
    let facts = blocking(move || git::probe(&probe_root)).await;
    daemon.apply(|store| {
        let (id, events) = agents::find_or_create_project(store, &root, facts.as_ref());
        let descriptor = store.projects.get(&id).map(|p| store.project_descriptor(p));
        (json!(descriptor), events)
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectIdRequest {
    request_id: String,
    project_id: String,
}

/// `project.remove.request` (`handleProjectRemoveRequest`): the project's
/// workspaces still in the directory are archived with their agents, then
/// the project is forgotten. Nothing on disk changes: worktrees, branches
/// and files stay, as in Paseo, which archives only the records here. An
/// unknown id is accepted with nothing removed.
async fn project_remove(daemon: &Arc<Daemon>, request: ProjectIdRequest) -> Vec<Value> {
    let project_id = request.project_id.clone();
    let agent_ids: Vec<String> = daemon.read(|store| {
        let active: Vec<&str> = (store.workspaces.iter())
            .filter(|(w, _)| w.project_id == project_id)
            .map(|(w, _)| w.workspace_id.as_str())
            .collect();
        (store.agents.iter())
            .filter(|(a, _)| {
                a.workspace_id
                    .as_deref()
                    .is_some_and(|w| active.contains(&w))
            })
            .map(|(a, _)| a.id.clone())
            .collect()
    });
    // `archiveWorkspaceContents`: a step that fails is logged and skipped.
    for agent_id in agent_ids {
        if let Err(error) = agents::archive(daemon, &agent_id).await {
            warn!(agent_id, %error, "an agent was not archived with its project");
        }
    }
    let archived_at = crate::clock::now_iso();
    let removed = daemon.apply(|store| store.remove_project(&project_id, &archived_at));
    vec![session(
        "project.remove.response",
        json!({"requestId": request.request_id, "projectId": request.project_id,
               "accepted": true, "removedWorkspaceIds": removed, "error": null}),
    )]
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateDirectory {
    request_id: String,
    parent_path: String,
    name: String,
}

struct DirectoryError {
    code: &'static str,
    message: String,
    path: Option<String>,
}

fn directory_error(code: &'static str, message: String) -> DirectoryError {
    DirectoryError {
        code,
        message,
        path: None,
    }
}

/// `validateDirectoryName`.
fn validate_directory_name(name: &str) -> Result<(), DirectoryError> {
    let invalid = |message: &str| Err(directory_error("invalid_name", message.into()));
    if name.trim().is_empty() {
        return invalid("Directory name cannot be empty");
    }
    if name != name.trim() {
        return invalid("Directory name cannot start or end with whitespace");
    }
    if name == "." || name == ".." {
        return invalid("Directory name cannot be . or ..");
    }
    if name.contains(['/', '\\', '\0']) {
        return invalid("Directory name must be a single name without path separators");
    }
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return invalid("Directory name cannot be an absolute path");
    }
    Ok(())
}

/// `project.create_directory.request`: makes a folder and registers it.
async fn create_directory(daemon: &Arc<Daemon>, request: CreateDirectory) -> Vec<Value> {
    let made = make_directory(&request);
    let reply = |path: Value, project: Value, error: Value, code: Value| {
        vec![session(
            "project.create_directory.response",
            json!({"requestId": request.request_id, "directoryPath": path, "project": project,
                   "error": error, "errorCode": code}),
        )]
    };
    match made {
        Ok(path) => {
            let project = register_project(daemon, path.clone()).await;
            reply(path.into(), project, Value::Null, Value::Null)
        }
        Err(error) => reply(
            json!(error.path),
            Value::Null,
            error.message.into(),
            error.code.into(),
        ),
    }
}

fn make_directory(request: &CreateDirectory) -> Result<String, DirectoryError> {
    validate_directory_name(&request.name)?;
    let parent = request.parent_path.trim();
    if parent.is_empty() {
        return Err(directory_error(
            "parent_directory_not_found",
            "Parent directory is required".into(),
        ));
    }
    let parent = git::resolve(Path::new(&git::expand_tilde(parent)));
    let path = git::resolve(&parent.join(&request.name));
    if path.parent() != Some(parent.as_path()) {
        return Err(directory_error(
            "invalid_name",
            "Directory name must resolve inside the selected parent".into(),
        ));
    }
    match std::fs::metadata(&parent) {
        Ok(meta) if meta.is_dir() => {}
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(directory_error(
                "permission_denied",
                format!(
                    "Permission denied reading parent directory: {}",
                    parent.display()
                ),
            ));
        }
        _ => {
            return Err(directory_error(
                "parent_directory_not_found",
                format!("Parent directory not found: {}", parent.display()),
            ));
        }
    }
    let shown = text(&path);
    std::fs::create_dir(&path).map_err(|err| {
        let (code, message) = match err.kind() {
            std::io::ErrorKind::AlreadyExists => (
                "directory_exists",
                format!("Directory already exists: {shown}"),
            ),
            std::io::ErrorKind::NotFound => (
                "parent_directory_not_found",
                format!("Parent directory not found: {}", parent.display()),
            ),
            std::io::ErrorKind::PermissionDenied => (
                "permission_denied",
                format!("Permission denied creating directory: {shown}"),
            ),
            _ => (
                "filesystem_error",
                format!("Failed to create directory: {err}"),
            ),
        };
        DirectoryError {
            code,
            message,
            path: Some(shown.clone()),
        }
    })?;
    Ok(shown)
}

/// `open_project_request`: the folder's own workspace (the first that is
/// not a background one), made with its project when there is none.
async fn open_project(daemon: &Arc<Daemon>, request: CwdRequest) -> Vec<Value> {
    let cwd = git::expand_tilde(&request.cwd);
    let reply = |workspace: Value, error: Value, code: Option<&str>| {
        let mut payload =
            json!({"requestId": request.request_id, "workspace": workspace, "error": error});
        if let Some(code) = code {
            payload["errorCode"] = code.into();
        }
        vec![session("open_project_response", payload)]
    };
    if !is_dir(&cwd) {
        return reply(
            Value::Null,
            format!("Directory not found: {cwd}").into(),
            Some("directory_not_found"),
        );
    }
    let cwd = text(&git::resolve(Path::new(&cwd)));
    let probe_dir = PathBuf::from(&cwd);
    let facts = blocking(move || git::probe(&probe_dir)).await;
    let workspace = daemon.apply(|store| {
        let (id, events) = agents::placement(store, &cwd, facts, false);
        let descriptor = store
            .workspaces
            .get(&id)
            .map(|w| store.workspace_descriptor(w));
        (json!(descriptor), events)
    });
    reply(workspace, Value::Null, None)
}

// --- reads for the forms -------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirectorySuggestions {
    request_id: String,
    query: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    include_files: Option<bool>,
    #[serde(default)]
    include_directories: Option<bool>,
    #[serde(default)]
    match_mode: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// `directory_suggestions_request` ([`crate::suggest`]).
async fn directory_suggestions(request: DirectorySuggestions) -> Vec<Value> {
    let cwd = request
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let mut options = match cwd {
        Some(cwd) => {
            suggest::Options::workspace(PathBuf::from(git::expand_tilde(cwd)), &request.query)
        }
        None => suggest::Options::home(PathBuf::from(git::expand_tilde("~")), &request.query),
    };
    if let Some(files) = request.include_files {
        options.include_files = files;
    }
    if let Some(directories) = request.include_directories {
        options.include_directories = directories;
    }
    options.suffix = request.match_mode.as_deref() == Some("suffix");
    options.limit = request.limit.map(|l| l as usize);
    let entries = blocking(move || suggest::search(options)).await;
    let directories: Vec<&str> = (entries.iter())
        .filter(|e| e.kind == suggest::Kind::Directory)
        .map(|e| e.path.as_str())
        .collect();
    vec![session(
        "directory_suggestions_response",
        json!({"directories": directories, "entries": entries, "error": null,
               "requestId": request.request_id}),
    )]
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchSuggestions {
    request_id: String,
    cwd: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// `branch_suggestions_request`: the starting points a worktree can branch
/// off or check out.
async fn branch_suggestions(request: BranchSuggestions) -> Vec<Value> {
    let cwd = PathBuf::from(git::expand_tilde(&request.cwd));
    let (query, limit) = (request.query.clone(), request.limit);
    let found =
        blocking(move || worktrees::branch_suggestions(&cwd, query.as_deref(), limit)).await;
    let payload = match found {
        Ok(details) => {
            let names: Vec<&str> = details.iter().map(|b| b.name.as_str()).collect();
            json!({"branches": names, "branchDetails": details, "error": null,
                   "requestId": request.request_id})
        }
        Err(error) => json!({"branches": [], "branchDetails": [], "error": error,
                             "requestId": request.request_id}),
    };
    vec![session("branch_suggestions_response", payload)]
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ValidateBranch {
    request_id: String,
    cwd: String,
    branch_name: String,
}

/// `validate_branch_request`: whether a branch exists here or on `origin`.
async fn validate_branch(request: ValidateBranch) -> Vec<Value> {
    let cwd = PathBuf::from(git::expand_tilde(&request.cwd));
    let name = request.branch_name.clone();
    let resolved = blocking(move || {
        worktrees::assert_safe_ref(&name, "branch")?;
        worktrees::resolve_branch(&cwd, &name)
    })
    .await;
    let (exists, reference, remote, error) = match resolved {
        Ok(worktrees::BranchResolution::Local(name)) => (true, json!(name), false, Value::Null),
        Ok(worktrees::BranchResolution::RemoteOnly { remote_ref, .. }) => {
            (true, json!(remote_ref), true, Value::Null)
        }
        Ok(worktrees::BranchResolution::NotFound) => (false, Value::Null, false, Value::Null),
        Err(error) => (false, Value::Null, false, error.into()),
    };
    vec![session(
        "validate_branch_response",
        json!({"exists": exists, "resolvedRef": reference, "isRemote": remote, "error": error,
               "requestId": request.request_id}),
    )]
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeList {
    request_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    repo_root: Option<String>,
}

/// `paseo_worktree_list_request`: Burrow's worktrees of a repository.
async fn worktree_list(daemon: &Arc<Daemon>, request: WorktreeList) -> Vec<Value> {
    let reply = |worktrees: Value, error: Value| {
        vec![session(
            "paseo_worktree_list_response",
            json!({"worktrees": worktrees, "error": error, "requestId": request.request_id}),
        )]
    };
    let Some(cwd) = request.repo_root.clone().or(request.cwd.clone()) else {
        return reply(
            json!([]),
            json!({"code": "UNKNOWN", "message": "cwd or repoRoot is required"}),
        );
    };
    let base = daemon.config.worktrees_root();
    let cwd = PathBuf::from(git::expand_tilde(&cwd));
    let listed = blocking(move || {
        let repo = repo_root_for(&cwd, &base)?;
        worktrees::list(&repo, &base)
    })
    .await;
    match listed {
        Ok(entries) => reply(
            entries
                .into_iter()
                .map(|(entry, created)| {
                    json!({"worktreePath": entry.path, "createdAt": created,
                           "branchName": entry.branch_name, "head": entry.head})
                })
                .collect(),
            Value::Null,
        ),
        Err(error) => reply(json!([]), json!({"code": "UNKNOWN", "message": error})),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceIdRequest {
    request_id: String,
    workspace_id: String,
}

/// `workspace_setup_status_request`: the last setup snapshot, if any.
fn setup_status(daemon: &Daemon, request: WorkspaceIdRequest) -> Vec<Value> {
    let snapshot = daemon
        .setups
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&request.workspace_id)
        .cloned();
    vec![session(
        "workspace_setup_status_response",
        json!({"requestId": request.request_id, "workspaceId": request.workspace_id,
               "snapshot": snapshot}),
    )]
}

// --- worktree workspaces ------------------------------------------------------------

/// `resolveRepoRoot`: the checkout to cut from. A worktree of Burrow's
/// own is cut from its main checkout.
fn repo_root_for(cwd: &Path, base_root: &Path) -> Result<PathBuf, String> {
    let facts = git::probe(cwd).ok_or("Create worktree requires a git repository")?;
    if worktrees::owned_worktree(cwd, base_root).is_some()
        && let Some(main) = facts.main_repo_root
    {
        return Ok(git::canonical(Path::new(&main)));
    }
    Ok(git::canonical(Path::new(&facts.worktree_root)))
}

/// What a new worktree workspace is made from (`CreatePaseoWorktreeInput`).
#[derive(Debug, Clone, Default)]
pub(crate) struct WorktreeSpec {
    /// The checkout (or a folder in it) the worktree is cut from.
    pub cwd: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub worktree_slug: Option<String>,
    /// `branch-off` (the default) or `checkout`.
    pub action: Option<String>,
    /// The branch to check out, or the base to branch off.
    pub ref_name: Option<String>,
    /// A new branch's name; else the worktree's slug.
    pub branch_name: Option<String>,
    /// A change request to check out (`checkoutSource`, `githubPrNumber`).
    pub change_request: bool,
    /// The base when no `ref_name` is given (`baseBranch`), instead of the
    /// repository's default branch.
    pub base_branch: Option<String>,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub background: Option<bool>,
    pub caller_workspace_id: Option<String>,
}

/// A worktree workspace just made.
#[derive(Debug, Clone)]
pub(crate) struct MadeWorktree {
    pub workspace_id: String,
    pub workspace_cwd: String,
    pub worktree_path: String,
    pub branch_name: String,
    pub repo_root: String,
}

const CHANGE_REQUESTS_UNSUPPORTED: &str =
    "Checking out a change request needs a forge, which this daemon does not offer";

/// `createWorktreeCore` and `createPaseoWorktree`: cuts the worktree, then
/// registers its workspace. A worktree made for a workspace that then could
/// not be registered is removed again.
pub(crate) async fn make_worktree_workspace(
    daemon: &Arc<Daemon>,
    spec: WorktreeSpec,
) -> Result<MadeWorktree, WorktreeError> {
    let base_root = daemon.config.worktrees_root();
    let cut_spec = spec.clone();
    let cut = blocking(move || cut_worktree(&cut_spec, &base_root)).await?;
    let (created, repo_root, workspace_cwd, facts) = cut;
    let rollback = |error: WorktreeError| {
        let (repo, path, base) = (
            PathBuf::from(&repo_root),
            PathBuf::from(&created.worktree_path),
            daemon.config.worktrees_root(),
        );
        async move {
            let removed = blocking(move || worktrees::delete(Some(&repo), &path, &base)).await;
            match removed {
                Ok(()) => error,
                Err(cleanup) => WorktreeError {
                    code: error.code,
                    message: format!("{}; rollback also failed: {cleanup}", error.message),
                },
            }
        }
    };
    let caller_background = match &spec.caller_workspace_id {
        Some(id) if spec.background.is_none() => {
            match daemon.read(|store| store.workspaces.get(id).map(|w| w.background)) {
                Some(background) => Some(background),
                None => {
                    return Err(rollback(format!("Caller workspace {id} not found").into()).await);
                }
            }
        }
        _ => None,
    };
    let background = spec.background.or(caller_background).unwrap_or(false);
    let title = (spec.title.as_deref().map(str::trim))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| spec.first_prompt.as_deref().and_then(agents::derive_title));
    let repo_facts = {
        let repo = PathBuf::from(&repo_root);
        blocking(move || git::probe(&repo)).await
    };
    let source = text(&git::resolve(Path::new(&spec.cwd)));
    let made = daemon.apply(|store| {
        let mut events = Vec::new();
        // `resolveSourceProjectForWorktree`: the named project, else the
        // project of a workspace at the source or the repository, else the
        // repository's own.
        let project_id = match &spec.project_id {
            Some(id) if store.projects.get(id).is_some() => id.clone(),
            Some(id) => return (Err(format!("Unknown project: {id}")), events),
            None => {
                let sourced = (store.workspaces.iter())
                    .find(|(w, _)| w.cwd == source)
                    .or_else(|| store.workspaces.iter().find(|(w, _)| w.cwd == repo_root))
                    .map(|(w, _)| w.project_id.clone())
                    .filter(|id| store.projects.get(id).is_some());
                match sourced {
                    Some(id) => id,
                    None => {
                        let (id, made) =
                            agents::find_or_create_project(store, &repo_root, repo_facts.as_ref());
                        events.extend(made);
                        id
                    }
                }
            }
        };
        if let Some(id) = &spec.workspace_id
            && (store.workspaces.get(id).is_some() || store.archived_workspaces.contains_key(id))
        {
            return (Err("workspace_id_conflict".to_string()), events);
        }
        let new = NewWorkspace {
            cwd: workspace_cwd.clone(),
            git: Some(facts),
            background,
            title,
            project_id: Some(project_id),
            base_branch: Some(created.comparison_base_ref.clone()),
            workspace_id: spec.workspace_id.clone(),
        };
        match agents::new_workspace(store, new) {
            Ok((id, made)) => {
                events.extend(made);
                (Ok(id), events)
            }
            Err(error) => (Err(error), events),
        }
    });
    match made {
        Ok(workspace_id) => Ok(MadeWorktree {
            workspace_id,
            workspace_cwd,
            worktree_path: created.worktree_path,
            branch_name: created.branch_name,
            repo_root,
        }),
        Err(error) => Err(rollback(error.into()).await),
    }
}

type Cut = (worktrees::Created, String, String, GitFacts);

/// The git half of [`make_worktree_workspace`]: the intent, the slug, the
/// worktree and where the workspace sits in it.
fn cut_worktree(spec: &WorktreeSpec, base_root: &Path) -> Result<Cut, WorktreeError> {
    let cwd = git::resolve(Path::new(&spec.cwd));
    let source = git::probe(&cwd)
        .ok_or_else(|| WorktreeError::unknown("Create worktree requires a git repository"))?;
    let repo = repo_root_for(&cwd, base_root)?;
    let relative =
        git::relative_inside(Path::new(&source.worktree_root), &cwd).ok_or_else(|| {
            WorktreeError::unknown(format!(
                "Workspace cwd is outside its source worktree: {}",
                cwd.display()
            ))
        })?;
    let requested_slug = match spec.worktree_slug.as_deref().filter(|s| !s.is_empty()) {
        Some(slug) => Some(worktrees::worktree_slug(slug)?),
        None => None,
    };
    let ref_name = (spec.ref_name.as_deref().map(str::trim))
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    let default_base = || -> Result<String, String> {
        match spec.base_branch.as_deref().filter(|b| !b.is_empty()) {
            Some(base) => Ok(base.to_string()),
            None => worktrees::require_default_branch(&repo),
        }
    };
    let intent = if spec.action.as_deref() == Some("checkout") {
        if spec.change_request {
            return Err(WorktreeError::unknown(CHANGE_REQUESTS_UNSUPPORTED));
        }
        match ref_name {
            Some(branch_name) => Intent::CheckoutBranch { branch_name },
            None => {
                return Err(WorktreeError {
                    code: "missing_checkout_target",
                    message: "action \"checkout\" requires refName or checkoutSource".into(),
                });
            }
        }
    } else if spec.change_request {
        return Err(WorktreeError::unknown(CHANGE_REQUESTS_UNSUPPORTED));
    } else {
        let slug = match &requested_slug {
            Some(slug) => slug.clone(),
            None => worktrees::worktree_slug(&worktrees::random_name())?,
        };
        let branch_name = (spec.branch_name.as_deref().map(str::trim))
            .filter(|b| !b.is_empty())
            .map(str::to_string)
            .unwrap_or(slug);
        let base_branch = match ref_name {
            Some(base) => base,
            None => default_base()?,
        };
        Intent::BranchOff {
            base_branch,
            branch_name,
        }
    };
    let slug = match requested_slug {
        Some(slug) => slug,
        None => worktrees::worktree_slug(intent.branch_name())?,
    };
    let created = worktrees::create(&repo, &slug, &intent, base_root)?;
    let worktree_path = PathBuf::from(&created.worktree_path);
    let workspace_cwd = git::resolve(&worktree_path.join(&relative));
    let finish = || -> Result<GitFacts, String> {
        if !workspace_cwd.is_dir() {
            return Err(format!(
                "Selected project directory is missing from the worktree: {}",
                workspace_cwd.display()
            ));
        }
        worktrees::seed_paseo_config(&cwd, &workspace_cwd)?;
        let remote_url = git::read(&repo, &["config", "--get", "remote.origin.url"]);
        Ok(GitFacts {
            worktree_root: created.worktree_path.clone(),
            branch: Some(created.branch_name.clone()),
            remote_url,
            main_repo_root: Some(text(&git::canonical(&repo))),
            paseo_owned: true,
        })
    };
    match finish() {
        Ok(facts) => Ok((created.clone(), text(&repo), text(&workspace_cwd), facts)),
        Err(error) => {
            let cleanup = worktrees::delete(Some(&repo), &worktree_path, base_root);
            Err(WorktreeError::unknown(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; rollback also failed: {cleanup}"),
            }))
        }
    }
}

// --- setup scripts ------------------------------------------------------------------

/// `runWorktreeSetupInBackground`: the workspace's `paseo.json` setup,
/// reported to `workspace_setup_progress` subscribers and kept for
/// `workspace_setup_status_request`, then a fresh `workspace_update`.
pub(crate) fn start_workspace_setup(daemon: &Arc<Daemon>, made: &MadeWorktree) {
    let daemon = daemon.clone();
    let made = made.clone();
    tokio::spawn(async move {
        let progress_daemon = daemon.clone();
        let workspace_id = made.workspace_id.clone();
        let (path, branch) = (made.worktree_path.clone(), made.branch_name.clone());
        let publish =
            move |status: &str, results: &[setup::CommandResult], error: Option<String>| {
                let snapshot = json!({
                    "status": status,
                    "detail": setup::detail(&path, &branch, results),
                    "error": error,
                });
                progress_daemon
                    .setups
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(workspace_id.clone(), snapshot.clone());
                let mut payload = snapshot;
                payload["workspaceId"] = workspace_id.clone().into();
                progress_daemon.apply(|store| {
                    // The workspace says how its setup stands, so Paseo's app
                    // knows to offer it a setup tab.
                    let mut events = match store.workspaces.get(&workspace_id) {
                        Some(record) if record.setup_status.as_deref() != Some(status) => {
                            let mut record = record.clone();
                            record.setup_status = Some(status.to_string());
                            store.upsert_workspace(record)
                        }
                        _ => Vec::new(),
                    };
                    events.push(StoreEvent::Session {
                        kind: "workspace_setup_progress",
                        payload,
                    });
                    ((), events)
                });
            };
        let worktree = made.clone();
        blocking(move || {
            publish("running", &[], None);
            let cwd = PathBuf::from(&worktree.workspace_cwd);
            let commands = match setup::setup_commands(&cwd) {
                Ok(commands) => commands,
                Err(error) => {
                    publish("failed", &[], Some(error));
                    return;
                }
            };
            if commands.is_empty() {
                publish("completed", &[], None);
                return;
            }
            let env = match setup::runtime_env(
                Path::new(&worktree.worktree_path),
                &worktree.branch_name,
                &worktree.repo_root,
            ) {
                Ok(env) => env,
                Err(error) => {
                    publish("failed", &[], Some(error));
                    return;
                }
            };
            match setup::run(&commands, &cwd, &env, |so_far| {
                publish("running", so_far, None)
            }) {
                Ok(results) => publish("completed", &results, None),
                Err((error, results)) => publish("failed", &results, Some(error)),
            }
        })
        .await;
        let id = made.workspace_id.clone();
        daemon.apply(|store| ((), store.workspace_changed(&id).into_iter().collect()));
    });
}

/// `runAsyncWorktreeBootstrap`: for an agent made with git options, the
/// setup lands on its timeline as one `paseo_worktree_setup` tool call.
pub(crate) fn start_agent_setup(daemon: &Arc<Daemon>, made: &MadeWorktree, agent_id: &str) {
    let daemon = daemon.clone();
    let made = made.clone();
    let agent_id = agent_id.to_string();
    tokio::spawn(async move {
        let worktree = made.clone();
        let (results, error) = blocking(move || {
            let cwd = PathBuf::from(&worktree.workspace_cwd);
            let commands = setup::setup_commands(&cwd).map_err(|e| (e, Vec::new()))?;
            let env = setup::runtime_env(
                Path::new(&worktree.worktree_path),
                &worktree.branch_name,
                &worktree.repo_root,
            )
            .map_err(|e| (e, Vec::new()))?;
            setup::run(&commands, &cwd, &env, |_| {})
        })
        .await
        .map_or_else(
            |(error, results)| (results, Some(error)),
            |results| (results, None),
        );
        let detail = setup::detail(&made.worktree_path, &made.branch_name, &results);
        let item = json!({
            "type": "tool_call", "name": "paseo_worktree_setup",
            "callId": uuid::Uuid::new_v4().to_string(),
            "status": if error.is_some() { "failed" } else { "completed" },
            "detail": detail,
            "error": error.map(|message| json!({"message": message})),
        });
        daemon.apply(|store| {
            let event = store.append_timeline(&agent_id, item, None, crate::clock::now_iso());
            ((), event.into_iter().collect())
        });
    });
}

// --- workspace.create ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkspace {
    request_id: String,
    source: WorkspaceSource,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    subscribe: Option<bool>,
    #[serde(default)]
    internal: Option<bool>,
    #[serde(default)]
    background: Option<bool>,
    #[serde(default)]
    caller_agent_id: Option<String>,
    #[serde(default)]
    first_agent_context: Option<FirstAgentContext>,
    #[serde(default)]
    agent: Option<InitialAgent>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FirstAgentContext {
    #[serde(default)]
    prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum WorkspaceSource {
    Directory {
        path: String,
        #[serde(default, rename = "projectId")]
        project_id: Option<String>,
    },
    Worktree(Box<WorktreeSource>),
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeSource {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    ref_name: Option<String>,
    #[serde(default)]
    base_branch: Option<String>,
    #[serde(default)]
    branch_name: Option<String>,
    #[serde(default)]
    checkout_source: Option<Value>,
    #[serde(default)]
    github_pr_number: Option<u64>,
    #[serde(default)]
    worktree_slug: Option<String>,
}

/// `WorkspaceInitialAgentSchema`: `agent.create.request` without its
/// request, workspace and git fields.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitialAgent {
    config: AgentConfig,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    initial_prompt: Option<String>,
    #[serde(default)]
    client_message_id: Option<String>,
    #[serde(default)]
    images: Option<Vec<ImagePayload>>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    caller_agent_id: Option<String>,
    #[serde(default)]
    background: Option<Value>,
}

/// A failure with Paseo's wire code, if it has one.
struct Failure {
    message: String,
    code: Option<&'static str>,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message,
            code: None,
        }
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

/// The creation snapshot (`CreationSnapshotSchema`) as it moves through its
/// phases; each change bumps `revision`, as Paseo's creation service does.
struct Creation {
    snapshot: Value,
    subscribed: bool,
    updates: Box<dyn Fn(Vec<Value>) + Send>,
}

impl Creation {
    fn publish(&mut self, changes: Value) {
        let revision = self.snapshot["revision"].as_u64().unwrap_or(0) + 1;
        if let (Some(snapshot), Value::Object(changes)) = (self.snapshot.as_object_mut(), changes) {
            snapshot.extend(changes);
        }
        self.snapshot["revision"] = revision.into();
        self.notify();
    }

    fn notify(&self) {
        if self.subscribed {
            (self.updates)(vec![session(
                "workspace.create.update",
                self.snapshot.clone(),
            )]);
        }
    }
}

/// The request as its idempotency key fingerprints it.
fn fingerprint(message: &Value) -> Value {
    let mut intent = message.clone();
    if let Some(fields) = intent.as_object_mut() {
        for key in ["type", "requestId", "subscribe", "idempotencyKey"] {
            fields.remove(key);
        }
    }
    intent
}

/// `workspace.create.request`: a new workspace for a folder, or for a new
/// git worktree, never reusing one; with an initial agent when asked.
async fn create_workspace(
    daemon: &Arc<Daemon>,
    request: CreateWorkspace,
    message: &Value,
    updates: impl Fn(Vec<Value>) + Send + 'static,
) -> Vec<Value> {
    let request_id = request.request_id.clone();
    let key = request
        .idempotency_key
        .clone()
        .unwrap_or_else(|| request_id.clone());
    let print = fingerprint(message);
    if request.idempotency_key.is_some() {
        let stored = daemon
            .creations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned();
        match stored {
            Some((stored_print, mut reply)) if stored_print == print => {
                reply["message"]["payload"]["requestId"] = request_id.into();
                return vec![reply];
            }
            Some(_) => {
                return vec![session(
                    "workspace.create.response",
                    json!({"requestId": request_id, "workspace": null,
                           "error": "workspace_request_key_conflict", "setupTerminalId": null}),
                )];
            }
            None => {}
        }
    }
    let reply = Reply {
        request_id: request_id.clone(),
        keyed: request.idempotency_key.is_some(),
        key: key.clone(),
        print,
    };
    let agent_id =
        (request.agent.as_ref()).map(|a| a.agent_id.clone().unwrap_or_else(crate::ids::agent_id));
    let workspace_id = request
        .workspace_id
        .clone()
        .unwrap_or_else(crate::ids::workspace_id);
    let mut creation = Creation {
        snapshot: json!({
            "kind": "workspace", "idempotencyKey": key, "revision": 0, "phase": "accepted",
            "error": null, "workspaceId": workspace_id, "agentId": agent_id,
        }),
        subscribed: request.subscribe == Some(true),
        updates: Box::new(updates),
    };
    creation.notify();

    let provisioned = provision(daemon, &request, &workspace_id).await;
    let made = match provisioned {
        Ok(made) => made,
        Err(failure) => {
            // Paseo: only a refusal before anything was made has a known outcome.
            let known = matches!(
                failure.code,
                Some("directory_not_found" | "source_required")
            );
            let mut changes = json!({"phase": "failed", "error": failure.message,
                                     "failedStage": "workspace", "outcomeUnknown": !known});
            if let Some(code) = failure.code {
                changes["errorCode"] = code.into();
            }
            creation.publish(changes);
            return finish(daemon, reply, creation, None);
        }
    };
    let descriptor = describe(daemon, &made.workspace_id);
    creation.publish(json!({"phase": "workspace_ready", "workspace": descriptor,
                            "workspaceId": made.workspace_id}));
    if let Some(worktree) = &made.worktree {
        start_workspace_setup(daemon, worktree);
    }

    let mut agent = None;
    if let (Some(input), Some(agent_id)) = (request.agent, agent_id) {
        let workspace_dir = descriptor
            .as_ref()
            .map(|w| w.workspace_directory.clone())
            .unwrap_or_else(|| made.cwd.clone());
        match create_initial_agent(daemon, input, &made, &workspace_dir, &agent_id).await {
            Ok((snapshot, prompted)) => {
                let value = json!(snapshot);
                creation.publish(json!({"phase": "agent_ready", "agent": value,
                                        "workspaceId": made.workspace_id}));
                if prompted {
                    creation.publish(json!({"phase": "prompt_started", "agent": value}));
                }
                agent = Some(value);
            }
            Err(error) => {
                let unknown = daemon.read(|store| store.agent(&agent_id).is_some());
                creation.publish(
                    json!({"phase": "failed", "error": error, "failedStage": "agent",
                                        "outcomeUnknown": unknown}),
                );
                return finish(daemon, reply, creation, None);
            }
        }
    }
    creation.publish(json!({"phase": "completed"}));
    finish(daemon, reply, creation, agent)
}

/// A workspace for one run of a schedule that starts a new agent (Paseo's
/// `createDirectoryWorkspace` and `createPaseoWorktreeWorkspace`): a new
/// one for `cwd`, or for a new worktree of it. Returns its id and the
/// folder the agent runs in.
pub(crate) async fn workspace_for_run(
    daemon: &Arc<Daemon>,
    cwd: &str,
    worktree: bool,
    prompt: &str,
) -> Result<(String, String), String> {
    let source = if worktree {
        json!({"kind": "worktree", "cwd": cwd})
    } else {
        json!({"kind": "directory", "path": cwd})
    };
    make_workspace(daemon, source, None, false, Some(prompt)).await
}

/// A new workspace from a `workspace.create` source (`{kind: "directory",
/// path}` or `{kind: "worktree", cwd, action, …}`), as the request makes
/// one, for the agent tools and schedules. Returns its id and the folder
/// an agent in it runs in.
pub(crate) async fn make_workspace(
    daemon: &Arc<Daemon>,
    source: Value,
    title: Option<&str>,
    background: bool,
    prompt: Option<&str>,
) -> Result<(String, String), String> {
    let request: CreateWorkspace = serde_json::from_value(json!({
        "requestId": "tool", "source": source, "title": title, "background": background,
        "firstAgentContext": {"prompt": prompt},
    }))
    .map_err(|err| err.to_string())?;
    let made = provision(daemon, &request, &crate::ids::workspace_id())
        .await
        .map_err(|failure| failure.message)?;
    if let Some(worktree) = &made.worktree {
        start_workspace_setup(daemon, worktree);
    }
    let directory = describe(daemon, &made.workspace_id)
        .map(|w| w.workspace_directory)
        .unwrap_or(made.cwd);
    Ok((made.workspace_id, directory))
}

/// What [`finish`] needs to answer and keep the reply.
struct Reply {
    request_id: String,
    /// The client gave an idempotency key: keep the reply for its retries.
    keyed: bool,
    key: String,
    print: Value,
}

/// The `workspace.create.response`, kept for retries once its outcome is
/// final (completed, or failed with an unknown outcome), as Paseo keeps it.
fn finish(daemon: &Daemon, reply: Reply, creation: Creation, agent: Option<Value>) -> Vec<Value> {
    let snapshot = creation.snapshot;
    let mut payload = json!({
        "requestId": reply.request_id,
        "workspace": snapshot.get("workspace").cloned().unwrap_or(Value::Null),
        "creation": snapshot,
        "error": snapshot["error"],
        "setupTerminalId": null,
    });
    if let Some(agent) = agent {
        payload["agent"] = agent;
    }
    if let Some(code) = snapshot.get("errorCode") {
        payload["errorCode"] = code.clone();
    }
    let frame = session("workspace.create.response", payload);
    let keep = snapshot["phase"] == "completed" || snapshot["outcomeUnknown"] == true;
    if reply.keyed && keep {
        daemon
            .creations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(reply.key, (reply.print, frame.clone()));
    }
    vec![frame]
}

/// A workspace made by [`provision`].
struct Provisioned {
    workspace_id: String,
    cwd: String,
    /// The folder an initial agent's `cwd` is relative to.
    source: String,
    worktree: Option<MadeWorktree>,
}

fn describe(daemon: &Daemon, workspace_id: &str) -> Option<WorkspaceDescriptor> {
    daemon.read(|store| {
        store
            .workspaces
            .get(workspace_id)
            .map(|w| store.workspace_descriptor(w))
    })
}

/// `createRequestedWorkspace`: the refusals, then a folder's workspace or a
/// worktree's.
async fn provision(
    daemon: &Arc<Daemon>,
    request: &CreateWorkspace,
    workspace_id: &str,
) -> Result<Provisioned, Failure> {
    if request.internal == Some(true) {
        return Err(
            "Public internal workspaces were removed; use background workspace creation.".into(),
        );
    }
    if request
        .agent
        .as_ref()
        .is_some_and(|a| a.background.is_some())
    {
        return Err("Configure background on the workspace, not its initial agent.".into());
    }
    let caller_workspace = match &request.caller_agent_id {
        Some(id) => Some(
            daemon
                .read(|store| store.agents.get(id).map(|a| a.workspace_id.clone()))
                .ok_or_else(|| format!("Caller agent {id} not found"))?,
        ),
        None => None,
    }
    .flatten();
    let first_prompt = match &request.agent {
        Some(agent) => agent.initial_prompt.clone(),
        None => request
            .first_agent_context
            .as_ref()
            .and_then(|c| c.prompt.clone()),
    };
    let taken = daemon.read(|store| {
        store.workspaces.get(workspace_id).is_some()
            || store.archived_workspaces.contains_key(workspace_id)
    });
    if taken {
        return Err("workspace_id_conflict".into());
    }
    match &request.source {
        WorkspaceSource::Directory { path, project_id } => {
            let cwd = git::expand_tilde(path);
            if !is_dir(&cwd) {
                return Err(Failure {
                    message: format!("Directory not found: {cwd}"),
                    code: Some("directory_not_found"),
                });
            }
            let cwd = text(&git::resolve(Path::new(&cwd)));
            let probe_dir = PathBuf::from(&cwd);
            let facts = blocking(move || git::probe(&probe_dir)).await;
            let title = (request.title.as_deref().map(str::trim))
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .or_else(|| first_prompt.as_deref().and_then(agents::derive_title));
            let background = match (request.background, &caller_workspace) {
                (Some(background), _) => background,
                (None, Some(caller)) => daemon
                    .read(|store| store.workspaces.get(caller).map(|w| w.background))
                    .ok_or_else(|| format!("Caller workspace {caller} not found"))?,
                (None, None) => false,
            };
            // A named project has its kind and key brought up to date.
            let named_root = (project_id.as_ref()).and_then(|id| {
                daemon.read(|store| store.projects.get(id).map(|p| p.root_path.clone()))
            });
            let named_facts = match &named_root {
                Some(root) => {
                    let root = PathBuf::from(root);
                    blocking(move || git::probe(&root)).await
                }
                None => None,
            };
            let made = daemon.apply(|store| {
                let mut events = Vec::new();
                if let Some(id) = project_id {
                    let Some(project) = store.projects.get(id).cloned() else {
                        return (Err(format!("Unknown project: {id}")), events);
                    };
                    let kind = if named_facts.is_some() {
                        ProjectKind::Git
                    } else {
                        ProjectKind::NonGit
                    };
                    let key = crate::project_key::derive(
                        &project.root_path,
                        named_facts.as_ref(),
                        &store.server_id,
                    );
                    if kind != project.kind || key != project.project_key {
                        events.extend(store.upsert_project(crate::store::ProjectRecord {
                            kind,
                            project_key: key,
                            ..project
                        }));
                    }
                }
                let new = NewWorkspace {
                    cwd: cwd.clone(),
                    git: facts,
                    background,
                    title,
                    project_id: project_id.clone(),
                    base_branch: None,
                    workspace_id: Some(workspace_id.to_string()),
                };
                match agents::new_workspace(store, new) {
                    Ok((id, made)) => {
                        events.extend(made);
                        (Ok(id), events)
                    }
                    Err(error) => (Err(error), events),
                }
            })?;
            Ok(Provisioned {
                workspace_id: made,
                cwd: cwd.clone(),
                source: cwd,
                worktree: None,
            })
        }
        WorkspaceSource::Worktree(source) => {
            let cwd = match (&source.cwd, &source.project_id) {
                (Some(cwd), _) => git::expand_tilde(cwd),
                (None, Some(project_id)) => daemon
                    .read(|store| store.projects.get(project_id).map(|p| p.root_path.clone()))
                    .ok_or_else(|| format!("Project not found: {project_id}"))?,
                (None, None) => {
                    return Err(Failure {
                        message: "cwd or projectId is required for a worktree-backed workspace"
                            .into(),
                        code: Some("source_required"),
                    });
                }
            };
            let spec = WorktreeSpec {
                cwd: cwd.clone(),
                workspace_id: Some(workspace_id.to_string()),
                project_id: source.project_id.clone(),
                worktree_slug: source.worktree_slug.clone(),
                action: source.action.clone(),
                ref_name: source.ref_name.clone(),
                branch_name: source.branch_name.clone(),
                change_request: source.checkout_source.is_some()
                    || source.github_pr_number.is_some(),
                base_branch: source.base_branch.clone(),
                title: request.title.clone(),
                first_prompt,
                background: request.background,
                caller_workspace_id: caller_workspace,
            };
            let made = make_worktree_workspace(daemon, spec)
                .await
                .map_err(|error| Failure::from(error.message))?;
            Ok(Provisioned {
                workspace_id: made.workspace_id.clone(),
                cwd: made.workspace_cwd.clone(),
                source: text(&git::resolve(Path::new(&cwd))),
                worktree: Some(made),
            })
        }
    }
}

/// The initial agent, in the new workspace. Its `cwd` must be inside the
/// source folder; it then works in the workspace's directory, since Paseo
/// places an agent made for a workspace in that workspace's `cwd`. Returns
/// it and whether its first prompt went out.
async fn create_initial_agent(
    daemon: &Arc<Daemon>,
    input: InitialAgent,
    made: &Provisioned,
    workspace_dir: &str,
    agent_id: &str,
) -> Result<(burrow_protocol::directory::AgentSnapshot, bool), String> {
    let wanted = git::resolve(Path::new(&input.config.cwd));
    if !wanted.starts_with(git::resolve(Path::new(&made.source))) {
        return Err("Agent directory must be inside the workspace source".into());
    }
    let cwd = workspace_dir.to_string();
    let prompted = input
        .initial_prompt
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty())
        || input.images.as_ref().is_some_and(|i| !i.is_empty());
    let agent = agents::create(
        daemon,
        CreateAgent {
            provider: input.config.provider,
            cwd,
            model: input.config.model,
            mode_id: input.config.mode_id,
            thinking_option_id: input.config.thinking_option_id,
            title: input.config.title,
            workspace_id: Some(made.workspace_id.clone()),
            labels: input.labels.unwrap_or_default(),
            initial_prompt: input.initial_prompt,
            client_message_id: input.client_message_id,
            images: images(input.images),
            system_prompt: input.config.system_prompt,
            env: input.config.env.unwrap_or_default(),
            background: false,
            agent_id: Some(input.agent_id.unwrap_or_else(|| agent_id.to_string())),
            caller_agent_id: input.caller_agent_id,
        },
    )
    .await?;
    Ok((agent, prompted))
}

// --- archiving ----------------------------------------------------------------------

/// `archive_workspace_request`.
async fn archive_workspace_request(
    daemon: &Arc<Daemon>,
    request: WorkspaceIdRequest,
) -> Vec<Value> {
    let (archived_at, error) = match archive_workspace(daemon, &request.workspace_id).await {
        Ok(at) => (Value::String(at), Value::Null),
        Err(error) => (Value::Null, Value::String(error)),
    };
    vec![session(
        "archive_workspace_response",
        json!({"requestId": request.request_id, "workspaceId": request.workspace_id,
               "archivedAt": archived_at, "error": error}),
    )]
}

/// `archiveByScope` for one workspace: its agents are archived, then the
/// workspace, and a worktree Burrow made is removed from disk once no
/// other workspace uses it (after `paseo.json`'s `worktree.teardown`). A
/// teardown or removal that fails leaves the folder; the workspace stays
/// archived. Returns `archivedAt`.
pub(crate) async fn archive_workspace(
    daemon: &Arc<Daemon>,
    workspace_id: &str,
) -> Result<String, String> {
    let found = daemon.read(|store| {
        store
            .workspaces
            .get(workspace_id)
            .cloned()
            .map(Ok)
            .or_else(|| {
                store
                    .archived_workspaces
                    .get(workspace_id)
                    .cloned()
                    .map(Err)
            })
    });
    let workspace = match found {
        Some(Ok(workspace)) => workspace,
        Some(Err(archived)) => {
            return Ok(archived.archived_at.unwrap_or_else(crate::clock::now_iso));
        }
        None => return Err(format!("Workspace not found: {workspace_id}")),
    };
    let agent_ids: Vec<String> = daemon.read(|store| {
        (store.agents.iter())
            .filter(|(a, _)| a.workspace_id.as_deref() == Some(workspace_id))
            .map(|(a, _)| a.id.clone())
            .collect()
    });
    for agent_id in agent_ids {
        if let Err(error) = agents::archive(daemon, &agent_id).await {
            warn!(agent_id, %error, "an agent was not archived with its workspace");
        }
    }
    let archived_at = crate::clock::now_iso();
    daemon.apply(|store| ((), store.archive_workspace(workspace_id, &archived_at)));
    remove_worktree_if_unused(daemon, &workspace).await;
    Ok(archived_at)
}

async fn remove_worktree_if_unused(daemon: &Arc<Daemon>, workspace: &WorkspaceRecord) {
    let Some(git) = workspace.git.clone().filter(|g| g.paseo_owned) else {
        return;
    };
    let in_use = daemon.read(|store| {
        (store.workspaces.iter()).any(|(w, _)| {
            w.git
                .as_ref()
                .is_some_and(|other| git::same_path(&other.worktree_root, &git.worktree_root))
        })
    });
    if in_use {
        return;
    }
    let base = daemon.config.worktrees_root();
    let cwd = workspace.cwd.clone();
    let removed = blocking(move || {
        let worktree = PathBuf::from(&git.worktree_root);
        if worktree.exists() {
            let commands = setup::teardown_commands(Path::new(&cwd))?;
            if !commands.is_empty() {
                let repo = git.main_repo_root.clone().unwrap_or_default();
                let env = setup::runtime_env(
                    &worktree,
                    git.branch.as_deref().unwrap_or_default(),
                    &repo,
                )?;
                setup::run(&commands, Path::new(&cwd), &env, |_| {}).map_err(|(error, _)| error)?;
            }
        }
        let repo = git.main_repo_root.as_deref().map(Path::new);
        worktrees::delete(repo, &worktree, &base)
    })
    .await;
    if let Err(error) = removed {
        warn!(workspace = workspace.workspace_id, %error, "the worktree was left on disk");
    }
}
