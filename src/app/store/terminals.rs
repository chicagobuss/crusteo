//! Terminals: the PTYs open on each host, the lists of them per workspace,
//! and the local emulator each attached one feeds.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use fleet::{HostId, requests};
use gpui_kit::gpui::{App, AppContext, Context, Entity, Task};
use serde_json::{Value, json};

use super::FleetStore;

/// How long, and how much, a host's frames for a slot not known yet are held
/// (`Link::early_frames`).
const EARLY_FRAMES_FOR: std::time::Duration = std::time::Duration::from_secs(5);
const EARLY_FRAMES_MAX: usize = 8 * 1024 * 1024;

/// One open PTY. The emulator is local; bytes come from the daemon's slot.
pub struct TerminalSession {
    pub emulator: term::Terminal,
    pub attaching: bool,
    pub offline: bool,
    pub error: Option<String>,
    pub title: Option<String>,
    rows: u32,
    cols: u32,
    claimed: bool,
    attached_size: Option<(u32, u32)>,
    subscription_id: Option<String>,
    attach_gen: u64,
}

impl TerminalSession {
    fn new(_host: &HostId, _id: &str) -> Self {
        Self {
            emulator: term::Terminal::new(24, 80),
            attaching: false,
            offline: false,
            error: None,
            title: None,
            rows: 0,
            cols: 0,
            claimed: false,
            attached_size: None,
            subscription_id: None,
            attach_gen: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TerminalRecord {
    id: String,
    name: String,
    title: Option<String>,
}

pub(super) struct TerminalList {
    cwd: String,
    loaded: bool,
    terminals: Vec<TerminalRecord>,
    subscription: Option<String>,
    epoch: u64,
    request: u64,
    pending: bool,
    revision: u64,
}

#[derive(Clone)]
pub struct TerminalWatch {
    pub host: HostId,
    pub workspace: String,
    pub cwd: String,
}

impl FleetStore {
    /// A shell in every fifth workspace, so Lanes shows terminal rows.
    /// Stress hosts never connect, so nothing replaces these lists.
    pub(super) fn seed_stress_shells(&mut self) {
        let shells: Vec<(HostId, String)> = (self.fleet.hosts.iter())
            .flat_map(|host| {
                (host.directory.workspaces.keys())
                    .step_by(5)
                    .map(|workspace| (host.id().clone(), workspace.clone()))
            })
            .collect();
        for (n, key) in shells.into_iter().enumerate() {
            self.terminal_lists.insert(
                key,
                TerminalList {
                    cwd: String::new(),
                    loaded: true,
                    terminals: vec![TerminalRecord {
                        id: format!("stress-shell-{n}"),
                        name: "zsh".into(),
                        title: None,
                    }],
                    subscription: None,
                    epoch: 0,
                    request: 0,
                    pending: false,
                    revision: 0,
                },
            );
        }
    }

    /// The host's connection dropped: its terminals wait for it to return.
    pub(super) fn mark_terminals_offline(&mut self, host: &HostId, cx: &mut Context<Self>) {
        let sessions: Vec<Entity<TerminalSession>> = self
            .terminals
            .iter()
            .filter(|((h, _), _)| h == host)
            .map(|(_, session)| session.clone())
            .collect();
        for session in sessions {
            session.update(cx, |session, cx| {
                session.offline = true;
                session.attaching = false;
                session.subscription_id = None;
                session.claimed = false;
                session.attached_size = None;
                cx.notify();
            });
        }
    }

    // ---- terminals --------------------------------------------------------

    /// Membership generation used only by terminal tab reconciliation.
    pub fn terminal_nav_revision(&self) -> u64 {
        self.terminal_nav_revision
    }

    /// Terminal ids per workspace for tab reconciliation. `None` until the
    /// live list has arrived on this connection.
    pub fn terminal_nav_lists(&self) -> BTreeMap<String, Option<Vec<String>>> {
        let mut out = BTreeMap::new();
        for ((host, workspace), list) in &self.terminal_lists {
            let key = format!("{host}:{workspace}");
            let value = list.loaded.then(|| {
                list.terminals
                    .iter()
                    .filter(|t| !self.terminal_is_closing(host, &t.id))
                    .map(|t| t.id.clone())
                    .collect()
            });
            out.insert(key, value);
        }
        out
    }

    /// The workspace a listed terminal belongs to.
    pub fn terminal_workspace(&self, host: &HostId, id: &str) -> Option<String> {
        self.terminal_lists
            .iter()
            .find(|((h, _), list)| h == host && list.terminals.iter().any(|t| t.id == id))
            .map(|((_, workspace), _)| workspace.clone())
    }

    fn terminal_is_closing(&self, host: &HostId, id: &str) -> bool {
        self.closing_terminals
            .get(host)
            .is_some_and(|ids| ids.contains(id))
    }

    /// The terminals of a workspace whose list has loaded, in the host's
    /// order, without those being closed.
    pub fn workspace_terminal_ids<'a>(
        &'a self,
        host: &'a HostId,
        workspace: &str,
    ) -> impl Iterator<Item = &'a String> {
        self.terminal_lists
            .get(&(host.clone(), workspace.to_string()))
            .filter(|list| list.loaded)
            .into_iter()
            .flat_map(|list| list.terminals.iter())
            .filter(move |record| !self.terminal_is_closing(host, &record.id))
            .map(|record| &record.id)
    }

    pub fn workspace_terminals_revision(&self, host: &HostId, workspace: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        if let Some(list) = self
            .terminal_lists
            .get(&(host.clone(), workspace.to_string()))
        {
            (list.loaded, list.request, list.revision).hash(&mut hash);
        }
        hash.finish()
    }

    pub fn terminal_session(&self, host: &HostId, id: &str) -> Option<Entity<TerminalSession>> {
        self.terminals.get(&(host.clone(), id.to_string())).cloned()
    }

    /// Whether a terminal on `host` waits for its attach to be answered.
    fn attaching_any(&self, host: &HostId, cx: &App) -> bool {
        (self.terminals.iter()).any(|((h, _), session)| h == host && session.read(cx).attaching)
    }

    pub fn terminal_label(&self, host: &HostId, id: &str, cx: &App) -> String {
        if let Some(session) = self.terminal_session(host, id) {
            let title = session.read(cx).title.clone();
            if let Some(title) = title.filter(|t| !t.is_empty()) {
                return title;
            }
        }
        for ((h, _), list) in &self.terminal_lists {
            if h != host {
                continue;
            }
            if let Some(record) = list.terminals.iter().find(|t| t.id == id) {
                if let Some(title) = record.title.clone().filter(|t| !t.is_empty()) {
                    return title;
                }
                if !record.name.is_empty() {
                    return record.name.clone();
                }
            }
        }
        "Terminal".into()
    }

    /// Subscribes to the standalone terminals of the workspaces on screen.
    pub fn ensure_terminal_watches(
        &mut self,
        owner: &'static str,
        wanted: Vec<TerminalWatch>,
        cx: &mut Context<Self>,
    ) {
        self.terminal_watch_sets.insert(owner, wanted);
        let wanted = terminal_watch_union(&self.terminal_watch_sets);
        let stale: Vec<_> = self
            .terminal_lists
            .keys()
            .filter(|key| !wanted.contains_key(*key))
            .cloned()
            .collect();
        for key in stale {
            if let Some(list) = self.terminal_lists.remove(&key)
                && let Some(subscription) = list.subscription
            {
                self.request(&key.0, requests::release(&subscription), cx)
                    .detach();
            }
        }
        self.closing_terminals.retain(|host, _| {
            self.terminal_lists
                .keys()
                .any(|(listed_host, _)| listed_host == host)
        });
        for watch in wanted.into_values() {
            let key = (watch.host.clone(), watch.workspace.clone());
            let online = self
                .links
                .get(&watch.host)
                .is_some_and(|link| link.sync.online());
            let epoch = self
                .links
                .get(&watch.host)
                .map(|link| link.sync.epoch())
                .unwrap_or(0);
            let fresh = self.terminal_lists.get(&key).is_some_and(|list| {
                (list.subscription.is_some() || list.pending)
                    && list.epoch == epoch
                    && list.cwd == watch.cwd
            });
            if !online || fresh {
                self.terminal_lists
                    .entry(key)
                    .or_insert_with(|| TerminalList {
                        cwd: watch.cwd,
                        loaded: false,
                        terminals: Vec::new(),
                        subscription: None,
                        epoch: 0,
                        request: 0,
                        pending: false,
                        revision: 0,
                    });
                continue;
            }
            self.watch_terminals(watch, epoch, cx);
        }
    }

    pub fn open_terminal(
        &mut self,
        host: &HostId,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalSession> {
        let key = (host.clone(), id.to_string());
        if let Some(session) = self.terminals.get(&key) {
            if let Some(link) = self.links.get_mut(host) {
                link.open_terminals.insert(id.to_string());
            }
            return session.clone();
        }
        let session = cx.new(|_| TerminalSession::new(host, id));
        self.terminals.insert(key, session.clone());
        if let Some(link) = self.links.get_mut(host) {
            link.open_terminals.insert(id.to_string());
        }
        session
    }

    pub fn retain_terminals(&mut self, keep: &BTreeSet<(HostId, String)>, cx: &mut Context<Self>) {
        let drop_keys: Vec<(HostId, String)> = self
            .terminals
            .keys()
            .filter(|key| !keep.contains(key))
            .cloned()
            .collect();
        for (host, id) in drop_keys {
            self.detach_terminal(&host, &id, cx);
            self.terminals.remove(&(host, id));
        }
    }

    pub fn create_terminal(
        &mut self,
        host: &HostId,
        workspace: &str,
        cwd: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<String, String>> {
        let task = self.write(host, requests::create_terminal(cwd, workspace, 24, 80), cx);
        let host = host.clone();
        let workspace = workspace.to_string();
        cx.spawn(async move |this, cx| {
            let payload = task.await?;
            if let Some(error) = payload["error"].as_str().filter(|e| !e.is_empty()) {
                return Err(error.to_string());
            }
            let id = payload["terminal"]["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() {
                return Err("The host did not create a terminal".into());
            }
            let record = TerminalRecord {
                id: id.clone(),
                name: payload["terminal"]["name"]
                    .as_str()
                    .unwrap_or("Terminal")
                    .to_string(),
                title: payload["terminal"]["title"].as_str().map(str::to_string),
            };
            let _ = this.update(cx, |store, cx| {
                store.note_created_terminal(&host, &workspace, record);
                store.open_terminal(&host, &id, cx);
                store.changed(cx);
            });
            Ok(id)
        })
    }

    pub fn kill_terminal(&mut self, host: &HostId, id: &str, cx: &mut Context<Self>) {
        self.closing_terminals
            .entry(host.clone())
            .or_default()
            .insert(id.to_string());
        for ((h, _), list) in &mut self.terminal_lists {
            if h == host {
                let before = list.terminals.len();
                list.terminals.retain(|t| t.id != id);
                if list.terminals.len() != before {
                    list.revision += 1;
                }
            }
        }
        self.detach_terminal(host, id, cx);
        self.act(host, requests::kill_terminal(id), cx);
        self.changed(cx);
    }

    pub fn terminal_write(&self, host: &HostId, id: &str, data: &str) {
        if data.is_empty() {
            return;
        }
        self.send_raw(host, requests::terminal_input(id, data));
    }

    pub fn terminal_resize(
        &mut self,
        host: &HostId,
        id: &str,
        rows: u32,
        cols: u32,
        cx: &mut Context<Self>,
    ) {
        if rows == 0 || cols == 0 {
            return;
        }
        let Some(session) = self.terminal_session(host, id) else {
            return;
        };
        let (attaching, subscribed, same) = session.update(cx, |session, cx| {
            session.emulator.resize(rows as usize, cols as usize);
            session.rows = rows;
            session.cols = cols;
            cx.notify();
            (
                session.attaching,
                session.subscription_id.is_some(),
                session.attached_size == Some((rows, cols)) && session.claimed,
            )
        });
        if same || attaching {
            return;
        }
        let online = self.links.get(host).is_some_and(|link| link.sync.online());
        if !online {
            return;
        }
        if subscribed {
            self.send_raw(host, requests::terminal_resize(id, rows, cols, "update"));
            session.update(cx, |session, _| {
                session.attached_size = Some((rows, cols));
                session.claimed = true;
            });
        } else {
            self.attach_terminal(host, id, cx);
        }
    }

    fn note_created_terminal(&mut self, host: &HostId, workspace: &str, record: TerminalRecord) {
        let Some(list) = self
            .terminal_lists
            .get_mut(&(host.clone(), workspace.to_string()))
        else {
            return;
        };
        if !list.terminals.iter().any(|t| t.id == record.id) {
            list.terminals.insert(0, record);
            list.revision += 1;
        }
    }

    fn watch_terminals(&mut self, watch: TerminalWatch, epoch: u64, cx: &mut Context<Self>) {
        let key = (watch.host.clone(), watch.workspace.clone());
        let previous = self
            .terminal_lists
            .get(&key)
            .and_then(|list| list.subscription.clone());
        let (loaded, terminals) = self
            .terminal_lists
            .get(&key)
            .map(|list| (list.loaded, list.terminals.clone()))
            .unwrap_or((false, Vec::new()));
        self.terminal_watch_request += 1;
        let request = self.terminal_watch_request;
        self.terminal_lists.insert(
            key,
            TerminalList {
                cwd: watch.cwd.clone(),
                loaded,
                terminals,
                subscription: None,
                epoch,
                request,
                pending: true,
                revision: 0,
            },
        );
        if let Some(previous) = previous {
            self.request(&watch.host, requests::release(&previous), cx)
                .detach();
        }
        let task = self.request(
            &watch.host,
            requests::subscribe_terminals(&watch.cwd, &watch.workspace),
            cx,
        );
        let host = watch.host.clone();
        let workspace = watch.workspace.clone();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |store, cx| {
                if !store.is_current(&host, epoch) {
                    return;
                }
                let key = (host.clone(), workspace.clone());
                let current = store
                    .terminal_lists
                    .get(&key)
                    .is_some_and(|list| list.request == request);
                if !current {
                    if let Ok(payload) = result
                        && let Some(subscription) = payload["subscriptionId"].as_str()
                    {
                        store
                            .request(&host, requests::release(subscription), cx)
                            .detach();
                    }
                    return;
                }
                if let Some(list) = store.terminal_lists.get_mut(&key) {
                    list.pending = false;
                }
                if let Ok(payload) = result {
                    store.apply_terminal_list(&host, &workspace, &payload, cx);
                }
            });
        })
        .detach();
    }

