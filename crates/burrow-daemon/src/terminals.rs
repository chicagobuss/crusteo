//! Workspace terminals, as Paseo's daemon serves them: a PTY per terminal
//! on this host, which any client can list, watch, attach to, type into,
//! resize, rename, read back and kill (Paseo 0.15's `*_terminal*` requests).
//!
//! - **A terminal** belongs to a workspace and runs in a folder: the user's
//!   shell, or a command it was made with. It lives until its program exits
//!   or it is killed, or until Burrow stops: Burrow does not keep terminals
//!   across a restart (it does not advertise `terminalsSurviveRestart`).
//! - **Its screen** is kept by crusteo's own emulator (`term`), which also
//!   answers what the program asks of the terminal (cursor position, device
//!   attributes, colours), as Paseo's daemon does, since clients do not.
//! - **Attaching** (`subscribe_terminal_request`) gives the socket a slot.
//!   The reply goes first, then the screen as it is (one Restore frame, or
//!   replay chunks for a client that takes them), then the program's
//!   output as it comes, in Output frames on that slot, a few milliseconds
//!   of it at a time. A socket that falls behind gets a fresh screen once
//!   it has caught up, rather than everything it missed.
//! - **Watching** (`subscribe_terminals_request`) gets the folder's or the
//!   workspace's whole terminal list whenever one starts, ends or is renamed.
//! - **Size:** a client claims the terminal's size, and only the one that
//!   claimed it resizes it with an update (Paseo's size ownership).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use burrow_protocol::envelope::session;
use burrow_protocol::terminal::{
    OPCODE_OUTPUT, OPCODE_REPLAY_BEGIN, OPCODE_REPLAY_DATA, OPCODE_REPLAY_END, OPCODE_RESTORE,
    REPLAY_CHUNK, decode_resize, encode_raw,
};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::agent_requests::{ClientShape, Late};
use crate::connection::Frames;
use crate::daemon::Daemon;
use crate::subscriptions::Subscriptions;

/// The session messages this module answers.
pub const KINDS: &[&str] = &[
    "list_terminals_request",
    "subscribe_terminals_request",
    "unsubscribe_terminals_request",
    "create_terminal_request",
    "subscribe_terminal_request",
    "unsubscribe_terminal_request",
    "terminal_input",
    "kill_terminal_request",
    "capture_terminal_request",
    "terminal.rename.request",
];

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
/// Output is gathered this long before it is sent, so a burst goes as one
/// frame (Paseo's coalescer).
const COALESCE: Duration = Duration::from_millis(5);
/// History sent with the visible screen, unless asked otherwise, and at
/// most, to a client that takes it in one frame.
const VISIBLE_HISTORY: usize = 200;
const VISIBLE_HISTORY_MAX: usize = 500;
/// All of it: the emulator keeps this many lines.
const FULL_HISTORY: usize = 10_000;
/// A socket with this much waiting stops getting output, and gets the
/// screen afresh once it is down to [`CAUGHT_UP`].
const BEHIND: usize = 2 * 1024 * 1024;
const CAUGHT_UP: usize = 256 * 1024;
/// The most output in one Output frame.
const OUTPUT_CHUNK: usize = 64 * 1024;
const TITLE_MAX: usize = 200;

/// What the program printed, numbered, or that it ended.
enum Chunk {
    Output(u64, Vec<u8>),
    Exit,
}

struct Screen {
    term: term::Terminal,
    /// The number of the last output fed in.
    revision: u64,
    /// The streams that hear output: their key, and where it goes.
    listeners: Vec<(u64, mpsc::UnboundedSender<Chunk>)>,
    /// What the terminal is called now: one set by a rename stays.
    title: Option<String>,
    renamed: bool,
    rows: u16,
    cols: u16,
    /// The connection whose client last claimed the size.
    owner: Option<u64>,
}

