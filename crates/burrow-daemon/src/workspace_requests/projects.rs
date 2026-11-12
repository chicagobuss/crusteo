//! Projects: registering a folder, creating one, opening it, and removing it
//! with its workspaces.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use burrow_protocol::envelope::session;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use super::{blocking, is_dir, text};
use crate::agents;
use crate::daemon::Daemon;
use crate::git;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CwdRequest {
    request_id: String,
    cwd: String,
}

/// `project.add.request`: registers a folder as a project, git or not,
/// without making a workspace. A folder already registered is answered with
/// its project, its kind and key brought up to date.
pub(super) async fn project_add(daemon: &Arc<Daemon>, request: CwdRequest) -> Vec<Value> {
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
pub(super) struct ProjectIdRequest {
    request_id: String,
    project_id: String,
}

/// `project.remove.request` (`handleProjectRemoveRequest`): the project's
/// workspaces still in the directory are archived with their agents, then
/// the project is forgotten. Nothing on disk changes: worktrees, branches
/// and files stay, as in Paseo, which archives only the records here. An
/// unknown id is accepted with nothing removed.
pub(super) async fn project_remove(daemon: &Arc<Daemon>, request: ProjectIdRequest) -> Vec<Value> {
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
pub(super) struct CreateDirectory {
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
pub(super) async fn create_directory(daemon: &Arc<Daemon>, request: CreateDirectory) -> Vec<Value> {
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
pub(super) async fn open_project(daemon: &Arc<Daemon>, request: CwdRequest) -> Vec<Value> {
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
