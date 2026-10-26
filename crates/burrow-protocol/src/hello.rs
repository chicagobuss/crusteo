//! The client's first frame.

use serde::Deserialize;
use serde_json::{Map, Value};

/// `WSHelloMessageSchema`. Unknown top-level fields are ignored, as in Paseo
/// 0.9, so a hello written for a newer daemon still validates.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub client_id: String,
    pub client_type: ClientType,
    pub protocol_version: i64,
    #[serde(default)]
    pub app_version: Option<String>,
    #[serde(default)]
    pub auth: Option<Value>,
    /// Booleans keyed by capability name; unknown keys are kept.
    #[serde(default)]
    pub capabilities: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientType {
    Mobile,
    Browser,
    Cli,
    Mcp,
    Hub,
}

/// Plugin client ids are reserved for the daemon's own plugin hosts.
const RESERVED_CLIENT_ID_PREFIX: &str = "plugin:";

impl Hello {
    /// Schema validation only. The daemon then checks, in Paseo's order: the
    /// protocol version, auth, an empty trimmed clientId, a reserved clientId.
    pub fn from_frame(frame: Value) -> Result<Self, String> {
        serde_json::from_value(frame).map_err(|err| err.to_string())
    }

    /// The session identity: the clientId, trimmed.
    pub fn client_id(&self) -> &str {
        self.client_id.trim()
    }

    pub fn has_reserved_client_id(&self) -> bool {
        self.client_id().starts_with(RESERVED_CLIENT_ID_PREFIX)
    }

    /// Whether the client advertised `name` as true.
    pub fn has(&self, name: &str) -> bool {
        self.capabilities
            .as_ref()
            .and_then(|caps| caps.get(name))
            .and_then(Value::as_bool)
            == Some(true)
    }

    /// The client asked for `hello.rejected` before an auth or protocol close.
    pub fn wants_rejection_frame(&self) -> bool {
        self.auth.is_some() || self.has(capability::HELLO_REJECTION)
    }
}

/// Client capability names (`packages/protocol/src/client-capabilities.ts`).
pub mod capability {
    pub const HELLO_REJECTION: &str = "hello_rejection";
    pub const OWNED_SUBSCRIPTIONS: &str = "owned_subscriptions";
    pub const EXPLICIT_EVENT_SUBSCRIPTIONS: &str = "explicit_event_subscriptions";
    pub const SELECTIVE_AGENT_TIMELINE: &str = "selective_agent_timeline";
    pub const COMPACT_PROVIDER_SNAPSHOTS: &str = "compact_provider_snapshots";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_the_reference_hello() {
        let hello = Hello::from_frame(json!({
            "type": "hello", "clientId": " cid_1 ", "clientType": "cli", "protocolVersion": 1,
            "appVersion": "test/0.1", "capabilities": {"owned_subscriptions": true}, "extra": 1,
        }))
        .unwrap();
        assert_eq!(hello.client_id(), "cid_1");
        assert!(hello.has(capability::OWNED_SUBSCRIPTIONS));
        assert!(!hello.wants_rejection_frame());
    }

    #[test]
    fn refuses_bad_hellos() {
        let base = json!({"type":"hello","clientType":"cli","protocolVersion":1});
        assert!(
            Hello::from_frame(base.clone()).is_err(),
            "clientId is required"
        );
        let mut plugin = base.clone();
        plugin["clientId"] = json!("plugin:x");
        assert!(Hello::from_frame(plugin).unwrap().has_reserved_client_id());
        let mut bad_type = base;
        bad_type["clientId"] = json!("c");
        bad_type["clientType"] = json!("toaster");
        assert!(Hello::from_frame(bad_type).is_err());
    }
}
