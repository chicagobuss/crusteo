//! A small in-process Paseo daemon, for tests and the `--demo` fleet. It
//! speaks enough of protocol v1 for Reef: hello, `server_info`, the
//! directory reads with subscriptions, timeline subscriptions and paged
//! timeline reads, a providers snapshot, pushes the caller makes, and the
//! writes Reef's actions send (permission answers, cancel, send,
//! archive, create, delete, label assignments), which it records whole.
//! Given usage reports, it says it has usage sources and
//! answers `usage.list_reports.request` as a daemon does: one update per
//! report, then the response. It answers any other request with
//! `rpc_error` and records that too, so a test can prove exactly what the
//! Reef sent. Told to, it has Paseo 0.12's background workspaces: it says
//! so in `server_info`, and leaves them and their agents out of reads and
//! pushes for a client that does not send `includeBackground`.
//! [`FakeFeed`] is a usage feed to go with it.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// One timeline row as the daemon serves it, projected.
#[derive(Debug, Clone)]
struct Row {
    seq: u64,
    entry: Value,
}

#[derive(Default)]
struct Socket {
    tx: Option<mpsc::UnboundedSender<String>>,
    agents_sub: Option<String>,
    workspaces_sub: Option<String>,
    events_sub: Option<String>,
    timeline_subs: Vec<(String, Vec<String>)>,
    /// The directory subscriptions asked for background workspaces.
    include_background: bool,
}

#[derive(Default)]
struct State {
    server_id: String,
    workspaces: BTreeMap<String, Value>,
    /// Label definitions: name and colour.
    labels: Vec<Value>,
    agents: BTreeMap<String, Value>,
    timelines: HashMap<String, Vec<Row>>,
    epoch: String,
    sockets: HashMap<u64, Socket>,
    next_socket: u64,
    next_sub: u64,
    /// Every request type received, in order.
    requests: Vec<String>,
    /// Requests it refused: anything but the reads and writes it knows.
    refused: Vec<Value>,
    /// The writes it served, whole.
    writes: Vec<Value>,
    /// Each agent's current turn id.
    turns: HashMap<String, String>,
    next_turn: u64,
    /// Client ids that said hello.
    clients: Vec<String>,
    /// Sends to refuse next, as a busy daemon would.
    fail_sends: usize,
    /// Hold replies to sends until released, as a slow daemon would.
    hold_sends: bool,
    held: Vec<(mpsc::UnboundedSender<String>, String)>,
    /// Usage reports, as `usage.list_reports.update` payloads carry them;
    /// None for a daemon without usage sources.
    usage_reports: Option<Vec<Value>>,
    /// The providers snapshot's entries, in Paseo's plain encoding.
    providers: Vec<Value>,
    /// Agents and workspaces it made, for their ids.
    created: u64,
    /// Advertises `backgroundWorkspaces` and hides them unless asked.
    background_workspaces: bool,
    /// The directory reads received, whole.
    directory_reads: Vec<Value>,
    /// Image files on the host by path, mime type and base64, as
    /// `file_explorer_request` reads them.
    files: HashMap<String, (String, String)>,
}

