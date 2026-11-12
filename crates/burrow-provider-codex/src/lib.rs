//! The Codex provider: runs the user's own `codex` (found on PATH, as Paseo
//! does) as `codex app-server`, one process per session.
//!
//! The model catalog comes from app-server itself (`model/list`, with the
//! user's configured defaults), read once at startup and refreshed by every
//! session that starts.
//!
//! Set `BURROW_CODEX_TRACE=<dir>` to record every frame each session
//! exchanges with app-server as JSONL (`{"dir":"in"|"out","frame":...}`),
//! the format the replay tests use.

#![forbid(unsafe_code)]

pub mod blocking;
pub mod catalog;
pub mod history;
pub mod process;
pub mod session;
pub mod tools;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_provider::{
    Capabilities, HistoryItem, Provider, ProviderError, ProviderInfo, Session as ProviderSession,
    SessionSpec,
};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::catalog::Catalog;
use crate::session::{Launcher, Session, Settings, Transport};

/// How long the startup catalog probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Makes the transport of a session that does not run `codex` itself.
pub type Launch = Arc<dyn Fn() -> std::io::Result<Transport> + Send + Sync>;

/// Codex, found and version-checked.
#[derive(Clone)]
pub struct CodexProvider {
    binary: PathBuf,
    version: Option<Vec<u32>>,
    catalog: Arc<Mutex<Catalog>>,
    trace_dir: Option<PathBuf>,
    launch: Option<Launch>,
}

