//! Close codes, error codes and the timing constants clients depend on.

use std::time::Duration;

/// WebSocket close codes and reasons, as Paseo sends them.
pub mod close {
    pub const SHUTTING_DOWN: (u16, &str) = (1001, "Server shutting down");
    pub const HELLO_TIMEOUT: (u16, &str) = (4001, "Hello timeout");
    pub const MESSAGE_BEFORE_HELLO: (u16, &str) = (4002, "Session message before hello");
    pub const INVALID_HELLO: (u16, &str) = (4002, "Invalid hello");
    pub const INVALID_PLUGIN_CLIENT_ID: (u16, &str) = (4002, "Invalid plugin clientId");
    pub const UNEXPECTED_HELLO: (u16, &str) = (4002, "Unexpected hello");
    pub const INCOMPATIBLE_PROTOCOL: (u16, &str) = (4003, "Incompatible protocol version");
    pub const PASSWORD_REQUIRED: (u16, &str) = (4401, "Password required");
    pub const INCORRECT_PASSWORD: (u16, &str) = (4401, "Incorrect password");
}

/// `rpc_error.payload.code` values.
pub mod rpc {
    /// The session message failed validation and carried a `requestId`. Paseo
    /// uses it for unknown types *and* for known types with bad fields.
    pub const UNKNOWN_SCHEMA: &str = "unknown_schema";
    pub const INVALID_MESSAGE: &str = "invalid_message";
    pub const HANDLER_ERROR: &str = "handler_error";
    pub const ACCESS_DENIED: &str = "access_denied";
    pub const INVALID_REQUEST: &str = "invalid_request";
}

/// The first frame must be a hello within this long.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// After the first bare `ping`, a socket that sends nothing for this long is
/// terminated. A socket that never pings is never leased.
pub const SOCKET_LEASE: Duration = Duration::from_secs(45);
/// How often leases are checked. Expiry is observed 45 to 55 s after the last frame.
pub const LEASE_CHECK_EVERY: Duration = Duration::from_secs(10);
/// A session outlives its last socket by this long, so a reconnect with the
/// same clientId resumes it. Subscriptions never survive.
pub const SESSION_RESUME_GRACE: Duration = Duration::from_secs(90);
/// Outbound bytes queued on one socket beyond this terminate it.
pub const OUTBOUND_HIGH_WATER_BYTES: usize = 64 * 1024 * 1024;
/// Directory removals remembered for `changes`-mode sync reads.
pub const DIRECTORY_TOMBSTONES: usize = 5000;
/// The page size the official app uses for timeline reads.
pub const TIMELINE_PAGE: usize = 40;
/// The daemon's default when a timeline read has no `limit`.
pub const TIMELINE_DEFAULT_LIMIT: usize = 200;
/// Largest `page.limit` a directory read accepts.
pub const DIRECTORY_PAGE_MAX: usize = 200;
