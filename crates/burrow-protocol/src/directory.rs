//! Agents, workspaces and projects as the directory reads and pushes carry
//! them, and the requests that read them.
//!
//! Fields follow `packages/protocol/src/messages.ts`. A field that is
//! `nullable()` there is an `Option` that serializes as `null`; one that is
//! `optional()` is skipped when absent. Payloads Burrow does not model yet
//! (`pendingPermissions`, `features`, `persistence`, ...) are raw JSON.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `AgentSnapshotPayloadSchema`. Always sent whole; there is no patch form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSnapshot {
    pub id: String,
    pub provider: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<Value>>,
    /// Sent as `null` when unset, as Paseo does.
    #[serde(default)]
    pub thinking_option_id: Option<String>,
    /// The thinking level the provider actually uses.
    #[serde(default)]
    pub effective_thinking_option_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_user_message_at: Option<String>,
    pub status: AgentStatus,
    #[serde(default)]
    pub active_turn: Option<ActiveTurn>,
    pub capabilities: AgentCapabilities,
    pub current_mode_id: Option<String>,
    pub available_modes: Vec<AgentMode>,
    pub pending_permissions: Vec<Value>,
    pub persistence: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_info: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub requires_attention: bool,
    #[serde(default)]
    pub attention_reason: Option<AttentionReason>,
    #[serde(default)]
    pub attention_timestamp: Option<String>,
    #[serde(default)]
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Initializing,
    Idle,
    Running,
    Error,
    Closed,
}

