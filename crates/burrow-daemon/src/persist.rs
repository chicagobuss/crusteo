//! Durable state under Burrow's home directory (never `~/.paseo`).
//!
//! ```text
//! <home>/server-id                 daemon identity
//! <home>/projects.json             project records
//! <home>/workspaces.json           workspace records, with their label assignments
//! <home>/workspace-labels.json     the label catalog, `[{name, color}]` as in Paseo
//! <home>/agents/<agentId>.json     one agent record each (active and archived)
//! <home>/timelines/<agentId>.jsonl a header line with the epoch, then one row per line
//! ```
//!
//! The store sends operations while it holds its lock, so they reach the
//! writer thread in the order the changes happened; the thread does the I/O
//! off the lock. JSON records are written to a temporary file and renamed.
//! Timelines are append-only: rows are immutable once they have a seq, so a
//! restart keeps every epoch and seq and clients' cursors stay valid. A torn
//! last line (a crash mid-write) is ignored on load.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use burrow_protocol::directory::AgentSnapshot;
use burrow_protocol::labels::LabelDefinition;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::store::{ProjectRecord, WorkspaceRecord};
use crate::timeline::Row;

const TIMELINE_FORMAT: u32 = 1;
/// Paseo's name for the label catalog (it keeps it under `projects/`).
const LABELS_FILE: &str = "workspace-labels.json";
/// Open timeline files kept for appending.
const OPEN_TIMELINES: usize = 64;

/// One agent's record: its snapshot, which carries the provider's resume
/// handle in `persistence`, plus what Burrow needs to restart it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRecord {
    #[serde(flatten)]
    pub snapshot: AgentSnapshot,
    /// The status before the daemon stopped; the snapshot shows `closed`
    /// until the agent is loaded again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<burrow_protocol::directory::AgentStatus>,
    /// Provider settings the agent was created with (system prompt and the like).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_config: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TimelineHeader {
    format: u32,
    epoch: String,
}

enum Op {
    Projects(Vec<ProjectRecord>),
    Workspaces(Vec<WorkspaceRecord>),
    Labels(Vec<LabelDefinition>),
    Agent(Box<AgentRecord>),
    Rows { agent_id: String, rows: Vec<Row> },
    NewTimeline { agent_id: String, epoch: String },
    DeleteAgent(String),
    Flush(mpsc::Sender<()>),
}

/// The handle the store holds. Cloning shares the writer.
#[derive(Clone)]
pub struct Persist {
    home: PathBuf,
    tx: mpsc::Sender<Op>,
}

impl std::fmt::Debug for Persist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Persist").field("home", &self.home).finish()
    }
}

/// Owns the writer thread; dropping it flushes and joins.
pub struct Writer {
    handle: Option<JoinHandle<()>>,
    persist: Persist,
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.persist.flush();
        // The thread ends when every sender is gone; the store may still hold
        // one, so do not wait for that here.
        let _ = self.handle.take();
    }
}

/// What was on disk at startup.
#[derive(Debug, Default)]
pub struct Loaded {
    pub projects: Vec<ProjectRecord>,
    pub workspaces: Vec<WorkspaceRecord>,
    pub labels: Vec<LabelDefinition>,
    pub agents: Vec<AgentRecord>,
}

