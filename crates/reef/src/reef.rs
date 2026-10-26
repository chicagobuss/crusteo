//! Reef's one stateful task. It owns the upstream state and every
//! phone's subscriptions; daemon events, request replies and phone
//! messages all come to it, so nothing needs a lock. Deltas are gathered
//! and sent every [`Limits::flush`], one message per changed view.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use fleet::HostConfig;

use crate::catalog;
use crate::config::{self, ReefConfig, host_id};
use crate::lists;
use crate::proto::{self, ClientMsg, Limits, ViewSpec, code};
use crate::settings::{self, Settings};
use crate::upstream::{Apply, HostEvent, NewAgent, Reply, Upstream};
use crate::views::{self, ConversationView, TurnView};

/// What the listener tells Reef about phones.
pub(crate) enum FromServer {
    Connected {
        conn: u64,
        device: String,
        out: mpsc::Sender<String>,
    },
    Message {
        conn: u64,
        message: ClientMsg,
    },
    Closed {
        conn: u64,
    },
    /// An image by its content key, for `/image/<key>`.
    Image {
        key: u64,
        reply: tokio::sync::oneshot::Sender<Option<fleet::prompt::ImageAttachment>>,
    },
    /// The hosts, for `/api/hosts`: what a desktop in reef mode connects to.
    Hosts {
        reply: tokio::sync::oneshot::Sender<Value>,
    },
    /// The label catalog, for `/api/labels`, after an edit if one came.
    Labels {
        edit: Option<Value>,
        reply: tokio::sync::oneshot::Sender<Value>,
    },
}

/// A running Reef.
pub struct ReefHandle {
    /// Where it listens; with port 0 in the config, the port picked.
    pub addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    listener: tokio::task::AbortHandle,
}

impl ReefHandle {
    /// Runs until Reef stops, which it does not on its own.
    pub async fn wait(self) {
        let _ = self.task.await;
    }

    /// Stops Reef: the listener closes, daemons are hung up, and every
    /// phone's socket closes as its queue does.
    pub fn stop(&self) {
        self.listener.abort();
        self.task.abort();
    }
}

/// Binds the listener, dials every host and starts serving. Call inside a
/// tokio runtime.
pub async fn start(config: ReefConfig) -> std::io::Result<ReefHandle> {
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let addr = listener.local_addr()?;
    let (events_tx, events_rx) = mpsc::channel::<HostEvent>(256);
    let (apply_tx, apply_rx) = mpsc::unbounded_channel::<Apply>();
    let (server_tx, server_rx) = mpsc::unbounded_channel::<FromServer>();
    let limits = config.limits.clone();
    let gate = Arc::new(crate::server::Gate {
        devices: config.devices.clone(),
        open: config.open,
        host_names: config.host_names.clone(),
    });
    let accept = tokio::spawn(crate::server::serve(
        listener,
        gate,
        limits.clone(),
        server_tx,
    ));
    let listener = accept.abort_handle();
    let task = tokio::spawn(async move {
        if let Some(feed) = config.usage {
            crate::usage::watch_feed(feed, config.limits.usage_feed, apply_tx.clone());
        }
        let mut upstream = Upstream::new(config.hosts, config.client_id, events_tx, apply_tx);
        upstream.usage_every = limits.usage_ask;
        // Reef keeps the fleet's labels; agents carry them.
        let labels_path = config.data_dir.as_ref().map(|dir| dir.join(catalog::FILE));
        upstream.fleet.labels = Some(
            labels_path
                .as_deref()
                .map(catalog::load)
                .unwrap_or_default(),
        );
        upstream.connect_all();
        let hosts_file = config.hosts_file;
        let settings_path = config.data_dir.map(|dir| dir.join(settings::FILE));
        let settings = settings_path
            .as_deref()
            .map(Settings::load)
            .unwrap_or_default();
        let mut reef = State {
            up: upstream,
            phones: HashMap::new(),
            limits,
            hosts_sent: Value::Null,
            settings,
            settings_path,
            hosts_file,
            labels_path,
            started: std::time::Instant::now(),
            labels_saved: None,
        };
        reef.labels_saved = reef.up.fleet.labels.as_ref().map(|c| c.revision);
        reef.run(events_rx, apply_rx, server_rx).await;
        accept.abort();
    });
    Ok(ReefHandle {
        addr,
        task,
        listener,
    })
}

struct Phone {
    out: mpsc::Sender<String>,
    subs: HashMap<String, Sub>,
}

/// One view a phone subscribed to, and what it was last sent.
struct Sub {
    spec: ViewSpec,
    seq: u64,
    /// Owed a fresh snapshot: new, asked to resync, or fell behind.
    stale: bool,
    /// Worked out again at the next send though nothing changed: a map
    /// whose focus moved.
    poke: bool,
    sent: Sent,
}

enum Sent {
    Nothing,
    List {
        total: usize,
        groups: Value,
        rows: Vec<(String, Value)>,
    },
    Conversation(Box<SentConversation>),
    Map {
        lanes: Value,
        tiles: Vec<(String, Value)>,
        cards: Vec<(String, Value)>,
    },
}

#[derive(Default)]
struct SentConversation {
    /// The window starts at this item; None until the first page lands.
    anchor: Option<String>,
    epoch: Option<String>,
    view: Option<ConversationView>,
    fields: Map<String, Value>,
    older: Option<Older>,
}

/// An `older` not answered yet.
#[derive(Clone)]
struct Older {
    req: Option<String>,
    turns: usize,
    /// The conversation's page count when it was asked for or, once the
    /// Reef asked the daemon for a page, then; it settles past it.
    pages: u64,
    fetched: bool,
}

/// What working out a conversation's messages asks of Reef besides.
#[derive(Default)]
struct Wants {
    /// Fetch the page before what is loaded, for an `older`.
    fetch_older: bool,
}

