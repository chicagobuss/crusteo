//! Burrow: a daemon that speaks the Paseo WebSocket protocol (v1).
//!
//! Phase 0 serves the handshake, owned subscriptions, the socket lease and
//! the directory and timeline reads a client makes on connect, from an
//! in-memory store. See `docs/design.md` for the architecture and roadmap.

#![forbid(unsafe_code)]

pub mod agent_requests;
pub mod agents;
pub mod attachments;
pub mod clock;
pub mod coalesce;
pub mod connection;
pub mod daemon;
pub mod discovery;
pub mod git;
pub mod handlers;
pub mod home;
pub mod ids;
pub mod labels;
pub mod paseo_import;
pub mod persist;
pub mod project_key;
pub mod seed;
pub mod server;
pub mod setup;
pub mod store;
pub mod subscriptions;
pub mod suggest;
pub mod timeline;
pub mod workspace_requests;
pub mod worktrees;

pub use daemon::{Config, DEFAULT_LISTEN, Daemon};
pub use server::{Running, start};
