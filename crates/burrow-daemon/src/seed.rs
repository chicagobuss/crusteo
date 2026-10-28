//! A demo directory: one project, one workspace, one idle agent with a short
//! conversation. Nothing on disk is read or written; the paths are labels.

use std::collections::BTreeMap;

use burrow_protocol::directory::{
    AgentCapabilities, AgentMode, AgentSnapshot, AgentStatus, ProjectKind, WorkspaceKind,
};
use serde_json::json;

use crate::clock::now_iso;
use crate::daemon::Daemon;
use crate::ids;
use crate::store::{ProjectRecord, WorkspaceRecord};

/// The provider id of placeholder agents. It runs no model.
pub const DEMO_PROVIDER: &str = "echo";
pub const DEMO_ROOT: &str = "/tmp/burrow-demo";

/// A fresh idle agent snapshot for `cwd`.
pub fn agent(id: &str, cwd: &str, workspace_id: Option<&str>, title: &str) -> AgentSnapshot {
    let now = now_iso();
    AgentSnapshot {
        id: id.into(),
        provider: DEMO_PROVIDER.into(),
        cwd: cwd.into(),
        workspace_id: workspace_id.map(str::to_string),
        model: None,
        features: Some(Vec::new()),
        thinking_option_id: None,
        effective_thinking_option_id: None,
        created_at: now.clone(),
        updated_at: now,
        last_user_message_at: None,
        status: AgentStatus::Idle,
        active_turn: None,
        capabilities: AgentCapabilities {
            supports_streaming: true,
            ..AgentCapabilities::default()
        },
        current_mode_id: Some("default".into()),
        available_modes: vec![AgentMode {
            id: "default".into(),
            label: "Default".into(),
            description: None,
        }],
        pending_permissions: Vec::new(),
        persistence: None,
        runtime_info: None,
        last_usage: None,
        last_error: None,
        title: Some(title.into()),
        labels: BTreeMap::new(),
        requires_attention: false,
        attention_reason: None,
        attention_timestamp: None,
        archived_at: None,
    }
}

/// Seeds the demo directory and returns the agent id.
pub fn demo(daemon: &Daemon) -> String {
    let project_id = ids::project_id();
    let workspace_id = ids::workspace_id();
    let agent_id = ids::agent_id();
    daemon.apply(|store| {
        let mut events = store.upsert_project(ProjectRecord {
            project_id: project_id.clone(),
            project_key: project_id.clone(),
            root_path: DEMO_ROOT.into(),
            kind: ProjectKind::NonGit,
            display_name: "burrow-demo".into(),
            custom_name: None,
        });
        events.extend(store.upsert_workspace(WorkspaceRecord {
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            cwd: DEMO_ROOT.into(),
            kind: WorkspaceKind::Directory,
            display_name: "burrow-demo".into(),
            title: None,
            pinned_at: None,
            labels: Vec::new(),
            git: None,
            background: false,
            base_branch: None,
            archived_at: None,
            created_at: None,
            setup_status: None,
        }));
        let mut snapshot = agent(&agent_id, DEMO_ROOT, Some(&workspace_id), "Demo agent");
        snapshot.last_user_message_at = Some(snapshot.created_at.clone());
        events.extend(store.upsert_agent(snapshot));
        let turn = Some("foreground-turn-1".to_string());
        let message_id = ids::agent_id();
        events.extend(store.append_timeline(
            &agent_id,
            json!({"type": "user_message", "text": "Hello",
                   "messageId": message_id, "clientMessageId": message_id}),
            turn.clone(),
            now_iso(),
        ));
        events.extend(store.append_timeline(
            &agent_id,
            json!({"type": "assistant_message",
                   "text": "Hello. I am a placeholder agent served by burrow; I run no model."}),
            turn,
            now_iso(),
        ));
        ((), events)
    });
    agent_id
}