/// A running fake daemon. Dropping it does not stop it; tests end with
/// their runtime.
#[derive(Clone)]
pub struct FakeDaemon {
    pub addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

fn session(message: Value) -> String {
    json!({ "type": "session", "message": message }).to_string()
}

impl FakeDaemon {
    /// Listens on an ephemeral port of 127.0.0.1.
    pub async fn start(server_id: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(State {
            server_id: server_id.to_string(),
            epoch: format!("epoch-{server_id}"),
            providers: providers(),
            ..Default::default()
        }));
        let daemon = Self { addr, state };
        let accepting = daemon.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let daemon = accepting.clone();
                tokio::spawn(async move { daemon.serve(stream).await });
            }
        });
        Ok(daemon)
    }

    pub fn endpoint(&self) -> String {
        self.addr.to_string()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Request types received so far.
    pub fn requests(&self) -> Vec<String> {
        self.lock().requests.clone()
    }

    /// Requests refused because they are not reads.
    pub fn refused(&self) -> Vec<Value> {
        self.lock().refused.clone()
    }

    pub fn clients(&self) -> Vec<String> {
        self.lock().clients.clone()
    }

    /// Closes every connection, as a daemon restart would.
    pub fn drop_connections(&self) {
        let mut state = self.lock();
        for socket in state.sockets.values_mut() {
            socket.tx = None;
        }
    }

    /// Defines a label and its colour, as Paseo's label list has it.
    pub fn add_label(&self, name: &str, color: &str) {
        let mut state = self.lock();
        if !state.labels.iter().any(|l| l["name"] == name) {
            state.labels.push(json!({ "name": name, "color": color }));
        }
    }

    pub fn add_workspace(&self, workspace: Value) {
        let id = workspace["id"].as_str().unwrap_or_default().to_string();
        self.lock().workspaces.insert(id, workspace);
    }

    /// Adds or replaces an agent and tells subscribed sockets.
    pub fn upsert_agent(&self, agent: Value) {
        let mut state = self.lock();
        let id = agent["id"].as_str().unwrap_or_default().to_string();
        state.agents.insert(id, agent.clone());
        push_agent(&state, &agent);
    }

    /// Changes some fields of an agent and pushes the result.
    pub fn patch_agent(&self, id: &str, fields: Value) {
        patch_agent(&mut self.lock(), id, fields);
    }

    /// Every write Reef sent, whole, in order.
    pub fn writes(&self) -> Vec<Value> {
        self.lock().writes.clone()
    }

    /// Gives the daemon usage sources with these reports (see
    /// [`usage_report`]). Set before Reef connects: the feature is in
    /// the hello's answer.
    pub fn set_usage_reports(&self, reports: Vec<Value>) {
        self.lock().usage_reports = Some(reports);
    }

    /// Makes it a Paseo 0.12 daemon with background workspaces. Set before
    /// Reef connects: the feature is in the hello's answer.
    pub fn set_background_workspaces(&self, on: bool) {
        self.lock().background_workspaces = on;
    }

    /// The `fetch_workspaces_request`s and `fetch_agents_request`s received.
    pub fn directory_reads(&self) -> Vec<Value> {
        self.lock().directory_reads.clone()
    }

    /// Replaces the providers snapshot's entries (see [`providers`]).
    pub fn set_providers(&self, entries: Vec<Value>) {
        self.lock().providers = entries;
    }

    /// Replaces the providers, then tells sockets watching session events,
    /// as a Burrow does when a CLI changes under it.
    pub fn update_providers(&self, entries: Vec<Value>) {
        let mut state = self.lock();
        state.providers = entries.clone();
        for socket in state.sockets.values() {
            if let (Some(tx), Some(_)) = (&socket.tx, &socket.events_sub) {
                let _ = tx.send(session(json!({"type": "providers_snapshot_update",
                    "payload": {"entries": entries}})));
            }
        }
    }

    /// Puts an image file on the host, `data` in base64, for
    /// `file_explorer_request` to read.
    pub fn add_file(&self, path: &str, mime_type: &str, data: &str) {
        (self.lock().files).insert(path.to_string(), (mime_type.into(), data.into()));
    }

    /// The workspace as the daemon has it now.
    pub fn workspace(&self, id: &str) -> Option<Value> {
        self.lock().workspaces.get(id).cloned()
    }

    pub fn agent(&self, id: &str) -> Option<Value> {
        self.lock().agents.get(id).cloned()
    }

    /// Refuses the next `count` sends (`accepted: false`).
    pub fn fail_next_sends(&self, count: usize) {
        self.lock().fail_sends = count;
    }

    /// While on, replies to sends wait until [`Self::release_sends`]; the
    /// send itself (the turn starting, the echo) happens at once.
    pub fn hold_sends(&self, on: bool) {
        self.lock().hold_sends = on;
    }

    pub fn release_sends(&self) {
        let held = std::mem::take(&mut self.lock().held);
        for (tx, text) in held {
            let _ = tx.send(text);
        }
    }

    /// The agent's current turn id.
    pub fn current_turn(&self, agent: &str) -> String {
        self.lock()
            .turns
            .get(agent)
            .cloned()
            .unwrap_or_else(|| "foreground-turn-1".into())
    }

    /// Starts a turn the agent runs on its own, as an orchestrator does.
    pub fn start_turn(&self, agent: &str, at: &str) {
        let mut state = self.lock();
        state.next_turn += 1;
        let turn = format!("autonomous-turn-{}", 100 + state.next_turn);
        state.turns.insert(agent.to_string(), turn.clone());
        let event = json!({"type": "turn_started", "provider": "claude", "turnId": turn});
        push_stream(&state, agent, event, at, None);
        patch_agent(
            &mut state,
            agent,
            json!({"status": "running", "updatedAt": at,
                   "activeTurn": {"turnId": turn, "startedAt": at}}),
        );
    }

    /// Ends the agent's current turn with a reply, as a provider would.
    pub fn finish_turn(&self, agent: &str, at: &str, reply: &str) {
        let mut state = self.lock();
        let turn = state
            .turns
            .get(agent)
            .cloned()
            .unwrap_or_else(|| "foreground-turn-1".into());
        let item = json!({"type": "assistant_message", "text": reply, "messageId": format!("{turn}-reply")});
        stream_row(&mut state, agent, &turn, at, item);
        let event = json!({"type": "turn_completed", "provider": "claude", "turnId": turn});
        push_stream(&state, agent, event, at, None);
        patch_agent(
            &mut state,
            agent,
            json!({"status": "idle", "activeTurn": null, "updatedAt": at}),
        );
    }

    /// Adds a row to an agent's timeline without telling anyone, as
    /// history from before Reef connected.
    pub fn seed_row(&self, agent: &str, turn: &str, at: &str, item: Value) -> u64 {
        let mut state = self.lock();
        let rows = state.timelines.entry(agent.to_string()).or_default();
        let seq = rows.last().map_or(1, |r| r.seq + 1);
        rows.push(Row {
            seq,
            entry: json!({
                "provider": "claude", "item": item, "timestamp": at,
                "seqStart": seq, "seqEnd": seq,
                "sourceSeqRanges": [{ "startSeq": seq, "endSeq": seq }],
                "turnId": turn, "collapsed": [],
            }),
        });
        seq
    }

    /// Adds a row and streams it live to sockets watching the agent.
    /// Assistant text streams as the chunk given; the stored row holds
    /// the text so far, as the daemon's projection does.
    pub fn stream_row(&self, agent: &str, turn: &str, at: &str, item: Value) -> u64 {
        stream_row(&mut self.lock(), agent, turn, at, item)
    }

    /// A turn or permission event: no seq, straight to watchers.
    pub fn stream_event(&self, agent: &str, at: &str, event: Value) {
        push_stream(&self.lock(), agent, event, at, None);
    }

    async fn serve(self, stream: tokio::net::TcpStream) {
        let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        let (mut sink, mut frames) = socket.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let id = {
            let mut state = self.lock();
            state.next_socket += 1;
            let id = state.next_socket;
            state.sockets.insert(
                id,
                Socket {
                    tx: Some(tx.clone()),
                    ..Default::default()
                },
            );
            id
        };
        let writer = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        drop(tx);
        let mut hello = false;
        while let Some(Ok(frame)) = frames.next().await {
            let Message::Text(text) = frame else { continue };
            let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            let reply = {
                let mut state = self.lock();
                let Some(tx) = state.sockets.get(&id).and_then(|s| s.tx.clone()) else {
                    break;
                };
                match value["type"].as_str() {
                    Some("hello") if !hello => {
                        hello = true;
                        state
                            .clients
                            .push(value["clientId"].as_str().unwrap_or_default().to_string());
                        Some((
                            tx,
                            session(json!({"type": "status", "payload": {
                                "status": "server_info", "protocolVersion": 1,
                                "serverId": state.server_id, "hostname": state.server_id,
                                "version": "0.11.2-fake",
                                "features": {"ownedSubscriptions": true, "workspaceLabels": true,
                                             "usageSources": state.usage_reports.is_some(),
                                             "backgroundWorkspaces": state.background_workspaces},
                            }})),
                        ))
                    }
                    Some("ping") => Some((tx, json!({"type": "pong"}).to_string())),
                    Some("session") if hello => {
                        // A usage read is answered in several messages:
                        // the reports first.
                        if value["message"]["type"] == "usage.list_reports.request" {
                            for report in state.usage_reports.iter().flatten() {
                                let _ = tx.send(session(json!({
                                    "type": "usage.list_reports.update",
                                    "payload": {"requestId": value["message"]["requestId"],
                                                "report": report},
                                })));
                            }
                        }
                        let reply = session(handle(&mut state, id, &value["message"]));
                        if state.hold_sends
                            && value["message"]["type"] == "send_agent_message_request"
                        {
                            state.held.push((tx, reply));
                            None
                        } else {
                            Some((tx, reply))
                        }
                    }
                    _ => None,
                }
            };
            if let Some((tx, text)) = reply {
                let _ = tx.send(text);
            }
        }
        self.lock().sockets.remove(&id);
        writer.abort();
    }
}

/// Tells sockets watching the directory about an agent.
/// Whether a background workspace, or an agent in one, stays out of what a
/// socket is sent: Paseo 0.12 leaves them out unless the client asked.
fn hidden_from(state: &State, include_background: bool, workspace_id: &Value) -> bool {
    state.background_workspaces
        && !include_background
        && workspace_id
            .as_str()
            .and_then(|id| state.workspaces.get(id))
            .is_some_and(|w| w["background"] == true)
}

