//! Adding and removing projects and making workspaces, git worktrees
//! included, as Paseo 0.12 does (`session.ts` `handleProjectAddRequest`,
//! `handleProjectRemoveRequest`, `handleWorkspaceCreation` and its helpers, `workspace-provisioning-service.ts`, `worktree-session.ts`,
//! `paseo-worktree-service.ts`, `workspace-archive-service.ts`), and the
//! reads a new-project or new-workspace form makes: the folder search and
//! the starting branches.
//!
//! Every request here runs as a task, since git and the filesystem block;
//! replies go through the connection's `late` channel.
//!
//! The work is split by seam: `projects` registers, creates, opens and
//! removes projects, `forms` answers the reads a form makes, `worktree` cuts
//! a worktree, `workspaces` serves `workspace.create`, `setup` runs the
//! `paseo.json` scripts a workspace asks for, and `archive` archives a
//! workspace and takes its worktree away.

mod archive;
mod forms;
mod projects;
mod setup;
mod workspaces;
mod worktree;

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::agent_requests::Late;
use crate::daemon::Daemon;

pub(crate) use archive::archive_workspace;
pub(crate) use setup::{start_agent_setup, start_workspace_setup};
pub(crate) use workspaces::{make_workspace, workspace_for_run};
pub(crate) use worktree::{MadeWorktree, WorktreeSpec, make_worktree_workspace};

use archive::archive_workspace_request;
use forms::{
    BranchSuggestions, DirectorySuggestions, ValidateBranch, WorktreeList, branch_suggestions,
    directory_suggestions, setup_status, validate_branch, worktree_list,
};
use projects::{
    CreateDirectory, CwdRequest, ProjectIdRequest, create_directory, open_project, project_add,
    project_remove,
};
use workspaces::{CreateWorkspace, create_workspace};

/// A request naming one workspace.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceIdRequest {
    request_id: String,
    workspace_id: String,
}

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