impl Persist {
    pub fn open(home: &Path) -> io::Result<(Persist, Writer, Loaded)> {
        fs::create_dir_all(home.join("agents"))?;
        fs::create_dir_all(home.join("timelines"))?;
        let loaded = load(home)?;
        let (tx, rx) = mpsc::channel();
        let persist = Persist {
            home: home.to_path_buf(),
            tx,
        };
        let root = home.to_path_buf();
        let handle = std::thread::Builder::new()
            .name("burrow-persist".into())
            .spawn(move || run(root, rx))?;
        Ok((
            persist.clone(),
            Writer {
                handle: Some(handle),
                persist,
            },
            loaded,
        ))
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    fn send(&self, op: Op) {
        if self.tx.send(op).is_err() {
            warn!("persistence writer has stopped; a change was not saved");
        }
    }

    pub fn projects(&self, projects: Vec<ProjectRecord>) {
        self.send(Op::Projects(projects));
    }

    pub fn workspaces(&self, workspaces: Vec<WorkspaceRecord>) {
        self.send(Op::Workspaces(workspaces));
    }

    pub fn labels(&self, labels: Vec<LabelDefinition>) {
        self.send(Op::Labels(labels));
    }

    pub fn agent(&self, record: AgentRecord) {
        self.send(Op::Agent(Box::new(record)));
    }

    /// Starts a timeline file over with a new epoch.
    pub fn new_timeline(&self, agent_id: &str, epoch: &str) {
        self.send(Op::NewTimeline {
            agent_id: agent_id.into(),
            epoch: epoch.into(),
        });
    }

    pub fn rows(&self, agent_id: &str, rows: Vec<Row>) {
        self.send(Op::Rows {
            agent_id: agent_id.into(),
            rows,
        });
    }

    /// Removes an agent's record and timeline for good.
    pub fn delete_agent(&self, agent_id: &str) {
        self.send(Op::DeleteAgent(agent_id.into()));
    }

    /// Waits until everything sent so far is written.
    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        self.send(Op::Flush(tx));
        let _ = rx.recv();
    }

    /// Reads an agent's timeline: its epoch and rows. `None` if there is none.
    pub fn load_timeline(&self, agent_id: &str) -> io::Result<Option<(String, Vec<Row>)>> {
        self.flush();
        load_timeline(&timeline_path(&self.home, agent_id))
    }
}

fn timeline_path(home: &Path, agent_id: &str) -> PathBuf {
    home.join("timelines")
        .join(format!("{}.jsonl", file_safe(agent_id)))
}

fn agent_path(home: &Path, agent_id: &str) -> PathBuf {
    home.join("agents")
        .join(format!("{}.json", file_safe(agent_id)))
}

/// Ids are UUIDs, but never let one name a path outside its directory.
fn file_safe(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = path.with_extension("tmp");
    {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_data()?;
    }
    fs::rename(&temp, path)
}

/// A file that is already gone is removed.
fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_atomic(path, &bytes)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Reads what is saved in `home` without creating or changing anything. A
/// home that does not exist yet reads as empty.
pub fn read(home: &Path) -> io::Result<Loaded> {
    load(home)
}

fn load(home: &Path) -> io::Result<Loaded> {
    let projects = read_json(&home.join("projects.json"))?.unwrap_or_default();
    let workspaces = read_json(&home.join("workspaces.json"))?.unwrap_or_default();
    let labels = read_json(&home.join(LABELS_FILE))?.unwrap_or_default();
    let mut agents = Vec::new();
    let entries = match fs::read_dir(home.join("agents")) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Loaded {
                projects,
                workspaces,
                labels,
                agents,
            });
        }
        Err(err) => return Err(err),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match read_json::<AgentRecord>(&path) {
            Ok(Some(record)) => agents.push(record),
            Ok(None) => {}
            Err(err) => warn!(path = %path.display(), %err, "skipping an unreadable agent record"),
        }
    }
    agents.sort_by(|a, b| a.snapshot.created_at.cmp(&b.snapshot.created_at));
    Ok(Loaded {
        projects,
        workspaces,
        labels,
        agents,
    })
}

fn load_timeline(path: &Path) -> io::Result<Option<(String, Vec<Row>)>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut lines = BufReader::new(file).lines();
    let Some(header) = lines.next() else {
        return Ok(None);
    };
    let header: TimelineHeader = serde_json::from_str(&header?)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if header.format != TIMELINE_FORMAT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("timeline format {} is not supported", header.format),
        ));
    }
    let mut rows: Vec<Row> = Vec::new();
    for line in lines {
        let line = line?;
        match serde_json::from_str::<Row>(&line) {
            // Seqs must grow by one; anything else is a torn or foreign line.
            Ok(row) if rows.last().is_none_or(|last| row.seq == last.seq + 1) => rows.push(row),
            _ => {
                warn!(path = %path.display(), "ignoring the rest of a damaged timeline");
                break;
            }
        }
    }
    Ok(Some((header.epoch, rows)))
}