pub struct Pty {
    id: String,
    name: String,
    cwd: String,
    workspace_id: String,
    screen: Mutex<Screen>,
    /// What goes to the program, for its writer thread ([`write_input`]).
    input: std::sync::mpsc::Sender<Vec<u8>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

impl Pty {
    fn screen(&self) -> MutexGuard<'_, Screen> {
        self.screen.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Paseo's `TerminalInfo`, with or without its folder.
    fn info(&self, with_cwd: bool) -> Value {
        let mut info = json!({"id": self.id, "name": self.name, "workspaceId": self.workspace_id,
                              "activity": null});
        if with_cwd {
            info["cwd"] = self.cwd.clone().into();
        }
        if let Some(title) = self.screen().title.clone() {
            info["title"] = title.into();
        }
        info
    }

    /// Queues `bytes` for the program. Never waits: a program that stops
    /// reading its input holds up only its writer thread.
    fn write(&self, bytes: &[u8]) {
        let _ = self.input.send(bytes.to_vec());
    }

    /// A resize from `connection`: a claim (or one with no intent) takes the
    /// size; an update counts only from the connection that holds it.
    fn resize(&self, connection: u64, rows: u32, cols: u32, claim: bool) {
        let (rows, cols) = (clamp_size(rows), clamp_size(cols));
        let mut screen = self.screen();
        if claim {
            screen.owner = Some(connection);
        } else if screen.owner != Some(connection) {
            return;
        }
        if (screen.rows, screen.cols) == (rows, cols) {
            return;
        }
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let resized = self
            .master
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .resize(size);
        if let Err(error) = resized {
            warn!(terminal = self.id, %error, "could not resize the terminal");
            return;
        }
        screen.term.resize(rows as usize, cols as usize);
        (screen.rows, screen.cols) = (rows, cols);
    }

    fn kill(&self) {
        let killed = self.killer.lock().unwrap_or_else(|p| p.into_inner()).kill();
        if let Err(error) = killed {
            warn!(terminal = self.id, %error, "could not kill the terminal");
        }
    }
}

fn clamp_size(n: u32) -> u16 {
    n.clamp(1, 1000) as u16
}

/// A client watching a folder's or a workspace's terminals.
struct Watcher {
    connection: u64,
    id: String,
    cwd: String,
    workspace_id: Option<String>,
    late: Late,
}

/// An attached client: its slot on its socket and the task that feeds it.
struct Stream {
    subscription_id: String,
    terminal_id: String,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct Slots {
    next: u16,
    active: HashMap<u8, Stream>,
}

#[derive(Default)]
struct Inner {
    terminals: Vec<Arc<Pty>>,
    watchers: Vec<Watcher>,
    streams: HashMap<u64, Slots>,
    next_listener: u64,
}

#[derive(Default)]
pub struct Terminals {
    inner: Mutex<Inner>,
}

impl Terminals {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn get(&self, id: &str) -> Option<Arc<Pty>> {
        (self.lock().terminals.iter()).find(|t| t.id == id).cloned()
    }

    /// How many terminals run.
    pub fn count(&self) -> usize {
        self.lock().terminals.len()
    }

    /// The terminals a list asks for: a workspace's, else those at or
    /// under a folder, else all.
    fn listed(&self, cwd: Option<&str>, workspace_id: Option<&str>) -> Vec<Arc<Pty>> {
        (self.lock().terminals.iter())
            .filter(|t| match (workspace_id, cwd) {
                (Some(workspace), _) => t.workspace_id == workspace,
                (None, Some(cwd)) => within(&t.cwd, cwd),
                (None, None) => true,
            })
            .cloned()
            .collect()
    }

    /// Tells every watcher whose list `pty` is in (or was) its list afresh.
    fn changed(&self, pty: &Pty) {
        let watchers: Vec<(String, String, Option<String>, Late)> = (self.lock().watchers.iter())
            .filter(|w| match &w.workspace_id {
                Some(workspace) => *workspace == pty.workspace_id,
                None => within(&pty.cwd, &w.cwd),
            })
            .map(|w| {
                (
                    w.id.clone(),
                    w.cwd.clone(),
                    w.workspace_id.clone(),
                    w.late.clone(),
                )
            })
            .collect();
        for (id, cwd, workspace_id, late) in watchers {
            let payload = self.list_payload(&id, &cwd, workspace_id.as_deref());
            let _ = late.send(session("terminals_changed", payload));
        }
    }

