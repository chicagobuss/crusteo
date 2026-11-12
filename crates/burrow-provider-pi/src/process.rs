//! pi's `--mode rpc` stdio: newline-delimited JSON on stdin and stdout, split
//! on LF only, in a process group of its own.
//!
//! Records split only on LF, as pi's RPC docs require (a JSON string may hold
//! U+2028). Dropping the input sender closes stdin, which is pi's orderly
//! shutdown; a pi that has not exited shortly after its stdout ends is killed
//! ([`burrow_provider::process`]).

pub use burrow_provider::process::Output;

use std::path::Path;
use std::time::Duration;

use burrow_provider::process::{self, Config, Process};
use tokio::process::Command;

/// How much of pi's stderr to keep, for error messages.
const STDERR_TAIL: usize = 8192;
/// How long pi may take to exit once its stdout ends.
const EXIT_WAIT: Duration = Duration::from_secs(3);

/// Starts pi with piped stdio in a process group of its own, recording every
/// frame to `trace` when there is one.
pub fn spawn(command: Command, trace: Option<&Path>) -> std::io::Result<Process> {
    process::spawn(
        command,
        Config {
            provider: "pi",
            trace,
            stderr_tail: STDERR_TAIL,
            exit_wait: EXIT_WAIT,
            ..Config::default()
        },
    )
}