struct State {
    up: Upstream,
    phones: HashMap<u64, Phone>,
    limits: Limits,
    hosts_sent: Value,
    settings: Settings,
    /// Where settings are saved; None keeps them in memory (tests).
    settings_path: Option<std::path::PathBuf>,
    /// Where hosts added or removed from a phone are saved.
    hosts_file: Option<std::path::PathBuf>,
    /// Where the label catalog is saved; None keeps it in memory (tests).
    labels_path: Option<std::path::PathBuf>,
    /// When Reef started, for how long the label import waits for hosts.
    started: std::time::Instant,
    /// The catalog revision last saved.
    labels_saved: Option<u64>,
}

/// The fleet as a desktop in reef mode takes it: each host's id, name and
/// address as Reef reaches it. Never a daemon's password: that stays with
/// each desktop.
pub(crate) fn desktop_hosts(fleet: &fleet::Fleet) -> Value {
    let hosts: Vec<Value> = (fleet.hosts.iter())
        .map(|host| {
            json!({
                "id": host.id(),
                "name": host.name(),
                "endpoint": host.config.endpoint,
                "password": host.config.password.is_some(),
            })
        })
        .collect();
    json!({ "reef": env!("CARGO_PKG_VERSION"), "hosts": hosts })
}

/// A host to add, from a phone: a name, and an address with the daemon's
/// port unless it names another.
fn new_host(name: &str, body: &Value) -> Result<HostConfig, &'static str> {
    let endpoint = body["endpoint"].as_str().unwrap_or_default().trim();
    config::check_host_name(name)?;
    if endpoint.is_empty() || endpoint.len() > 256 || endpoint.contains(char::is_whitespace) {
        return Err("A host needs an address, like 192.0.2.20 or myhost:6767");
    }
    let endpoint = if endpoint.contains(':') {
        endpoint.to_string()
    } else {
        format!("{endpoint}:{DAEMON_PORT}")
    };
    let password = body["password"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    Ok(HostConfig {
        id: host_id(name),
        name: name.to_string(),
        endpoint: Some(endpoint),
        password,
    })
}

/// The port a Paseo daemon listens on unless told otherwise.
const DAEMON_PORT: u16 = 6767;

/// Most images one message carries.
const MAX_IMAGES: usize = 8;

/// A send's images: `[{data: <base64>, mime_type}]`, as the daemon takes them.
fn images(value: &Value) -> Result<Vec<fleet::prompt::ImageAttachment>, &'static str> {
    let Some(list) = value.as_array() else {
        return if value.is_null() {
            Ok(Vec::new())
        } else {
            Err("images is not a list")
        };
    };
    if list.len() > MAX_IMAGES {
        return Err("too many images");
    }
    list.iter()
        .map(|image| {
            let data = image["data"].as_str().filter(|d| !d.is_empty());
            let mime = image["mime_type"]
                .as_str()
                .filter(|m| m.starts_with("image/"));
            match (data, mime) {
                (Some(data), Some(mime)) => Ok(fleet::prompt::ImageAttachment::new(data, mime)),
                _ => Err("an image needs data and an image mime_type"),
            }
        })
        .collect()
}

/// Most labels one workspace may be given from a phone.
const MAX_LABELS: usize = 20;

/// Label names, each trimmed, at most 64 characters, once each as crusteo
/// matches them.
fn labels(value: &Value) -> Result<Vec<String>, &'static str> {
    let Some(list) = value.as_array() else {
        return if value.is_null() {
            Ok(Vec::new())
        } else {
            Err("labels is not a list")
        };
    };
    let mut names: Vec<String> = Vec::new();
    for name in list {
        let name = name.as_str().ok_or("a label is a name")?;
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if name.is_empty() || name.chars().count() > 64 {
            return Err("A label needs a name, up to 64 characters");
        }
        let key = fleet::directory::label_key(&name);
        if !names.iter().any(|n| fleet::directory::label_key(n) == key) {
            names.push(name);
        }
    }
    if names.len() > MAX_LABELS {
        return Err("too many labels");
    }
    Ok(names)
}

/// Why a frame could not be queued.
enum Full {
    /// The phone is too slow; its views resync when it catches up.
    Lagging,
    Gone,
}

fn send(out: &mpsc::Sender<String>, message: &Value) -> Result<(), Full> {
    match out.try_send(message.to_string()) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => Err(Full::Lagging),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(Full::Gone),
    }
}

