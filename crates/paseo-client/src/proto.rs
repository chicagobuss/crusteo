//! Typed views of the daemon messages Crusteo uses.
//!
//! Every type tolerates fields it does not know and missing optional fields,
//! because the fleet runs several daemon versions. Anything Crusteo does not
//! model stays reachable as raw JSON.

use std::mem::size_of;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Capabilities advertised in `hello`. The same set the Paseo app sends, minus
/// browser hosting. `owned_subscriptions` matters most: without it the daemon
/// broadcasts directory traffic to every client.
pub fn capabilities() -> Value {
    json!({
        "hello_rejection": true,
        "owned_subscriptions": true,
        "all_providers": true,
        "selective_agent_timeline": true,
        "reasoning_merge_enum": true,
        "custom_mode_icons": true,
        "terminal_reflowable_snapshot": true,
        "provider_subagents": true,
        "projected_subagent_timeline": true,
        "project_updates": true,
        "compact_provider_snapshots": true,
        "provider_snapshot_references": true,
        "timeline_replacement_invalidation": true,
        "timeline_notifications": true,
        "plugin_timeline_items": true,
        "workspace_setup_blocked": true,
        "explicit_event_subscriptions": true,
    })
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerInfo {
    pub server_id: String,
    pub hostname: String,
    pub version: String,
    pub features: Map<String, Value>,
    pub raw: Value,
}

impl ServerInfo {
    /// Estimate the copies owned by this projection as well as its raw JSON.
    pub(crate) fn allocated_bytes(&self) -> usize {
        self.features.iter().fold(
            json_bytes(&self.raw)
                .saturating_add(self.server_id.capacity())
                .saturating_add(self.hostname.capacity())
                .saturating_add(self.version.capacity()),
            |total, (key, value)| {
                total
                    .saturating_add(128)
                    .saturating_add(key.capacity())
                    .saturating_add(json_bytes(value))
            },
        )
    }

    /// Reads a `status` message whose payload is `server_info`.
    pub fn from_status(message: &Value) -> Option<Self> {
        if message["type"] != "status" {
            return None;
        }
        let payload = &message["payload"];
        if payload["status"] != "server_info" {
            return None;
        }
        Some(Self {
            server_id: payload["serverId"].as_str().unwrap_or_default().to_string(),
            hostname: payload["hostname"].as_str().unwrap_or_default().to_string(),
            version: payload["version"].as_str().unwrap_or_default().to_string(),
            features: payload["features"].as_object().cloned().unwrap_or_default(),
            raw: payload.clone(),
        })
    }

    pub fn has(&self, feature: &str) -> bool {
        self.features.get(feature).and_then(Value::as_bool) == Some(true)
    }
}

/// The feature a daemon advertises when it hides background workspaces, and
/// their agents, from clients that do not send `includeBackground`.
pub const BACKGROUND_WORKSPACES: &str = "backgroundWorkspaces";

/// Directory sync cursor: `{generation, afterSeq}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    pub generation: String,
    pub after_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncMeta {
    #[serde(default)]
    pub generation: String,
    #[serde(default)]
    pub head_seq: u64,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub removals: Vec<Removal>,
}

impl SyncMeta {
    pub fn is_snapshot(&self) -> bool {
        self.mode != "changes"
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Removal {
    #[serde(alias = "name")]
    pub id: String,
    #[serde(default)]
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    #[serde(default)]
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiffStat {
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GitRuntime {
    #[serde(default)]
    pub current_branch: Option<String>,
    #[serde(default)]
    pub remote_url: Option<String>,
    #[serde(default)]
    pub is_dirty: Option<bool>,
    #[serde(default)]
    pub ahead_of_origin: Option<i64>,
    #[serde(default)]
    pub behind_of_origin: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    #[serde(default)]
    pub number: Option<u64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub is_merged: bool,
    #[serde(default)]
    pub checks_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GithubRuntime {
    #[serde(default)]
    pub pull_request: Option<PullRequest>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Script {
    #[serde(default)]
    pub script_name: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub lifecycle: String,
    #[serde(default)]
    pub health: Option<String>,
}

/// `WorkspaceDescriptorPayload`.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub project_display_name: String,
    #[serde(default)]
    pub project_custom_name: Option<String>,
    #[serde(default)]
    pub project_root_path: String,
    #[serde(default)]
    pub workspace_directory: Option<String>,
    #[serde(default)]
    pub worktree_slug: Option<String>,
    #[serde(default)]
    pub project_kind: String,
    #[serde(default)]
    pub workspace_kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub pinned_at: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub archiving_at: Option<String>,
    /// A background workspace (Paseo 0.12): the daemon lists it, and every
    /// agent in it, only to clients that ask with `includeBackground`.
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub status_entered_at: Option<String>,
    #[serde(default)]
    pub diff_stat: Option<DiffStat>,
    #[serde(default)]
    pub scripts: Vec<Script>,
    #[serde(default)]
    pub git_runtime: Option<GitRuntime>,
    #[serde(default)]
    pub github_runtime: Option<GithubRuntime>,
    #[serde(default)]
    pub project: Option<Value>,
}

impl Workspace {
    pub fn directory(&self) -> &str {
        self.workspace_directory
            .as_deref()
            .filter(|dir| !dir.is_empty())
            .unwrap_or(&self.project_root_path)
    }

    pub fn branch(&self) -> Option<&str> {
        self.git_runtime
            .as_ref()
            .and_then(|git| git.current_branch.as_deref())
            .filter(|branch| !branch.is_empty() && *branch != "HEAD")
    }

    pub fn project_key(&self) -> Option<&str> {
        self.project
            .as_ref()
            .and_then(|project| project["projectKey"].as_str())
    }
}

/// `WorkspaceProjectDescriptorPayload`.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub project_id: String,
    #[serde(default)]
    pub project_key: Option<String>,
    #[serde(default)]
    pub project_display_name: String,
    #[serde(default)]
    pub project_custom_name: Option<String>,
    #[serde(default)]
    pub project_root_path: String,
    #[serde(default)]
    pub project_kind: String,
    #[serde(default)]
    pub project_icon_revision: Option<String>,
}

impl Project {
    pub fn from_workspace(workspace: &Workspace) -> Self {
        Self {
            project_id: workspace.project_id.clone(),
            project_key: workspace.project_key().map(str::to_string),
            project_display_name: workspace.project_display_name.clone(),
            project_custom_name: workspace.project_custom_name.clone(),
            project_root_path: workspace.project_root_path.clone(),
            project_kind: workspace.project_kind.clone(),
            project_icon_revision: None,
        }
    }

    pub fn name(&self) -> String {
        if let Some(name) = self
            .project_custom_name
            .as_deref()
            .filter(|n| !n.is_empty())
        {
            return name.to_string();
        }
        if !self.project_display_name.is_empty() {
            return self.project_display_name.clone();
        }
        self.project_root_path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&self.project_root_path)
            .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PendingPermission {
    #[serde(default)]
    pub id: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// `AgentSnapshotPayload`. Fields Crusteo does not read live in `raw`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Agent {
    pub id: String,
    pub provider: String,
    pub cwd: String,
    pub model: Option<String>,
    pub title: Option<String>,
    pub status: String,
    pub workspace_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub last_user_message_at: Option<String>,
    pub requires_attention: bool,
    pub attention_reason: Option<String>,
    pub archived_at: Option<String>,
    pub pending_permissions: Vec<Value>,
    pub labels: Map<String, Value>,
    pub raw: Value,
}

impl Agent {
    pub fn from_value(value: Value) -> Option<Self> {
        let id = value["id"].as_str()?.to_string();
        let text = |key: &str| value[key].as_str().map(str::to_string);
        let labels = value["labels"].as_object().cloned().unwrap_or_default();
        Some(Self {
            id,
            provider: text("provider").unwrap_or_default(),
            cwd: text("cwd").unwrap_or_default(),
            model: text("model"),
            title: text("title").filter(|t| !t.is_empty()),
            status: text("status").unwrap_or_default(),
            workspace_id: text("workspaceId"),
            parent_agent_id: text("parentAgentId").or_else(|| {
                labels
                    .get("paseo.parent-agent-id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
            created_at: text("createdAt"),
            updated_at: text("updatedAt"),
            last_user_message_at: text("lastUserMessageAt"),
            requires_attention: value["requiresAttention"].as_bool().unwrap_or(false),
            attention_reason: text("attentionReason"),
            archived_at: text("archivedAt"),
            pending_permissions: value["pendingPermissions"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            labels,
            raw: value,
        })
    }

    pub fn display_title(&self) -> String {
        match self.title.as_deref() {
            Some(title) if !title.eq_ignore_ascii_case("new agent") => title.to_string(),
            _ => "New agent".into(),
        }
    }

    /// The provider id without a model suffix (`claude/opus` → `claude`).
    pub fn provider_id(&self) -> &str {
        self.provider.split('/').next().unwrap_or(&self.provider)
    }

    /// The provider's own session id, which its CLI resumes from.
    pub fn session_id(&self) -> Option<&str> {
        let persistence = &self.raw["persistence"];
        persistence["nativeHandle"]
            .as_str()
            .or_else(|| persistence["sessionId"].as_str())
            .filter(|id| !id.is_empty())
    }

    /// A shell command that resumes this agent's session in its provider's
    /// own CLI, from the agent's directory on its host; `None` for a
    /// provider without one or an agent with no session yet.
    pub fn resume_command(&self) -> Option<String> {
        let id = self.session_id()?;
        let resume = match self.provider_id() {
            "claude" => format!("claude --resume {id}"),
            "codex" => format!("codex resume {id}"),
            "gemini" => format!("gemini --resume {id}"),
            "opencode" => format!("opencode --session {id}"),
            "cursor" => format!("cursor-agent --resume {id}"),
            "copilot" => format!("copilot --resume {id}"),
            _ => return None,
        };
        if self.cwd.is_empty() {
            return Some(resume);
        }
        // Single-quoted for POSIX shells, where the hosts' agents run.
        let cwd = self.cwd.replace('\'', r"'\''");
        Some(format!("cd '{cwd}' && {resume}"))
    }

    /// The turn phase the daemon reports, if any (`open` while a turn runs).
    pub fn turn_open(&self) -> bool {
        self.raw["activeTurn"].is_object() || self.raw["turn"]["phase"] == "open"
    }

    pub fn model_label(&self) -> Option<&str> {
        self.model.as_deref().filter(|model| !model.is_empty())
    }

    pub fn bucket(&self) -> Bucket {
        let reason = self.attention_reason.as_deref();
        if !self.pending_permissions.is_empty() || reason == Some("permission") {
            Bucket::NeedsInput
        } else if self.status == "error" || reason == Some("error") {
            Bucket::Failed
        } else if self.status == "running" || self.turn_open() {
            Bucket::Running
        } else if self.requires_attention {
            Bucket::Attention
        } else {
            Bucket::Done
        }
    }
}

/// Paseo's state buckets, highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bucket {
    NeedsInput,
    Failed,
    Running,
    Attention,
    Done,
}

impl Bucket {
    pub fn parse(status: &str) -> Self {
        match status {
            "needs_input" => Bucket::NeedsInput,
            "failed" => Bucket::Failed,
            "running" => Bucket::Running,
            "attention" => Bucket::Attention,
            _ => Bucket::Done,
        }
    }

    /// The name [`Bucket::parse`] reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            Bucket::NeedsInput => "needs_input",
            Bucket::Failed => "failed",
            Bucket::Running => "running",
            Bucket::Attention => "attention",
            Bucket::Done => "done",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Bucket::NeedsInput => "Needs input",
            Bucket::Failed => "Failed",
            Bucket::Running => "Working",
            Bucket::Attention => "Ready to review",
            Bucket::Done => "Done",
        }
    }

    /// Sidebar status-group order.
    pub const GROUP_ORDER: [Bucket; 5] = [
        Bucket::NeedsInput,
        Bucket::Failed,
        Bucket::Attention,
        Bucket::Running,
        Bucket::Done,
    ];
}

// Approximate allocations, not serialized bytes: JSON strings, array
// capacity, and map nodes can be much larger than their wire representation.
pub(crate) fn json_bytes(value: &Value) -> usize {
    let extra = match value {
        Value::String(text) => text.capacity(),
        Value::Array(items) => items.iter().fold(
            items.capacity().saturating_mul(size_of::<Value>()),
            |total, item| total.saturating_add(json_bytes(item)),
        ),
        Value::Object(fields) => fields.iter().fold(0usize, |total, (key, value)| {
            total
                .saturating_add(128) // map node, key and allocator overhead
                .saturating_add(key.capacity())
                .saturating_add(json_bytes(value))
        }),
        _ => 0,
    };
    size_of::<Value>().saturating_add(extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info_from_status() {
        let message = json!({"type":"status","payload":{"status":"server_info","serverId":"srv_1","hostname":"alpha","version":"0.10.1","features":{"directorySync":true}}});
        let info = ServerInfo::from_status(&message).unwrap();
        assert_eq!(info.server_id, "srv_1");
        assert!(info.has("directorySync"));
        assert!(!info.has("projectList"));
    }

    #[test]
    fn workspace_tolerates_missing_fields() {
        let ws: Workspace =
            serde_json::from_value(json!({"id":"wks_1","status":"running","unknownField":1}))
                .unwrap();
        assert_eq!(ws.id, "wks_1");
        assert_eq!(Bucket::parse(&ws.status), Bucket::Running);
        assert!(!ws.background);
        let ws: Workspace =
            serde_json::from_value(json!({"id":"wks_2","background":true})).unwrap();
        assert!(ws.background);
    }

    #[test]
    fn agent_bucket_priority() {
        let agent = Agent::from_value(
            json!({"id":"a","status":"running","pendingPermissions":[{"id":"p"}]}),
        )
        .unwrap();
        assert_eq!(agent.bucket(), Bucket::NeedsInput);
        let agent =
            Agent::from_value(json!({"id":"a","status":"idle","requiresAttention":true})).unwrap();
        assert_eq!(agent.bucket(), Bucket::Attention);
    }

    #[test]
    fn resume_command_uses_the_provider_session() {
        let agent = Agent::from_value(json!({
            "id": "a", "provider": "claude", "cwd": "/home/me/it's here",
            "persistence": {"provider": "claude", "sessionId": "s1", "nativeHandle": "n1"},
        }))
        .unwrap();
        assert_eq!(agent.session_id(), Some("n1"));
        assert_eq!(
            agent.resume_command().unwrap(),
            r"cd '/home/me/it'\''s here' && claude --resume n1"
        );
        let codex = Agent::from_value(json!({
            "id": "b", "provider": "codex/gpt-5", "persistence": {"sessionId": "s2"},
        }))
        .unwrap();
        assert_eq!(codex.resume_command().unwrap(), "codex resume s2");
        let fresh = Agent::from_value(json!({"id": "c", "provider": "claude"})).unwrap();
        assert_eq!(fresh.resume_command(), None);
        let unknown = Agent::from_value(json!({
            "id": "d", "provider": "mystery", "persistence": {"sessionId": "s3"},
        }))
        .unwrap();
        assert_eq!(unknown.resume_command(), None);
    }
}
