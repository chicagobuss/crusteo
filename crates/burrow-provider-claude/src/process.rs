//! Claude Code's stdio: newline-delimited JSON on stdin and stdout, split on
//! LF only, in a process group of its own.
//!
//! [`burrow_provider::process`] is the transport; this is the shape the
//! session and the replay tests drive.

pub use burrow_provider::process::Output;

use std::path::Path;

use burrow_provider::process::{self, Config, Process};
use tokio::process::Command;

/// Starts the CLI with piped stdio in a process group of its own, recording
/// every frame to `trace` when there is one.
pub fn spawn(command: Command, trace: Option<&Path>) -> std::io::Result<Process> {
    process::spawn(
        command,
        Config {
            provider: "claude",
            trace,
            ..Config::default()
        },
    )
}
