//! `paseo.json`'s setup scripts, run in the background for a new worktree
//! workspace, and reported on its timeline or to its subscribers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use super::{MadeWorktree, blocking};
use crate::daemon::Daemon;
use crate::setup as scripts;
use crate::store::StoreEvent;

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
            move |status: &str, results: &[scripts::CommandResult], error: Option<String>| {
                let snapshot = json!({
                    "status": status,
                    "detail": scripts::detail(&path, &branch, results),
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
            let commands = match scripts::setup_commands(&cwd) {
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
            let env = match scripts::runtime_env(
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
            match scripts::run(&commands, &cwd, &env, |so_far| {
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
            let commands = scripts::setup_commands(&cwd).map_err(|e| (e, Vec::new()))?;
            let env = scripts::runtime_env(
                Path::new(&worktree.worktree_path),
                &worktree.branch_name,
                &worktree.repo_root,
            )
            .map_err(|e| (e, Vec::new()))?;
            scripts::run(&commands, &cwd, &env, |_| {})
        })
        .await
        .map_or_else(
            |(error, results)| (results, Some(error)),
            |results| (results, None),
        );
        let detail = scripts::detail(&made.worktree_path, &made.branch_name, &results);
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
