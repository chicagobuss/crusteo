//! The `agy` child process's stdio: newline-delimited JSON on stdin and
//! stdout, in a process group of its own.
//!
//! Stopping follows Paseo: `SIGINT` (an interrupt) or closing stdin (a close),
//! then `SIGTERM` after a second and `SIGKILL` after two, and the group is
//! ended once the CLI has exited, because a tool can outlive it. All of that
//! is [`burrow_provider::process`]; this is the shape the session drives.

pub use burrow_provider::process::{Output, Stop};

use std::path::Path;

use burrow_provider::process::{self, Config};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::mpsc;

/// How much of the CLI's stderr to keep, for error messages (Paseo keeps
/// 8 KiB).
const STDERR_TAIL: usize = 8192;

/// The ends of a CLI process (or a test double). Dropping it closes the
/// process as [`Stop::Close`] does.
pub struct Transport {
    pub input: mpsc::UnboundedSender<Value>,
    pub stop: mpsc::UnboundedSender<Stop>,
    pub output: mpsc::UnboundedReceiver<Output>,
}

/// Starts `command` with piped stdio in its own process group. With `trace`,
/// every frame is also written there as `{"dir":"in"|"out","frame":...}`.
pub fn spawn(command: Command, trace: Option<&Path>) -> std::io::Result<Transport> {
    let process = process::spawn_stoppable(
        command,
        Config {
            provider: "agy",
            trace,
            stderr_tail: STDERR_TAIL,
            stops: true,
            ..Config::default()
        },
    )?;
    Ok(Transport {
        input: process.input,
        stop: process.stop,
        output: process.output,
    })
}
