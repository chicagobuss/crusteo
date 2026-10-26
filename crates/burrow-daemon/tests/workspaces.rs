//! Adding projects and making workspaces, git worktrees included, through
//! `paseo-client`, on real scratch git repositories: the shapes Paseo 0.12
//! answers with, where the worktrees land, their setup scripts, the starting
//! branches and the folder search, archiving, and a restart. Agents are the
//! echo provider's; nothing reaches a network or a model.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::git::canonical;
use burrow_daemon::{Config, Daemon, Running, start};
use burrow_provider::echo::EchoProvider;
use paseo_client::{Client, ConnState, Event, EventReceiver};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "burrow-workspaces-{name}-{}",
        burrow_daemon::ids::agent_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    canonical(&dir)
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Runs git for a fixture, away from the user's own git configuration.
fn git(dir: &Path, args: &[&str]) -> String {
    let empty = std::env::temp_dir().join("burrow-workspaces-gitconfig");
    if !empty.exists() {
        std::fs::write(&empty, "").unwrap();
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Burrow Test",
            "-c",
            "user.email=burrow@example.invalid",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &empty)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository on `main` with one commit, a `feature` branch, and a
/// `paseo.json` whose setup prints the worktree's branch.
fn repository(name: &str) -> PathBuf {
    let repo = scratch(name);
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::create_dir_all(repo.join("app")).unwrap();
    std::fs::write(repo.join("app").join("main.txt"), "app\n").unwrap();
    std::fs::write(
        repo.join("paseo.json"),
        r#"{"worktree": {"setup": ["git branch --show-current"]}}"#,
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    git(&repo, &["branch", "feature"]);
    repo
}

struct Harness {
    running: Running,
    daemon: Arc<Daemon>,
    client: Client,
    events: EventReceiver,
}

async fn harness(home: &Path, server_id: &str) -> Harness {
    let mut config = Config::ephemeral();
    config.home = Some(home.to_path_buf());
    config.server_id = server_id.into();
    let daemon = Daemon::open(config).unwrap();
    daemon.agents.register(Arc::new(EchoProvider::default()));
    let running = start(daemon.clone()).await.unwrap();
    let client = Client::connect(paseo_client::Config::new(
        running.local_addr.to_string(),
        paseo_client::generate_client_id(),
    ));
    let events = client.events();
    timeout(WAIT, async {
        loop {
            if let Event::State(ConnState::Connected(info)) = events.recv().await.unwrap() {
                for feature in [
                    "projectAdd",
                    "stableProjectIdentity",
                    "projectCreateDirectory",
                    "workspaceMultiplicity",
                ] {
                    assert!(info.has(feature), "{feature}");
                }
                return;
            }
        }
    })
    .await
    .expect("connected");
    let harness = Harness {
        running,
        daemon,
        client,
        events,
    };
    harness
        .request(json!({"type": "session.events.set_subscription.request",
            "events": ["project.update", "workspace_setup_progress"]}))
        .await;
    harness
        .request(
            json!({"type": "fetch_workspaces_request", "includeBackground": true,
            "subscribe": {}}),
        )
        .await;
    harness
}

impl Harness {
    async fn request(&self, message: Value) -> Value {
        self.client.request(message).await.expect("request")
    }

    /// Pushed messages up to and including the first that matches.
    async fn until(&self, what: &str, matches: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        timeout(WAIT, async {
            loop {
                if let Event::Message(message) = self.events.recv().await.unwrap() {
                    let hit = matches(&message);
                    seen.push(message);
                    if hit {
                        return;
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {what} within {WAIT:?}; saw {seen:#?}"));
        seen
    }

    async fn add_project(&self, cwd: &Path) -> Value {
        let reply = self
            .request(json!({"type": "project.add.request", "cwd": text(cwd)}))
            .await;
        assert!(reply["error"].is_null(), "{reply}");
        reply["project"].clone()
    }

    async fn create(&self, message: Value) -> Value {
        let mut message = message;
        message["type"] = "workspace.create.request".into();
        self.request(message).await
    }

    async fn stop(self) {
        drop(self.client);
        self.running.shutdown().await;
        self.daemon.close();
    }
}

fn worktrees_root(home: &Path) -> PathBuf {
    canonical(&home.join("worktrees"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projects_are_added_with_paseos_keys() {
    let home = scratch("home");
    let server_id = burrow_daemon::ids::server_id();
    let h = harness(&home, &server_id).await;
    let repo = repository("repo");
    let plain = scratch("plain");

    let project = h.add_project(&repo).await;
    assert_eq!(project["projectKind"], "git");
    assert_eq!(project["projectRootPath"], text(&repo));
    assert_eq!(
        project["projectDisplayName"],
        repo.file_name().unwrap().to_str().unwrap()
    );
    // No remote: the key is the folder on this host.
    assert_eq!(
        project["projectKey"],
        format!("host:{server_id}:{}", text(&repo))
    );
    assert!(project["projectId"].as_str().unwrap().starts_with("prj_"));
    let pushed = h
        .until("project.update", |m| {
            m["type"] == "project.update"
                && m["payload"]["project"]["projectId"] == project["projectId"]
        })
        .await;
    assert_eq!(pushed.last().unwrap()["payload"]["kind"], "upsert");

    let other = h.add_project(&plain).await;
    assert_eq!(other["projectKind"], "non_git");
    // Adding it again answers the same project.
    let again = h.add_project(&repo).await;
    assert_eq!(again["projectId"], project["projectId"]);

    // A remote makes the key the remote's, as on every other host.
    git(
        &repo,
        &["remote", "add", "origin", "git@github.com:Example/Shop.git"],
    );
    let keyed = h.add_project(&repo).await;
    assert_eq!(keyed["projectId"], project["projectId"]);
    assert_eq!(keyed["projectKey"], "remote:github.com/example/shop");

    let missing = h
        .request(json!({"type": "project.add.request", "cwd": text(&plain.join("gone"))}))
        .await;
    assert!(missing["project"].is_null());
    assert_eq!(missing["errorCode"], "directory_not_found");
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .starts_with("Directory not found: ")
    );

    // Projects without a workspace are listed, and are the empty ones.
    let listed = h.request(json!({"type": "project.list.request"})).await;
    assert_eq!(listed["projects"].as_array().unwrap().len(), 2);
    let workspaces = h.request(json!({"type": "fetch_workspaces_request"})).await;
    assert_eq!(workspaces["emptyProjects"].as_array().unwrap().len(), 2);

    // A new folder, made and registered.
    let made = h
        .request(json!({"type": "project.create_directory.request",
            "parentPath": text(&plain), "name": "fresh"}))
        .await;
    assert!(made["error"].is_null(), "{made}");
    assert_eq!(made["directoryPath"], text(&plain.join("fresh")));
    assert_eq!(made["project"]["projectKind"], "non_git");
    let taken = h
        .request(json!({"type": "project.create_directory.request",
            "parentPath": text(&plain), "name": "fresh"}))
        .await;
    assert_eq!(taken["errorCode"], "directory_exists");
    let bad = h
        .request(json!({"type": "project.create_directory.request",
            "parentPath": text(&plain), "name": "a/b"}))
        .await;
    assert_eq!(bad["errorCode"], "invalid_name");
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspaces_are_made_for_folders_and_for_new_worktrees() {
    let home = scratch("home");
    let h = harness(&home, &burrow_daemon::ids::server_id()).await;
    let repo = repository("repo");
    let project = h.add_project(&repo).await;
    let project_id = project["projectId"].as_str().unwrap();

    // A folder: never reused, named after its branch.
    let folder = h
        .create(json!({"idempotencyKey": "k-folder",
            "source": {"kind": "directory", "path": text(&repo), "projectId": project_id}}))
        .await;
    assert!(folder["error"].is_null(), "{folder}");
    assert_eq!(folder["setupTerminalId"], Value::Null);
    let workspace = &folder["workspace"];
    assert_eq!(workspace["workspaceKind"], "local_checkout");
    assert_eq!(workspace["name"], "main");
    assert_eq!(workspace["projectId"], project_id);
    assert!(workspace.get("worktreeSlug").is_none());
    let creation = &folder["creation"];
    assert_eq!(creation["phase"], "completed");
    assert_eq!(creation["revision"], 2);
    assert_eq!(creation["idempotencyKey"], "k-folder");
    assert_eq!(creation["agentId"], Value::Null);
    let twice = h
        .create(json!({"source": {"kind": "directory", "path": text(&repo)}}))
        .await;
    assert_ne!(twice["workspace"]["id"], workspace["id"]);
    assert_eq!(twice["workspace"]["projectId"], project_id);

    // A retry with the same key gets the same workspace; a different
    // request under that key is refused.
    let retried = h
        .create(json!({"idempotencyKey": "k-folder",
            "source": {"kind": "directory", "path": text(&repo), "projectId": project_id}}))
        .await;
    assert_eq!(retried["workspace"]["id"], workspace["id"]);
    let clashing = h
        .create(json!({"idempotencyKey": "k-folder", "title": "Other",
            "source": {"kind": "directory", "path": text(&repo)}}))
        .await;
    assert_eq!(clashing["error"], "workspace_request_key_conflict");

    // A worktree checking out an existing branch, from the project.
    let checkout = h
        .create(json!({"title": "Feature work",
            "source": {"kind": "worktree", "projectId": project_id, "action": "checkout",
                       "refName": "feature"}}))
        .await;
    assert!(checkout["error"].is_null(), "{checkout}");
    let ws = &checkout["workspace"];
    assert_eq!(ws["workspaceKind"], "worktree");
    assert_eq!(ws["projectId"], project_id);
    assert_eq!(ws["name"], "Feature work");
    assert_eq!(ws["title"], "Feature work");
    assert_eq!(ws["worktreeSlug"], "feature");
    assert_eq!(ws["gitRuntime"]["currentBranch"], "feature");
    assert_eq!(ws["gitRuntime"]["isPaseoOwnedWorktree"], true);
    let path = PathBuf::from(ws["workspaceDirectory"].as_str().unwrap());
    // `<home>/worktrees/<project hash>/<slug>`, git's own worktree.
    let folder_of = path.parent().unwrap();
    assert_eq!(folder_of.parent().unwrap(), worktrees_root(&home));
    let hash = folder_of.file_name().unwrap().to_str().unwrap();
    assert_eq!(hash.len(), 8);
    assert_eq!(git(&path, &["branch", "--show-current"]), "feature");
    assert!(git(&repo, &["worktree", "list", "--porcelain"]).contains("feature"));
    assert!(path.join(".git").is_file());
    let metadata = std::fs::read_to_string(
        PathBuf::from(git(&path, &["rev-parse", "--absolute-git-dir"]))
            .join("paseo")
            .join("worktree.json"),
    )
    .unwrap();
    assert!(metadata.contains("\"baseRefName\": \"main\""), "{metadata}");

    // Branching off a starting ref, with a branch name.
    let branched = h
        .create(
            json!({"source": {"kind": "worktree", "cwd": text(&repo), "refName": "feature",
                                  "branchName": "topic/login"}}),
        )
        .await;
    assert!(branched["error"].is_null(), "{branched}");
    let topic = PathBuf::from(
        branched["workspace"]["workspaceDirectory"]
            .as_str()
            .unwrap(),
    );
    assert_eq!(git(&topic, &["branch", "--show-current"]), "topic/login");
    assert_eq!(branched["workspace"]["worktreeSlug"], "topic-login");
    assert_eq!(branched["workspace"]["name"], "topic/login");
    assert_eq!(
        git(&topic, &["rev-parse", "HEAD"]),
        git(&repo, &["rev-parse", "feature"])
    );
    // A branch name that is taken: as in Paseo, a new branch named after the
    // slug, off the taken one, in a folder of its own.
    let again = h
        .create(json!({"source": {"kind": "worktree", "cwd": text(&repo), "branchName": "topic/login"}}))
        .await;
    assert!(again["error"].is_null(), "{again}");
    let second = PathBuf::from(again["workspace"]["workspaceDirectory"].as_str().unwrap());
    assert_eq!(git(&second, &["branch", "--show-current"]), "topic-login");
    assert_eq!(again["workspace"]["worktreeSlug"], "topic-login-1");
    assert_eq!(
        git(&second, &["rev-parse", "HEAD"]),
        git(&repo, &["rev-parse", "topic/login"])
    );

    // Nothing named: a random two-word branch off the default branch.
    let unnamed = h
        .create(json!({"source": {"kind": "worktree", "cwd": text(&repo)}}))
        .await;
    assert!(unnamed["error"].is_null(), "{unnamed}");
    let slug = unnamed["workspace"]["worktreeSlug"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(slug.split('-').count(), 2, "{slug}");
    let unnamed_path = PathBuf::from(unnamed["workspace"]["workspaceDirectory"].as_str().unwrap());
    assert_eq!(git(&unnamed_path, &["branch", "--show-current"]), slug);

    // A folder inside the checkout maps to the same folder in the worktree.
    let nested = h
        .create(
            json!({"source": {"kind": "worktree", "cwd": text(&repo.join("app")),
                                  "branchName": "nested"}}),
        )
        .await;
    assert!(nested["error"].is_null(), "{nested}");
    let nested_dir = PathBuf::from(nested["workspace"]["workspaceDirectory"].as_str().unwrap());
    assert_eq!(nested_dir.file_name().unwrap(), "app");
    assert!(nested_dir.join("main.txt").is_file());

    // The setup script ran in the new worktree and was reported.
    let progress = h
        .until("workspace setup", |m| {
            m["type"] == "workspace_setup_progress"
                && m["payload"]["workspaceId"] == branched["workspace"]["id"]
                && m["payload"]["status"] == "completed"
        })
        .await;
    let ran = &progress.last().unwrap()["payload"]["detail"];
    assert_eq!(
        ran["commands"][0]["log"].as_str().unwrap().trim(),
        "topic/login"
    );
    assert_eq!(ran["commands"][0]["cwd"], text(&topic));
    let status = h
        .request(json!({"type": "workspace_setup_status_request",
            "workspaceId": branched["workspace"]["id"]}))
        .await;
    let snapshot = &status["snapshot"];
    assert_eq!(snapshot["status"], "completed");
    assert_eq!(snapshot["detail"]["type"], "worktree_setup");
    assert_eq!(snapshot["detail"]["branchName"], "topic/login");
    assert_eq!(snapshot["detail"]["commands"][0]["status"], "completed");
    assert!(
        snapshot["detail"]["log"]
            .as_str()
            .unwrap()
            .starts_with("==> [1/1] Running: git branch --show-current")
    );

    // Burrow's worktrees of this repository.
    let listed = h
        .request(json!({"type": "paseo_worktree_list_request", "cwd": text(&repo)}))
        .await;
    assert!(listed["error"].is_null(), "{listed}");
    let mut branches: Vec<String> = (listed["worktrees"].as_array().unwrap().iter())
        .map(|w| w["branchName"].as_str().unwrap().to_string())
        .collect();
    branches.sort();
    let mut expected = vec![
        "feature".to_string(),
        "nested".into(),
        "topic/login".into(),
        "topic-login".into(),
        slug.clone(),
    ];
    expected.sort();
    assert_eq!(branches, expected);
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_workspace_starts_its_first_agent() {
    let home = scratch("home");
    let h = harness(&home, &burrow_daemon::ids::server_id()).await;
    let repo = repository("repo");
    let project = h.add_project(&repo).await;

    // As crusteo's new-workspace form sends it.
    let reply = h
        .create(json!({"idempotencyKey": "k-agent", "subscribe": true,
            "source": {"kind": "worktree", "cwd": text(&repo), "projectId": project["projectId"]},
            "firstAgentContext": {"prompt": "Fix the login page"},
            "agent": {"config": {"provider": "echo", "cwd": text(&repo.join("app"))},
                      "initialPrompt": "Fix the login page", "clientMessageId": "m-1",
                      "labels": {}}}))
        .await;
    assert!(reply["error"].is_null(), "{reply}");
    let workspace = &reply["workspace"];
    let agent = &reply["agent"];
    assert_eq!(agent["workspaceId"], workspace["id"]);
    // The prompt names the workspace; the agent, asked for in a folder of
    // the source, works in the workspace's directory, as in Paseo.
    assert_eq!(workspace["title"], "Fix the login page");
    let dir = PathBuf::from(workspace["workspaceDirectory"].as_str().unwrap());
    assert_eq!(agent["cwd"], text(&dir));
    assert_eq!(
        git(&dir, &["branch", "--show-current"]),
        workspace["worktreeSlug"]
    );
    let creation = &reply["creation"];
    assert_eq!(creation["phase"], "completed");
    assert_eq!(creation["revision"], 4);
    assert_eq!(creation["agentId"], agent["id"]);
    assert_eq!(creation["workspaceId"], workspace["id"]);
    // Each phase was pushed while it happened.
    let updates: Vec<Value> = (h
        .until("the completed creation", |m| {
            m["type"] == "workspace.create.update" && m["payload"]["phase"] == "completed"
        })
        .await
        .into_iter())
    .filter(|m| m["type"] == "workspace.create.update")
    .collect();
    let phases: Vec<&str> = updates
        .iter()
        .map(|u| u["payload"]["phase"].as_str().unwrap())
        .collect();
    assert_eq!(
        phases,
        [
            "accepted",
            "workspace_ready",
            "agent_ready",
            "prompt_started",
            "completed"
        ]
    );

    // An agent in a directory outside the source is refused, and the
    // workspace stays.
    let outside = scratch("outside");
    let refused = h
        .create(json!({"source": {"kind": "directory", "path": text(&repo)},
            "agent": {"config": {"provider": "echo", "cwd": text(&outside)}, "labels": {}}}))
        .await;
    assert_eq!(
        refused["error"],
        "Agent directory must be inside the workspace source"
    );
    assert_eq!(refused["creation"]["failedStage"], "agent");
    assert!(refused["workspace"]["id"].is_string());
    let background = h
        .create(json!({"source": {"kind": "directory", "path": text(&repo)},
            "agent": {"config": {"provider": "echo", "cwd": text(&repo)}, "labels": {},
                      "background": true}}))
        .await;
    assert_eq!(
        background["error"],
        "Configure background on the workspace, not its initial agent."
    );
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_made_with_git_options_get_worktrees() {
    let home = scratch("home");
    let h = harness(&home, &burrow_daemon::ids::server_id()).await;
    let repo = repository("repo");
    h.add_project(&repo).await;

    let reply = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "echo", "cwd": text(&repo)},
            "git": {"createWorktree": true, "createNewBranch": true,
                    "newBranchName": "Agent Branch", "baseBranch": "feature"}}))
        .await;
    assert!(reply["error"].is_null(), "{reply}");
    let agent = &reply["agent"];
    let cwd = PathBuf::from(agent["cwd"].as_str().unwrap());
    assert_eq!(git(&cwd, &["branch", "--show-current"]), "agent-branch");
    assert_eq!(
        git(&cwd, &["rev-parse", "HEAD"]),
        git(&repo, &["rev-parse", "feature"])
    );
    let workspaces = h
        .request(json!({"type": "fetch_workspaces_request", "includeBackground": true}))
        .await;
    let workspace = (workspaces["entries"].as_array().unwrap().iter())
        .find(|w| w["id"] == agent["workspaceId"])
        .expect("its workspace")
        .clone();
    assert_eq!(workspace["workspaceKind"], "worktree");
    assert_eq!(workspace["worktreeSlug"], "agent-branch");

    // Its setup lands on its timeline.
    let id = agent["id"].as_str().unwrap();
    let mut setup = Value::Null;
    for _ in 0..200 {
        let tail = h
            .request(
                json!({"type": "fetch_agent_timeline_request", "agentId": id,
                "direction": "tail", "limit": 40}),
            )
            .await;
        if let Some(entry) = (tail["entries"].as_array().unwrap().iter())
            .find(|e| e["item"]["name"] == "paseo_worktree_setup")
        {
            setup = entry["item"].clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(setup["status"], "completed", "{setup}");
    assert_eq!(setup["detail"]["branchName"], "agent-branch");

    // The newer target form, checking out a branch.
    git(&repo, &["branch", "release"]);
    let target = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "echo", "cwd": text(&repo)},
            "worktree": {"mode": "checkout-branch", "branch": "release"}}))
        .await;
    assert!(target["error"].is_null(), "{target}");
    let cwd = PathBuf::from(target["agent"]["cwd"].as_str().unwrap());
    assert_eq!(git(&cwd, &["branch", "--show-current"]), "release");

    // A branch made in place, without a worktree.
    let plain = repository("inplace");
    let in_place = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "echo", "cwd": text(&plain)},
            "git": {"createNewBranch": true, "newBranchName": "quick-fix"}}))
        .await;
    assert!(in_place["error"].is_null(), "{in_place}");
    assert_eq!(git(&plain, &["branch", "--show-current"]), "quick-fix");

    let both = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "echo", "cwd": text(&repo)},
            "git": {"createWorktree": true},
            "worktree": {"mode": "checkout-branch", "branch": "release"}}))
        .await;
    assert_eq!(
        both["error"],
        "create_agent_request worktree cannot be combined with git options"
    );
    let pr = h
        .request(json!({"type": "agent.create.request", "labels": {},
            "config": {"provider": "echo", "cwd": text(&repo)},
            "worktree": {"mode": "checkout-pr", "prNumber": 7}}))
        .await;
    assert!(
        pr["error"].as_str().unwrap().contains("change request"),
        "{pr}"
    );
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_carry_paseos_messages() {
    let home = scratch("home");
    let h = harness(&home, &burrow_daemon::ids::server_id()).await;
    let repo = repository("repo");
    let plain = scratch("plain");

    let not_git = h
        .create(json!({"source": {"kind": "worktree", "cwd": text(&plain)}}))
        .await;
    assert_eq!(
        not_git["error"],
        "Create worktree requires a git repository"
    );
    assert!(not_git["workspace"].is_null());
    assert_eq!(not_git["creation"]["phase"], "failed");
    assert_eq!(not_git["creation"]["failedStage"], "workspace");

    let missing = h
        .create(json!({"source": {"kind": "directory", "path": text(&plain.join("gone"))}}))
        .await;
    assert_eq!(missing["errorCode"], "directory_not_found");
    assert_eq!(missing["creation"]["outcomeUnknown"], false);

    let sourceless = h.create(json!({"source": {"kind": "worktree"}})).await;
    assert_eq!(sourceless["errorCode"], "source_required");
    assert_eq!(
        sourceless["error"],
        "cwd or projectId is required for a worktree-backed workspace"
    );

    let no_target = h
        .create(json!({"source": {"kind": "worktree", "cwd": text(&repo), "action": "checkout"}}))
        .await;
    assert_eq!(
        no_target["error"],
        "action \"checkout\" requires refName or checkoutSource"
    );

    let unknown = h
        .create(
            json!({"source": {"kind": "worktree", "cwd": text(&repo), "action": "checkout",
                                  "refName": "nope"}}),
        )
        .await;
    assert_eq!(unknown["error"], "Unknown branch: nope");

    let bad_base = h
        .create(json!({"source": {"kind": "worktree", "cwd": text(&repo), "refName": "nope"}}))
        .await;
    assert_eq!(bad_base["error"], "Base branch not found: nope");

    let project = h
        .create(json!({"source": {"kind": "worktree", "projectId": "prj_0000000000000000"}}))
        .await;
    assert_eq!(project["error"], "Project not found: prj_0000000000000000");

    let internal = h
        .create(json!({"internal": true, "source": {"kind": "directory", "path": text(&repo)}}))
        .await;
    assert_eq!(
        internal["error"],
        "Public internal workspaces were removed; use background workspace creation."
    );

    let bad_slug = h
        .create(json!({"source": {"kind": "worktree", "cwd": text(&repo), "worktreeSlug": "--"}}))
        .await;
    assert_eq!(
        bad_slug["error"],
        "Invalid worktree name: Branch name cannot be empty"
    );

    // Nothing was left behind by the refusals.
    let listed = h
        .request(json!({"type": "paseo_worktree_list_request", "cwd": text(&repo)}))
        .await;
    assert!(
        listed["worktrees"].as_array().unwrap().is_empty(),
        "{listed}"
    );
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forms_read_branches_and_folders() {
    let home = scratch("home");
    let h = harness(&home, &burrow_daemon::ids::server_id()).await;
    let repo = repository("repo");
    git(&repo, &["branch", "fix/login"]);

    let branches = h
        .request(json!({"type": "branch_suggestions_request", "cwd": text(&repo)}))
        .await;
    assert!(branches["error"].is_null(), "{branches}");
    let mut names: Vec<&str> = (branches["branches"].as_array().unwrap().iter())
        .map(|b| b.as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["feature", "fix/login", "main"]);
    let detail = &branches["branchDetails"][0];
    assert_eq!(detail["hasLocal"], true);
    assert_eq!(detail["hasRemote"], false);
    assert!(detail["committerDate"].as_i64().unwrap() > 0);
    let filtered = h
        .request(
            json!({"type": "branch_suggestions_request", "cwd": text(&repo),
            "query": "log"}),
        )
        .await;
    assert_eq!(filtered["branches"], json!(["fix/login"]));
    let not_git = h
        .request(json!({"type": "branch_suggestions_request", "cwd": text(&home)}))
        .await;
    assert!(
        not_git["error"]
            .as_str()
            .unwrap()
            .starts_with("Not a git repository")
    );

    let valid = h
        .request(
            json!({"type": "validate_branch_request", "cwd": text(&repo),
            "branchName": "feature"}),
        )
        .await;
    assert_eq!(
        (
            valid["exists"].clone(),
            valid["resolvedRef"].clone(),
            valid["isRemote"].clone()
        ),
        (json!(true), json!("feature"), json!(false))
    );
    let absent = h
        .request(
            json!({"type": "validate_branch_request", "cwd": text(&repo),
            "branchName": "nope"}),
        )
        .await;
    assert_eq!(absent["exists"], false);
    let unsafe_ref = h
        .request(
            json!({"type": "validate_branch_request", "cwd": text(&repo),
            "branchName": "a..b"}),
        )
        .await;
    assert_eq!(unsafe_ref["error"], "Invalid branch: a..b");

    // The folder search inside a workspace: relative, with ignored folders
    // left out.
    std::fs::create_dir_all(repo.join("app").join("screens")).unwrap();
    std::fs::create_dir_all(repo.join("generated").join("screens")).unwrap();
    std::fs::write(repo.join("generated").join("screens").join("a.txt"), "x").unwrap();
    std::fs::write(repo.join(".gitignore"), "generated/\n").unwrap();
    let found = h
        .request(
            json!({"type": "directory_suggestions_request", "cwd": text(&repo),
            "query": "screens"}),
        )
        .await;
    assert!(found["error"].is_null(), "{found}");
    assert_eq!(found["directories"], json!(["app/screens"]));
    assert_eq!(
        found["entries"],
        json!([{"path": "app/screens", "kind": "directory"}])
    );
    let top = h
        .request(
            json!({"type": "directory_suggestions_request", "cwd": text(&repo),
            "query": "", "includeFiles": true}),
        )
        .await;
    let paths: Vec<&str> = (top["entries"].as_array().unwrap().iter())
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"app") && paths.contains(&"README.md"),
        "{paths:?}"
    );
    assert!(!paths.contains(&".gitignore"));
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspaces_survive_a_restart_and_archive_their_worktrees() {
    let home = scratch("home");
    let server_id = burrow_daemon::ids::server_id();
    let h = harness(&home, &server_id).await;
    let repo = repository("repo");
    let project = h.add_project(&repo).await;
    let made = h
        .create(json!({"source": {"kind": "worktree", "cwd": text(&repo), "branchName": "keep"}}))
        .await;
    assert!(made["error"].is_null(), "{made}");
    let gone = h
        .create(json!({"source": {"kind": "worktree", "cwd": text(&repo), "branchName": "drop"}}))
        .await;
    let gone_id = gone["workspace"]["id"].as_str().unwrap().to_string();
    let gone_dir = PathBuf::from(gone["workspace"]["workspaceDirectory"].as_str().unwrap());
    let agent = h
        .request(
            json!({"type": "agent.create.request", "labels": {}, "workspaceId": gone_id,
            "config": {"provider": "echo", "cwd": text(&gone_dir)}}),
        )
        .await;
    assert!(agent["error"].is_null(), "{agent}");
    h.stop().await;

    let h = harness(&home, &server_id).await;
    let listed = h
        .request(json!({"type": "fetch_workspaces_request", "includeBackground": true}))
        .await;
    let kept = (listed["entries"].as_array().unwrap().iter())
        .find(|w| w["id"] == made["workspace"]["id"])
        .expect("the worktree workspace is back")
        .clone();
    assert_eq!(kept["workspaceKind"], "worktree");
    assert_eq!(kept["worktreeSlug"], "keep");
    assert_eq!(kept["gitRuntime"]["isPaseoOwnedWorktree"], true);
    assert_eq!(kept["projectId"], project["projectId"]);
    let saved = std::fs::read_to_string(home.join("workspaces.json")).unwrap();
    assert!(
        saved.contains("\"baseBranch\": \"refs/heads/main\""),
        "{saved}"
    );
    let projects = h.request(json!({"type": "project.list.request"})).await;
    assert_eq!(projects["projects"][0]["projectKey"], project["projectKey"]);

    // Archiving takes it out of the directory, archives its agent and
    // removes its worktree; the branch stays.
    let archived = h
        .request(json!({"type": "archive_workspace_request", "workspaceId": gone_id}))
        .await;
    assert!(archived["error"].is_null(), "{archived}");
    assert!(archived["archivedAt"].is_string());
    h.until("the removal", |m| {
        m["type"] == "workspace_update"
            && m["payload"]["kind"] == "remove"
            && m["payload"]["id"] == gone_id.as_str()
    })
    .await;
    assert!(!gone_dir.exists());
    assert!(git(&repo, &["branch", "--list", "drop"]).contains("drop"));
    let fetched = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent["agent"]["id"]}))
        .await;
    assert!(fetched["agent"]["archivedAt"].is_string(), "{fetched}");
    let again = h
        .request(json!({"type": "archive_workspace_request", "workspaceId": gone_id}))
        .await;
    assert_eq!(again["archivedAt"], archived["archivedAt"]);
    let unknown = h
        .request(
            json!({"type": "archive_workspace_request", "workspaceId": "wks_0000000000000000"}),
        )
        .await;
    assert_eq!(
        unknown["error"],
        "Workspace not found: wks_0000000000000000"
    );
    h.stop().await;

    // Archived stays archived.
    let h = harness(&home, &server_id).await;
    let listed = h
        .request(json!({"type": "fetch_workspaces_request", "includeBackground": true}))
        .await;
    assert!(!(listed["entries"].as_array().unwrap().iter()).any(|w| w["id"] == gone_id.as_str()));
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