fn run(home: PathBuf, rx: mpsc::Receiver<Op>) {
    let mut open: HashMap<String, File> = HashMap::new();
    for op in rx {
        let outcome = match op {
            Op::Projects(projects) => write_json(&home.join("projects.json"), &projects),
            Op::Workspaces(workspaces) => write_json(&home.join("workspaces.json"), &workspaces),
            Op::Labels(labels) => write_json(&home.join(LABELS_FILE), &labels),
            Op::Agent(record) => write_json(&agent_path(&home, &record.snapshot.id), &record),
            Op::NewTimeline { agent_id, epoch } => {
                open.remove(&agent_id);
                let header = TimelineHeader {
                    format: TIMELINE_FORMAT,
                    epoch,
                };
                serde_json::to_vec(&header)
                    .map_err(io::Error::other)
                    .and_then(|mut line| {
                        line.push(b'\n');
                        write_atomic(&timeline_path(&home, &agent_id), &line)
                    })
            }
            Op::Rows { agent_id, rows } => append_rows(&home, &mut open, &agent_id, &rows),
            Op::DeleteAgent(agent_id) => {
                open.remove(&agent_id);
                remove_file(&agent_path(&home, &agent_id))
                    .and(remove_file(&timeline_path(&home, &agent_id)))
            }
            Op::Flush(done) => {
                for file in open.values_mut() {
                    let _ = file.flush();
                }
                let _ = done.send(());
                Ok(())
            }
        };
        if let Err(err) = outcome {
            warn!(%err, "failed to save state");
        }
    }
}

fn append_rows(
    home: &Path,
    open: &mut HashMap<String, File>,
    agent_id: &str,
    rows: &[Row],
) -> io::Result<()> {
    if !open.contains_key(agent_id) {
        if open.len() >= OPEN_TIMELINES {
            open.clear();
        }
        let file = OpenOptions::new()
            .append(true)
            .open(timeline_path(home, agent_id))?;
        open.insert(agent_id.to_string(), file);
    }
    let file = open.get_mut(agent_id).expect("inserted above");
    let mut buffer = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut buffer, row).map_err(io::Error::other)?;
        buffer.push(b'\n');
    }
    file.write_all(&buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("burrow-persist-{}", crate::ids::agent_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn row(seq: u64) -> Row {
        Row {
            seq,
            provider: "echo".into(),
            item: json!({"type": "assistant_message", "text": seq.to_string()}),
            turn_id: Some("t1".into()),
            timestamp: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn a_timeline_keeps_its_epoch_and_seqs_and_survives_a_torn_line() {
        let home = scratch();
        let (persist, writer, _) = Persist::open(&home).unwrap();
        persist.new_timeline("a1", "epoch-1");
        persist.rows("a1", vec![row(1), row(2)]);
        persist.rows("a1", vec![row(3)]);
        persist.flush();
        drop(writer);
        // A crash mid-write leaves half a line.
        let path = timeline_path(&home, "a1");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"seq\":4,\"prov").unwrap();
        drop(file);

        let (epoch, rows) = load_timeline(&path).unwrap().unwrap();
        assert_eq!(epoch, "epoch-1");
        assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2, 3]);
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn records_round_trip() {
        let home = scratch();
        let (persist, writer, loaded) = Persist::open(&home).unwrap();
        assert!(loaded.agents.is_empty());
        let snapshot = crate::seed::agent("a1", "/tmp/x", Some("wks_1"), "Title");
        persist.agent(AgentRecord {
            snapshot: snapshot.clone(),
            last_status: None,
            provider_config: Some(json!({"systemPrompt": "be brief"})),
        });
        persist.flush();
        drop(writer);
        let loaded = load(&home).unwrap();
        assert_eq!(loaded.agents.len(), 1);
        assert_eq!(loaded.agents[0].snapshot, snapshot);
        assert_eq!(
            loaded.agents[0].provider_config,
            Some(json!({"systemPrompt": "be brief"}))
        );
        fs::remove_dir_all(&home).unwrap();
    }
}
