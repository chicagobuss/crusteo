//! The Paseo daemon WebSocket protocol, version 1, as Burrow speaks it.
//!
//! The wire is one JSON object per text frame, plus a few binary frame kinds
//! (terminal I/O, file transfer). Three envelope shapes exist:
//!
//! * bare frames: `hello`, `ping`/`pong`, `recording_state`, `hello.rejected`;
//! * inbound session frames: `{"type":"session","message":{"type":..,"requestId":..,<args>}}`,
//!   flat, with the arguments next to `type`;
//! * outbound session frames: `{"type":"session","message":{"type":..,"payload":{..}}}`,
//!   where `requestId` and `subscriptionId` live inside `payload`.
//!
//! This crate holds the parts of the protocol that are rules rather than
//! data (reply naming, close codes, error codes) and typed payloads for the
//! messages Burrow produces. Everything else stays `serde_json::Value`
//! until a phase needs it typed. See `docs/protocol-inventory.md`.

#![forbid(unsafe_code)]

pub mod codes;
pub mod directory;
pub mod envelope;
pub mod hello;
pub mod labels;
pub mod reply;
pub mod server_info;
pub mod timeline;

/// The protocol version in `hello.protocolVersion` and `server_info`. The
/// daemon refuses anything lower; there has never been a higher one.
pub const PROTOCOL_VERSION: u32 = 1;
