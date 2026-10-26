//! `paseo.json` setup scripts for a new worktree, run as Paseo 0.12 runs them
//! (`runWorktreeSetupCommands`, `buildWorktreeSetupDetail`).
//!
//! `worktree.setup` is one command or a list. Each runs in turn in the
//! workspace's directory, through `bash -c` (PowerShell on Windows), with the
//! daemon's environment and Paseo's variables: `PASEO_SOURCE_CHECKOUT_PATH`
//! and `PASEO_ROOT_PATH` (the main checkout), `PASEO_WORKTREE_PATH`,
//! `PASEO_BRANCH_NAME` and `PASEO_WORKTREE_PORT`. The first command that
//! fails stops the rest. Progress is reported when each command starts and
//! ends, as a `worktree_setup` detail clients render.
//!
//! Blocking: run it off the async threads.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Per command, Paseo keeps the head and tail of this much output.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const TRUNCATION_MARKER: &str = "\n...<output truncated in the middle>...\n";
/// A setup command has no time limit in Paseo; this only stops a hung one.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// One command's outcome; `exit_code` is `None` while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub command: String,
    pub cwd: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}

/// `worktree.setup` from `<dir>/paseo.json`; none without the file.
pub fn setup_commands(dir: &Path) -> Result<Vec<String>, String> {
    lifecycle_commands(dir, "setup")
}

/// `worktree.teardown`, run before an archived worktree is removed.
pub fn teardown_commands(dir: &Path) -> Result<Vec<String>, String> {
    lifecycle_commands(dir, "teardown")
}

fn lifecycle_commands(dir: &Path, which: &str) -> Result<Vec<String>, String> {
    let path = dir.join("paseo.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "Failed to parse paseo.json at {}: {err}",
                path.display()
            ));
        }
    };
    let config: Value = serde_json::from_str(&text)
        .map_err(|err| format!("Failed to parse paseo.json at {}: {err}", path.display()))?;
    // Paseo's schema `.catch`es a malformed section to "no commands".
    Ok(match &config["worktree"][which] {
        Value::String(one) if !one.trim().is_empty() => vec![one.clone()],
        Value::Array(many) => many
            .iter()
            .filter_map(Value::as_str)
            .filter(|c| !c.trim().is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    })
}

/// Paseo's runtime variables for a worktree's scripts.
pub fn runtime_env(
    worktree: &Path,
    branch: &str,
    repo_root: &str,
) -> Result<Vec<(String, String)>, String> {
    let port = crate::worktrees::runtime_port(worktree)?;
    Ok(vec![
        ("PASEO_SOURCE_CHECKOUT_PATH".into(), repo_root.into()),
        ("PASEO_ROOT_PATH".into(), repo_root.into()),
        (
            "PASEO_WORKTREE_PATH".into(),
            worktree.to_string_lossy().into_owned(),
        ),
        ("PASEO_BRANCH_NAME".into(), branch.into()),
        ("PASEO_WORKTREE_PORT".into(), port.to_string()),
    ])
}

/// Runs `commands` in `cwd`. `progress` gets every result so far, the
/// running one last, whenever one starts or ends. On failure the error
/// carries Paseo's message and the results up to the failed command.
pub fn run(
    commands: &[String],
    cwd: &Path,
    env: &[(String, String)],
    mut progress: impl FnMut(&[CommandResult]),
) -> Result<Vec<CommandResult>, (String, Vec<CommandResult>)> {
    let mut results: Vec<CommandResult> = Vec::new();
    for command in commands {
        let started = Instant::now();
        results.push(CommandResult {
            command: command.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
        });
        progress(&results);
        let (output, code) = execute(command, cwd, env);
        let last = results.last_mut().expect("pushed above");
        last.output = output;
        last.exit_code = code;
        last.duration_ms = started.elapsed().as_millis() as u64;
        let message = format!("Worktree setup command failed: {command}\n{}", last.output);
        progress(&results);
        if code != Some(0) {
            return Err((message.trim().to_string(), results));
        }
    }
    Ok(results)
}

fn execute(command: &str, cwd: &Path, env: &[(String, String)]) -> (String, Option<i32>) {
    let mut process = if cfg!(windows) {
        let mut process = Command::new("powershell");
        process.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            command,
        ]);
        process
    } else {
        let mut process = Command::new("bash");
        process.args(["-c", command]);
        process
    };
    process
        .current_dir(cwd)
        .env_remove("BASH_ENV")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        process.env(key, value);
    }
    match crate::git::run_command(process, COMMAND_TIMEOUT) {
        Ok(output) => (format!("{}{}", output.stdout, output.stderr), output.code),
        Err(err) => (err.to_string(), None),
    }
}

