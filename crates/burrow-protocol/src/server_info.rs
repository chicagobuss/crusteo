//! The welcome frame: a `status` session message with `status: "server_info"`.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::PROTOCOL_VERSION;

/// Every permission a session can hold. An owner (no password, or the right
/// one) holds all of them.
pub const OWNER_PERMISSIONS: &[&str] = &[
    "daemon.read",
    "daemon.manage",
    "tunnel.manage",
    "access.manage",
    "workspace.read",
    "workspace.write",
    "workspace.manage",
    "automation.manage",
    "hub.execute",
];

/// `server_info.features` names. Clients gate a feature on its flag and tell
/// the user to update the host when it is missing, so a daemon must only
/// advertise what it implements. Paseo 0.11 advertises about 85 flags; these
/// are the ones Burrow implements, or will, with the phase that brings them.
pub mod feature {
    // P0
    pub const OWNED_SUBSCRIPTIONS: &str = "ownedSubscriptions";
    pub const DIRECTORY_SYNC: &str = "directorySync";
    pub const SELECTIVE_AGENT_TIMELINE: &str = "selectiveAgentTimeline";
    pub const EXPLICIT_EVENT_SUBSCRIPTIONS: &str = "explicitEventSubscriptions";
    pub const PROJECT_LIST: &str = "projectList";
    pub const WORKSPACE_LABELS: &str = "workspaceLabels";
    /// Paseo 0.12: background workspaces stay out of directory reads and
    /// pushes unless a client sends `includeBackground`.
    pub const BACKGROUND_WORKSPACES: &str = "backgroundWorkspaces";
    /// `project.add.request`. Paseo's app adds projects only when the daemon
    /// also advertises `stableProjectIdentity`.
    pub const PROJECT_ADD: &str = "projectAdd";
    /// Project keys are Paseo's (`remote:<host>/<path>`, else
    /// `host:<serverId>:<path>`), so one repository joins across hosts.
    pub const STABLE_PROJECT_IDENTITY: &str = "stableProjectIdentity";
    /// `project.create_directory.request`.
    pub const PROJECT_CREATE_DIRECTORY: &str = "projectCreateDirectory";
    /// `project.remove.request` (Paseo since 0.1.97).
    pub const PROJECT_REMOVE: &str = "projectRemove";
    /// Workspaces are identified by id, several to a directory:
    /// `workspace.create.request` always makes a new one.
    pub const WORKSPACE_MULTIPLICITY: &str = "workspaceMultiplicity";
    /// Paseo 0.12: `send_agent_message_request` takes `sourceAgentId`, the
    /// agent sending it. Paseo's CLI refuses to send from inside an agent
    /// without it ("Update the Paseo host to send messages with agent
    /// provenance").
    pub const AGENT_MESSAGE_PROVENANCE: &str = "agentMessageProvenance";
    /// Paseo 0.13: workspaces carry `createdAt` and `lastUserMessageAt`.
    /// The app sorts by them only when every host advertises this.
    pub const WORKSPACE_SORT_TIMES: &str = "workspaceSortTimes";
    /// `workspace.setup.subscribe.request` and workspaces' `setupStatus`:
    /// Paseo 0.13's setup panel needs it ("Update the host to view
    /// workspace setup details").
    pub const WORKSPACE_SETUP_OBSERVATION: &str = "workspaceSetupObservation";
    /// Timeline pages keep turns whole on request (`wholeTurns`), which
    /// Paseo 0.14's app needs to fold finished turns.
    pub const WHOLE_TURN_TIMELINE: &str = "wholeTurnTimeline";
    /// `daemon.get_status.request`, which `paseo status` and `paseo daemon
    /// status` need ("Update the host to read daemon status" without it).
    /// Paseo's CLI then also asks for `daemon.get_pairing_offer`.
    pub const DAEMON_STATUS_RPC: &str = "daemonStatusRpc";
    // P1
    pub const PROVIDERS_SNAPSHOT: &str = "providersSnapshot";
    pub const CREATION_LIFECYCLE: &str = "creationLifecycle";
    pub const AGENT_THINKING_UPDATE: &str = "agentThinkingUpdate";
    pub const CANONICAL_SUBMITTED_PROMPTS: &str = "canonicalSubmittedPrompts";
    pub const WORKSPACE_TERMINALS: &str = "workspaceTerminals";
}