    pub(super) fn rewatch_terminal_lists(&mut self, host: &HostId, cx: &mut Context<Self>) {
        let epoch = self
            .links
            .get(host)
            .map(|link| link.sync.epoch())
            .unwrap_or(0);
        let watches: Vec<TerminalWatch> = self
            .terminal_lists
            .iter()
            .filter(|((h, _), _)| h == host)
            .map(|((_, workspace), list)| TerminalWatch {
                host: host.clone(),
                workspace: workspace.clone(),
                cwd: list.cwd.clone(),
            })
            .collect();
        for ((h, _), list) in &mut self.terminal_lists {
            if h == host {
                list.subscription = None;
                list.loaded = false;
                list.epoch = 0;
            }
        }
        for watch in watches {
            self.watch_terminals(watch, epoch, cx);
        }
    }

    fn apply_terminal_list(
        &mut self,
        host: &HostId,
        workspace: &str,
        payload: &Value,
        cx: &mut Context<Self>,
    ) {
        let records = parse_terminal_records(&payload["terminals"]);
        let changed = self
            .terminal_lists
            .get(&(host.clone(), workspace.to_string()))
            .is_some_and(|list| !list.loaded || list.terminals != records);
        let membership_changed = self
            .terminal_lists
            .get(&(host.clone(), workspace.to_string()))
            .is_some_and(|list| {
                !list.loaded
                    || !list
                        .terminals
                        .iter()
                        .map(|t| &t.id)
                        .eq(records.iter().map(|t| &t.id))
            });
        if membership_changed {
            self.terminal_nav_revision += 1;
        }
        let ids: BTreeSet<String> = records.iter().map(|r| r.id.clone()).collect();
        if let Some(closing) = self.closing_terminals.get_mut(host) {
            closing.retain(|id| ids.contains(id));
        }
        let previous: BTreeSet<String> = self
            .terminal_lists
            .get(&(host.clone(), workspace.to_string()))
            .map(|list| list.terminals.iter().map(|r| r.id.clone()).collect())
            .unwrap_or_default();
        if let Some(list) = self
            .terminal_lists
            .get_mut(&(host.clone(), workspace.to_string()))
        {
            list.loaded = true;
            if changed {
                list.revision += 1;
            }
            list.terminals = records;
            if let Some(subscription) = payload["subscriptionId"].as_str() {
                list.subscription = Some(subscription.to_string());
            }
            if let Some(link) = self.links.get(host) {
                list.epoch = link.sync.epoch();
            }
        }
        for record in self
            .terminal_lists
            .get(&(host.clone(), workspace.to_string()))
            .map(|list| list.terminals.clone())
            .unwrap_or_default()
        {
            if let Some(session) = self.terminal_session(host, &record.id) {
                session.update(cx, |session, cx| {
                    if session.title.is_none() {
                        session.title = record.title.clone();
                        cx.notify();
                    }
                });
            }
        }
        let gone: Vec<String> = previous
            .into_iter()
            .filter(|id| !ids.contains(id))
            .filter(|id| !self.terminal_is_closing(host, id))
            .filter(|id| self.terminal_session(host, id).is_some())
            .collect();
        for id in gone {
            self.fail_terminal(host, &id, "This terminal is gone".into(), cx);
        }
        if changed {
            cx.notify();
        }
    }

