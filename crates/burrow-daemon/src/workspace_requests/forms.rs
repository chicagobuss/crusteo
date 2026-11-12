//! The reads a new-project or new-workspace form makes: folder and branch
//! suggestions, branch validation, Burrow's worktrees of a repository, and a
//! workspace's setup status.

use std::path::PathBuf;
use std::sync::Arc;

use burrow_protocol::envelope::session;
use serde::Deserialize;
use serde_json::{Value, json};

use super::worktree::repo_root_for;
use super::{WorkspaceIdRequest, blocking};
use crate::daemon::Daemon;
use crate::git;
use crate::suggest;
use crate::worktrees;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DirectorySuggestions {
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
pub(super) async fn directory_suggestions(request: DirectorySuggestions) -> Vec<Value> {
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
pub(super) struct BranchSuggestions {
    request_id: String,
    cwd: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// `branch_suggestions_request`: the starting points a worktree can branch
/// off or check out.
pub(super) async fn branch_suggestions(request: BranchSuggestions) -> Vec<Value> {
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
pub(super) struct ValidateBranch {
    request_id: String,
    cwd: String,
    branch_name: String,
}

/// `validate_branch_request`: whether a branch exists here or on `origin`.
pub(super) async fn validate_branch(request: ValidateBranch) -> Vec<Value> {
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
pub(super) struct WorktreeList {
    request_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    repo_root: Option<String>,
}

/// `paseo_worktree_list_request`: Burrow's worktrees of a repository.
pub(super) async fn worktree_list(daemon: &Arc<Daemon>, request: WorktreeList) -> Vec<Value> {
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

/// `workspace_setup_status_request`: the last setup snapshot, if any.
pub(super) fn setup_status(daemon: &Daemon, request: WorkspaceIdRequest) -> Vec<Value> {
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
