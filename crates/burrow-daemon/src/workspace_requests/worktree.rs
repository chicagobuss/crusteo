//! Git worktrees: the checkout a workspace is cut from, and the workspace a
//! new worktree gets, rolled back when it cannot be registered.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agents::{self, NewWorkspace};
use crate::daemon::Daemon;
use crate::git::{self, GitFacts};
use crate::worktrees::{self, Intent, WorktreeError};

use super::{blocking, text};

pub(super) fn repo_root_for(cwd: &Path, base_root: &Path) -> Result<PathBuf, String> {
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