impl State {
    async fn run(
        &mut self,
        mut events: mpsc::Receiver<HostEvent>,
        mut apply: mpsc::UnboundedReceiver<Apply>,
        mut server: mpsc::UnboundedReceiver<FromServer>,
    ) {
        let mut flush = tokio::time::interval(self.limits.flush);
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut sweep = tokio::time::interval(std::time::Duration::from_secs(30));
        // Hosts due a usage read are asked, and rows whose pace or
        // staleness moved with the clock are sent.
        let tick = (usage_source::reports::ASK_TICK).min(self.limits.usage_ask);
        let mut usage = tokio::time::interval(tick);
        usage.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                Some((host, generation, event)) = events.recv() => {
                    self.up.on_event(&host, generation, event);
                }
                Some(work) = apply.recv() => work(&mut self.up),
                Some(message) = server.recv() => self.on_server(message),
                _ = flush.tick() => self.flush(),
                _ = sweep.tick() => self.up.sweep(self.limits.session_grace),
                _ = usage.tick() => {
                    self.up.ask_usage();
                    self.up.dirty_usage |= !self.up.usage.is_empty();
                }
                else => break,
            }
            self.deliver();
        }
    }

    /// Hands phones what the upstream side produced: action answers, and
    /// notifications for turns that ended.
    fn deliver(&mut self) {
        for (conn, message) in std::mem::take(&mut self.up.outbox) {
            self.reply(conn, message);
        }
        for (host, agent, failed) in std::mem::take(&mut self.up.finished) {
            // One read serves both the notification and the row's reply.
            if self.settings.notify_for(&views::agent_key(&host, &agent)) {
                self.up.describe_turn(&host, &agent, failed);
            } else {
                self.up.refresh_reply(&host, &agent);
            }
        }
        // A notice made just now from an open conversation is ready too.
        for notice in std::mem::take(&mut self.up.notices) {
            self.broadcast(&notice);
        }
    }

    fn broadcast(&mut self, message: &Value) {
        let conns: Vec<u64> = self.phones.keys().copied().collect();
        for conn in conns {
            self.reply(conn, message.clone());
        }
    }

    fn on_server(&mut self, message: FromServer) {
        match message {
            FromServer::Connected { conn, device, out } => {
                let welcome = json!({
                    "type": "welcome",
                    "v": proto::VERSION,
                    "hub": { "version": env!("CARGO_PKG_VERSION") },
                    "device": device,
                    "hosts": views::hosts(&self.up.fleet, &self.up.host_since()),
                    "labels": lists::label_colors(&self.up.fleet),
                    "settings": self.settings.public(),
                    "limits": {
                        "max_subscriptions": self.limits.max_subscriptions,
                        "list_max": self.limits.list_max,
                        "items_max": self.limits.items_max,
                        "text_max": self.limits.text_max,
                    },
                });
                if send(&out, &welcome).is_ok() {
                    let subs = HashMap::new();
                    self.phones.insert(conn, Phone { out, subs });
                }
            }
            FromServer::Message { conn, message } => self.on_message(conn, message),
            FromServer::Closed { conn } => self.drop_phone(conn),
            FromServer::Image { key, reply } => {
                let _ = reply.send(self.image(key));
            }
            FromServer::Hosts { reply } => {
                let _ = reply.send(desktop_hosts(&self.up.fleet));
            }
            FromServer::Labels { edit, reply } => {
                let _ = reply.send(self.labels_request(edit));
            }
        }
    }

    /// An image in a conversation Reef holds. Phones only learn keys
    /// from conversations they are shown, so those are the ones to look in.
    fn image(&self, key: u64) -> Option<fleet::prompt::ImageAttachment> {
        self.up.sessions.values().find_map(|session| {
            session.timeline.items().find_map(|item| match &item.kind {
                fleet::timeline::ItemKind::User { images, .. } => {
                    images.iter().find(|i| i.content_key() == key).cloned()
                }
                _ => None,
            })
        })
    }

    fn release(&mut self, spec: &ViewSpec) {
        if let ViewSpec::Conversation { host, agent, .. } = spec {
            self.up.close(host, agent);
        }
    }

    /// Queues an answer for a phone. One that cannot take it is dropped:
    /// it would wait forever for the answer otherwise.
    fn reply(&mut self, conn: u64, message: Value) {
        let Some(phone) = self.phones.get(&conn) else {
            return;
        };
        if send(&phone.out, &message).is_err() {
            self.drop_phone(conn);
        }
    }

    fn drop_phone(&mut self, conn: u64) {
        if let Some(phone) = self.phones.remove(&conn) {
            for sub in phone.subs.values() {
                self.release(&sub.spec);
            }
        }
    }

    fn sub_mut(&mut self, conn: u64, id: &str) -> Option<&mut Sub> {
        self.phones.get_mut(&conn)?.subs.get_mut(id)
    }

    fn on_message(&mut self, conn: u64, message: ClientMsg) {
        match message {
            ClientMsg::Hello { .. } => {
                self.reply(
                    conn,
                    proto::error(None, code::BAD_REQUEST, "already signed in"),
                );
            }
            ClientMsg::Ping { t } => self.reply(conn, json!({ "type": "pong", "t": t })),
            ClientMsg::Invalid { req, problem } => {
                let code = if problem.starts_with("unknown view") {
                    code::UNKNOWN_VIEW
                } else {
                    code::BAD_REQUEST
                };
                let message = match req {
                    Some(req) => proto::result_err(Some(&req), code, &problem),
                    None => proto::error(None, code, &problem),
                };
                self.reply(conn, message);
            }
            ClientMsg::Subscribe { id, view } => self.subscribe(conn, id, view),
            ClientMsg::Unsubscribe { id } => {
                let removed = self
                    .phones
                    .get_mut(&conn)
                    .and_then(|phone| phone.subs.remove(&id));
                if let Some(sub) = removed {
                    self.release(&sub.spec);
                }
            }
            ClientMsg::Window { id, offset, limit } => {
                if let Some(sub) = self.sub_mut(conn, &id) {
                    match &mut sub.spec {
                        ViewSpec::Agents { spec } => {
                            spec.offset = offset;
                            spec.limit = limit;
                        }
                        ViewSpec::NeedsYou { limit: l } => *l = limit,
                        ViewSpec::Map { .. } | ViewSpec::Conversation { .. } | ViewSpec::Usage => {}
                    }
                    sub.stale = true;
                }
                self.send_owed(conn);
            }
            ClientMsg::Resync { id } => {
                if let Some(sub) = self.sub_mut(conn, &id) {
                    sub.stale = true;
                }
                self.send_owed(conn);
            }
            ClientMsg::Focus { id, agents } => {
                if let Some(sub) = self.sub_mut(conn, &id)
                    && let ViewSpec::Map { focus, .. } = &mut sub.spec
                {
                    *focus = agents;
                    sub.poke = true;
                }
                self.send_owed(conn);
            }
            ClientMsg::Older { req, id, turns } => self.older(conn, req, id, turns),
            ClientMsg::Detail { req, id, item } => {
                let detail = self
                    .phones
                    .get(&conn)
                    .and_then(|p| p.subs.get(&id))
                    .and_then(|sub| match &sub.spec {
                        ViewSpec::Conversation { host, agent, .. } => {
                            self.up.sessions.get(&(host.clone(), agent.clone()))
                        }
                        _ => None,
                    })
                    .and_then(|session| views::detail(&session.timeline, &item, &self.limits));
                let message = match detail {
                    Some(detail) => proto::result_ok(req.as_deref(), json!({ "detail": detail })),
                    None => proto::result_err(req.as_deref(), code::NOT_FOUND, "no such item"),
                };
                self.reply(conn, message);
            }
            ClientMsg::Action { req, action, body } => {
                if let Err((code, message)) = self.act(conn, req.clone(), &action, &body) {
                    self.reply(conn, proto::result_err(req.as_deref(), code, &message));
                }
            }
            ClientMsg::CreateOptions { req, host } => {
                if self.up.fleet.host(&host_id(&host)).is_none() {
                    let message =
                        proto::result_err(req.as_deref(), code::NOT_FOUND, "no such host");
                    return self.reply(conn, message);
                }
                self.up.create_options(&host, Reply { conn, req });
            }
        }
    }

    /// A `create` action's agent, checked against what Reef knows: the
    /// host, the workspace or directory, the labels and the prompt. The
    /// provider and its choices are checked against the host's providers.
    fn new_agent(&self, body: &Value) -> Result<(String, NewAgent), (&'static str, String)> {
        let bad = |what: &str| (code::BAD_REQUEST, what.to_string());
        let text = |key: &str| {
            body[key]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        };
        let host = text("host").ok_or_else(|| bad("no host"))?;
        let found = self
            .up
            .fleet
            .host(&host_id(&host))
            .ok_or((code::NOT_FOUND, "no such host".to_string()))?;
        let provider = text("provider").ok_or_else(|| bad("no provider"))?;
        let (workspace, cwd) = match (text("workspace"), text("cwd")) {
            (Some(id), _) => {
                let workspace = found
                    .directory
                    .workspaces
                    .get(&id)
                    .ok_or((code::NOT_FOUND, "no such workspace".to_string()))?;
                (Some(id), workspace.directory().to_string())
            }
            (None, Some(cwd)) => {
                if cwd.len() > 4096 || cwd.contains('\n') {
                    return Err(bad("That is not a directory"));
                }
                // A directory a workspace already has is that workspace.
                let known = (found.directory.workspaces.values())
                    .find(|w| w.archiving_at.is_none() && w.directory() == cwd)
                    .map(|w| w.id.clone());
                (known, cwd)
            }
            (None, None) => return Err(bad("Pick a workspace or a directory")),
        };
        let labels = match body["labels"].is_null() {
            true => None,
            false => Some(labels(&body["labels"]).map_err(bad)?),
        };
        let message_id = text("message_id").unwrap_or_else(fleet::clock::uuid_v4);
        let prompt = fleet::prompt::Prompt {
            text: body["text"].as_str().unwrap_or_default().trim().to_string(),
            ..Default::default()
        };
        let new = NewAgent {
            provider,
            model: text("model"),
            mode: text("mode"),
            thinking: text("thinking"),
            workspace,
            cwd,
            labels,
            prompt,
            message_id,
        };
        Ok((host, new))
    }

    /// Starts an action. Daemon writes answer later, through the
    /// upstream outbox; settings answer at once. `Err` is an answer to
    /// send now.
    fn act(
        &mut self,
        conn: u64,
        req: Option<String>,
        action: &str,
        body: &Value,
    ) -> Result<(), (&'static str, String)> {
        let reply = Reply {
            conn,
            req: req.clone(),
        };
        let text = |key: &str| body[key].as_str().map(str::to_string);
        let bad = |what: &str| (code::BAD_REQUEST, what.to_string());
        if action == "settings" {
            if let Some(on) = body["notify_default"].as_bool() {
                self.settings.notify_default = on;
            }
            if let Some(on) = body["show_background"].as_bool() {
                self.settings.show_background = on;
            }
            self.settings_changed();
            self.reply(conn, proto::result_ok(req.as_deref(), json!({})));
            return Ok(());
        }
        if action == "host_add" || action == "host_remove" {
            return self.edit_hosts(conn, req, action, body);
        }
        if action == "create" {
            let (host, new) = self.new_agent(body)?;
            self.up.create(&host, new, reply);
            return Ok(());
        }
        if !proto::ACTIONS.contains(&action) {
            return Err(bad("unknown action"));
        }
        let host = text("host").ok_or_else(|| bad("no host"))?;
        let agent = text("agent").ok_or_else(|| bad("no agent"))?;
        if self.up.fleet.agent(&host_id(&host), &agent).is_none() {
            return Err((code::NOT_FOUND, "no such agent".into()));
        }
        match action {
            "notify" => {
                let key = views::agent_key(&host, &agent);
                self.settings.set_notify(&key, body["on"].as_bool());
                self.settings_changed();
                self.reply(conn, proto::result_ok(req.as_deref(), json!({})));
            }
            "permission" => {
                let ask = text("ask").ok_or_else(|| bad("no ask"))?;
                self.up
                    .pending_request(&host, &agent, &ask)
                    .ok_or((code::NOT_FOUND, "That request is no longer pending".into()))?;
                let allow = match body["decision"].as_str() {
                    Some("allow") => true,
                    Some("deny") => false,
                    _ => return Err(bad("decision is allow or deny")),
                };
                let fallback = if allow { "accept" } else { "reject" };
                let action_id = text("action_id").unwrap_or_else(|| fallback.into());
                let response = if allow {
                    json!({ "behavior": "allow", "selectedActionId": action_id })
                } else {
                    json!({
                        "behavior": "deny",
                        "selectedActionId": action_id,
                        "message": text("message").unwrap_or_else(|| "Denied by user".into()),
                    })
                };
                self.up
                    .respond_permission(&host, &agent, &ask, response, reply);
            }
            "answer" => {
                let ask = text("ask").ok_or_else(|| bad("no ask"))?;
                let request = self
                    .up
                    .pending_request(&host, &agent, &ask)
                    .ok_or((code::NOT_FOUND, "That question is no longer pending".into()))?;
                let questions = fleet::questions::parse(&request["input"])
                    .ok_or_else(|| bad("that request is not a question"))?;
                let response = if body["dismiss"] == true {
                    fleet::questions::dismiss(&request, &questions, &[], &[])
                } else {
                    let (selected, others) = selections(&body["answers"], questions.len());
                    let unanswered = questions
                        .iter()
                        .enumerate()
                        .any(|(i, q)| !q.answered(&selected[i], &others[i]));
                    if unanswered {
                        return Err(bad("Answer every question first"));
                    }
                    let answers = fleet::questions::answers(&questions, &selected, &others);
                    fleet::questions::submit(&request, answers)
                };
                self.up
                    .respond_permission(&host, &agent, &ask, response, reply);
            }
            "stop" => self.up.stop(&host, &agent, reply),
            "send" | "steer" => {
                let id = text("message_id").filter(|id| !id.is_empty());
                let images = images(&body["images"]).map_err(bad)?;
                let message = text("text")
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| (!images.is_empty()).then(String::new));
                match (action, id, message) {
                    // Steering a message that waits in the queue.
                    ("steer", Some(id), None) => self.up.steer_queued(&host, &agent, &id, reply),
                    (_, Some(id), Some(message)) => {
                        let steer = action == "steer";
                        let prompt = fleet::prompt::Prompt {
                            text: message,
                            images,
                        };
                        self.up.send(&host, &agent, prompt, id, steer, reply);
                    }
                    (_, None, _) => return Err(bad("no message_id")),
                    (_, _, None) => return Err(bad("nothing to send")),
                }
            }
            "unqueue" => {
                let id = text("message_id").ok_or_else(|| bad("no message_id"))?;
                self.up.unqueue(&host, &agent, &id, reply);
            }
            "archive" => self.up.archive(&host, &agent, reply),
            "delete" => self.up.delete(&host, &agent, reply),
            "relabel" => {
                if !body["labels"].is_array() {
                    return Err(bad("labels is a list of label names"));
                }
                let labels = labels(&body["labels"]).map_err(bad)?;
                self.up.relabel(&host, &agent, labels, reply);
            }
            _ => return Err(bad("unknown action")),
        }
        Ok(())
    }

    /// Adds or removes a host, saved to the hosts file first so what a
    /// phone sees is what the next start will have.
    fn edit_hosts(
        &mut self,
        conn: u64,
        req: Option<String>,
        action: &str,
        body: &Value,
    ) -> Result<(), (&'static str, String)> {
        let bad = |what: &str| (code::BAD_REQUEST, what.to_string());
        let name = body["name"].as_str().unwrap_or_default().trim().to_string();
        let mut configs: Vec<HostConfig> = self
            .up
            .fleet
            .hosts
            .iter()
            .map(|h| h.config.clone())
            .collect();
        let added = if action == "host_add" {
            let config = new_host(&name, body).map_err(bad)?;
            if configs.iter().any(|h| h.name.eq_ignore_ascii_case(&name)) {
                return Err(bad("A host has that name already"));
            }
            configs.push(config.clone());
            Some(config)
        } else {
            if !configs.iter().any(|h| h.name == name) {
                return Err((code::NOT_FOUND, "no such host".into()));
            }
            configs.retain(|h| h.name != name);
            None
        };
        if let Some(path) = &self.hosts_file {
            config::save_hosts(path, &configs)
                .map_err(|err| (code::UPSTREAM, format!("Could not save the hosts: {err}")))?;
        }
        match added {
            Some(config) => self.up.add_host(config),
            None => self.up.remove_host(&name),
        };
        self.reply(conn, proto::result_ok(req.as_deref(), json!({})));
        Ok(())
    }

    /// Writes the label catalog to its file, if Reef has one.
    fn save_labels(&mut self) {
        let Some(catalog) = &self.up.fleet.labels else {
            return;
        };
        self.labels_saved = Some(catalog.revision);
        if let Some(path) = &self.labels_path
            && let Err(err) = catalog::save(path, catalog)
        {
            eprintln!("reef: saving labels: {err}");
        }
    }

    /// A desktop's `/api/labels`: the catalog, after `edit` if one came.
    fn labels_request(&mut self, edit: Option<Value>) -> Value {
        let Some(catalog) = self.up.fleet.labels.as_mut() else {
            return json!({ "ok": false, "error": "Reef keeps no labels" });
        };
        let answer = match edit {
            None => json!({ "ok": true }),
            Some(edit) => match catalog::edit(catalog, &edit) {
                Ok(mut answer) => {
                    answer["ok"] = json!(true);
                    self.save_labels();
                    self.up.dirty_fleet = true;
                    answer
                }
                Err(error) => json!({ "ok": false, "error": error }),
            },
        };
        let mut answer = answer;
        answer["catalog"] = json!(self.up.fleet.labels);
        answer
    }

    /// Saves the settings and tells every phone; agent rows carry their
    /// own choice, so the lists are worked out again.
    fn settings_changed(&mut self) {
        self.save_settings();
        self.up.dirty_fleet = true;
        let message = json!({ "type": "settings", "settings": self.settings.public() });
        self.broadcast(&message);
    }

    fn save_settings(&self) {
        if let Some(path) = &self.settings_path
            && let Err(err) = self.settings.save(path)
        {
            eprintln!("reef: saving settings: {err}");
        }
    }

    /// Drops the notify choices of agents no host has any more, once every
    /// host is connected and has sent its whole agent list: an agent
    /// missing from a host still loading is not gone.
    fn prune_settings(&mut self) {
        if !self.up.all_synced() {
            return;
        }
        let fleet = &self.up.fleet;
        let before = self.settings.notify.len();
        self.settings.notify.retain(|key, _| {
            key.split_once('/')
                .is_some_and(|(host, agent)| fleet.agent(&host_id(host), agent).is_some())
        });
        if self.settings.notify.len() != before {
            self.save_settings();
        }
    }

    fn subscribe(&mut self, conn: u64, id: String, view: ViewSpec) {
        let Some(phone) = self.phones.get(&conn) else {
            return;
        };
        if !phone.subs.contains_key(&id) && phone.subs.len() >= self.limits.max_subscriptions {
            let message = proto::error(Some(&id), code::TOO_MANY, "too many views open");
            self.reply(conn, message);
            return;
        }
        if let ViewSpec::Conversation { host, agent, .. } = &view {
            if self.up.fleet.agent(&host_id(host), agent).is_none() {
                let message = proto::error(Some(&id), code::NOT_FOUND, "no such agent");
                self.reply(conn, message);
                return;
            }
            self.up.open(host, agent);
        }
        let sent = match view {
            ViewSpec::Conversation { .. } => Sent::Conversation(Box::default()),
            _ => Sent::Nothing,
        };
        let sub = Sub {
            spec: view,
            seq: 0,
            stale: true,
            poke: false,
            sent,
        };
        let replaced = self
            .phones
            .get_mut(&conn)
            .and_then(|phone| phone.subs.insert(id, sub));
        if let Some(old) = replaced {
            self.release(&old.spec);
        }
        self.send_owed(conn);
    }

    fn older(&mut self, conn: u64, req: Option<String>, id: String, turns: usize) {
        let found = self
            .phones
            .get(&conn)
            .and_then(|p| p.subs.get(&id))
            .map(|sub| match &sub.spec {
                ViewSpec::Conversation { host, agent, .. } => Some((host.clone(), agent.clone())),
                _ => None,
            });
        let key = match found {
            Some(Some(key)) => key,
            Some(None) => {
                let message =
                    proto::result_err(req.as_deref(), code::BAD_REQUEST, "not a conversation");
                self.reply(conn, message);
                return;
            }
            None => {
                let message = proto::result_err(req.as_deref(), code::NOT_FOUND, "no such view");
                self.reply(conn, message);
                return;
            }
        };
        let pages = self.up.sessions.get(&key).map_or(0, |s| s.pages);
        if let Some(Sub {
            sent: Sent::Conversation(sent),
            ..
        }) = self.sub_mut(conn, &id)
        {
            sent.older = Some(Older {
                req,
                turns,
                pages,
                fetched: false,
            });
        }
        // Settled at the next flush: with what is loaded, or once the page
        // it needs lands.
        self.up.dirty_sessions.insert(key);
    }

    /// Every changed view's delta, and the snapshots owed.
    fn flush(&mut self) {
        let fleet_changed = std::mem::take(&mut self.up.dirty_fleet);
        let sessions = std::mem::take(&mut self.up.dirty_sessions);
        let usage_changed = std::mem::take(&mut self.up.dirty_usage);
        if fleet_changed {
            self.prune_settings();
            catalog::keep_up(&mut self.up.fleet, self.started, std::time::Instant::now());
            // Relabels may have made labels too.
            let revision = self.up.fleet.labels.as_ref().map(|c| c.revision);
            if revision != self.labels_saved {
                self.save_labels();
            }
            // Hosts and the label colours travel together.
            let hosts = json!({
                "type": "hosts",
                "hosts": views::hosts(&self.up.fleet, &self.up.host_since()),
                "labels": lists::label_colors(&self.up.fleet),
            });
            if hosts != self.hosts_sent {
                self.hosts_sent = hosts.clone();
                let message = hosts;
                let conns: Vec<u64> = self.phones.keys().copied().collect();
                for conn in conns {
                    self.reply(conn, message.clone());
                }
            }
        }
        self.send_views(None, fleet_changed, usage_changed, &sessions);
    }

    /// Only the snapshots `conn` is owed, now rather than at the next tick.
    fn send_owed(&mut self, conn: u64) {
        self.send_views(Some(conn), false, false, &HashSet::new());
    }

    fn send_views(
        &mut self,
        only: Option<u64>,
        fleet_changed: bool,
        usage_changed: bool,
        sessions: &HashSet<(String, String)>,
    ) {
        // Lists are worked out once, whoever asks.
        let mut needs_you: Option<Vec<(String, Value)>> = None;
        let mut usage: Option<Vec<(String, Value)>> = None;
        let mut fetches: Vec<(u64, String)> = Vec::new();
        // Agents shown whose latest reply is not known yet.
        let mut wanted: Vec<(String, String)> = Vec::new();
        let cx = lists::Context {
            settings: &self.settings,
            replies: &self.up.replies.known,
            labels: self.up.fleet.labels.as_ref(),
        };
        let mut drop = Vec::new();
        for (conn, phone) in self.phones.iter_mut() {
            if only.is_some_and(|only| only != *conn) {
                continue;
            }
            // A phone that fell behind gets its snapshots once its queue
            // has room for them again.
            let room = phone.out.capacity() > phone.out.max_capacity() / 2;
            let mut lagging = false;
            for (id, sub) in phone.subs.iter_mut() {
                let touched = match &sub.spec {
                    ViewSpec::Conversation { host, agent, .. } => {
                        fleet_changed || sessions.contains(&(host.clone(), agent.clone()))
                    }
                    ViewSpec::Usage => usage_changed,
                    _ => fleet_changed,
                };
                if (sub.stale && !room) || (!touched && !sub.stale && !sub.poke) {
                    continue;
                }
                sub.poke = false;
                let mut wants = Wants::default();
                let messages = match &sub.spec {
                    ViewSpec::NeedsYou { limit } => {
                        let rows =
                            needs_you.get_or_insert_with(|| views::needs_you(&self.up.fleet, &cx));
                        let out = lists::ListOut {
                            total: rows.len(),
                            groups: Vec::new(),
                            rows: views::window(rows, 0, *limit),
                        };
                        wanted.extend(shown_agents(&out));
                        list_message(id, sub, out, 0).into_iter().collect()
                    }
                    ViewSpec::Agents { spec } => {
                        let out = lists::agent_list(&self.up.fleet, spec, &cx);
                        wanted.extend(shown_agents(&out));
                        let offset = spec.offset;
                        list_message(id, sub, out, offset).into_iter().collect()
                    }
                    ViewSpec::Map { spec, focus } => {
                        let out = lists::map_view(&self.up.fleet, spec, focus, &cx);
                        wanted.extend(out.cards.iter().filter_map(|(key, _)| {
                            let (host, id) = key.split_once('/')?;
                            Some((host.to_string(), id.to_string()))
                        }));
                        map_message(id, sub, out).into_iter().collect()
                    }
                    ViewSpec::Conversation { .. } => {
                        conversation_messages(id, sub, &self.up, &cx, &self.limits, &mut wants)
                    }
                    ViewSpec::Usage => {
                        let rows = usage.get_or_insert_with(|| {
                            crate::usage::rows(&self.up.usage, usage_source::unix_now())
                        });
                        let out = lists::ListOut {
                            total: rows.len(),
                            groups: Vec::new(),
                            rows: rows.clone(),
                        };
                        list_message(id, sub, out, 0).into_iter().collect()
                    }
                };
                if wants.fetch_older {
                    fetches.push((*conn, id.clone()));
                }
                for message in messages {
                    match send(&phone.out, &message) {
                        Ok(()) => {}
                        // An `older`'s answer is owed like any result
                        // (3.13): one that does not fit closes the socket.
                        Err(Full::Lagging) if message["type"] != "result" => lagging = true,
                        Err(Full::Lagging | Full::Gone) => {
                            if !drop.contains(conn) {
                                drop.push(*conn);
                            }
                        }
                    }
                }
            }
            if lagging {
                // Every view of it resyncs: deltas already queued may
                // build on ones that were dropped.
                for sub in phone.subs.values_mut() {
                    sub.stale = true;
                }
            }
        }
        for conn in drop {
            self.drop_phone(conn);
        }
        for (conn, id) in fetches {
            self.fetch_older(conn, &id);
        }
        self.up.want_replies(wanted);
        self.up.pump_replies();
    }

    /// Asks the daemon for the page an `older` needs; if there is none to
    /// ask for, settles it with what is loaded.
    fn fetch_older(&mut self, conn: u64, id: &str) {
        let Some(sub) = self.phones.get(&conn).and_then(|p| p.subs.get(id)) else {
            return;
        };
        let ViewSpec::Conversation { host, agent, .. } = &sub.spec else {
            return;
        };
        let key = (host.clone(), agent.clone());
        let asked = self.up.fetch_older(&key.0, &key.1);
        let pages = self.up.sessions.get(&key).map_or(0, |s| s.pages);
        if let Some(Sub {
            sent: Sent::Conversation(sent),
            ..
        }) = self.sub_mut(conn, id)
            && let Some(older) = &mut sent.older
        {
            older.fetched = true;
            older.pages = pages;
        }
        if !asked {
            // Settle now from what is there.
            self.up.dirty_sessions.insert(key);
        }
    }
}