impl Harness {
    async fn remove_project(&self, project_id: &str) -> Value {
        self.request(json!({"type": "project.remove.request", "projectId": project_id}))
            .await
    }

    async fn project_ids(&self) -> Vec<String> {
        let listed = self.request(json!({"type": "project.list.request"})).await;
        (listed["projects"].as_array().unwrap().iter())
            .map(|p| p["projectId"].as_str().unwrap().to_string())
            .collect()
    }
}

fn saved_projects(home: &Path) -> String {
    std::fs::read_to_string(home.join("projects.json")).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_and_unknown_projects_are_removed() {
    let home = scratch("home");
    let h = harness(&home, &burrow_daemon::ids::server_id()).await;
    assert_eq!(h.daemon.server_info().features["projectRemove"], true);
    let plain = scratch("plain");
    let repo = repository("repo");
    let project = h.add_project(&plain).await;
    let id = project["projectId"].as_str().unwrap();
    let kept = h.add_project(&repo).await;

    let reply = h.remove_project(id).await;
    assert_eq!(
        reply,
        json!({"requestId": reply["requestId"], "projectId": id, "accepted": true,
               "removedWorkspaceIds": [], "error": null})
    );
    // With no workspace of its own, the removal names the project itself,
    // then event subscribers get `project.update`.
    let pushed = h
        .until("project.update remove", |m| {
            m["type"] == "project.update" && m["payload"]["kind"] == "remove"
        })
        .await;
    let removal = (pushed.iter())
        .find(|m| m["type"] == "workspace_update")
        .expect("a workspace_update removal");
    assert_eq!(removal["payload"]["kind"], "remove");
    assert_eq!(removal["payload"]["id"], id);
    assert_eq!(removal["payload"]["removedProjectId"], id);
    assert!(removal["payload"].get("seq").is_none());
    assert_eq!(pushed.last().unwrap()["payload"]["projectId"], id);
    assert!(pushed.last().unwrap()["payload"]["seq"].is_u64());

    assert_eq!(h.project_ids().await, [kept["projectId"].as_str().unwrap()]);
    let workspaces = h.request(json!({"type": "fetch_workspaces_request"})).await;
    assert_eq!(workspaces["emptyProjects"].as_array().unwrap().len(), 1);
    h.daemon.flush();
    assert!(!saved_projects(&home).contains(id));
    // The folder is untouched, and adding it again makes a new project.
    assert!(plain.is_dir());
    let again = h.add_project(&plain).await;
    assert_ne!(again["projectId"], project["projectId"]);

    // An unknown id is accepted, with nothing removed, as in Paseo.
    let unknown = h.remove_project("prj_0000000000000000").await;
    assert_eq!(unknown["accepted"], true);
    assert_eq!(unknown["removedWorkspaceIds"], json!([]));
    assert!(unknown["error"].is_null());
    h.until("the unknown id's removal", |m| {
        m["type"] == "workspace_update"
            && m["payload"]["removedProjectId"] == "prj_0000000000000000"
    })
    .await;
    assert_eq!(h.project_ids().await.len(), 2);
    let malformed = h
        .client
        .request(json!({"type": "project.remove.request"}))
        .await;
    assert!(malformed.is_err(), "{malformed:?}");
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_a_project_archives_its_workspaces_and_keeps_the_disk() {
    let home = scratch("home");
    let server_id = burrow_daemon::ids::server_id();
    let h = harness(&home, &server_id).await;
    let repo = repository("repo");
    let project = h.add_project(&repo).await;
    let project_id = project["projectId"].as_str().unwrap().to_string();

    // One worktree workspace archived before (its worktree removed then),
    // one still active with an agent, and the checkout's own workspace.
    let old = h
        .create(
            json!({"source": {"kind": "worktree", "projectId": project_id,
                                  "branchName": "old"}}),
        )
        .await;
    let old_id = old["workspace"]["id"].as_str().unwrap().to_string();
    let archived = h
        .request(json!({"type": "archive_workspace_request", "workspaceId": old_id}))
        .await;
    assert!(archived["error"].is_null(), "{archived}");
    let live = h
        .create(
            json!({"source": {"kind": "worktree", "projectId": project_id,
                                  "branchName": "live"}}),
        )
        .await;
    let live_id = live["workspace"]["id"].as_str().unwrap().to_string();
    let live_dir = PathBuf::from(live["workspace"]["workspaceDirectory"].as_str().unwrap());
    let agent = h
        .request(
            json!({"type": "agent.create.request", "labels": {}, "workspaceId": live_id,
            "config": {"provider": "echo", "cwd": text(&live_dir)}}),
        )
        .await;
    assert!(agent["error"].is_null(), "{agent}");
    let agent_id = agent["agent"]["id"].as_str().unwrap().to_string();
    let local = h
        .create(json!({"source": {"kind": "directory", "path": text(&repo),
                                  "projectId": project_id}}))
        .await;
    let local_id = local["workspace"]["id"].as_str().unwrap().to_string();
    let other = h.add_project(&scratch("other")).await;

    let reply = h.remove_project(&project_id).await;
    assert_eq!(reply["accepted"], true, "{reply}");
    let mut removed: Vec<&str> = (reply["removedWorkspaceIds"].as_array().unwrap().iter())
        .map(|id| id.as_str().unwrap())
        .collect();
    removed.sort();
    let mut expected = vec![live_id.as_str(), local_id.as_str()];
    expected.sort();
    assert_eq!(removed, expected);

    // Each active workspace leaves with the project named, and no
    // `emptyProject`; then the project goes.
    let pushed = h
        .until("project.update remove", |m| {
            m["type"] == "project.update" && m["payload"]["kind"] == "remove"
        })
        .await;
    let removals: Vec<&Value> = (pushed.iter())
        .filter(|m| m["type"] == "workspace_update" && m["payload"]["kind"] == "remove")
        .filter(|m| m["payload"]["id"] != old_id.as_str())
        .collect();
    assert_eq!(removals.len(), 2, "{removals:#?}");
    for removal in &removals {
        assert_eq!(removal["payload"]["removedProjectId"], project_id.as_str());
        assert!(removal["payload"].get("emptyProject").is_none());
    }
    let fetched = h
        .request(json!({"type": "fetch_agent_request", "agentId": agent_id}))
        .await;
    assert!(fetched["agent"]["archivedAt"].is_string(), "{fetched}");

    // Nothing on disk changed: the live worktree and both branches stay.
    assert!(live_dir.is_dir());
    assert_eq!(git(&live_dir, &["branch", "--show-current"]), "live");
    assert!(git(&repo, &["branch", "--list", "old"]).contains("old"));
    assert!(git(&repo, &["worktree", "list"]).contains("live"));
    assert!(repo.join("README.md").is_file());

    let listed = h
        .request(json!({"type": "fetch_workspaces_request", "includeBackground": true}))
        .await;
    assert!(listed["entries"].as_array().unwrap().is_empty(), "{listed}");
    assert_eq!(
        h.project_ids().await,
        [other["projectId"].as_str().unwrap()]
    );
    h.stop().await;

    // After a restart the project stays gone; its workspaces stay archived,
    // still naming it, as in Paseo.
    let h = harness(&home, &server_id).await;
    assert_eq!(
        h.project_ids().await,
        [other["projectId"].as_str().unwrap()]
    );
    let listed = h
        .request(json!({"type": "fetch_workspaces_request", "includeBackground": true}))
        .await;
    assert!(listed["entries"].as_array().unwrap().is_empty(), "{listed}");
    assert!(!saved_projects(&home).contains(&project_id));
    let saved = std::fs::read_to_string(home.join("workspaces.json")).unwrap();
    let records: Vec<Value> = serde_json::from_str(&saved).unwrap();
    for id in [&old_id, &live_id, &local_id] {
        let record = (records.iter())
            .find(|r| r["workspaceId"] == id.as_str())
            .expect("kept");
        assert!(record["archivedAt"].is_string(), "{record}");
        assert_eq!(record["projectId"], project_id.as_str());
    }

    // A project left with only archived workspaces: the removal names the
    // first of them.
    let again = h.add_project(&repo).await;
    let again_id = again["projectId"].as_str().unwrap().to_string();
    let made = h
        .create(json!({"source": {"kind": "directory", "path": text(&repo),
                                  "projectId": again_id}}))
        .await;
    let made_id = made["workspace"]["id"].as_str().unwrap().to_string();
    h.request(json!({"type": "archive_workspace_request", "workspaceId": made_id}))
        .await;
    let reply = h.remove_project(&again_id).await;
    assert_eq!(reply["removedWorkspaceIds"], json!([]));
    let pushed = h
        .until("the archived workspace's removal", |m| {
            m["type"] == "workspace_update" && m["payload"]["removedProjectId"] == again_id.as_str()
        })
        .await;
    assert_eq!(pushed.last().unwrap()["payload"]["id"], made_id.as_str());
    assert_eq!(
        h.project_ids().await,
        [other["projectId"].as_str().unwrap()]
    );
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}
