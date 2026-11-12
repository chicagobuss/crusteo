//! `codex app-server`'s stdio: newline-delimited JSON-RPC on stdin and stdout,
//! split on LF only, in a process group of its own.
//!
//! Codex writes U+2028 and U+2029 raw inside JSON strings, so a reader that
//! also breaks on those (Node's `readline`) cuts messages apart; Paseo
//! replaced its reader for that reason. [`burrow_provider::process`] keeps the
//! LF-only split for every provider.

pub use burrow_provider::process::Output;

use std::path::Path;

use burrow_provider::process::{self, Config, Process};
use tokio::process::Command;

/// Starts app-server with piped stdio in a process group of its own,
/// recording every frame to `trace` when there is one.
pub fn spawn(command: Command, trace: Option<&Path>) -> std::io::Result<Process> {
    process::spawn(
        command,
        Config {
            provider: "codex",
            trace,
            ..Config::default()
        },
    )
}