    fn list_payload(&self, subscription: &str, cwd: &str, workspace_id: Option<&str>) -> Value {
        let terminals: Vec<Value> = (self.listed(Some(cwd), workspace_id).iter())
            .map(|t| t.info(false))
            .collect();
        let mut payload =
            json!({"subscriptionId": subscription, "cwd": cwd, "terminals": terminals});
        if let Some(workspace) = workspace_id {
            payload["workspaceId"] = workspace.into();
        }
        payload
    }

    /// The terminal ended: out of the list, and its watchers told.
    fn ended(&self, pty: &Arc<Pty>) {
        self.lock().terminals.retain(|t| !Arc::ptr_eq(t, pty));
        info!(terminal = pty.id, "terminal ended");
        self.changed(pty);
    }

    /// A socket closed: what it watched and attached goes with it, and the
    /// sizes it held are free for the next client to set.
    pub fn gone(&self, connection: u64) {
        let mut inner = self.lock();
        inner.watchers.retain(|w| w.connection != connection);
        if let Some(slots) = inner.streams.remove(&connection) {
            for stream in slots.active.into_values() {
                stream.task.abort();
            }
        }
        for pty in &inner.terminals {
            let mut screen = pty.screen();
            if screen.owner == Some(connection) {
                screen.owner = None;
            }
        }
    }

    /// A subscription released by id: a stream or a watcher of this
    /// socket's. Whether there was one.
    pub fn release(&self, connection: u64, subscription_id: &str) -> bool {
        let mut inner = self.lock();
        let before = inner.watchers.len();
        inner
            .watchers
            .retain(|w| !(w.connection == connection && w.id == subscription_id));
        let mut found = inner.watchers.len() != before;
        if let Some(slots) = inner.streams.get_mut(&connection) {
            let slot = (slots.active.iter())
                .find(|(_, s)| s.subscription_id == subscription_id)
                .map(|(slot, _)| *slot);
            if let Some(stream) = slot.and_then(|slot| slots.active.remove(&slot)) {
                stream.task.abort();
                found = true;
            }
        }
        found
    }

    /// The stream on `slot` ended by itself: its slot is free.
    fn stream_ended(&self, connection: u64, slot: u8) {
        if let Some(slots) = self.lock().streams.get_mut(&connection) {
            slots.active.remove(&slot);
        }
    }

    /// The terminal attached on this socket's `slot`.
    fn on_slot(&self, connection: u64, slot: u8) -> Option<Arc<Pty>> {
        let id = {
            let inner = self.lock();
            let stream = inner.streams.get(&connection)?.active.get(&slot)?;
            stream.terminal_id.clone()
        };
        self.get(&id)
    }

    /// An Input (0x02) or Resize (0x03) frame from a client, on a slot it
    /// holds. Anything else is dropped.
    pub fn binary(&self, connection: u64, bytes: &[u8]) {
        let (Some(&opcode), Some(&slot)) = (bytes.first(), bytes.get(1)) else {
            return;
        };
        let Some(pty) = self.on_slot(connection, slot) else {
            return;
        };
        match opcode {
            0x02 => pty.write(&bytes[2..]),
            0x03 => {
                if let Some(size) = decode_resize(&bytes[2..]) {
                    let claim =
                        size.intent != Some(burrow_protocol::terminal::ResizeIntent::Update);
                    pty.resize(connection, size.rows, size.cols, claim);
                }
            }
            _ => {}
        }
    }