    pub(super) fn on_terminals_changed(
        &mut self,
        host: &HostId,
        payload: &Value,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = payload["workspaceId"].as_str() else {
            return;
        };
        if !self
            .terminal_lists
            .contains_key(&(host.clone(), workspace.to_string()))
        {
            return;
        }
        self.apply_terminal_list(host, workspace, payload, cx);
    }

    pub(super) fn on_terminal_exit(
        &mut self,
        host: &HostId,
        payload: &Value,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = payload["terminalId"].as_str() else {
            return;
        };
        let message = payload["error"]
            .as_str()
            .filter(|e| !e.is_empty())
            .unwrap_or("This terminal is gone")
            .to_string();
        self.fail_terminal(host, id, message, cx);
    }

    pub(super) fn on_terminal_frame(
        &mut self,
        host: &HostId,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) {
        let Some(frame) = paseo_client::terminal::decode_frame(bytes) else {
            return;
        };
        let Some(id) = self
            .links
            .get(host)
            .and_then(|link| link.terminal_slots.get(&frame.slot))
            .cloned()
        else {
            // Its slot may be in a reply not read yet: hold it while an
            // attach is under way, a few seconds and a few megabytes at most.
            let attaching = self.attaching_any(host, cx);
            if let Some(link) = self.links.get_mut(host).filter(|_| attaching) {
                let now = std::time::Instant::now();
                link.early_frames
                    .retain(|(at, _)| now.duration_since(*at) < EARLY_FRAMES_FOR);
                let held: usize = link.early_frames.iter().map(|(_, f)| f.len()).sum();
                if held + bytes.len() <= EARLY_FRAMES_MAX {
                    link.early_frames.push((now, bytes.to_vec()));
                }
            }
            return;
        };
        let Some(session) = self.terminal_session(host, &id) else {
            return;
        };
        let title_changed = session.update(cx, |session, cx| {
            match frame.opcode {
                paseo_client::terminal::Opcode::Output => session.emulator.feed(&frame.payload),
                paseo_client::terminal::Opcode::Restore => session.emulator.restore(&frame.payload),
                paseo_client::terminal::Opcode::Snapshot => {
                    if let Some(state) = paseo_client::terminal::decode_snapshot(&frame.payload) {
                        if session.title.is_none() {
                            session.title = state.title.clone();
                        }
                        let ansi = paseo_client::terminal::snapshot_to_ansi(&state);
                        session.emulator.restore(&ansi);
                    }
                }
                _ => {}
            }
            let title = session.emulator.title();
            let changed = title != session.title && title.is_some();
            if title.is_some() {
                session.title = title;
            }
            cx.notify();
            changed
        });
        if title_changed {
            self.changed(cx);
        }
    }