/// The P0 feature set, Paseo 0.12's background workspaces, adding and
/// removing projects, and adding workspaces.
pub const P0_FEATURES: &[&str] = &[
    feature::OWNED_SUBSCRIPTIONS,
    feature::DIRECTORY_SYNC,
    feature::SELECTIVE_AGENT_TIMELINE,
    feature::EXPLICIT_EVENT_SUBSCRIPTIONS,
    feature::PROJECT_LIST,
    feature::WORKSPACE_LABELS,
    feature::BACKGROUND_WORKSPACES,
    feature::PROJECT_ADD,
    feature::STABLE_PROJECT_IDENTITY,
    feature::PROJECT_CREATE_DIRECTORY,
    feature::PROJECT_REMOVE,
    feature::WORKSPACE_MULTIPLICITY,
    feature::AGENT_MESSAGE_PROVENANCE,
    feature::WORKSPACE_SORT_TIMES,
    feature::WORKSPACE_SETUP_OBSERVATION,
    // Paseo's app reads tools and models only from a host that says so: its
    // pickers stayed on "Loading..." without it.
    feature::PROVIDERS_SNAPSHOT,
    feature::WHOLE_TURN_TIMELINE,
    feature::DAEMON_STATUS_RPC,
    // `set_agent_thinking_request`; `paseo agent update --thinking` refuses
    // without the flag.
    feature::AGENT_THINKING_UPDATE,
];

/// `ServerInfoStatusPayloadSchema`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub status: &'static str,
    pub protocol_version: u32,
    pub server_id: String,
    pub hostname: Option<String>,
    pub version: Option<String>,
    pub permissions: Vec<String>,
    pub desktop_managed: bool,
    pub features: Map<String, Value>,
}

impl ServerInfo {
    pub fn new(server_id: impl Into<String>, hostname: Option<String>, version: &str) -> Self {
        Self {
            status: "server_info",
            protocol_version: PROTOCOL_VERSION,
            server_id: server_id.into(),
            hostname,
            version: Some(version.to_string()),
            permissions: OWNER_PERMISSIONS.iter().map(|p| p.to_string()).collect(),
            desktop_managed: false,
            features: P0_FEATURES
                .iter()
                .map(|name| (name.to_string(), Value::Bool(true)))
                .collect(),
        }
    }

    /// The whole frame, ready to send.
    pub fn frame(&self) -> Value {
        crate::envelope::session("status", self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_has_the_shape_clients_wait_for() {
        let frame = ServerInfo::new("srv_test", Some("example-host".into()), "0.1.0").frame();
        let payload = &frame["message"]["payload"];
        assert_eq!(frame["message"]["type"], "status");
        assert_eq!(payload["status"], "server_info");
        assert_eq!(payload["protocolVersion"], 1);
        assert_eq!(payload["serverId"], "srv_test");
        assert_eq!(payload["features"]["directorySync"], true);
        assert_eq!(payload["features"]["backgroundWorkspaces"], true);
        assert_eq!(payload["features"]["projectAdd"], true);
        assert_eq!(payload["features"]["stableProjectIdentity"], true);
        assert_eq!(payload["features"]["projectRemove"], true);
        assert_eq!(payload["features"]["agentMessageProvenance"], true);
        assert_eq!(payload["features"]["daemonStatusRpc"], true);
        assert_eq!(payload["features"]["agentThinkingUpdate"], true);
        assert_eq!(payload["permissions"].as_array().unwrap().len(), 9);
    }
}