fn push_agent(state: &State, agent: &Value) {
    let project = project_of(state, agent);
    for socket in state.sockets.values() {
        if hidden_from(state, socket.include_background, &agent["workspaceId"]) {
            continue;
        }
        if let (Some(tx), Some(sub)) = (&socket.tx, &socket.agents_sub) {
            let _ = tx.send(session(json!({"type": "agent_update", "payload": {
                "kind": "upsert", "agent": agent, "project": project, "subscriptionId": sub,
            }})));
        }
    }
}

/// Tells sockets watching the directory about a workspace.
fn push_workspace(state: &State, workspace: &Value) {
    for socket in state.sockets.values() {
        if hidden_from(state, socket.include_background, &workspace["id"]) {
            continue;
        }
        if let (Some(tx), Some(sub)) = (&socket.tx, &socket.workspaces_sub) {
            let _ = tx.send(session(json!({"type": "workspace_update", "payload": {
                "kind": "upsert", "workspace": workspace, "subscriptionId": sub,
            }})));
        }
    }
}

/// Tells sockets watching the directory that an agent is gone.
fn push_removal(state: &State, agent: &str) {
    for socket in state.sockets.values() {
        if let (Some(tx), Some(sub)) = (&socket.tx, &socket.agents_sub) {
            let _ = tx.send(session(json!({"type": "agent_update", "payload": {
                "kind": "remove", "agentId": agent, "subscriptionId": sub,
            }})));
        }
    }
}

/// Makes an agent as `agent.create.request` asks: in the workspace named,
/// else the one for its directory, made if there is none.
fn create(state: &mut State, message: &Value) -> Option<Value> {
    let config = &message["config"];
    let cwd = config["cwd"].as_str()?.to_string();
    state.created += 1;
    let n = state.created;
    let workspace = match message["workspaceId"].as_str() {
        Some(id) => {
            state.workspaces.get(id)?;
            id.to_string()
        }
        None => {
            let directory = |w: &Value| {
                (w["workspaceDirectory"].as_str())
                    .or(w["projectRootPath"].as_str())
                    .is_some_and(|d| d == cwd)
            };
            let found = (state.workspaces.values())
                .find(|w| directory(w))
                .map(|w| w["id"].as_str().unwrap_or_default().to_string());
            match found {
                Some(id) => id,
                None => {
                    let project = cwd.rsplit('/').next().unwrap_or("new").to_string();
                    let id = format!("wks_new_{n}");
                    let mut made = workspace(&id, &project, &project, &[]);
                    made["projectRootPath"] = json!(cwd);
                    state.workspaces.insert(id.clone(), made.clone());
                    push_workspace(state, &made);
                    id
                }
            }
        }
    };
    let now = fleet::clock::now_iso();
    let prompt = message["initialPrompt"].as_str().unwrap_or_default();
    let title = prompt.lines().next().unwrap_or("New agent").to_string();
    let id = format!("agt_new_{n}");
    let mut extra = json!({
        "provider": config["provider"], "model": config["model"], "cwd": cwd,
        "createdAt": now, "labels": message["labels"],
    });
    for (field, key) in [
        ("modeId", "currentModeId"),
        ("thinkingOptionId", "thinkingOptionId"),
    ] {
        if config[field].is_string() {
            extra[key] = config[field].clone();
        }
    }
    let made = agent(&id, &workspace, &title, &now, extra);
    state.agents.insert(id.clone(), made.clone());
    push_agent(state, &made);
    if !prompt.is_empty() {
        let item = json!({"type": "user_message", "text": prompt,
            "messageId": message["clientMessageId"], "clientMessageId": message["clientMessageId"]});
        stream_row(state, &id, "foreground-turn-1", &now, item);
    }
    Some(made)
}

fn patch_agent(state: &mut State, id: &str, fields: Value) {
    let Some(agent) = state.agents.get_mut(id) else {
        return;
    };
    if let (Some(target), Value::Object(fields)) = (agent.as_object_mut(), fields) {
        target.extend(fields);
    }
    let agent = agent.clone();
    push_agent(state, &agent);
}

/// Adds a row and streams it to sockets watching the agent. Assistant
/// text streams as the chunk given; the stored row holds the text so far,
/// as the daemon's projection does.
fn stream_row(state: &mut State, agent: &str, turn: &str, at: &str, item: Value) -> u64 {
    let rows = state.timelines.entry(agent.to_string()).or_default();
    let head = rows
        .last()
        .map_or(0, |row| row.entry["seqEnd"].as_u64().unwrap_or(row.seq));
    let seq = head + 1;
    match rows.last_mut() {
        Some(last)
            if item["type"] == "assistant_message"
                && last.entry["item"]["type"] == "assistant_message"
                && last.entry["item"]["messageId"] == item["messageId"]
                && last.entry["turnId"] == turn =>
        {
            let text = format!(
                "{}{}",
                last.entry["item"]["text"].as_str().unwrap_or_default(),
                item["text"].as_str().unwrap_or_default()
            );
            last.entry["item"]["text"] = json!(text);
            last.entry["seqEnd"] = json!(seq);
            last.entry["timestamp"] = json!(at);
        }
        _ => rows.push(Row {
            seq,
            entry: json!({
                "provider": "claude", "item": item, "timestamp": at,
                "seqStart": seq, "seqEnd": seq, "turnId": turn, "collapsed": [],
            }),
        }),
    }
    let epoch = state.epoch.clone();
    let event = json!({"type": "timeline", "provider": "claude", "item": item, "turnId": turn});
    push_stream(state, agent, event, at, Some((seq, epoch)));
    seq
}

fn push_stream(state: &State, agent: &str, event: Value, at: &str, seq: Option<(u64, String)>) {
    for socket in state.sockets.values() {
        let Some(tx) = &socket.tx else { continue };
        for (sub, agents) in &socket.timeline_subs {
            if agents.iter().any(|a| a == agent) {
                let mut payload = json!({
                    "agentId": agent, "event": event, "timestamp": at, "subscriptionId": sub,
                });
                if let Some((seq, epoch)) = &seq {
                    payload["seq"] = json!(seq);
                    payload["epoch"] = json!(epoch);
                }
                let _ = tx.send(session(json!({"type": "agent_stream", "payload": payload})));
            }
        }
    }
}