/// The `worktree_setup` detail for `workspace_setup_progress` and the
/// agent timeline's `paseo_worktree_setup` tool call.
pub fn detail(worktree_path: &str, branch: &str, results: &[CommandResult]) -> Value {
    let total = results.len();
    let mut truncated = false;
    let mut lines = Vec::new();
    let commands: Vec<Value> = results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let (log, cut) = command_log(&result.output);
            truncated |= cut;
            lines.push(format!(
                "==> [{}/{total}] Running: {}",
                index + 1,
                result.command
            ));
            if !log.is_empty() {
                lines.push(log.trim_end_matches('\n').to_string());
            }
            if let Some(code) = result.exit_code {
                lines.push(format!(
                    "<== [{}/{total}] Exit {code} in {:.2}s",
                    index + 1,
                    result.duration_ms as f64 / 1000.0
                ));
            }
            let status = match result.exit_code {
                None => "running",
                Some(0) => "completed",
                Some(_) => "failed",
            };
            let mut command = json!({
                "index": index + 1, "command": result.command, "cwd": result.cwd, "log": log,
                "status": status, "exitCode": result.exit_code,
            });
            if result.duration_ms > 0 {
                command["durationMs"] = result.duration_ms.into();
            }
            command
        })
        .collect();
    let mut detail = json!({
        "type": "worktree_setup", "worktreePath": worktree_path, "branchName": branch,
        "log": lines.join("\n"), "commands": commands,
    });
    if truncated {
        detail["truncated"] = true.into();
    }
    detail
}

/// A command's output as Paseo shows it: the middle of a long one cut out,
/// and carriage-return overwrites applied per line.
fn command_log(output: &str) -> (String, bool) {
    let (text, truncated) = if output.len() > MAX_OUTPUT_BYTES {
        let budget = MAX_OUTPUT_BYTES - TRUNCATION_MARKER.len();
        let head = floor_boundary(output, budget / 2);
        let tail = ceil_boundary(output, output.len() - (budget - budget / 2));
        (
            format!("{}{TRUNCATION_MARKER}{}", &output[..head], &output[tail..]),
            true,
        )
    } else {
        (output.to_string(), false)
    };
    let text: Vec<&str> = text
        .split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.rsplit('\r').next().unwrap_or(line)
        })
        .collect();
    (text.join("\n"), truncated)
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_boundary(text: &str, mut at: usize) -> usize {
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_render_like_paseo() {
        let results = vec![
            CommandResult {
                command: "echo hi".into(),
                cwd: "/w".into(),
                output: "hi\n".into(),
                exit_code: Some(0),
                duration_ms: 1500,
            },
            CommandResult {
                command: "make".into(),
                cwd: "/w".into(),
                output: "50%\r100%\n".into(),
                exit_code: None,
                duration_ms: 0,
            },
        ];
        let detail = detail("/w", "b", &results);
        assert_eq!(
            detail["log"],
            "==> [1/2] Running: echo hi\nhi\n<== [1/2] Exit 0 in 1.50s\n==> [2/2] Running: make\n100%"
        );
        assert_eq!(detail["commands"][1]["status"], "running");
        assert!(detail["commands"][1].get("durationMs").is_none());
        assert_eq!(detail["commands"][0]["durationMs"], 1500);
    }

    #[test]
    fn commands_come_from_paseo_json() {
        let dir = std::env::temp_dir().join(format!("burrow-setup-{}", crate::ids::agent_id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(setup_commands(&dir).unwrap().is_empty());
        std::fs::write(dir.join("paseo.json"), r#"{"worktree":{"setup":"npm ci"}}"#).unwrap();
        assert_eq!(setup_commands(&dir).unwrap(), ["npm ci"]);
        std::fs::write(
            dir.join("paseo.json"),
            r#"{"worktree":{"setup":["a"," ","b"]}}"#,
        )
        .unwrap();
        assert_eq!(setup_commands(&dir).unwrap(), ["a", "b"]);
        std::fs::write(dir.join("paseo.json"), "{").unwrap();
        assert!(
            setup_commands(&dir)
                .unwrap_err()
                .starts_with("Failed to parse paseo.json")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