    fn attach_terminal(&mut self, host: &HostId, id: &str, cx: &mut Context<Self>) {
        let Some(link) = self.links.get(host) else {
            return;
        };
        if !link.sync.online() {
            return;
        }
        let epoch = link.sync.epoch();
        let Some(session) = self.terminal_session(host, id) else {
            return;
        };
        let (rows, cols, generation) = session.update(cx, |session, _| {
            session.attach_gen += 1;
            session.attaching = true;
            session.error = None;
            session.offline = false;
            (session.rows, session.cols, session.attach_gen)
        });
        if rows > 0 && cols > 0 {
            self.send_raw(host, requests::terminal_resize(id, rows, cols, "claim"));
        }
        let restore = self.links.get(host).and_then(|_| {
            self.fleet.host(host).and_then(|h| {
                h.has("terminal-restore-modes").then(|| {
                    let mut value = json!({
                        "mode": "visible-snapshot",
                        "scrollbackLines": 200,
                    });
                    if rows > 0 && cols > 0 {
                        value["size"] = json!({ "rows": rows, "cols": cols });
                    }
                    value
                })
            })
        });
        let task = self.request(host, requests::subscribe_terminal(id, restore), cx);
        let host = host.clone();
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |store, cx| {
                if !store.is_current(&host, epoch) {
                    return;
                }
                let Some(session) = store.terminal_session(&host, &id) else {
                    return;
                };
                if session.read(cx).attach_gen != generation {
                    return;
                }
                match result {
                    Ok(payload) => {
                        if let Some(error) = payload["error"].as_str().filter(|e| !e.is_empty()) {
                            store.fail_terminal(&host, &id, error.to_string(), cx);
                            return;
                        }
                        let Some(slot) = payload["slot"].as_u64() else {
                            store.fail_terminal(&host, &id, "The host did not attach".into(), cx);
                            return;
                        };
                        let slot = slot as u8;
                        let subscription = payload["subscriptionId"].as_str().map(str::to_string);
                        let mut early = Vec::new();
                        if let Some(link) = store.links.get_mut(&host) {
                            link.terminal_slots.retain(|_, terminal| terminal != &id);
                            link.terminal_slots.insert(slot, id.clone());
                            let (theirs, others) = std::mem::take(&mut link.early_frames)
                                .into_iter()
                                .partition(|(_, frame)| frame.get(1) == Some(&slot));
                            link.early_frames = others;
                            early = theirs;
                        }
                        // Frames that came before the reply was read.
                        for (_, frame) in early {
                            store.on_terminal_frame(&host, &frame, cx);
                        }
                        let previous = session.update(cx, |session, cx| {
                            session.attaching = false;
                            session.offline = false;
                            session.error = None;
                            session.claimed = rows > 0 && cols > 0;
                            if rows > 0 && cols > 0 {
                                session.attached_size = Some((rows, cols));
                            }
                            let previous = session.subscription_id.take();
                            session.subscription_id = subscription;
                            cx.notify();
                            previous
                        });
                        if let Some(previous) = previous {
                            store
                                .request(&host, requests::release(&previous), cx)
                                .detach();
                        }
                        let current = session.read(cx);
                        if current.rows > 0
                            && current.cols > 0
                            && current.attached_size != Some((current.rows, current.cols))
                        {
                            let (rows, cols) = (current.rows, current.cols);
                            store.send_raw(
                                &host,
                                requests::terminal_resize(&id, rows, cols, "update"),
                            );
                            session.update(cx, |session, _| {
                                session.attached_size = Some((rows, cols));
                                session.claimed = true;
                            });
                        }
                    }
                    Err(err) => store.fail_terminal(&host, &id, err, cx),
                }
            });
        })
        .detach();
    }

    pub(super) fn reattach_terminals(&mut self, host: &HostId, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .links
            .get(host)
            .map(|link| link.open_terminals.iter().cloned().collect())
            .unwrap_or_default();
        for id in ids {
            if let Some(session) = self.terminal_session(host, &id) {
                session.update(cx, |session, _| {
                    session.subscription_id = None;
                    session.claimed = false;
                    session.attached_size = None;
                    session.attaching = false;
                    session.offline = false;
                });
            }
            self.attach_terminal(host, &id, cx);
        }
    }

    fn fail_terminal(&mut self, host: &HostId, id: &str, message: String, cx: &mut Context<Self>) {
        if let Some(link) = self.links.get_mut(host) {
            link.terminal_slots.retain(|_, terminal| terminal != id);
        }
        if let Some(session) = self.terminal_session(host, id) {
            session.update(cx, |session, cx| {
                session.attaching = false;
                session.offline = false;
                session.error = Some(message);
                session.subscription_id = None;
                cx.notify();
            });
        }
    }

    fn detach_terminal(&mut self, host: &HostId, id: &str, cx: &mut Context<Self>) {
        if let Some(link) = self.links.get_mut(host) {
            link.open_terminals.remove(id);
            link.terminal_slots.retain(|_, terminal| terminal != id);
        }
        if let Some(session) = self.terminal_session(host, id) {
            let subscription = session.update(cx, |session, _| session.subscription_id.take());
            if let Some(subscription) = subscription.filter(|id| !id.is_empty()) {
                self.request(host, requests::release(&subscription), cx)
                    .detach();
            }
        }
    }

    pub(super) fn mark_terminal_lists_stale(&mut self, host: &HostId) {
        for ((h, _), list) in &mut self.terminal_lists {
            if h == host {
                list.subscription = None;
                list.loaded = false;
                list.epoch = 0;
            }
        }
    }
}