/// The writes Reef may send, done the way a daemon does them: the
/// pushes first, then the reply.
fn write(state: &mut State, message: &Value) -> Option<(&'static str, Value)> {
    let agent = message["agentId"].as_str().unwrap_or_default().to_string();
    let now = fleet::clock::now_iso();
    match message["type"].as_str()? {
        "agent_permission_response" => {
            let id = message["requestId"].as_str().unwrap_or_default();
            let pending: Vec<Value> = state.agents.get(&agent)?["pendingPermissions"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|p| p["id"] != id)
                .collect();
            let mut fields = json!({ "pendingPermissions": pending, "updatedAt": now });
            if pending.is_empty() {
                fields["attentionReason"] = Value::Null;
                fields["requiresAttention"] = json!(false);
            }
            patch_agent(state, &agent, fields);
            Some((
                "agent_permission_resolved",
                json!({ "agentId": agent, "resolution": message["response"] }),
            ))
        }
        "cancel_agent_request" => {
            let turn = state.turns.get(&agent).cloned();
            let event = json!({"type": "turn_canceled", "provider": "claude", "turnId": turn, "reason": "Interrupted"});
            push_stream(state, &agent, event, &now, None);
            patch_agent(
                state,
                &agent,
                json!({ "status": "idle", "activeTurn": null, "updatedAt": now }),
            );
            let snapshot = state.agents.get(&agent).cloned();
            Some((
                "cancel_agent_response",
                json!({ "agentId": agent, "agent": snapshot, "error": null }),
            ))
        }
        "send_agent_message_request" => {
            state.agents.get(&agent)?;
            if state.fail_sends > 0 {
                state.fail_sends -= 1;
                return Some((
                    "send_agent_message_response",
                    json!({ "agentId": agent, "accepted": false, "error": "The agent is busy" }),
                ));
            }
            let running = state.agents.get(&agent)?["status"] == "running";
            if !running {
                state.next_turn += 1;
                let turn = format!("foreground-turn-{}", 100 + state.next_turn);
                state.turns.insert(agent.clone(), turn.clone());
                let event = json!({"type": "turn_started", "provider": "claude", "turnId": turn});
                push_stream(state, &agent, event, &now, None);
            }
            let turn = state.turns.get(&agent).cloned().unwrap_or_default();
            let id = message["messageId"].clone();
            let mut item = json!({"type": "user_message", "text": message["text"], "messageId": id, "clientMessageId": id});
            if message["images"].is_array() {
                item["images"] = message["images"].clone();
            }
            stream_row(state, &agent, &turn, &now, item);
            patch_agent(
                state,
                &agent,
                json!({ "status": "running", "updatedAt": now,
                        "activeTurn": { "turnId": turn, "startedAt": now } }),
            );
            Some((
                "send_agent_message_response",
                json!({ "agentId": agent, "accepted": true, "error": null }),
            ))
        }
        "archive_agent_request" => {
            let found = state.agents.get_mut(&agent)?;
            found["archivedAt"] = json!(now);
            for socket in state.sockets.values() {
                if let (Some(tx), Some(sub)) = (&socket.tx, &socket.agents_sub) {
                    let _ = tx.send(session(json!({"type": "agent_update", "payload": {
                        "kind": "remove", "agentId": agent, "subscriptionId": sub,
                    }})));
                }
            }
            Some((
                "agent_archived",
                json!({ "agentId": agent, "archivedAt": now }),
            ))
        }
        "agent.create.request" => {
            let made = create(state, message)?;
            Some((
                "agent.create.response",
                json!({ "agent": made, "error": null }),
            ))
        }
        "update_agent_request" => {
            // Labels merge, as Paseo's do; a name renames.
            let found = state.agents.get(&agent)?;
            let mut labels = found["labels"].as_object().cloned().unwrap_or_default();
            labels.extend(message["labels"].as_object().cloned().unwrap_or_default());
            let mut fields = json!({ "labels": labels, "updatedAt": now });
            if message["name"].is_string() {
                fields["title"] = message["name"].clone();
            }
            patch_agent(state, &agent, fields);
            Some((
                "update_agent_response",
                json!({ "agentId": agent, "accepted": true, "error": null }),
            ))
        }
        "delete_agent_request" => {
            state.agents.remove(&agent)?;
            state.timelines.remove(&agent);
            push_removal(state, &agent);
            Some(("agent_deleted", json!({ "agentId": agent })))
        }
        "workspace.label.assignment.set.request" => {
            let id = message["workspaceId"].as_str()?;
            let name = message["label"]["name"].as_str()?.to_string();
            let found = state.workspaces.get_mut(id)?;
            let mut labels: Vec<Value> = found["labels"].as_array().cloned().unwrap_or_default();
            labels.retain(|l| l.as_str() != Some(name.as_str()));
            if message["assigned"] == true {
                labels.push(json!(name));
            }
            found["labels"] = json!(labels);
            let workspace = found.clone();
            if !state.labels.iter().any(|l| l["name"] == name.as_str()) {
                state.labels.push(message["label"].clone());
            }
            push_workspace(state, &workspace);
            Some((
                "workspace.label.assignment.set.response",
                json!({ "workspace": workspace }),
            ))
        }
        _ => None,
    }
}

fn project_of(state: &State, agent: &Value) -> Value {
    let workspace = agent["workspaceId"]
        .as_str()
        .and_then(|id| state.workspaces.get(id));
    match workspace {
        Some(w) => json!({
            "projectKey": w["projectId"], "projectName": w["projectDisplayName"],
            "workspaceName": w["name"],
        }),
        None => Value::Null,
    }
}

