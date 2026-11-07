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
//! off the lock. JSON records are written to a temporary file and renamed;
//! a record that fails to save is kept and tried again on every flush, and
//! a flush reports what is still not on disk. Closing the [`Writer`] stops
//! the thread, so a daemon that has closed saves nothing more: a task it
//! left running cannot overwrite what the next daemon on the home saves.
//! Timelines are append-only: rows are immutable once they have a seq, so a
//! restart keeps every epoch and seq and clients' cursors stay valid. A torn
//! last line (a crash mid-write) is ignored on load.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use burrow_protocol::directory::AgentSnapshot;
use burrow_protocol::labels::LabelDefinition;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{error, warn};

use crate::store::{ProjectRecord, WorkspaceRecord};
use crate::timeline::Row;

const TIMELINE_FORMAT: u32 = 1;
/// Paseo's name for the label catalog (it keeps it under `projects/`).
const LABELS_FILE: &str = "workspace-labels.json";
/// Open timeline files kept for appending.
const OPEN_TIMELINES: usize = 64;
/// How long a save waits out a file Windows will not let it replace.
const SAVE_PATIENCE: Duration = Duration::from_secs(2);

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
    Rows {
        agent_id: String,
        rows: Vec<Row>,
    },
    NewTimeline {
        agent_id: String,
        epoch: String,
    },
    DeleteAgent(String),
    /// Finishes everything sent before it, tries failed saves again, and
    /// answers with what is still not saved.
    Flush(mpsc::Sender<Result<(), String>>),
    /// A flush that waits out locked files, after which the writer stops.
    Stop(mpsc::Sender<Result<(), String>>),
}

fn stopped() -> io::Error {
    io::Error::other("the persistence writer has stopped")
}

