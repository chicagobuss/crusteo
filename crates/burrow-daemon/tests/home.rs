//! The state directory's move from its carapace name, `~/.carapace`, to
//! `~/.burrow`, on a made-up user home with a real git worktree in it: the
//! identity, the records' paths, the timelines' image paths and the
//! worktree's link to its repository all follow.

use std::path::{Path, PathBuf};
use std::process::Command;

use burrow_daemon::git::{GitFacts, canonical};
use burrow_daemon::store::{ProjectRecord, WorkspaceRecord};
use burrow_daemon::{Config, home, persist};
use burrow_protocol::directory::{ProjectKind, WorkspaceKind};

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "burrow-home-move-{}",
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
    let empty = std::env::temp_dir().join("burrow-home-move-gitconfig");
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

#[test]
fn the_carapace_home_moves_with_its_worktrees_and_identity() {
    let user = scratch();
    let repo = user.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "first"]);

    // carapace's home, as it left it: an identity, a project, a workspace
    // in a worktree it made, and a timeline with a prompt image.
    let old = user.join(".carapace");
    let old_tree = old.join("worktrees").join("p1").join("feature");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "feature", &text(&old_tree)],
    );
    std::fs::write(old.join("server-id"), "srv_moved\n").unwrap();
    let project = ProjectRecord {
        project_id: "prj_1".into(),
        project_key: "example/repo".into(),
        root_path: text(&repo),
        kind: ProjectKind::Git,
        display_name: "repo".into(),
        custom_name: None,
    };
    let workspace = WorkspaceRecord {
        workspace_id: "wks_1".into(),
        project_id: "prj_1".into(),
        cwd: text(&old_tree),
        kind: WorkspaceKind::Worktree,
        display_name: "feature".into(),
        title: None,
        pinned_at: None,
        labels: Vec::new(),
        git: Some(GitFacts {
            worktree_root: text(&old_tree),
            branch: Some("feature".into()),
            remote_url: None,
            main_repo_root: Some(text(&repo)),
            paseo_owned: true,
        }),
        background: false,
        base_branch: Some("main".into()),
        archived_at: None,
        created_at: None,
        setup_status: None,
    };
    std::fs::write(
        old.join("projects.json"),
        serde_json::to_vec_pretty(&vec![project]).unwrap(),
    )
    .unwrap();
    std::fs::write(
        old.join("workspaces.json"),
        serde_json::to_vec_pretty(&vec![workspace]).unwrap(),
    )
    .unwrap();
    let image = old.join("attachments").join("ab.png");
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    std::fs::write(&image, b"png").unwrap();
    std::fs::create_dir_all(old.join("timelines")).unwrap();
    let row = serde_json::json!({"images": [{"path": text(&image), "mimeType": "image/png"}],
                                 "elsewhere": text(&user.join(".carapace-backup"))});
    std::fs::write(
        old.join("timelines").join("agt_1.jsonl"),
        format!("{{\"format\":1,\"epoch\":\"e1\"}}\n{row}\n"),
    )
    .unwrap();

    let home = home::prepare_default(&user);

    let new = user.join(".burrow");
    assert_eq!(home, new);
    assert!(!old.exists(), "moved, not copied");
    assert_eq!(home::server_id(&new).unwrap(), "srv_moved");
    let new_tree = new.join("worktrees").join("p1").join("feature");
    let loaded = persist::read(&new).unwrap();
    assert_eq!(
        loaded.projects[0].root_path,
        text(&repo),
        "outside the home"
    );
    let moved = &loaded.workspaces[0];
    assert_eq!(moved.cwd, text(&new_tree));
    let facts = moved.git.as_ref().unwrap();
    assert_eq!(facts.worktree_root, text(&new_tree));
    assert_eq!(facts.main_repo_root.as_deref(), Some(text(&repo).as_str()));
    let timeline = std::fs::read_to_string(new.join("timelines").join("agt_1.jsonl")).unwrap();
    let new_image = serde_json::to_string(&text(&new.join("attachments").join("ab.png"))).unwrap();
    assert!(timeline.contains(&new_image), "{timeline}");
    let backup = serde_json::to_string(&text(&user.join(".carapace-backup"))).unwrap();
    assert!(timeline.contains(&backup), "another folder is left alone");
    assert!(new.join("attachments").join("ab.png").is_file());

    // The worktree still works, and its repository knows where it went.
    assert_eq!(git(&new_tree, &["branch", "--show-current"]), "feature");
    let listed = git(&repo, &["worktree", "list", "--porcelain"]).replace('\\', "/");
    assert!(!listed.contains("prunable"), "{listed}");
    assert!(listed.contains(".burrow/worktrees/p1/feature"), "{listed}");
    assert!(!listed.contains(".carapace"), "{listed}");

    // New worktrees go beside it.
    let config = Config {
        home: Some(new.clone()),
        ..Config::ephemeral()
    };
    assert_eq!(config.worktrees_root(), new.join("worktrees"));

    // A second start finds nothing to move.
    assert!(home::move_legacy(&user).unwrap().is_none());
    assert_eq!(home::prepare_default(&user), new);
    let _ = std::fs::remove_dir_all(&user);
}