fn handle(state: &mut State, socket: u64, message: &Value) -> Value {
    let kind = message["type"].as_str().unwrap_or_default().to_string();
    let request_id = message["requestId"].clone();
    state.requests.push(kind.clone());
    state.next_sub += 1;
    let sub = format!("sub-{}", state.next_sub);
    let reply = |kind: &str, mut payload: Value| {
        payload["requestId"] = request_id.clone();
        json!({ "type": kind, "payload": payload })
    };
    match kind.as_str() {
        "session.events.set_subscription.request" => {
            if let Some(s) = state.sockets.get_mut(&socket) {
                s.events_sub = Some(sub.clone());
            }
            reply(
                "session.events.set_subscription.response",
                json!({ "subscriptionId": sub }),
            )
        }
        "fetch_workspaces_request" => {
            state.directory_reads.push(message.clone());
            let include = message["includeBackground"] == true;
            let entries: Vec<Value> = (state.workspaces.values())
                .filter(|w| !hidden_from(state, include, &w["id"]))
                .cloned()
                .collect();
            let mut payload = json!({ "entries": entries, "emptyProjects": [],
                "pageInfo": { "nextCursor": null, "hasMore": false } });
            if message.get("subscribe").is_some() {
                payload["subscriptionId"] = json!(sub);
                if let Some(s) = state.sockets.get_mut(&socket) {
                    s.workspaces_sub = Some(sub.clone());
                    s.include_background = include;
                }
            }
            reply("fetch_workspaces_response", payload)
        }
        // As Paseo answers a read of an image file: base64, or why not.
        "file_explorer_request" => {
            let path = message["path"].as_str().unwrap_or_default();
            let payload = match state.files.get(path) {
                Some((mime, data)) => json!({ "error": null, "file": {
                    "kind": "image", "encoding": "base64", "content": data, "mimeType": mime } }),
                None => json!({ "error": "File not found", "file": null }),
            };
            reply("file_explorer_response", payload)
        }
        "get_providers_snapshot_request" => {
            let entries = state.providers.clone();
            reply(
                "get_providers_snapshot_response",
                json!({ "entries": entries, "generatedAt": fleet::clock::now_iso() }),
            )
        }
        "workspace.label.list.request" => {
            let labels = state.labels.clone();
            reply("workspace.label.list.response", json!({ "labels": labels }))
        }
        "fetch_agents_request" => {
            state.directory_reads.push(message.clone());
            let include = message["includeBackground"] == true;
            let entries: Vec<Value> = state
                .agents
                .values()
                .filter(|a| a["archivedAt"].is_null())
                .filter(|a| !hidden_from(state, include, &a["workspaceId"]))
                .map(|a| json!({ "agent": a, "project": project_of(state, a) }))
                .collect();
            let mut payload =
                json!({ "entries": entries, "pageInfo": { "nextCursor": null, "hasMore": false } });
            if message.get("subscribe").is_some() {
                payload["subscriptionId"] = json!(sub);
                if let Some(s) = state.sockets.get_mut(&socket) {
                    s.agents_sub = Some(sub.clone());
                    s.include_background = include;
                }
            }
            reply("fetch_agents_response", payload)
        }
        "agent.timeline.set_subscription.request" => {
            let agents: Vec<String> = message["agentIds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect();
            if let Some(s) = state.sockets.get_mut(&socket) {
                s.timeline_subs.push((sub.clone(), agents.clone()));
            }
            reply(
                "agent.timeline.set_subscription.response",
                json!({ "agentIds": agents, "subscriptionId": sub }),
            )
        }
        "subscription.release.request" => {
            let released = message["subscriptionId"].as_str().unwrap_or_default();
            if let Some(s) = state.sockets.get_mut(&socket) {
                s.timeline_subs.retain(|(id, _)| id != released);
            }
            reply(
                "subscription.release.response",
                json!({ "subscriptionId": released }),
            )
        }
        "usage.list_reports.request" if state.usage_reports.is_some() => {
            reply("usage.list_reports.response", json!({ "error": null }))
        }
        "fetch_agent_timeline_request" => {
            let agent = message["agentId"].as_str().unwrap_or_default();
            reply(
                "fetch_agent_timeline_response",
                timeline_page(state, agent, message),
            )
        }
        "agent_permission_response"
        | "cancel_agent_request"
        | "send_agent_message_request"
        | "archive_agent_request"
        | "agent.create.request"
        | "delete_agent_request"
        | "update_agent_request"
        | "workspace.label.assignment.set.request" => match write(state, message) {
            Some((reply_kind, payload)) => {
                state.writes.push(message.clone());
                reply(reply_kind, payload)
            }
            None => reply(
                "rpc_error",
                json!({ "requestType": kind, "error": "no such agent", "code": "not_found" }),
            ),
        },
        _ => {
            state.refused.push(message.clone());
            reply(
                "rpc_error",
                json!({ "requestType": kind, "error": "the fake daemon only serves reads",
                        "code": "access_denied" }),
            )
        }
    }
}

fn timeline_page(state: &State, agent: &str, message: &Value) -> Value {
    let rows = state.timelines.get(agent).cloned().unwrap_or_default();
    let limit = message["limit"].as_u64().unwrap_or(40).max(1) as usize;
    let mut direction = message["direction"].as_str().unwrap_or("tail").to_string();
    let cursor = message["cursor"]["seq"].as_u64().unwrap_or(0);
    let stale = message["cursor"]["epoch"]
        .as_str()
        .is_some_and(|e| e != state.epoch);
    if stale {
        direction = "tail".into();
    }
    let end_of = |row: &Row| row.entry["seqEnd"].as_u64().unwrap_or(row.seq);
    let page: Vec<&Row> = match direction.as_str() {
        "before" => {
            let older: Vec<&Row> = rows.iter().filter(|r| end_of(r) < cursor).collect();
            older[older.len().saturating_sub(limit)..].to_vec()
        }
        "after" => rows
            .iter()
            .filter(|r| end_of(r) > cursor)
            .take(limit)
            .collect(),
        _ => rows[rows.len().saturating_sub(limit)..].iter().collect(),
    };
    let first = rows.first().map(|r| r.seq);
    let last = rows.last().map(end_of);
    let start = page.first().map(|r| r.seq);
    let end = page.last().map(|r| end_of(r));
    json!({
        "agentId": agent,
        "agent": state.agents.get(agent),
        "direction": direction,
        "projection": "projected",
        "epoch": state.epoch,
        "reset": stale,
        "staleCursor": stale,
        "gap": false,
        "window": { "minSeq": first.unwrap_or(1), "maxSeq": last.unwrap_or(0), "nextSeq": last.unwrap_or(0) + 1 },
        "startCursor": start.map(|seq| json!({ "epoch": state.epoch, "seq": seq })),
        "endCursor": end.map(|seq| json!({ "epoch": state.epoch, "seq": seq })),
        "hasOlder": start.is_some_and(|s| first.is_some_and(|f| s > f)),
        "hasNewer": end.is_some_and(|e| last.is_some_and(|l| e < l)),
        "entries": page.iter().map(|r| r.entry.clone()).collect::<Vec<_>>(),
        "error": null,
    })
}

// ---- made-up data ---------------------------------------------------------

/// A workspace in Paseo's shape.
pub fn workspace(id: &str, project: &str, name: &str, labels: &[&str]) -> Value {
    json!({
        "id": id, "projectId": format!("prj_{project}"), "projectDisplayName": project,
        "projectRootPath": format!("/home/dev/code/{project}"), "projectKind": "git",
        "workspaceKind": "worktree", "name": name, "status": "done", "labels": labels,
    })
}

/// An agent in Paseo's shape, idle; `extra` overrides fields.
pub fn agent(id: &str, workspace: &str, title: &str, updated_at: &str, extra: Value) -> Value {
    let mut agent = json!({
        "id": id, "provider": "claude", "model": "claude-opus-5-5", "cwd": "/home/dev/code/shop",
        "thinkingOptionId": null, "effectiveThinkingOptionId": "medium",
        "workspaceId": workspace, "title": title, "status": "idle",
        "createdAt": "2026-10-17T08:00:00.000Z", "updatedAt": updated_at,
        "labels": {}, "pendingPermissions": [], "requiresAttention": false, "archivedAt": null,
    });
    if let (Some(target), Value::Object(extra)) = (agent.as_object_mut(), extra) {
        target.extend(extra);
    }
    agent
}

/// A bigger made-up fleet for one daemon, for the demo: six workspaces
/// across projects and labels, and `count` agents in every state, each
/// with a prompt and a reply, active over the last few hours.
pub fn seed_many(daemon: &FakeDaemon, prefix: &str, count: usize) {
    const PROJECTS: [&str; 6] = ["shop", "search", "docs", "infra", "mobile", "billing-svc"];
    const LABELS: [&[&str]; 6] = [
        &["billing"],
        &["search", "perf"],
        &["docs"],
        &["infra", "ci"],
        &["mobile"],
        &["billing", "ci"],
    ];
    const TASKS: [&str; 12] = [
        "Fix the flaky retry test",
        "Speed up the search index build",
        "Write the release notes",
        "Bump the build images",
        "Port the parser to streaming",
        "Find why checkout is slow",
        "Add tests around token refresh",
        "Clean up dead feature flags",
        "Review the migration plan",
        "Draft the onboarding guide",
        "Trim the app bundle",
        "Reconcile the invoice totals",
    ];
    const REPLIES: [&str; 6] = [
        "---\n\n**Done:** the tests pass and the change is on `fix/retry`.",
        "I found the cause; it is a missing index on the orders table.",
        "## Draft written\n\nIt needs a look at the [second section](https://example.com/doc).",
        "The images are bumped and CI is green.",
        "Halfway there: the reader streams, the writer still buffers.",
        "Nothing to change after all; the numbers already match.",
    ];
    // Paseo's own colour names; perf has no definition, so it gets one
    // picked from its name.
    for (name, color) in [
        ("billing", "orange"),
        ("search", "sky"),
        ("docs", "emerald"),
        ("infra", "indigo"),
        ("ci", "amber"),
        ("mobile", "violet"),
    ] {
        daemon.add_label(name, color);
    }
    for (w, project) in PROJECTS.iter().enumerate() {
        daemon.add_workspace(workspace(
            &format!("wks_{prefix}_m{w}"),
            project,
            &format!("{project}-{}", ["main", "fix", "next"][w % 3]),
            LABELS[w],
        ));
    }
    let now = fleet::clock::now_millis();
    // Each host's agents differ a little from the next host's.
    let shift = prefix.bytes().next().unwrap_or(0) as usize;
    for i in 0..count {
        let id = format!("agt_{prefix}_m{i}");
        let at = |minutes: usize| fleet::clock::iso_from_millis(now - (minutes as i64) * 60_000);
        let age = 3 + i * 11 + (shift % 7) * 4;
        let mut extra = json!({});
        if i % 9 == 4 {
            extra = json!({"status": "running",
                "activeTurn": {"turnId": "foreground-turn-1", "startedAt": at(age)}});
        } else if i % 13 == 6 {
            extra = json!({"status": "error", "lastError": "The provider stopped"});
        } else if i % 11 == 2 {
            extra = json!({"requiresAttention": true, "attentionReason": "permission",
                "pendingPermissions": [shell_permission(&format!("perm_{prefix}_m{i}"), "make deploy-preview")]});
        }
        daemon.upsert_agent(agent(
            &id,
            &format!("wks_{prefix}_m{}", i % PROJECTS.len()),
            TASKS[(i * 5 + shift) % TASKS.len()],
            &at(age),
            extra,
        ));
        daemon.seed_row(
            &id,
            "foreground-turn-1",
            &at(age + 5),
            json!({"type": "user_message",
            "text": TASKS[(i * 5 + shift) % TASKS.len()], "messageId": format!("{id}-m")}),
        );
        daemon.seed_row(
            &id,
            "foreground-turn-1",
            &at(age),
            json!({"type": "assistant_message",
            "text": REPLIES[(i + shift) % REPLIES.len()], "messageId": format!("{id}-r")}),
        );
    }
}

/// A usage report as a daemon's usage source makes one: `windows` are
/// (id, label, used percent, seconds until it resets).
pub fn usage_report(
    id: &str,
    source: &str,
    account: &str,
    plan: &str,
    windows: &[(&str, &str, f64, i64)],
) -> Value {
    let now = fleet::clock::now_millis();
    let windows: Vec<Value> = (windows.iter())
        .map(|(id, label, used, resets_in)| {
            json!({"id": id, "label": label, "usedPct": used,
                   "resetsAt": fleet::clock::iso_from_millis(now + resets_in * 1000)})
        })
        .collect();
    json!({
        "id": id, "sourceId": source, "sourceLabel": source,
        "account": {"label": account}, "fetchedAt": fleet::clock::iso_from_millis(now),
        "report": {"status": "available", "planLabel": plan, "windows": windows},
    })
}

/// `report` (from [`usage_report`]) as it is when its latest read failed
/// with `error`. With `kept_for` it is as Paseo 0.12 sends it, carrying
/// the report as its last successful one, read that many seconds ago; an
/// older daemon sends none.
pub fn usage_failure(report: &Value, error: &str, kept_for: Option<i64>) -> Value {
    let mut failed = report.clone();
    failed["report"] = json!({"status": "error", "error": error});
    if let Some(secs) = kept_for {
        let at = fleet::clock::now_millis() - secs * 1000;
        failed["lastSuccessful"] = json!({
            "fetchedAt": fleet::clock::iso_from_millis(at),
            "report": report["report"],
        });
    }
    failed
}

/// A usage feed: answers `GET /v1/usage` with the body last given, and
/// counts how often it was asked.
#[derive(Clone)]
pub struct FakeFeed {
    pub addr: SocketAddr,
    state: Arc<Mutex<(String, usize)>>,
}

impl FakeFeed {
    pub async fn start(body: &str) -> std::io::Result<Self> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let feed = Self {
            addr: listener.local_addr()?,
            state: Arc::new(Mutex::new((body.to_string(), 0))),
        };
        let state = feed.state.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    let mut head = vec![0u8; 4096];
                    let n = stream.read(&mut head).await.unwrap_or(0);
                    let asked = String::from_utf8_lossy(&head[..n]).to_string();
                    let (status, body) = if asked.starts_with("GET /v1/usage ") {
                        let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
                        state.1 += 1;
                        ("200 OK", state.0.clone())
                    } else {
                        ("404 Not Found", String::new())
                    };
                    let answer = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(answer.as_bytes()).await;
                });
            }
        });
        Ok(feed)
    }

    /// The feed's base URL, for `--usage`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn set_body(&self, body: &str) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).0 = body.to_string();
    }

    /// How many times it was read.
    pub fn reads(&self) -> usize {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).1
    }
}

