//! Where an agent lives: the workspace its directory belongs to, the project
//! at its root, and the environment its CLI is started with.

use burrow_protocol::directory::{ProjectKind, WorkspaceKind};
use serde_json::Value;

use crate::clock;
use crate::daemon::Daemon;
use crate::git::GitFacts;
use crate::ids;
use crate::store::{ProjectRecord, Store, StoreEvent, WorkspaceRecord};

/// Finds or makes the workspace an agent in `cwd` belongs to. Finding
/// skips background workspaces, as Paseo's lookup by path does, and a
/// `background` create always makes a new one: background is how a
/// workspace is created, not which one is picked.
pub(crate) fn placement(
    store: &mut Store,
    cwd: &str,
    git: Option<GitFacts>,
    background: bool,
) -> (String, Vec<StoreEvent>) {
    if !background
        && let Some((workspace, _)) =
            (store.workspaces.iter()).find(|(w, _)| w.cwd == cwd && !w.background)
    {
        return (workspace.workspace_id.clone(), Vec::new());
    }
    // Without a project id this cannot fail.
    new_workspace(
        store,
        NewWorkspace {
            cwd: cwd.to_string(),
            git,
            background,
            ..NewWorkspace::default()
        },
    )
    .unwrap_or_default()
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// `findOrCreateProjectForDirectory`: the active project rooted at `root`,
/// its kind and key brought up to date, or a new one. Returns its id.
pub(crate) fn find_or_create_project(
    store: &mut Store,
    root: &str,
    git: Option<&GitFacts>,
) -> (String, Vec<StoreEvent>) {
    let kind = if git.is_some() {
        ProjectKind::Git
    } else {
        ProjectKind::NonGit
    };
    let key = crate::project_key::derive(root, git, &store.server_id);
    let existing = (store.projects.iter())
        .find(|(p, _)| p.root_path == root)
        .map(|(p, _)| p.clone());
    match existing {
        Some(project) if project.kind == kind && project.project_key == key => {
            (project.project_id, Vec::new())
        }
        Some(project) => {
            let id = project.project_id.clone();
            let events = store.upsert_project(ProjectRecord {
                kind,
                project_key: key,
                ..project
            });
            (id, events)
        }
        None => {
            let id = ids::project_id();
            let events = store.upsert_project(ProjectRecord {
                project_id: id.clone(),
                project_key: key,
                root_path: root.to_string(),
                kind,
                display_name: basename(root),
                custom_name: None,
            });
            (id, events)
        }
    }
}

/// What a new workspace record holds.
#[derive(Debug, Clone, Default)]
pub(crate) struct NewWorkspace {
    pub cwd: String,
    pub git: Option<GitFacts>,
    pub background: bool,
    pub title: Option<String>,
    /// Its project; else the project rooted at `cwd`, made if missing.
    pub project_id: Option<String>,
    /// The exact ref a worktree Burrow made was cut from.
    pub base_branch: Option<String>,
    /// The id the client chose for it, else a new one.
    pub workspace_id: Option<String>,
}

/// Makes a workspace, as Paseo's `initialWorkspacePlacement` describes it:
/// a worktree when the checkout is a linked one, named after its branch.
pub(crate) fn new_workspace(
    store: &mut Store,
    workspace: NewWorkspace,
) -> Result<(String, Vec<StoreEvent>), String> {
    let mut events = Vec::new();
    let project_id = match workspace.project_id {
        Some(id) if store.projects.get(&id).is_some() => id,
        Some(id) => return Err(format!("Unknown project: {id}")),
        None => {
            let (id, made) = find_or_create_project(store, &workspace.cwd, workspace.git.as_ref());
            events.extend(made);
            id
        }
    };
    let kind = match &workspace.git {
        Some(git) if git.main_repo_root.is_some() => WorkspaceKind::Worktree,
        Some(_) => WorkspaceKind::LocalCheckout,
        None => WorkspaceKind::Directory,
    };
    let display_name = (workspace.git.as_ref())
        .and_then(|git| git.branch.clone())
        .filter(|branch| !branch.trim().is_empty() && !branch.eq_ignore_ascii_case("HEAD"))
        .unwrap_or_else(|| basename(&workspace.cwd));
    let workspace_id = workspace.workspace_id.unwrap_or_else(ids::workspace_id);
    events.extend(store.upsert_workspace(WorkspaceRecord {
        workspace_id: workspace_id.clone(),
        project_id,
        cwd: workspace.cwd,
        kind,
        display_name,
        title: workspace.title,
        pinned_at: None,
        labels: Vec::new(),
        git: workspace.git,
        background: workspace.background,
        base_branch: workspace.base_branch,
        archived_at: None,
        created_at: Some(clock::now_iso()),
        setup_status: None,
    }));
    Ok((workspace_id, events))
}

/// What an agent's CLI does not inherit from Burrow's environment, as Paseo
/// takes them out of its own:
/// - `PASEO_HOME`, which beside the `PASEO_HOST` agents get makes Paseo's CLI
///   refuse to pick a daemon;
/// - what a Paseo agent, terminal, or the desktop app sets, should Burrow be
///   started from one;
/// - a Claude Code session's markers, which Paseo clears for every provider.
pub const AGENT_UNSET: &[&str] = &[
    "PASEO_HOME",
    "PASEO_WORKSPACE_ID",
    "PASEO_TERMINAL_ID",
    "PASEO_NODE_ENV",
    "PASEO_DESKTOP_MANAGED",
    "PASEO_SUPERVISED",
    "ELECTRON_RUN_AS_NODE",
    "ELECTRON_NO_ATTACH_CONSOLE",
    "ESBUILD_BINARY_PATH",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_AGENT_SDK_VERSION",
];

/// Where Paseo's desktop app keeps its `paseo` CLI. Under Paseo's own
/// daemon an agent finds it there; under Burrow it is put on the agent's
/// `PATH` when no `paseo` is on it already.
const PASEO_APP_CLI_DIRS: &[&str] = &["/Applications/Paseo.app/Contents/Resources/bin"];

/// The environment an agent's CLI gets, as Paseo gives it: who it is
/// (`PASEO_AGENT_ID`, `PASEO_AGENT_CWD`), which Paseo's CLI uses to say a
/// message is from it, to make its children its own and to steer rather
/// than interrupt; where its daemon is (`PASEO_HOST`), so a `paseo` command
/// it runs reaches Burrow without `--host`; what the client that made it
/// asked for (`paseo run --env`); and the Paseo app's CLI on its `PATH`.
pub(super) fn agent_env(
    daemon: &Daemon,
    agent_id: &str,
    cwd: &str,
    provider_config: Option<&Value>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = (provider_config.and_then(|c| c["env"].as_object()))
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
        .collect();
    if let Some(path) = agent_path(std::env::var_os("PATH"), own_dir().as_deref()) {
        env.push(("PATH".into(), path));
    }
    if let Some(endpoint) = daemon.local_endpoint.get() {
        env.push(("PASEO_HOST".into(), endpoint.clone()));
    }
    env.push(("PASEO_AGENT_ID".into(), agent_id.to_string()));
    env.push(("PASEO_AGENT_CWD".into(), cwd.to_string()));
    env
}

/// `PATH` with `burrow` and `paseo` added where they are missing, as an
/// agent's commands get it: for a terminal's shell too. None when nothing
/// needs adding.
pub(crate) fn commands_path() -> Option<String> {
    agent_path(std::env::var_os("PATH"), own_dir().as_deref())
}

/// The folder Burrow runs from, when its program there is called `burrow`:
/// what an agent's `burrow` command should be. A program replaced by an
/// upgrade while it runs reads as `… (deleted)`; the new one is at the path.
fn own_dir() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = match exe.to_str().and_then(|e| e.strip_suffix(" (deleted)")) {
        Some(live) => std::path::PathBuf::from(live),
        None => exe,
    };
    let named = format!("burrow{}", std::env::consts::EXE_SUFFIX);
    (exe.file_name()? == named.as_str()).then(|| exe.parent().map(Into::into))?
}