    /// Kills every terminal of a workspace (it was archived).
    pub fn kill_workspace(&self, workspace_id: &str) {
        let ptys: Vec<Arc<Pty>> = (self.lock().terminals.iter())
            .filter(|t| t.workspace_id == workspace_id)
            .cloned()
            .collect();
        for pty in ptys {
            pty.kill();
        }
    }

    /// Kills every terminal: Burrow is stopping.
    pub fn kill_all(&self) {
        let ptys: Vec<Arc<Pty>> = self.lock().terminals.clone();
        for pty in ptys {
            pty.kill();
        }
    }
}

/// Whether `path` is `root` or under it.
fn within(path: &str, root: &str) -> bool {
    Path::new(path).starts_with(root)
}

/// Answers one of [`KINDS`]; None when it does not parse.
pub fn handle(
    daemon: &Arc<Daemon>,
    subs: &mut Subscriptions,
    shape: ClientShape,
    late: &Late,
    kind: &str,
    message: &Value,
) -> Option<Vec<Value>> {
    let text = |key: &str| message[key].as_str().map(str::to_string);
    let request_id = text("requestId");
    let reply = |kind: &str, mut payload: Value| {
        if let Some(id) = &request_id {
            payload["requestId"] = id.clone().into();
        }
        vec![session(kind, payload)]
    };
    let terminals = &daemon.terminals;
    let connection = subs.connection();
    match kind {
        "list_terminals_request" => {
            let (cwd, workspace) = (text("cwd"), text("workspaceId"));
            let listed: Vec<Value> = (terminals
                .listed(cwd.as_deref(), workspace.as_deref())
                .iter())
            .map(|t| t.info(true))
            .collect();
            let mut payload = json!({"terminals": listed});
            if let Some(cwd) = cwd {
                payload["cwd"] = cwd.into();
            }
            Some(reply("list_terminals_response", payload))
        }
        "subscribe_terminals_request" => {
            let cwd = text("cwd")?;
            let workspace = text("workspaceId");
            let id = format!("sub_terminals_{}", uuid::Uuid::new_v4().simple());
            let payload = terminals.list_payload(&id, &cwd, workspace.as_deref());
            terminals.lock().watchers.push(Watcher {
                connection,
                id,
                cwd,
                workspace_id: workspace,
                late: late.clone(),
            });
            Some(reply("terminals_changed", payload))
        }
        "unsubscribe_terminals_request" => {
            let (cwd, workspace) = (text("cwd")?, text("workspaceId"));
            (terminals.lock().watchers).retain(|w| {
                !(w.connection == connection && w.cwd == cwd && w.workspace_id == workspace)
            });
            Some(Vec::new())
        }
        "create_terminal_request" => {
            let created = create(daemon, message);
            let payload = match created {
                Ok(pty) => json!({"terminal": pty.info(true), "error": null}),
                Err(error) => json!({"terminal": null, "error": error}),
            };
            Some(reply("create_terminal_response", payload))
        }
        "subscribe_terminal_request" => {
            let terminal_id = text("terminalId")?;
            match attach(daemon, subs, shape, &terminal_id, message) {
                Ok(()) => Some(Vec::new()),
                Err(error) => Some(reply(
                    "subscribe_terminal_response",
                    json!({"terminalId": terminal_id, "error": error}),
                )),
            }
        }
        "unsubscribe_terminal_request" => {
            let terminal_id = text("terminalId")?;
            let ids: Vec<String> = (terminals.lock().streams.get(&connection))
                .map(|slots| {
                    (slots.active.values())
                        .filter(|s| s.terminal_id == terminal_id)
                        .map(|s| s.subscription_id.clone())
                        .collect()
                })
                .unwrap_or_default();
            for id in ids {
                terminals.release(connection, &id);
            }
            Some(Vec::new())
        }
        "terminal_input" => {
            let terminal_id = text("terminalId")?;
            let input = &message["message"];
            let Some(pty) = terminals.get(&terminal_id) else {
                return Some(Vec::new());
            };
            match input["type"].as_str() {
                Some("input") => pty.write(input["data"].as_str().unwrap_or_default().as_bytes()),
                Some("resize") => {
                    let (rows, cols) = (input["rows"].as_u64()?, input["cols"].as_u64()?);
                    let claim = input["intent"] != "update";
                    pty.resize(connection, rows as u32, cols as u32, claim);
                }
                // Mouse reports are Paseo's too; it ignores them.
                _ => {}
            }
            Some(Vec::new())
        }
        "kill_terminal_request" => {
            let terminal_id = text("terminalId")?;
            if let Some(pty) = terminals.get(&terminal_id) {
                pty.kill();
            }
            Some(reply(
                "kill_terminal_response",
                json!({"terminalId": terminal_id, "success": true}),
            ))
        }
        "capture_terminal_request" => {
            let terminal_id = text("terminalId")?;
            let (lines, total) = match terminals.get(&terminal_id) {
                Some(pty) => {
                    let screen = pty.screen();
                    screen
                        .term
                        .capture(message["start"].as_i64(), message["end"].as_i64())
                }
                None => (Vec::new(), 0),
            };
            Some(reply(
                "capture_terminal_response",
                json!({"terminalId": terminal_id, "lines": lines, "totalLines": total}),
            ))
        }
        "terminal.rename.request" => {
            let terminal_id = text("terminalId")?;
            let title = message["title"].as_str()?.trim().to_string();
            let error = if title.is_empty() {
                Some("Title is required")
            } else if title.chars().count() > TITLE_MAX {
                Some("Title is too long")
            } else {
                match terminals.get(&terminal_id) {
                    Some(pty) => {
                        {
                            let mut screen = pty.screen();
                            screen.title = Some(title);
                            screen.renamed = true;
                        }
                        terminals.changed(&pty);
                        None
                    }
                    None => Some("Terminal not found"),
                }
            };
            Some(reply(
                "terminal.rename.response",
                json!({"success": error.is_none(), "error": error}),
            ))
        }
        _ => None,
    }
}

/// `create_terminal_request`: a PTY in the workspace's folder running the
/// command asked for, else the user's shell. The error is Paseo's words.
fn create(daemon: &Arc<Daemon>, message: &Value) -> Result<Arc<Pty>, String> {
    if message["agentId"].as_str().is_some_and(|id| !id.is_empty()) {
        return Err(format!(
            "Agent-backed terminals are no longer supported for agent {}",
            message["agentId"].as_str().unwrap_or_default()
        ));
    }
    let cwd = message["cwd"].as_str().unwrap_or_default().to_string();
    if !Path::new(&cwd).is_absolute() {
        return Err("cwd must be absolute path".into());
    }
    let workspace_id = match message["workspaceId"].as_str().filter(|w| !w.is_empty()) {
        Some(id) => {
            let active = daemon
                .read(|store| (store.workspaces.get(id)).is_some_and(|w| w.archived_at.is_none()));
            if !active {
                return Err(format!("Workspace {id} is not active or does not exist"));
            }
            id.to_string()
        }
        None => daemon
            .read(|store| {
                (store.workspaces.iter())
                    .map(|(w, _)| w)
                    .find(|w| w.archived_at.is_none() && crate::git::same_path(&w.cwd, &cwd))
                    .map(|w| w.workspace_id.clone())
            })
            .ok_or("workspaceId is required")?,
    };
    let size = &message["size"];
    let rows = size["rows"]
        .as_u64()
        .map_or(DEFAULT_ROWS, |r| clamp_size(r as u32));
    let cols = size["cols"]
        .as_u64()
        .map_or(DEFAULT_COLS, |c| clamp_size(c as u32));
    let id = uuid::Uuid::new_v4().to_string();
    let name = {
        let same = (daemon.terminals.lock().terminals.iter())
            .filter(|t| t.cwd == cwd)
            .count();
        format!("Terminal {}", same + 1)
    };

    let mut command = match message["command"].as_str().filter(|c| !c.trim().is_empty()) {
        Some(program) => {
            let mut command = CommandBuilder::new(program);
            for arg in (message["args"].as_array().into_iter().flatten()).filter_map(Value::as_str)
            {
                command.arg(arg);
            }
            command
        }
        None => CommandBuilder::new_default_prog(),
    };
    command.cwd(&cwd);
    // Paseo's daemon's own variables are not the shell's.
    for key in [
        "PASEO_NODE_ENV",
        "PASEO_DESKTOP_MANAGED",
        "PASEO_SUPERVISED",
        "ELECTRON_RUN_AS_NODE",
        "ELECTRON_NO_ATTACH_CONSOLE",
        "PASEO_AGENT_ID",
        "PASEO_AGENT_CWD",
    ] {
        command.env_remove(key);
    }
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("PASEO_TERMINAL_ID", &id);
    command.env("PASEO_WORKSPACE_ID", &workspace_id);
    if let Some(endpoint) = daemon.local_endpoint.get() {
        command.env("PASEO_HOST", endpoint);
    }
    if let Some(path) = crate::agents::placement::commands_path() {
        command.env("PATH", path);
    }

    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("Could not open a terminal: {e}"))?;
    let mut child = pair.slave.spawn_command(command).map_err(|e| {
        let program = message["command"].as_str().unwrap_or("the shell");
        format!("{program}: {e}")
    })?;
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Could not read the terminal: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Could not write to the terminal: {e}"))?;
    let killer = child.clone_killer();
    let (input, queued) = std::sync::mpsc::channel();
    let writer_id = id.clone();
    std::thread::Builder::new()
        .name(format!("terminal-in-{}", &id[..8]))
        .spawn(move || write_input(&writer_id, writer, queued))
        .map_err(|e| format!("Could not start the terminal: {e}"))?;
    let pty = Arc::new(Pty {
        id,
        name,
        cwd,
        workspace_id,
        screen: Mutex::new(Screen {
            term: term::Terminal::answering(rows as usize, cols as usize),
            revision: 0,
            listeners: Vec::new(),
            title: None,
            renamed: false,
            rows,
            cols,
            owner: None,
        }),
        input,
        master: Mutex::new(pair.master),
        killer: Mutex::new(killer),
    });
    daemon.terminals.lock().terminals.push(pty.clone());
    info!(terminal = pty.id, cwd = pty.cwd, "terminal started");
    let (daemon2, pty2) = (daemon.clone(), pty.clone());
    std::thread::Builder::new()
        .name(format!("terminal-{}", &pty.id[..8]))
        .spawn(move || {
            read_output(&daemon2, &pty2, reader);
            let _ = child.wait();
            {
                let mut screen = pty2.screen();
                for (_, listener) in screen.listeners.drain(..) {
                    let _ = listener.send(Chunk::Exit);
                }
            }
            daemon2.terminals.ended(&pty2);
        })
        .map_err(|e| format!("Could not start the terminal: {e}"))?;
    daemon.terminals.changed(&pty);
    Ok(pty)
}

/// Writes what is queued for the program, in order, until the terminal is
/// gone or will not take more. It alone waits on a program that is not
/// reading its input, never the reader or a client's task.
fn write_input(
    id: &str,
    mut writer: Box<dyn Write + Send>,
    queued: std::sync::mpsc::Receiver<Vec<u8>>,
) {
    for bytes in queued {
        if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
            warn!(terminal = id, %error, "could not write to the terminal");
            return;
        }
    }
}