/// A providers snapshot's entries, as Paseo's plain encoding has them: two
/// ready providers with models, modes and thinking options, and one that
/// is not installed.
pub fn providers() -> Vec<Value> {
    let efforts = json!([{"id": "low", "label": "Low"}, {"id": "medium", "label": "Medium"},
                         {"id": "high", "label": "High"}]);
    vec![
        json!({
            "provider": "claude", "label": "Claude", "status": "ready", "enabled": true,
            "defaultModeId": "default",
            "modes": [{"id": "default", "label": "Always ask"}, {"id": "acceptEdits", "label": "Accept edits"},
                      {"id": "plan", "label": "Plan"}, {"id": "bypassPermissions", "label": "Bypass"}],
            "models": [
                {"id": "claude-opus-5-5", "label": "Opus 5.5", "isDefault": true,
                 "thinkingOptions": efforts, "defaultThinkingOptionId": "medium"},
                {"id": "claude-sonnet-5", "label": "Sonnet 5",
                 "thinkingOptions": efforts, "defaultThinkingOptionId": "low"},
                {"id": "claude-haiku-4-5", "label": "Haiku 4.5"},
            ],
        }),
        json!({
            "provider": "codex", "label": "Codex", "status": "ready", "enabled": true,
            "defaultModeId": "auto",
            "modes": [{"id": "read-only", "label": "Read only"}, {"id": "auto", "label": "Auto"},
                      {"id": "full-access", "label": "Full access"}],
            "models": [{"id": "gpt-5-codex", "label": "GPT-5 Codex", "isDefault": true,
                        "thinkingOptions": efforts, "defaultThinkingOptionId": "medium"}],
        }),
        json!({"provider": "opencode", "label": "OpenCode", "status": "unavailable", "enabled": true,
               "modes": [], "models": []}),
    ]
}

