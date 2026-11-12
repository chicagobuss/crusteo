//! The ACP agent's stdio: newline-delimited JSON-RPC on stdin and stdout, in a
//! process group of its own ([`burrow_provider::process`]).

pub use burrow_provider::process::Output;

use std::path::Path;

use burrow_provider::process::{self, Config};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::mpsc;

/// The two ends of an agent process (or a test double). Dropping `output`
/// ends the process's group.
pub struct Transport {
    pub input: mpsc::UnboundedSender<Value>,
    pub output: mpsc::UnboundedReceiver<Output>,
}

/// Starts `command` with piped stdio, in a process group of its own. With
/// `trace`, every frame is also written there as `{"dir":"in"|"out","frame":...}`.
pub fn spawn(command: Command, trace: Option<&Path>) -> std::io::Result<Transport> {
    let process = process::spawn(
        command,
        Config {
            provider: "acp",
            trace,
            ..Config::default()
        },
    )?;
    Ok(Transport {
        input: process.input,
        output: process.output,
    })
}