impl std::fmt::Debug for CodexProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexProvider")
            .field("binary", &self.binary)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl CodexProvider {
    /// Finds `codex` (or `binary`), reads its version and asks app-server
    /// for its models. `None` when it cannot be run.
    pub fn detect(binary: Option<&Path>) -> Option<Self> {
        let binary = binary.map_or_else(|| PathBuf::from("codex"), Path::to_path_buf);
        let output = std::process::Command::new(&binary)
            .arg("--version")
            .output()
            .ok()?;
        let version = catalog::parse_version(&String::from_utf8_lossy(&output.stdout));
        let catalog = probe(&binary).unwrap_or_else(|err| {
            tracing::warn!(%err, "could not read Codex's models; offering none until a session starts");
            Catalog::default()
        });
        Some(Self {
            binary,
            version,
            catalog: Arc::new(Mutex::new(catalog)),
            trace_dir: burrow_provider::env::var_os("BURROW_CODEX_TRACE").map(PathBuf::from),
            launch: None,
        })
    }

    /// A provider whose sessions talk to `launch`'s transports instead of a
    /// `codex` process: a scripted app-server in tests.
    pub fn with_launcher(version: Option<Vec<u32>>, catalog: Catalog, launch: Launch) -> Self {
        Self {
            binary: PathBuf::from("codex"),
            version,
            catalog: Arc::new(Mutex::new(catalog)),
            trace_dir: None,
            launch: Some(launch),
        }
    }

    pub fn version(&self) -> Option<String> {
        self.version
            .as_ref()
            .map(|v| v.iter().map(u32::to_string).collect::<Vec<_>>().join("."))
    }

    fn catalog(&self) -> std::sync::MutexGuard<'_, Catalog> {
        self.catalog.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// `model/list` and the user's config from a short-lived app-server.
/// A thread's conversation as Codex keeps it, read through `binary`'s
/// app-server (`thread/read`), for a thread Burrow did not record.
pub fn read_thread(binary: &Path, cwd: &str, thread: &str) -> Result<Vec<HistoryItem>, String> {
    let mut client = blocking::Client::start(binary, Some(Path::new(cwd)), PROBE_TIMEOUT)?;
    let read = client.call(
        "thread/read",
        json!({"threadId": thread, "includeTurns": true}),
        PROBE_TIMEOUT,
    )?;
    Ok(history::from_thread(
        &read,
        Some(cwd).filter(|c| !c.is_empty()),
    ))
}

/// Up to `limit` of Codex's threads on this host that ran in `cwd` (any
/// when none), the most recently used first, through `binary`'s app-server
/// (`thread/list`, by `updated_at`, page by page), as Paseo's
/// `readCodexThreadWindow` lists them. A directory is asked of Codex, but one
/// that ignores the key lists every thread, so a wider window is read for it
/// and its rows are kept by their own directory.
pub fn recent_threads(
    binary: &Path,
    cwd: Option<&str>,
    limit: usize,
) -> Result<Vec<burrow_provider::RecentSession>, String> {
    let mut client = blocking::Client::start(binary, None, PROBE_TIMEOUT)?;
    let window = if cwd.is_some() { limit.max(50) } else { limit };
    let mut seen = std::collections::HashSet::new();
    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    while sessions.len() < window {
        let mut params = json!({"limit": window - sessions.len(), "sortKey": "updated_at"});
        if let Some(cwd) = cwd {
            params["cwd"] = cwd.into();
        }
        if let Some(cursor) = &cursor {
            params["cursor"] = cursor.clone().into();
        }
        let page = client.call("thread/list", params, PROBE_TIMEOUT)?;
        let before = sessions.len();
        for thread in page["data"].as_array().into_iter().flatten() {
            let new = (thread["id"].as_str()).is_some_and(|id| seen.insert(id.to_string()));
            if let Some(session) = session_of(thread).filter(|_| new) {
                sessions.push(session);
            }
        }
        cursor = page["nextCursor"].as_str().map(str::to_string);
        // A page with nothing new would let a stuck cursor page for ever.
        if cursor.is_none() || sessions.len() == before {
            break;
        }
    }
    if let Some(cwd) = cwd {
        sessions.retain(|session| same_dir(&session.cwd, cwd));
    }
    sessions.truncate(limit);
    Ok(sessions)
}

/// A `thread/list` row as a session. Its name is the thread's, else its
/// preview, which is also the only prompt the row has. A row that does not
/// say its directory is not one.
fn session_of(thread: &Value) -> Option<burrow_provider::RecentSession> {
    let id = thread["id"].as_str()?;
    let cwd = thread["cwd"].as_str().filter(|c| !c.is_empty())?;
    let preview = thread["preview"].as_str().map(str::to_string);
    let name = thread["name"].as_str().filter(|n| !n.trim().is_empty());
    Some(burrow_provider::RecentSession {
        id: id.to_string(),
        cwd: cwd.to_string(),
        title: name.map(str::to_string).or_else(|| preview.clone()),
        first_prompt: preview.clone(),
        last_prompt: preview,
        last_activity_ms: thread_time(&thread["updatedAt"])
            .or_else(|| thread_time(&thread["createdAt"]))
            .unwrap_or(0),
    })
}

/// Whether two paths name one directory, symlinks resolved where they exist.
fn same_dir(a: &str, b: &str) -> bool {
    a == b
        || matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// A thread's time as Codex sends it, seconds or milliseconds since the
/// epoch or an RFC 3339 string, in milliseconds.
fn thread_time(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => {
            let n = n.as_f64()?;
            Some(if n < 1e12 {
                (n * 1000.) as i64
            } else {
                n as i64
            })
        }
        Value::String(text) => {
            time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                .ok()
                .map(|at| (at.unix_timestamp_nanos() / 1_000_000) as i64)
        }
        _ => None,
    }
}

fn probe(binary: &Path) -> Result<Catalog, String> {
    let mut client = blocking::Client::start(binary, None, PROBE_TIMEOUT)?;
    let models = client.call("model/list", json!({}), PROBE_TIMEOUT)?;
    let config = client
        .call("config/read", json!({}), PROBE_TIMEOUT)
        .unwrap_or(Value::Null);
    Ok(Catalog::from_responses(&models, &config))
}

/// What the provider offers for a Codex of `version` with `catalog`.
pub fn info(version: Option<&[u32]>, catalog: &Catalog) -> ProviderInfo {
    let auto_review = catalog::auto_review_available(version);
    ProviderInfo {
        id: "codex".into(),
        label: "Codex".into(),
        description: Some(
            "OpenAI's Codex workspace agent with sandbox controls and optional network access"
                .into(),
        ),
        modes: catalog::modes(auto_review),
        default_mode_id: Some(catalog::default_mode(auto_review).into()),
        models: catalog.models(),
        capabilities: Capabilities {
            streaming: true,
            session_persistence: true,
            dynamic_modes: false,
            // A client's `mcpServers` are not passed to app-server yet.
            mcp_servers: false,
            reasoning_stream: true,
            tool_invocations: true,
            steer: true,
        },
    }
}

impl Provider for CodexProvider {
    fn info(&self) -> ProviderInfo {
        info(self.version.as_deref(), &self.catalog())
    }

    fn cli(&self) -> Option<burrow_provider::CliInfo> {
        Some(burrow_provider::CliInfo::of(&self.binary, self.version()))
    }

    fn start(&self, spec: SessionSpec) -> Result<ProviderSession, ProviderError> {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let settings = Settings {
            cwd: spec.cwd.clone(),
            model: spec.model.clone(),
            mode: spec.mode_id.clone(),
            thinking: spec.thinking_option_id.clone(),
            system_prompt: spec.system_prompt.clone(),
            mcp_servers: spec.mcp_servers.clone(),
        };
        let catalog = self.catalog.clone();
        let resume = spec.resume.clone();
        match &self.launch {
            Some(launch) => {
                let launcher = FnLauncher(launch.clone());
                let session = Session::new(launcher, settings, resume, catalog, events);
                tokio::spawn(session.run(command_rx));
            }
            None => {
                let launcher = CliLauncher {
                    binary: self.binary.clone(),
                    cwd: PathBuf::from(&spec.cwd),
                    env: spec.env.clone(),
                    unset: spec.unset.clone(),
                    trace: self.trace_dir.as_ref().map(|dir| {
                        let name = format!("codex-{}.jsonl", uuid::Uuid::new_v4().simple());
                        dir.join(name)
                    }),
                };
                let session = Session::new(launcher, settings, resume, catalog, events);
                tokio::spawn(session.run(command_rx));
            }
        }
        Ok(ProviderSession {
            commands,
            events: event_rx,
        })
    }

    fn history(&self, cwd: &str, session_id: &str) -> Vec<HistoryItem> {
        read_thread(&self.binary, cwd, session_id).unwrap_or_else(|err| {
            tracing::warn!(%err, session_id, "could not read the Codex thread");
            Vec::new()
        })
    }

    fn recent_sessions(&self, limit: usize) -> Result<Vec<burrow_provider::RecentSession>, String> {
        recent_threads(&self.binary, None, limit)
    }

    fn imports_sessions(&self) -> bool {
        true
    }

    fn importable_sessions(
        &self,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<burrow_provider::RecentSession>, String> {
        recent_threads(&self.binary, cwd, limit)
    }

    /// Paseo's Codex numbers its own turns.
    fn foreground_turn_id(&self, ordinal: u64) -> String {
        format!("codex-turn-{ordinal}")
    }

    fn model_fields(&self, model_id: &str) -> Map<String, Value> {
        self.catalog().fields(model_id)
    }
}

struct FnLauncher(Launch);

impl Launcher for FnLauncher {
    fn launch(&mut self) -> std::io::Result<Transport> {
        (self.0)()
    }
}

struct CliLauncher {
    binary: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    unset: Vec<String>,
    trace: Option<PathBuf>,
}

impl Launcher for CliLauncher {
    fn launch(&mut self) -> std::io::Result<Transport> {
        let mut command = tokio::process::Command::new(&self.binary);
        command.arg("app-server");
        if self.cwd.is_dir() {
            command.current_dir(&self.cwd);
        }
        for key in &self.unset {
            command.env_remove(key);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let process = process::spawn(command, self.trace.as_deref())?;
        Ok(Transport {
            input: process.input,
            output: process.output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_is_a_session_named_by_its_name_else_its_preview() {
        let named = session_of(&json!({
            "id": "t1", "cwd": "/work/app", "name": "Parser work", "preview": "fix the parser",
            "createdAt": 100, "updatedAt": 200,
        }))
        .unwrap();
        assert_eq!(named.id, "t1");
        assert_eq!(named.cwd, "/work/app");
        assert_eq!(named.title.as_deref(), Some("Parser work"));
        assert_eq!(named.first_prompt.as_deref(), Some("fix the parser"));
        assert_eq!(named.last_prompt.as_deref(), Some("fix the parser"));
        assert_eq!(named.last_activity_ms, 200_000);

        // A blank name is no name; with no update time, the creation time.
        let plain = session_of(&json!({
            "id": "t2", "cwd": "/work/app", "name": "  ", "preview": "add a test",
            "createdAt": 100,
        }))
        .unwrap();
        assert_eq!(plain.title.as_deref(), Some("add a test"));
        assert_eq!(plain.last_activity_ms, 100_000);

        // Nothing to show it by, and no directory.
        let bare = session_of(&json!({"id": "t3", "cwd": "/work/app"})).unwrap();
        assert_eq!(
            (bare.title, bare.first_prompt, bare.last_prompt),
            (None, None, None)
        );
        assert!(session_of(&json!({"id": "t4", "preview": "x"})).is_none());
        assert!(session_of(&json!({"id": "t5", "cwd": "", "preview": "x"})).is_none());
    }

    /// A `codex` whose app-server answers `initialize` and gives each
    /// `thread/list` the next of `pages`, keeping the requests it was sent.
    #[cfg(unix)]
    fn fake_codex(name: &str, pages: &[Value]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("burrow-fake-codex-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (n, page) in pages.iter().enumerate() {
            std::fs::write(dir.join(format!("page{n}.json")), page.to_string()).unwrap();
        }
        // Requests are one line each, their `id` first.
        let script = r#"#!/bin/sh
dir="$(dirname "$0")"
lists=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id" ;;
    *'"method":"thread/list"'*)
      printf '%s\n' "$line" >> "$dir/requests"
      printf '{"id":%s,"result":%s}\n' "$id" "$(cat "$dir/page$lists.json")"
      lists=$((lists + 1)) ;;
  esac
done
"#;
        let binary = dir.join("codex");
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        binary
    }

    #[cfg(unix)]
    fn requests(binary: &Path) -> Vec<Value> {
        let text = std::fs::read_to_string(binary.with_file_name("requests")).unwrap();
        (text.lines())
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["params"].clone())
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_directorys_threads_are_asked_for_by_it_and_kept_by_it() {
        let pages = [
            json!({"data": [
                {"id": "t1", "cwd": "/work/app", "name": "Renamed", "preview": "one", "updatedAt": 300},
                {"id": "t2", "cwd": "/work/other", "preview": "two", "updatedAt": 250},
                {"id": "t3", "preview": "no directory", "updatedAt": 240},
            ], "nextCursor": "c2"}),
            json!({"data": [
                {"id": "t1", "cwd": "/work/app", "preview": "one again", "updatedAt": 300},
                {"id": "t4", "cwd": "/work/app", "preview": "four", "updatedAt": 200},
            ]}),
        ];
        let binary = fake_codex("scoped", &pages);

        let found = recent_threads(&binary, Some("/work/app"), 2).unwrap();
        let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            ["t1", "t4"],
            "the other directory's thread is left out"
        );
        assert_eq!(found[0].title.as_deref(), Some("Renamed"));
        assert_eq!(
            found[0].first_prompt.as_deref(),
            Some("one"),
            "t1 only once"
        );
        let sent = requests(&binary);
        // A wide window, whatever the limit, since Codex may not narrow it.
        assert_eq!(
            sent[0],
            json!({"limit": 50, "sortKey": "updated_at", "cwd": "/work/app"})
        );
        assert_eq!(sent[1]["cursor"], "c2");
        assert_eq!(sent[1]["cwd"], "/work/app");
        assert_eq!(sent.len(), 2);
        let _ = std::fs::remove_dir_all(binary.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn every_directorys_threads_are_listed_when_none_is_asked_for() {
        let pages = [json!({"data": [
            {"id": "t1", "cwd": "/work/app", "preview": "one", "updatedAt": 300},
            {"id": "t2", "cwd": "/work/other", "preview": "two", "updatedAt": 250},
            {"id": "t3", "cwd": "/work/more", "preview": "three", "updatedAt": 200},
        ]})];
        let binary = fake_codex("all", &pages);

        let found = recent_threads(&binary, None, 2).unwrap();
        let cwds: Vec<&str> = found.iter().map(|s| s.cwd.as_str()).collect();
        assert_eq!(cwds, ["/work/app", "/work/other"]);
        assert_eq!(
            requests(&binary),
            [json!({"limit": 2, "sortKey": "updated_at"})]
        );
        let _ = std::fs::remove_dir_all(binary.parent().unwrap());
    }
}