/// A permission request for a shell command.
pub fn shell_permission(id: &str, command: &str) -> Value {
    json!({
        "id": id, "provider": "claude", "name": "Bash", "kind": "tool",
        "input": { "command": command },
        "detail": { "type": "shell", "command": command },
    })
}

/// A question, as Claude Code's AskUserQuestion asks one.
pub fn question(id: &str) -> Value {
    json!({
        "id": id, "provider": "claude", "kind": "question", "name": "AskUserQuestion",
        "title": "Question",
        "input": { "questions": [{
            "header": "Scope",
            "question": "Ship the fix alone, or with the refactor?",
            "options": [{ "label": "The fix alone" }, { "label": "With the refactor" }],
        }]},
    })
}

/// The demo and test fleet for one daemon: four workspaces and six agents
/// covering every state, one of them an orchestrator whose conversation
/// has turns no message started.
/// A background workspace, as Paseo 0.12 makes them for helpers, holding
/// one running agent: `wks_<prefix>_bg` and `agt_<prefix>_bg`.
pub fn seed_background(daemon: &FakeDaemon, prefix: &str) {
    let mut helper = workspace(
        &format!("wks_{prefix}_bg"),
        "shop",
        "shop-helper",
        &["billing"],
    );
    helper["background"] = json!(true);
    daemon.add_workspace(helper);
    daemon.upsert_agent(agent(
        &format!("agt_{prefix}_bg"),
        &format!("wks_{prefix}_bg"),
        "Summarise the nightly logs",
        "2026-10-17T09:55:00.000Z",
        json!({ "status": "running",
                "activeTurn": { "turnId": "foreground-turn-1", "startedAt": "2026-10-17T09:54:00.000Z" } }),
    ));
}

pub fn seed(daemon: &FakeDaemon, prefix: &str) {
    for (name, color) in [
        ("billing", "orange"),
        ("search", "sky"),
        ("infra", "indigo"),
    ] {
        daemon.add_label(name, color);
    }
    let w = |n: &str| format!("wks_{prefix}_{n}");
    daemon.add_workspace(workspace(&w("1"), "shop", "shop-checkout", &["billing"]));
    daemon.add_workspace(workspace(&w("2"), "shop", "shop-search", &["search"]));
    daemon.add_workspace(workspace(&w("3"), "docs", "docs-site", &[]));
    daemon.add_workspace(workspace(
        &w("4"),
        "infra",
        "infra-ci",
        &["infra", "billing"],
    ));
    let a = |n: &str| format!("agt_{prefix}_{n}");
    daemon.upsert_agent(agent(
        &a("asks"),
        &w("1"),
        "Pick a scope for the checkout fix",
        "2026-10-17T09:50:00.000Z",
        json!({ "requiresAttention": true, "attentionReason": "permission",
                "pendingPermissions": [question(&format!("perm_{prefix}_q"))] }),
    ));
    daemon.upsert_agent(agent(
        &a("perm"),
        &w("2"),
        "Rebuild the search index",
        "2026-10-17T09:40:00.000Z",
        json!({ "requiresAttention": true, "attentionReason": "permission",
                "pendingPermissions": [shell_permission(&format!("perm_{prefix}_s"), "cargo run --bin reindex")] }),
    ));
    daemon.upsert_agent(agent(
        &a("failed"),
        &w("4"),
        "Why is CI red again",
        "2026-10-17T09:30:00.000Z",
        json!({ "status": "error", "requiresAttention": true, "attentionReason": "error" }),
    ));
    daemon.upsert_agent(agent(
        &a("orch"),
        &w("1"),
        "Orchestrate the billing migration",
        "2026-10-17T09:20:00.000Z",
        json!({ "requiresAttention": true, "attentionReason": "finished",
                "attentionTimestamp": "2026-10-17T09:20:00.000Z" }),
    ));
    daemon.upsert_agent(agent(
        &a("run"),
        &w("3"),
        "Write the onboarding guide",
        "2026-10-17T09:10:00.000Z",
        json!({ "status": "running",
                "activeTurn": { "turnId": "foreground-turn-1", "startedAt": "2026-10-17T09:09:00.000Z" } }),
    ));
    daemon
        .lock()
        .turns
        .insert(a("run"), "foreground-turn-1".into());
    daemon.upsert_agent(agent(
        &a("sub"),
        &w("1"),
        "Check the invoice totals",
        "2026-10-17T09:00:00.000Z",
        json!({ "parentAgentId": a("orch") }),
    ));

    // The orchestrator: one turn from a message, then two it ran on its
    // own, then another message.
    let o = a("orch");
    let t = |at: &str| format!("2026-10-17T{at}.000Z");
    daemon.seed_row(&o, "foreground-turn-1", &t("09:00:00"), json!({"type": "user_message",
        "text": "Migrate billing to the new ledger, one service at a time.", "messageId": "m-1", "clientMessageId": "m-1"}));
    daemon.seed_row(&o, "foreground-turn-1", &t("09:00:20"), json!({"type": "tool_call", "callId": "c-1",
        "name": "Bash", "detail": {"type": "shell", "command": "cargo test -p ledger", "output": "test result: ok. 42 passed"},
        "status": "completed", "error": null}));
    daemon.seed_row(
        &o,
        "foreground-turn-1",
        &t("09:01:12"),
        json!({"type": "assistant_message",
        "text": "The ledger tests pass. I'll start with invoices.", "messageId": "r-1"}),
    );
    daemon.seed_row(
        &o,
        "autonomous-turn-2",
        &t("09:10:00"),
        json!({"type": "assistant_message",
        "text": "Invoices moved; the subagent is checking the totals.", "messageId": "r-2"}),
    );
    daemon.seed_row(&o, "autonomous-turn-3", &t("09:15:00"), json!({"type": "tool_call", "callId": "c-2",
        "name": "Read", "detail": {"type": "read", "filePath": "/home/dev/code/shop/billing/totals.rs", "content": "fn total() {}"},
        "status": "completed", "error": null}));
    daemon.seed_row(
        &o,
        "autonomous-turn-3",
        &t("09:15:40"),
        json!({"type": "assistant_message",
        "text": "Totals match to the cent.", "messageId": "r-3"}),
    );
    daemon.seed_row(
        &o,
        "foreground-turn-4",
        &t("09:18:00"),
        json!({"type": "user_message",
        "text": "Great, carry on with refunds.", "messageId": "m-4", "clientMessageId": "m-4"}),
    );
    daemon.seed_row(
        &o,
        "foreground-turn-4",
        &t("09:20:00"),
        json!({"type": "assistant_message",
        "text": "Refunds are next; I'll report back when they are done.", "messageId": "r-4"}),
    );

    // A short exchange for the others; see also [`seed_agent_messages`].
    for (id, ask) in [
        (a("asks"), "Fix the rounding bug at checkout."),
        (a("perm"), "Rebuild the search index from scratch."),
        (a("failed"), "Find out why CI fails on main."),
        (a("run"), "Draft an onboarding guide for new contributors."),
        (
            a("sub"),
            "Check that every invoice total matches the ledger.",
        ),
    ] {
        daemon.seed_row(
            &id,
            "foreground-turn-1",
            &t("09:00:00"),
            json!({"type": "user_message",
            "text": ask, "messageId": format!("{id}-m"), "clientMessageId": format!("{id}-m")}),
        );
    }
}