/// A question form's answers, one `{"selected": [option indexes], "other":
/// "typed text"}` per question, in order, as `fleet::questions` takes them.
fn selections(
    answers: &Value,
    count: usize,
) -> (Vec<std::collections::BTreeSet<usize>>, Vec<String>) {
    let mut selected = vec![std::collections::BTreeSet::new(); count];
    let mut others = vec![String::new(); count];
    for (i, answer) in answers
        .as_array()
        .into_iter()
        .flatten()
        .take(count)
        .enumerate()
    {
        selected[i] = answer["selected"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n.as_u64().map(|n| n as usize))
            .collect();
        others[i] = answer["other"].as_str().unwrap_or_default().to_string();
    }
    (selected, others)
}

/// The agents a list's rows show, as (host, agent id).
fn shown_agents(out: &lists::ListOut) -> Vec<(String, String)> {
    out.rows
        .iter()
        .filter_map(|(_, row)| Some((row["host"].as_str()?.into(), row["id"].as_str()?.into())))
        .collect()
}

/// A list view's next message: a snapshot when owed, else a delta when
/// something in its window or its groups changed.
fn list_message(id: &str, sub: &mut Sub, out: lists::ListOut, offset: usize) -> Option<Value> {
    let lists::ListOut {
        total,
        groups,
        rows,
    } = out;
    let groups = Value::Array(groups);
    if sub.stale || !matches!(sub.sent, Sent::List { .. }) {
        sub.stale = false;
        let mut message = json!({
            "type": "snapshot", "id": id, "seq": sub.seq, "total": total, "offset": offset,
            "rows": rows.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>(),
        });
        if groups.as_array().is_some_and(|g| !g.is_empty()) {
            message["groups"] = groups.clone();
        }
        sub.sent = Sent::List {
            total,
            groups,
            rows,
        };
        return Some(message);
    }
    let Sent::List {
        total: old_total,
        groups: old_groups,
        rows: old,
    } = &sub.sent
    else {
        return None;
    };
    let mut delta = views::diff_rows(old, &rows).unwrap_or_default();
    if *old_total != total {
        delta.insert("total".into(), json!(total));
    }
    if *old_groups != groups {
        delta.insert("groups".into(), groups.clone());
    }
    if delta.is_empty() {
        return None;
    }
    sub.seq += 1;
    delta.insert("type".into(), json!("delta"));
    delta.insert("id".into(), json!(id));
    delta.insert("seq".into(), json!(sub.seq));
    sub.sent = Sent::List {
        total,
        groups,
        rows,
    };
    Some(Value::Object(delta))
}
/// A map's next message. Its delta nests the row-style changes of its
/// two lists, `tiles` and `cards`, beside `lanes` when they changed.
fn map_message(id: &str, sub: &mut Sub, out: lists::MapOut) -> Option<Value> {
    let lists::MapOut {
        lanes,
        tiles,
        cards,
    } = out;
    let lanes = Value::Array(lanes);
    let values = |rows: &[(String, Value)]| rows.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>();
    if sub.stale || !matches!(sub.sent, Sent::Map { .. }) {
        sub.stale = false;
        let message = json!({
            "type": "snapshot", "id": id, "seq": sub.seq,
            "lanes": lanes, "tiles": values(&tiles), "cards": values(&cards),
        });
        sub.sent = Sent::Map {
            lanes,
            tiles,
            cards,
        };
        return Some(message);
    }
    let Sent::Map {
        lanes: old_lanes,
        tiles: old_tiles,
        cards: old_cards,
    } = &sub.sent
    else {
        return None;
    };
    let mut delta = Map::new();
    if *old_lanes != lanes {
        delta.insert("lanes".into(), lanes.clone());
    }
    if let Some(changes) = views::diff_rows(old_tiles, &tiles) {
        delta.insert("tiles".into(), Value::Object(changes));
    }
    if let Some(changes) = views::diff_rows(old_cards, &cards) {
        delta.insert("cards".into(), Value::Object(changes));
    }
    if delta.is_empty() {
        return None;
    }
    sub.seq += 1;
    delta.insert("type".into(), json!("delta"));
    delta.insert("id".into(), json!(id));
    delta.insert("seq".into(), json!(sub.seq));
    sub.sent = Sent::Map {
        lanes,
        tiles,
        cards,
    };
    Some(Value::Object(delta))
}