/// Sends `op` built around a reply channel and waits for the answer.
fn ask(
    tx: &mpsc::Sender<Op>,
    op: impl FnOnce(mpsc::Sender<Result<(), String>>) -> Op,
) -> io::Result<()> {
    let (reply, answer) = mpsc::channel();
    tx.send(op(reply)).map_err(|_| stopped())?;
    answer
        .recv()
        .map_err(|_| stopped())?
        .map_err(io::Error::other)
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

/// Owns the writer thread; closing or dropping it saves what was sent and
/// stops the thread. Changes sent after that are not saved.
pub struct Writer {
    handle: Option<JoinHandle<()>>,
    tx: mpsc::Sender<Op>,
}

impl Writer {
    /// Saves everything sent so far, then stops the writer. The error says
    /// what could not be saved.
    pub fn close(mut self) -> io::Result<()> {
        self.stop()
    }

    fn stop(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let outcome = ask(&self.tx, Op::Stop);
        let _ = handle.join();
        outcome
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Err(err) = self.stop() {
            error!(%err, "state was not saved");
        }
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
                tx: persist.tx,
            },
            loaded,
        ))
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    fn send(&self, op: Op) {
        if self.tx.send(op).is_err() {
            warn!("the persistence writer has stopped; a change was not saved");
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

    /// Waits until everything sent so far is written. The error says what
    /// is not on disk (saves that failed, now and before), or that the
    /// writer has stopped.
    pub fn flush(&self) -> io::Result<()> {
        ask(&self.tx, Op::Flush)
    }

    /// Reads an agent's timeline: its epoch and rows. `None` if there is none.
    pub fn load_timeline(&self, agent_id: &str) -> io::Result<Option<(String, Vec<Row>)>> {
        // Only for the order: what failed to save was logged when it did.
        let _ = self.flush();
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

fn write_atomic(path: &Path, bytes: &[u8], patience: Duration) -> io::Result<()> {
    let temp = path.with_extension("tmp");
    let attempt = || {
        {
            let mut file = File::create(&temp)?;
            file.write_all(bytes)?;
            file.sync_data()?;
        }
        fs::rename(&temp, path)
    };
    let mut waited = Duration::ZERO;
    let mut pause = Duration::from_millis(10);
    loop {
        match attempt() {
            Err(err) if locked(&err) && waited < patience => {
                std::thread::sleep(pause);
                waited += pause;
                pause = (pause * 2).min(Duration::from_millis(200));
            }
            outcome => return outcome,
        }
    }
}

/// Windows will not replace a file another process holds open without
/// `FILE_SHARE_DELETE`, as a virus scanner or indexer looking at what was
/// just written does; that passes.
fn locked(err: &io::Error) -> bool {
    // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION.
    cfg!(windows) && matches!(err.raw_os_error(), Some(5 | 32 | 33))
}

/// A file that is already gone is removed.
fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Records whose last save failed, by file: tried again on every flush,
/// until a save of the same file succeeds or the record is deleted.
type Unsaved = BTreeMap<PathBuf, Vec<u8>>;

fn write_json<T: Serialize>(unsaved: &mut Unsaved, path: PathBuf, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    unsaved.remove(&path);
    write_atomic(&path, &bytes, SAVE_PATIENCE).inspect_err(|_| {
        unsaved.insert(path, bytes);
    })
}

/// A flush: buffered rows go out, failed saves are tried again (waiting
/// `patience` for a locked file), and what is still not saved is reported.
fn settle(
    open: &mut HashMap<String, File>,
    unsaved: &mut Unsaved,
    failures: &mut Vec<String>,
    patience: Duration,
) -> Result<(), String> {
    for file in open.values_mut() {
        if let Err(err) = file.flush() {
            failures.push(format!("a timeline: {err}"));
        }
    }
    let mut problems = std::mem::take(failures);
    unsaved.retain(|path, bytes| match write_atomic(path, bytes, patience) {
        Ok(()) => false,
        Err(err) => {
            problems.push(format!("{}: {err}", path.display()));
            true
        }
    });
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("not saved: {}", problems.join("; ")))
    }
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
    let mut unsaved = Unsaved::new();
    // Failures since the last flush that are not tried again.
    let mut failures: Vec<String> = Vec::new();
    for op in rx {
        // A failed record save is kept in `unsaved`, so it is not a failure
        // until a flush cannot save it either.
        let (outcome, retried) = match op {
            Op::Projects(projects) => (
                write_json(&mut unsaved, home.join("projects.json"), &projects),
                true,
            ),
            Op::Workspaces(workspaces) => (
                write_json(&mut unsaved, home.join("workspaces.json"), &workspaces),
                true,
            ),
            Op::Labels(labels) => (
                write_json(&mut unsaved, home.join(LABELS_FILE), &labels),
                true,
            ),
            Op::Agent(record) => (
                write_json(
                    &mut unsaved,
                    agent_path(&home, &record.snapshot.id),
                    &record,
                ),
                true,
            ),
            Op::NewTimeline { agent_id, epoch } => {
                open.remove(&agent_id);
                let header = TimelineHeader {
                    format: TIMELINE_FORMAT,
                    epoch,
                };
                let written = serde_json::to_vec(&header)
                    .map_err(io::Error::other)
                    .and_then(|mut line| {
                        line.push(b'\n');
                        write_atomic(&timeline_path(&home, &agent_id), &line, SAVE_PATIENCE)
                    });
                (written, false)
            }
            Op::Rows { agent_id, rows } => (append_rows(&home, &mut open, &agent_id, &rows), false),
            Op::DeleteAgent(agent_id) => {
                open.remove(&agent_id);
                let path = agent_path(&home, &agent_id);
                // A deleted agent's failed save must not bring it back.
                unsaved.remove(&path);
                let removed = remove_file(&path).and(remove_file(&timeline_path(&home, &agent_id)));
                (removed, false)
            }
            Op::Flush(done) => {
                let _ = done.send(settle(
                    &mut open,
                    &mut unsaved,
                    &mut failures,
                    Duration::ZERO,
                ));
                continue;
            }
            Op::Stop(done) => {
                let _ = done.send(settle(
                    &mut open,
                    &mut unsaved,
                    &mut failures,
                    SAVE_PATIENCE,
                ));
                break;
            }
        };
        if let Err(err) = outcome {
            if retried {
                warn!(%err, "failed to save state; trying again on the next flush");
            } else {
                warn!(%err, "failed to save state");
                failures.push(err.to_string());
            }
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
        persist.flush().unwrap();
        writer.close().unwrap();
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
        persist.flush().unwrap();
        writer.close().unwrap();
        let loaded = load(&home).unwrap();
        assert_eq!(loaded.agents.len(), 1);
        assert_eq!(loaded.agents[0].snapshot, snapshot);
        assert_eq!(
            loaded.agents[0].provider_config,
            Some(json!({"systemPrompt": "be brief"}))
        );
        fs::remove_dir_all(&home).unwrap();
    }

    fn label(name: &str) -> LabelDefinition {
        serde_json::from_value(json!({"name": name, "color": "teal"})).unwrap()
    }

    fn saved_labels(home: &Path) -> Vec<LabelDefinition> {
        load(home).unwrap().labels
    }

    /// A daemon that closed keeps its store, and tasks it started can still
    /// change it; none of that may reach the disk the next daemon uses.
    #[test]
    fn a_closed_writer_saves_nothing_more() {
        let home = scratch();
        let (persist, writer, _) = Persist::open(&home).unwrap();
        persist.labels(vec![label("kept")]);
        writer.close().unwrap();
        assert_eq!(saved_labels(&home), [label("kept")]);

        persist.labels(vec![label("late")]);
        assert!(persist.flush().is_err());
        assert_eq!(saved_labels(&home), [label("kept")]);
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_failed_save_is_reported_and_tried_again() {
        let home = scratch();
        let (persist, writer, _) = Persist::open(&home).unwrap();
        // A directory where the file goes makes every save of it fail.
        let blocker = home.join(LABELS_FILE);
        fs::create_dir(&blocker).unwrap();
        persist.labels(vec![label("first")]);
        persist.labels(vec![label("second")]);
        let err = persist.flush().unwrap_err().to_string();
        assert!(err.contains(LABELS_FILE), "{err}");
        assert!(persist.flush().is_err(), "still not saved");

        fs::remove_dir(&blocker).unwrap();
        persist.flush().unwrap();
        // The latest version is the one saved.
        assert_eq!(saved_labels(&home), [label("second")]);
        writer.close().unwrap();
        fs::remove_dir_all(&home).unwrap();
    }

    /// Windows refuses to replace a file open without `FILE_SHARE_DELETE`;
    /// a save waits for it to be let go.
    #[cfg(windows)]
    #[test]
    fn a_save_waits_out_a_file_held_open() {
        use std::os::windows::fs::OpenOptionsExt;
        let home = scratch();
        let (persist, writer, _) = Persist::open(&home).unwrap();
        persist.labels(vec![label("first")]);
        persist.flush().unwrap();
        let held = OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ only
            .open(home.join(LABELS_FILE))
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        persist.labels(vec![label("second")]);
        persist.flush().unwrap();
        release.join().unwrap();
        assert_eq!(saved_labels(&home), [label("second")]);
        writer.close().unwrap();
        fs::remove_dir_all(&home).unwrap();
    }
}