/// Reads the program's output until it closes the terminal: into the
/// screen, answers to its queries back to it, and on to every stream.
fn read_output(daemon: &Daemon, pty: &Pty, mut reader: Box<dyn Read + Send>) {
    let mut buffer = vec![0u8; 16 * 1024];
    // A character split across two reads waits for the rest of it.
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        carry.extend_from_slice(&buffer[..read]);
        let whole = match std::str::from_utf8(&carry) {
            Ok(_) => carry.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => carry.len(),
        };
        if whole == 0 {
            continue;
        }
        let bytes: Vec<u8> = carry.drain(..whole).collect();
        let retitled = {
            let mut screen = pty.screen();
            screen.term.feed(&bytes);
            let replies = screen.term.take_replies();
            if !replies.is_empty() {
                pty.write(&replies);
            }
            screen.revision += 1;
            let revision = screen.revision;
            (screen.listeners).retain(|(_, listener)| {
                listener
                    .send(Chunk::Output(revision, bytes.clone()))
                    .is_ok()
            });
            let title = screen.term.title();
            if !screen.renamed && title != screen.title {
                screen.title = title;
                true
            } else {
                false
            }
        };
        if retitled {
            daemon.terminals.changed(pty);
        }
    }
}

/// `subscribe_terminal_request`: a slot on this socket, the reply, then
/// the screen and the output, from a task of its own.
fn attach(
    daemon: &Arc<Daemon>,
    subs: &mut Subscriptions,
    shape: ClientShape,
    terminal_id: &str,
    message: &Value,
) -> Result<(), String> {
    let terminals = &daemon.terminals;
    let pty = terminals.get(terminal_id).ok_or("Terminal not found")?;
    let frames = subs
        .frames
        .clone()
        .ok_or("No terminal stream slots available")?;
    let connection = subs.connection();
    let restore = &message["restore"];
    let history = match restore["mode"].as_str() {
        Some("live") => None,
        Some("full-snapshot") if shape.chunked_terminal_replay => Some(
            restore["scrollbackLines"]
                .as_u64()
                .map_or(FULL_HISTORY, |n| n as usize),
        ),
        Some("full-snapshot") => Some(FULL_HISTORY),
        _ => {
            let asked = restore["scrollbackLines"]
                .as_u64()
                .map_or(VISIBLE_HISTORY, |n| n as usize);
            Some(if shape.chunked_terminal_replay {
                asked
            } else {
                asked.min(VISIBLE_HISTORY_MAX)
            })
        }
    };
    // A size sent with the attach claims it, when no one else holds it.
    if let (Some(rows), Some(cols)) = (
        restore["size"]["rows"].as_u64(),
        restore["size"]["cols"].as_u64(),
    ) {
        let free = pty.screen().owner.is_none_or(|owner| owner == connection);
        if free {
            pty.resize(connection, rows as u32, cols as u32, true);
        }
    }
    let subscription_id = format!("sub_terminal_{}", uuid::Uuid::new_v4().simple());
    let mut inner = terminals.lock();
    let slot = {
        let slots = inner.streams.entry(connection).or_default();
        if slots.active.len() >= 256 {
            return Err("No terminal stream slots available".into());
        }
        let mut slot = (slots.next % 256) as u8;
        while slots.active.contains_key(&slot) {
            slot = slot.wrapping_add(1);
        }
        slots.next = slot as u16 + 1;
        slot
    };
    inner.next_listener += 1;
    let key = inner.next_listener;
    let (tx, rx) = mpsc::unbounded_channel();
    // The screen and the listener under one lock: the output after it is
    // exactly what the screen does not have.
    let (snapshot, since, size) = {
        let mut screen = pty.screen();
        let snapshot = history.map(|lines| screen.term.serialize(lines));
        // Streams that ended while the program was quiet go now.
        screen
            .listeners
            .retain(|(_, listener)| !listener.is_closed());
        screen.listeners.push((key, tx));
        (snapshot, screen.revision, (screen.rows, screen.cols))
    };
    // The reply first, on the queue the frames use.
    frames.text(&session(
        "subscribe_terminal_response",
        json!({"terminalId": terminal_id, "slot": slot, "subscriptionId": subscription_id,
               "error": null, "requestId": message["requestId"]}),
    ));
    let feed = Feed {
        daemon: daemon.clone(),
        pty,
        frames,
        connection,
        slot,
        subscription_id: subscription_id.clone(),
        chunked: shape.chunked_terminal_replay,
    };
    let task = tokio::spawn(feed.run(rx, snapshot, since, size));
    let slots = inner.streams.entry(connection).or_default();
    slots.active.insert(
        slot,
        Stream {
            subscription_id,
            terminal_id: terminal_id.to_string(),
            task,
        },
    );
    Ok(())
}