/// `path` with what an agent's commands need added at the end, each only
/// when that program is not on it already: Burrow's own folder (`own`), for
/// `burrow`, and the Paseo app's CLI folder, for `paseo`, when the app is
/// installed. `None` when nothing needs adding.
fn agent_path(path: Option<std::ffi::OsString>, own: Option<&std::path::Path>) -> Option<String> {
    let paseo = PASEO_APP_CLI_DIRS.iter().map(std::path::Path::new);
    with_programs(
        path,
        [
            ("burrow", own.into_iter().collect()),
            ("paseo", paseo.collect()),
        ],
    )
}

fn with_programs<const N: usize>(
    path: Option<std::ffi::OsString>,
    wanted: [(&str, Vec<&std::path::Path>); N],
) -> Option<String> {
    let path = path?.into_string().ok()?;
    let mut dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
    let has = |dir: &std::path::Path, program: &str| {
        let file = format!("{program}{}", std::env::consts::EXE_SUFFIX);
        dir.join(file).is_file()
    };
    let before = dirs.len();
    for (program, places) in wanted {
        if dirs.iter().any(|d| has(d, program)) {
            continue;
        }
        if let Some(dir) = places.into_iter().find(|d| has(d, program)) {
            dirs.push(dir.to_path_buf());
        }
    }
    if dirs.len() == before {
        return None;
    }
    std::env::join_paths(dirs).ok()?.into_string().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_finds_burrow_and_paseo_on_its_path() {
        let root = std::env::temp_dir().join(format!("burrow-path-{}", uuid::Uuid::new_v4()));
        let (bin, own, app) = (root.join("bin"), root.join("burrow"), root.join("app"));
        for dir in [&bin, &own, &app] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let program = |dir: &std::path::Path, name: &str| {
            let file = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            std::fs::write(file, "").unwrap();
        };
        program(&own, "burrow");
        program(&app, "paseo");
        let path = || Some(std::env::join_paths([&bin]).unwrap());
        let wanted = || {
            [
                ("burrow", vec![own.as_path()]),
                ("paseo", vec![app.as_path()]),
            ]
        };
        let joined = |dirs: &[&std::path::PathBuf]| {
            std::env::join_paths(dirs).unwrap().into_string().unwrap()
        };

        // Neither on it: both folders added, after the agent's own.
        assert_eq!(
            with_programs(path(), wanted()),
            Some(joined(&[&bin, &own, &app]))
        );
        // Each only when missing.
        program(&bin, "paseo");
        assert_eq!(with_programs(path(), wanted()), Some(joined(&[&bin, &own])));
        program(&bin, "burrow");
        assert_eq!(with_programs(path(), wanted()), None);
        // A folder without the program is not added.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let elsewhere = Some(std::env::join_paths([&empty]).unwrap());
        assert_eq!(
            with_programs(elsewhere, [("burrow", vec![empty.as_path()])]),
            None
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
