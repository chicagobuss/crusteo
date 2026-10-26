//! Identifiers in Paseo's shapes. All are opaque to clients.

use uuid::Uuid;

fn hex16() -> String {
    Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// A project id: `prj_<16 hex>`.
pub fn project_id() -> String {
    format!("prj_{}", hex16())
}

/// A workspace id: `wks_<16 hex>`.
pub fn workspace_id() -> String {
    format!("wks_{}", hex16())
}

/// An agent id: a UUID.
pub fn agent_id() -> String {
    Uuid::new_v4().to_string()
}

/// A subscription id, assigned by the host: a UUID.
pub fn subscription_id() -> String {
    Uuid::new_v4().to_string()
}

/// A directory generation or timeline epoch: a UUID.
pub fn generation() -> String {
    Uuid::new_v4().to_string()
}

/// A daemon identity: `srv_<hex>`.
pub fn server_id() -> String {
    format!("srv_{}", Uuid::new_v4().simple())
}
