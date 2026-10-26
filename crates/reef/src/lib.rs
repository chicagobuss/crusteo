//! Reef: crusteo without the window. It keeps one connection to
//! every daemon in a hosts file, keeps the fleet model current with the same
//! `fleet` reducers the desktop app uses, and serves phones a compact stream
//! over one WebSocket: a snapshot of only what is on the phone's screen, then
//! small deltas. See `docs/reef/design.md` for the protocol.
//!
//! Layers, as in the desktop app:
//! - [`upstream`]: daemon connections, bootstrap and timeline streaming,
//!   headless. It sends the daemons reads, and the writes the phone's
//!   actions make, from a fixed list.
//! - [`views`]: pure projections of the fleet into phone-sized rows and
//!   grouped turns, and the diffs between two of them.
//! - [`reef`]: the one task that owns the state, applies daemon events and
//!   phone requests, and sends each phone its deltas.
//! - [`server`]: the listener: Skiff (the page phones load), the WebSocket and
//!   its auth.
//! - [`usage`]: account usage from the daemons and a usage feed, by
//!   `usage-source`'s rules, the ones crusteo's gauges follow.
//! - [`legacy`]: what crusteo-hub, Reef's old name, left behind.

pub mod catalog;
pub mod config;
pub mod fake;
pub mod legacy;
pub mod lists;
pub mod proto;
pub mod reef;
pub mod server;
pub mod settings;
pub mod upstream;
pub mod usage;
pub mod views;

/// Reply previews, shared with the desktop app's lists.
pub use fleet::preview;

pub use config::{Device, ReefConfig};
pub use reef::{ReefHandle, start};

/// The release version and the commit built, as --version and the startup
/// line show them.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CRUSTEO_COMMIT"), ")");