/// One attached client's stream.
struct Feed {
    daemon: Arc<Daemon>,
    pty: Arc<Pty>,
    frames: Frames,
    connection: u64,
    slot: u8,
    subscription_id: String,
    chunked: bool,
}

impl Feed {
    async fn run(
        self,
        mut rx: mpsc::UnboundedReceiver<Chunk>,
        snapshot: Option<Vec<u8>>,
        mut since: u64,
        size: (u16, u16),
    ) {
        if let Some(screen) = snapshot {
            self.restore(screen, size);
        }
        loop {
            let Some(chunk) = rx.recv().await else {
                break;
            };
            let Chunk::Output(revision, mut batch) = chunk else {
                self.exit();
                break;
            };
            if revision <= since {
                continue;
            }
            since = revision;
            // Whatever else comes within a few milliseconds goes with it.
            let deadline = tokio::time::Instant::now() + COALESCE;
            let mut ended = false;
            while batch.len() < OUTPUT_CHUNK {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(Chunk::Output(revision, more))) if revision > since => {
                        since = revision;
                        batch.extend(more);
                    }
                    Ok(Some(Chunk::Output(..))) => {}
                    Ok(Some(Chunk::Exit)) | Ok(None) => {
                        ended = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
            if self.frames.queued() > BEHIND {
                since = self.catch_up(&mut rx).await;
                if since == u64::MAX {
                    self.exit();
                    break;
                }
                continue;
            }
            for piece in batch.chunks(OUTPUT_CHUNK) {
                self.frames
                    .binary(encode_raw(OPCODE_OUTPUT, self.slot, piece));
            }
            if ended {
                self.exit();
                break;
            }
        }
        self.daemon
            .terminals
            .stream_ended(self.connection, self.slot);
    }

    /// The screen as it is, in one Restore frame or as replay chunks.
    fn restore(&self, screen: Vec<u8>, (rows, cols): (u16, u16)) {
        if !self.chunked {
            self.frames
                .binary(encode_raw(OPCODE_RESTORE, self.slot, &screen));
            return;
        }
        let begin = json!({"rows": rows, "cols": cols, "retainedHistoryLines": FULL_HISTORY});
        self.frames.binary(encode_raw(
            OPCODE_REPLAY_BEGIN,
            self.slot,
            begin.to_string().as_bytes(),
        ));
        for piece in screen.chunks(REPLAY_CHUNK) {
            self.frames
                .binary(encode_raw(OPCODE_REPLAY_DATA, self.slot, piece));
        }
        self.frames
            .binary(encode_raw(OPCODE_REPLAY_END, self.slot, &[]));
    }

    /// The socket fell behind: output is dropped until it has caught up,
    /// then the screen goes afresh. The revision it is current to, or
    /// `u64::MAX` when the terminal ended meanwhile.
    async fn catch_up(&self, rx: &mut mpsc::UnboundedReceiver<Chunk>) -> u64 {
        while self.frames.queued() > CAUGHT_UP {
            if self.frames.closed() {
                return u64::MAX;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            while let Ok(chunk) = rx.try_recv() {
                if matches!(chunk, Chunk::Exit) {
                    return u64::MAX;
                }
            }
        }
        let (screen, revision, size) = {
            let screen = self.pty.screen();
            (
                screen.term.serialize(VISIBLE_HISTORY_MAX),
                screen.revision,
                (screen.rows, screen.cols),
            )
        };
        self.restore(screen, size);
        revision
    }

    fn exit(&self) {
        self.frames.text(&session(
            "terminal_stream_exit",
            json!({"subscriptionId": self.subscription_id, "terminalId": self.pty.id}),
        ));
    }
}