impl AgentStatus {
    /// `getAgentStatusPriority`: lower sorts first.
    pub fn priority(self, pending_permissions: bool, requires_attention: bool) -> u8 {
        if pending_permissions {
            0
        } else if self == AgentStatus::Error {
            1
        } else if self == AgentStatus::Running {
            2
        } else if self == AgentStatus::Initializing || requires_attention {
            3
        } else {
            4
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttentionReason {
    Finished,
    Error,
    Permission,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveTurn {
    pub turn_id: String,
    pub started_at: Option<String>,
}

/// `AgentCapabilityFlagsSchema`: the six required flags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub supports_streaming: bool,
    pub supports_session_persistence: bool,
    pub supports_dynamic_modes: bool,
    pub supports_mcp_servers: bool,
    pub supports_reasoning_stream: bool,
    pub supports_tool_invocations: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMode {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `ProjectPlacementPayloadSchema`: where an agent sits in the project tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPlacement {
    pub project_key: String,
    pub project_name: String,
    pub workspace_name: Option<String>,
    pub checkout: Checkout,
}

/// `ProjectCheckoutLitePayloadSchema`, a union of three shapes that one
/// struct covers: non-git (`isGit:false`, branch/remote/roots null), a git
/// checkout, and a Paseo-owned worktree (`mainRepoRoot` set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkout {
    pub cwd: String,
    pub is_git: bool,
    pub current_branch: Option<String>,
    pub remote_url: Option<String>,
    pub worktree_root: Option<String>,
    pub is_paseo_owned_worktree: bool,
    pub main_repo_root: Option<String>,
}

impl Checkout {
    pub fn not_git(cwd: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            is_git: false,
            current_branch: None,
            remote_url: None,
            worktree_root: None,
            is_paseo_owned_worktree: false,
            main_repo_root: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    Git,
    NonGit,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    Directory,
    LocalCheckout,
    Checkout,
    Worktree,
}

/// The sidebar status bucket (`deriveAgentStateBucket`, aggregated per workspace).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateBucket {
    NeedsInput,
    Failed,
    Running,
    Attention,
    Done,
}

/// `WorkspaceProjectDescriptorPayloadSchema`: a project row, used by
/// `project.list.response`, `project.update` and `emptyProjects`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDescriptor {
    pub project_id: String,
    pub project_key: String,
    pub project_display_name: String,
    pub project_custom_name: Option<String>,
    pub project_custom_icon_revision: Option<String>,
    pub project_root_path: String,
    pub project_kind: ProjectKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_seq: Option<u64>,
}

/// `WorkspaceDescriptorPayloadSchema`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDescriptor {
    pub id: String,
    pub project_id: String,
    pub project_display_name: String,
    pub project_custom_name: Option<String>,
    pub project_custom_icon_revision: Option<String>,
    pub project_root_path: String,
    pub workspace_directory: String,
    /// The folder name of a worktree the daemon made; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_slug: Option<String>,
    pub project_kind: ProjectKind,
    pub workspace_kind: WorkspaceKind,
    pub name: String,
    pub title: Option<String>,
    pub pinned_at: Option<String>,
    /// Workspace label names. Paseo leaves the field out when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// A background workspace (Paseo 0.12): directory reads and pushes
    /// leave it and its agents out unless asked with `includeBackground`.
    #[serde(default)]
    pub background: bool,
    pub archiving_at: Option<String>,
    pub status: StateBucket,
    pub status_entered_at: Option<String>,
    pub activity_at: Option<String>,
    pub diff_stat: Option<Value>,
    pub scripts: Vec<Value>,
    pub git_runtime: Option<Value>,
    pub github_runtime: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_seq: Option<u64>,
}

/// `{agent, project, syncSeq?}` in `fetch_agents_response.entries`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    pub agent: AgentSnapshot,
    pub project: ProjectPlacement,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_seq: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
    pub has_more: bool,
}

/// `DirectorySyncMetadataSchema`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncMeta {
    pub generation: String,
    pub head_seq: u64,
    pub mode: SyncMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<SyncReason>,
    pub removals: Vec<SyncRemoval>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    Snapshot,
    Changes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncReason {
    NoCursor,
    GenerationChanged,
    CursorExpired,
    ChangesTooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncRemoval {
    pub id: String,
    pub seq: u64,
}

/// `DirectorySyncRequestSchema`: the client's cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCursor {
    #[serde(default)]
    pub generation: Option<String>,
    #[serde(default)]
    pub after_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PageRequest {
    pub limit: u32,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SortKey {
    pub key: String,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDirection {
    Asc,
    Desc,
}

/// `subscribe: {subscriptionId?}`. Modern clients must send `{}`: ids are
/// assigned by the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeRequest {
    #[serde(default)]
    pub subscription_id: Option<String>,
}

/// `AgentDirectoryFilterSchema`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentFilter {
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub project_keys: Option<Vec<String>>,
    #[serde(default)]
    pub statuses: Option<Vec<AgentStatus>>,
    #[serde(default)]
    pub include_archived: Option<bool>,
    #[serde(default)]
    pub requires_attention: Option<bool>,
    #[serde(default)]
    pub include_background: Option<bool>,
}

/// `FetchAgentsRequestMessageSchema`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchAgentsRequest {
    pub request_id: String,
    #[serde(default)]
    pub scope: Option<AgentScope>,
    /// Also the agents of background workspaces. Paseo 0.12 takes it here
    /// or in `filter`; a sequenced read can only have it here.
    #[serde(default)]
    pub include_background: Option<bool>,
    #[serde(default)]
    pub filter: Option<AgentFilter>,
    #[serde(default)]
    pub sort: Option<Vec<SortKey>>,
    #[serde(default)]
    pub page: Option<PageRequest>,
    #[serde(default)]
    pub subscribe: Option<SubscribeRequest>,
    #[serde(default)]
    pub sync: Option<SyncCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentScope {
    Active,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFilter {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub include_background: Option<bool>,
}

/// `FetchWorkspacesRequestMessageSchema`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchWorkspacesRequest {
    pub request_id: String,
    /// Also background workspaces, as for [`FetchAgentsRequest`].
    #[serde(default)]
    pub include_background: Option<bool>,
    #[serde(default)]
    pub filter: Option<WorkspaceFilter>,
    #[serde(default)]
    pub sort: Option<Vec<SortKey>>,
    #[serde(default)]
    pub page: Option<PageRequest>,
    #[serde(default)]
    pub subscribe: Option<SubscribeRequest>,
    #[serde(default)]
    pub sync: Option<SyncCursor>,
}

/// `ProjectListRequestMessageSchema`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectListRequest {
    pub request_id: String,
    #[serde(default)]
    pub sync: Option<SyncCursor>,
}

/// `FetchAgentRequestMessageSchema`: `agentId` is a full id, a unique
/// prefix, or an exact title.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchAgentRequest {
    pub request_id: String,
    pub agent_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_app_bootstrap_request() {
        let request: FetchAgentsRequest = serde_json::from_value(json!({
            "type": "fetch_agents_request", "requestId": "r1", "scope": "active",
            "sort": [{"key": "updated_at", "direction": "desc"}], "page": {"limit": 200},
            "subscribe": {}, "sync": {"generation": "g", "afterSeq": 42},
        }))
        .unwrap();
        assert_eq!(request.scope, Some(AgentScope::Active));
        assert_eq!(request.sync.unwrap().after_seq, Some(42));
        assert!(request.subscribe.unwrap().subscription_id.is_none());
        assert_eq!(request.include_background, None);
    }

    #[test]
    fn include_background_parses_at_the_top_and_in_the_filter() {
        let agents: FetchAgentsRequest = serde_json::from_value(json!({
            "type": "fetch_agents_request", "requestId": "r1", "includeBackground": true,
            "filter": {"includeBackground": false},
        }))
        .unwrap();
        assert_eq!(agents.include_background, Some(true));
        assert_eq!(agents.filter.unwrap().include_background, Some(false));
        let workspaces: FetchWorkspacesRequest = serde_json::from_value(json!({
            "type": "fetch_workspaces_request", "requestId": "r2",
            "filter": {"includeBackground": true},
        }))
        .unwrap();
        assert_eq!(workspaces.include_background, None);
        assert_eq!(workspaces.filter.unwrap().include_background, Some(true));
    }

    #[test]
    fn checkout_serializes_nulls() {
        let value = serde_json::to_value(Checkout::not_git("/tmp/x")).unwrap();
        assert_eq!(value["isGit"], false);
        assert!(value["worktreeRoot"].is_null());
        assert!(value.as_object().unwrap().contains_key("mainRepoRoot"));
    }
}