fn terminal_watch_union(
    owners: &HashMap<&'static str, Vec<TerminalWatch>>,
) -> HashMap<(HostId, String), TerminalWatch> {
    owners
        .values()
        .flatten()
        .map(|watch| ((watch.host.clone(), watch.workspace.clone()), watch.clone()))
        .collect()
}

fn parse_terminal_records(value: &Value) -> Vec<TerminalRecord> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            if entry["agentId"].as_str().is_some() {
                return None;
            }
            let id = entry["id"].as_str()?.to_string();
            Some(TerminalRecord {
                id,
                name: entry["name"].as_str().unwrap_or("Terminal").to_string(),
                title: entry["title"].as_str().map(str::to_string),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_watch_owners_preserve_shared_workspaces_and_drop_scrolled_out_ones() {
        let watch = |workspace: &str| TerminalWatch {
            host: HostId::new("mac"),
            workspace: workspace.into(),
            cwd: format!("/code/{workspace}"),
        };
        let mut owners = HashMap::from([
            ("paseo", vec![watch("shared")]),
            ("lanes", vec![watch("shared"), watch("old")]),
        ]);
        assert_eq!(terminal_watch_union(&owners).len(), 2);
        owners.insert("lanes", vec![watch("new")]);
        let union = terminal_watch_union(&owners);
        assert!(union.contains_key(&(HostId::new("mac"), "shared".into())));
        assert!(union.contains_key(&(HostId::new("mac"), "new".into())));
        assert!(!union.contains_key(&(HostId::new("mac"), "old".into())));
        owners.insert("lanes", Vec::new());
        assert_eq!(terminal_watch_union(&owners).len(), 1);
        owners.insert("paseo", Vec::new());
        assert!(terminal_watch_union(&owners).is_empty());
    }
}
