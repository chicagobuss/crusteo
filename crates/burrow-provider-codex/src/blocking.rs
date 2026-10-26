//! A short-lived `codex app-server` driven synchronously: for the model
//! catalog at startup and for reading a thread's history, outside any
//! session.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::session::initialize_params;

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<Value>,
    next_id: u64,
}

impl Client {
    /// Starts `binary app-server` and initializes it.
    pub fn start(binary: &Path, cwd: Option<&Path>, timeout: Duration) -> Result<Self, String> {
        let mut command = std::process::Command::new(binary);
        command
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(cwd) = cwd.filter(|c| c.is_dir()) {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(b'\n', &mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if let Ok(value) = serde_json::from_slice::<Value>(&buffer)
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        client.call("initialize", initialize_params(), timeout)?;
        client.write(&json!({"method": "initialized", "params": {}}))?;
        Ok(client)
    }

    fn write(&mut self, frame: &Value) -> Result<(), String> {
        writeln!(self.stdin, "{frame}").map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())
    }

    /// One request; notifications and app-server's own requests in between
    /// are skipped (the latter answered with `{}`).
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({"id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = self
                .lines
                .recv_timeout(left)
                .map_err(|_| format!("no answer to {method}"))?;
            if message.get("method").is_some() {
                if let Some(id) = message.get("id") {
                    self.write(&json!({"id": id, "result": {}}))?;
                }
                continue;
            }
            if message["id"] != id {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(error["message"].as_str().unwrap_or("error").to_string());
            }
            return Ok(message["result"].clone());
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
