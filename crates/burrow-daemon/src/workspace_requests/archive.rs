//! Archiving a workspace: its agents, then the workspace, then the worktree
//! Burrow made for it once nothing else uses it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use burrow_protocol::envelope::session;
use serde_json::{Value, json};
use tracing::warn;

use super::{WorkspaceIdRequest, blocking};
use crate::agents;
use crate::daemon::Daemon;
use crate::git;
use crate::setup;
use crate::store::WorkspaceRecord;
use crate::worktrees;

/// `archive_workspace_request`.
pub(super) async fn archive_workspace_request(
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
    // Its terminals go with it, as in Paseo.
    daemon.terminals.kill_workspace(workspace_id);
    daemon.apply(|store| ((), store.archive_workspace(workspace_id, &archived_at)));
    // On disk before the reply, and before its worktree goes: a restart
    // must not bring back a workspace whose folder is gone.
    let saving = daemon.clone();
    if let Err(error) = blocking(move || saving.flush()).await {
        warn!(workspace_id, %error, "an archived workspace was not saved");
        return Err(format!("Workspace archived but not saved: {error}"));
    }
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