/// A conversation with images kept as files, as a Burrow keeps a prompt's
/// (a phone screenshot sent from another client) and Paseo saves an
/// agent's image output and names it in the reply.
pub fn seed_images(daemon: &FakeDaemon, agent: &str) {
    use base64::Engine;
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let shot = "/home/dev/.burrow/attachments/demo-shot.png";
    let chart = "/tmp/paseo-attachments-demo/chart.png";
    daemon.add_file(shot, "image/png", &encode(include_bytes!("fake/shot.png")));
    daemon.add_file(
        chart,
        "image/png",
        &encode(include_bytes!("fake/chart.png")),
    );
    let at = |at: &str| format!("2026-10-17T{at}.000Z");
    daemon.seed_row(
        agent,
        "turn-images",
        &at("09:40:00"),
        json!({"type": "user_message", "text": "This screen looks off on my phone. Can you chart the load times?",
               "messageId": "demo-images-ask", "images": [{"path": shot, "mimeType": "image/png"}]}),
    );
    daemon.seed_row(
        agent,
        "turn-images",
        &at("09:41:30"),
        json!({"type": "assistant_message", "messageId": "demo-images-reply",
               "text": format!("The list's padding doubles on narrow screens. Load times by step:\n\n\
                   ![Image](file://{chart})\n\nThe fourth step is the slow one.")}),
    );
}

/// Agents of [`seed`]'s fleet messaging each other, as Paseo 0.12 shows it:
/// the orchestrator asks its subagent for a check and hears that it
/// finished, the subagent gets the request, and the agent asking a
/// question hears from an agent no host knows any more, whose title is
/// long. The subagent's later prompt carries the tag an older daemon or a
/// replayed history leaves.
pub fn seed_agent_messages(daemon: &FakeDaemon, prefix: &str) {
    let a = |n: &str| format!("agt_{prefix}_{n}");
    let t = |at: &str| format!("2026-10-17T{at}.000Z");
    let (orch, sub) = (a("orch"), a("sub"));
    let received = |id: &str, event: &str, sender: Value, text: &str| {
        json!({"type": "tool_call", "callId": format!("paseo-agent-message:{id}"),
            "name": "agent_message", "status": "completed", "error": null,
            "detail": {"type": "plain_text", "text": text, "icon": "bot"},
            "agentMessage": {"event": event, "sender": sender, "text": text}})
    };
    daemon.seed_row(&orch, "autonomous-turn-5", &t("09:21:00"), json!({"type": "tool_call",
        "callId": "c-send-1", "name": "mcp__paseo__send_agent_prompt",
        "detail": {"type": "unknown", "input": {"agentId": sub,
            "prompt": "Check the refund totals against the ledger too.\nSame tolerance as invoices."},
            "output": {"status": "accepted"}},
        "status": "completed", "error": null}));
    daemon.seed_row(
        &orch,
        "autonomous-turn-5",
        &t("09:21:05"),
        json!({"type": "assistant_message",
        "text": "Asked the subagent to check refunds as well.", "messageId": "r-5"}),
    );
    let done = format!(
        "Agent {sub} (Check the invoice totals) finished.\n\n<agent-response>\nRefund totals match the ledger; two rounding fixes applied.\n</agent-response>"
    );
    daemon.seed_row(
        &orch,
        "autonomous-turn-6",
        &t("09:24:00"),
        received(
            "n-1",
            "finished",
            json!({"id": sub, "title": "Check the invoice totals"}),
            &done,
        ),
    );
    daemon.seed_row(
        &orch,
        "autonomous-turn-6",
        &t("09:24:30"),
        json!({"type": "assistant_message",
        "text": "Refunds are migrated and checked.", "messageId": "r-6"}),
    );

    daemon.seed_row(
        &sub,
        "foreground-turn-2",
        &t("09:21:01"),
        received(
            "m-1",
            "message",
            json!({"id": orch, "title": "Orchestrate the billing migration"}),
            "Check the refund totals against the ledger too.\nSame tolerance as invoices.",
        ),
    );
    daemon.seed_row(
        &sub,
        "foreground-turn-2",
        &t("09:23:50"),
        json!({"type": "assistant_message",
        "text": "Refund totals match the ledger; two rounding fixes applied.", "messageId": "s-2"}),
    );
    daemon.seed_row(&sub, "foreground-turn-3", &t("09:30:00"), json!({"type": "user_message",
        "text": format!("<paseo-system version=\"1\" kind=\"agent-message\" source-agent-id=\"{orch}\" message-id=\"m-2\">\nThanks. Archive yourself when you are done.\n</paseo-system>"),
        "messageId": "s-m-3"}));

    let gone = "agt_gone_7f3a";
    let title = "Coordinate the quarterly ledger migration across checkout, refunds, invoicing and the reporting warehouse";
    daemon.seed_row(
        &a("asks"),
        "foreground-turn-2",
        &t("09:40:00"),
        received(
            "m-3",
            "message",
            json!({"id": gone, "title": title}),
            "Hold the rounding fix until the release branch is cut.",
        ),
    );
}