/// How many times `items_max` a conversation's window may grow to, by new
/// turns and `older`, before its oldest turns go.
const WINDOW_GROWTH: usize = 4;

/// A conversation's next messages: a snapshot or a delta, and the answer
/// to an `older` it settled.
fn conversation_messages(
    id: &str,
    sub: &mut Sub,
    up: &Upstream,
    cx: &lists::Context,
    limits: &Limits,
    wants: &mut Wants,
) -> Vec<Value> {
    let ViewSpec::Conversation { host, agent, turns } = &sub.spec else {
        return Vec::new();
    };
    let Sent::Conversation(sent) = &mut sub.sent else {
        return Vec::new();
    };
    let Some(session) = up.sessions.get(&(host.clone(), agent.clone())) else {
        return Vec::new();
    };
    let row = up.fleet.host(&host_id(host)).and_then(|h| {
        h.directory
            .agents
            .get(agent)
            .map(|a| views::agent_row(h, a, cx))
    });
    let cwd = up
        .fleet
        .agent(&host_id(host), agent)
        .map(|a| a.cwd.clone())
        .unwrap_or_default();
    let timeline = &session.timeline;

    let epoch = timeline.cursor.as_ref().map(|c| c.epoch.clone());
    let mut snapshot = sub.stale || sent.view.is_none();
    let anchored = sent
        .anchor
        .as_deref()
        .is_some_and(|a| timeline.items().any(|item| item.id == a));
    if (sent.epoch.is_some() && sent.epoch != epoch) || (sent.anchor.is_some() && !anchored) {
        // The daemon rebuilt the timeline: start over from its end.
        sent.anchor = None;
        snapshot = true;
    }
    let mut from = views::anchor_index(timeline, sent.anchor.as_deref(), *turns, limits);

    // An `older` shows what is loaded before the window; when nothing is,
    // it waits for one more page from the daemon.
    let mut settled = None;
    if let Some(older) = sent.older.clone() {
        let loaded_before = timeline
            .items()
            .take(from)
            .any(|item| !fleet::display::hidden_in_stream(&item.kind));
        let page_landed = older.fetched && session.pages > older.pages;
        if !loaded_before && timeline.has_older && !page_landed {
            if !older.fetched && !session.fetching() {
                wants.fetch_older = true;
            }
        } else {
            from = views::older_index(timeline, from, older.turns, limits);
            sent.older = None;
            settled = Some(older.req);
        }
    }
    // The window stays bounded (3.12): a snapshot carries the last
    // `items_max` items at most, and a window that new turns or `older`
    // widened loses its oldest turns once it passes WINDOW_GROWTH times
    // that. Not while an `older` is being settled, which would undo it.
    if settled.is_none() && sent.older.is_none() {
        let budget = if snapshot {
            limits.items_max
        } else {
            limits.items_max.saturating_mul(WINDOW_GROWTH)
        };
        from = from.max(views::window_floor(timeline, budget));
    }
    if !timeline.is_empty() {
        sent.anchor = views::item_id(timeline, from);
    }
    sent.epoch = epoch;
    let directory = up.fleet.host(&host_id(host)).map(|h| &h.directory);
    let titles = |id: &str| {
        let agent = directory?.agents.get(id)?;
        Some(agent.display_title())
    };
    let view = views::conversation(timeline, &cwd, from, limits, &titles);

    let mut fields = Map::new();
    fields.insert("agent".into(), row.unwrap_or(Value::Null));
    fields.insert("loading".into(), json!(session.loading));
    fields.insert("error".into(), json!(session.error));
    fields.insert("has_older".into(), json!(view.has_older));
    fields.insert("working".into(), json!(view.working));
    fields.insert("asks".into(), Value::Array(view.asks.clone()));
    let queued: Vec<Value> = up
        .queues
        .get(&(host.clone(), agent.clone()))
        .into_iter()
        .flatten()
        .map(|m| {
            let mut queued =
                json!({ "id": m.id, "text": proto::clip(&m.prompt.text, limits.text_max).0 });
            if !m.prompt.images.is_empty() {
                queued["images"] = json!(m.prompt.images.len());
            }
            if let Some(error) = &m.error {
                queued["error"] = json!(error);
            }
            queued
        })
        .collect();
    fields.insert("queued".into(), Value::Array(queued));

    let mut out = Vec::new();
    if snapshot {
        sub.stale = false;
        let mut message = fields.clone();
        message.insert("type".into(), json!("snapshot"));
        message.insert("id".into(), json!(id));
        message.insert("seq".into(), json!(sub.seq));
        let turns: Vec<Value> = view.turns.iter().map(views::turn_json).collect();
        message.insert("turns".into(), Value::Array(turns));
        out.push(Value::Object(message));
    } else {
        let old: &[TurnView] = sent.view.as_ref().map_or(&[], |v| v.turns.as_slice());
        let mut delta = views::diff_turns(old, &view.turns);
        for (key, value) in &fields {
            if sent.fields.get(key) != Some(value) {
                delta.insert(key.clone(), value.clone());
            }
        }
        if !delta.is_empty() {
            sub.seq += 1;
            delta.insert("type".into(), json!("delta"));
            delta.insert("id".into(), json!(id));
            delta.insert("seq".into(), json!(sub.seq));
            out.push(Value::Object(delta));
        }
    }
    if let Some(req) = settled {
        let has_older = view.has_older;
        out.push(proto::result_ok(
            req.as_deref(),
            json!({ "has_older": has_older }),
        ));
    }
    sent.fields = fields;
    sent.view = Some(view);
    out
}
