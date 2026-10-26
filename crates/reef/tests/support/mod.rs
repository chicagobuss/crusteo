//! The test phone and Reef: a Reef on fake daemons, and a phone that keeps
//! each view by applying snapshots and deltas exactly as the spec says,
//! checking every sequence number.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reef::fake::FakeDaemon;
use reef::{Device, ReefConfig};
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub const TOKEN: &str = "test-device-token";
pub const WAIT: Duration = Duration::from_secs(15);

/// A WebSocket to Reef at `addr`. Reef is listening before `start`
/// returns, but under a full parallel test run a loopback connect on
/// Windows now and then fails with an I/O error anyway, so those are
/// retried briefly.
pub async fn connect_ws(addr: SocketAddr) -> Ws {
    let url = format!("ws://{addr}/ws");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut pause = Duration::from_millis(10);
    loop {
        match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((ws, _)) => return ws,
            Err(Error::Io(err)) if Instant::now() < deadline => {
                eprintln!("connect to {url} failed, retrying: {err}");
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(Duration::from_millis(200));
            }
            Err(err) => panic!("connect to {url}: {err}"),
        }
    }
}

pub async fn reef_with(daemons: &[(&str, &FakeDaemon)]) -> SocketAddr {
    reef_saving_to(daemons, None).await
}

/// A Reef that keeps its settings in `dir`.
pub async fn reef_saving_to(
    daemons: &[(&str, &FakeDaemon)],
    dir: Option<std::path::PathBuf>,
) -> SocketAddr {
    start_reef(daemons, dir, false).await
}

/// A Reef that asks no phone for a token.
pub async fn open_reef_with(daemons: &[(&str, &FakeDaemon)]) -> SocketAddr {
    start_reef(daemons, None, true).await
}

/// A Reef that saves hosts added or removed from a phone to `hosts_file`.
pub async fn reef_with_hosts_file(
    daemons: &[(&str, &FakeDaemon)],
    hosts_file: std::path::PathBuf,
) -> SocketAddr {
    start_reef_with(daemons, None, false, Some(hosts_file)).await
}

async fn start_reef(
    daemons: &[(&str, &FakeDaemon)],
    dir: Option<std::path::PathBuf>,
    open: bool,
) -> SocketAddr {
    start_reef_with(daemons, dir, open, None).await
}

async fn start_reef_with(
    daemons: &[(&str, &FakeDaemon)],
    dir: Option<std::path::PathBuf>,
    open: bool,
    hosts_file: Option<std::path::PathBuf>,
) -> SocketAddr {
    reef_configured(daemons, |config| {
        config.data_dir = dir;
        config.open = open;
        config.hosts_file = hosts_file;
    })
    .await
}

/// A Reef on these daemons, with whatever else `configure` sets.
pub async fn reef_configured(
    daemons: &[(&str, &FakeDaemon)],
    configure: impl FnOnce(&mut ReefConfig),
) -> SocketAddr {
    let hosts = daemons
        .iter()
        .map(|(name, daemon)| fleet::HostConfig {
            id: reef::config::host_id(name),
            name: name.to_string(),
            endpoint: Some(daemon.endpoint()),
            password: None,
        })
        .collect();
    let mut config = ReefConfig::new(
        hosts,
        "cid_reef_test",
        vec![Device {
            name: "test phone".into(),
            token: TOKEN.into(),
        }],
    );
    config.bind = "127.0.0.1:0".parse().unwrap();
    config.limits.flush = Duration::from_millis(40);
    configure(&mut config);
    reef::start(config).await.unwrap().addr
}

#[derive(Debug, Default)]
pub struct View {
    pub seq: u64,
    pub snapshots: usize,
    pub deltas: usize,
    pub fields: Map<String, Value>,
    /// Rows of a list, or turns of a conversation, in order.
    pub parts: Vec<Value>,
    /// Item patches that only appended text.
    pub appends: usize,
    /// A map's cards.
    pub cards: Vec<Value>,
}

pub struct Phone {
    pub ws: Ws,
    pub views: HashMap<String, View>,
    pub results: HashMap<String, Value>,
    pub hosts: Value,
    pub errors: Vec<Value>,
    pub next_req: u64,
    pub notifications: Vec<Value>,
    pub settings: Value,
    pub pongs: usize,
    /// Label names and colours, by label id.
    pub labels: Value,
}

pub fn key_of(part: &Value) -> &str {
    part["key"].as_str().expect("every part has a key")
}

/// Puts `parts` in `order`; every key in it must be known.
pub fn reorder(parts: &mut Vec<Value>, order: &Value) {
    let order: Vec<&str> = order
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    let mut by_key: HashMap<String, Value> = parts
        .drain(..)
        .map(|part| (key_of(&part).to_string(), part))
        .collect();
    for key in order {
        parts.push(
            by_key
                .remove(key)
                .unwrap_or_else(|| panic!("order names unknown {key}")),
        );
    }
}

/// Applies a list delta's `remove`, `upsert` and `order` to `parts`.
pub fn apply_rows(parts: &mut Vec<Value>, changes: &Value) {
    upsert_rows(parts, changes);
    if let Some(order) = changes.get("order") {
        reorder(parts, order);
    }
}

/// `remove` and `upsert` only; a conversation orders after its turns.
pub fn upsert_rows(parts: &mut Vec<Value>, changes: &Value) {
    if let Some(keys) = changes["remove"].as_array() {
        parts.retain(|p| !keys.contains(&p["key"]));
    }
    for row in changes["upsert"].as_array().into_iter().flatten() {
        match parts.iter_mut().find(|p| p["key"] == row["key"]) {
            Some(existing) => *existing = row.clone(),
            None => parts.push(row.clone()),
        }
    }
}

pub fn apply_turn_patch(turn: &mut Value, patch: &Value) {
    for (field, value) in patch.as_object().unwrap() {
        match field.as_str() {
            "key" | "items" | "order" => {}
            _ if value.is_null() => {
                turn.as_object_mut().unwrap().remove(field);
            }
            _ => turn[field] = value.clone(),
        }
    }
    let items = turn["items"].as_array_mut().unwrap();
    for item in patch["items"].as_array().into_iter().flatten() {
        let id = item["id"].as_str().unwrap();
        let existing = items.iter_mut().find(|i| i["id"] == id);
        match (existing, item.get("append")) {
            (Some(existing), Some(append)) => {
                let text = format!(
                    "{}{}",
                    existing["text"].as_str().unwrap(),
                    append.as_str().unwrap()
                );
                existing["text"] = json!(text);
                existing["at"] = item["at"].clone();
            }
            (None, Some(_)) => panic!("append to unknown item {id}"),
            (Some(existing), None) => *existing = item.clone(),
            (None, None) => items.push(item.clone()),
        }
    }
    if let Some(order) = patch.get("order") {
        let mut by_id: HashMap<String, Value> = items
            .drain(..)
            .map(|i| (i["id"].as_str().unwrap().to_string(), i))
            .collect();
        for id in order.as_array().unwrap() {
            let id = id.as_str().unwrap();
            items.push(
                by_id
                    .remove(id)
                    .unwrap_or_else(|| panic!("order names unknown item {id}")),
            );
        }
    }
}

impl Phone {
    pub async fn connect(addr: SocketAddr, token: &str) -> (Self, Value) {
        let ws = connect_ws(addr).await;
        let mut phone = Phone {
            ws,
            views: HashMap::new(),
            results: HashMap::new(),
            hosts: Value::Null,
            errors: Vec::new(),
            next_req: 0,
            notifications: Vec::new(),
            settings: Value::Null,
            pongs: 0,
            labels: Value::Null,
        };
        phone
            .send(json!({"type": "hello", "v": 0, "token": token, "device": "test"}))
            .await;
        let first = phone.recv().await.expect("an answer to hello");
        (phone, first)
    }

    pub async fn send(&mut self, message: Value) {
        self.ws
            .send(Message::text(message.to_string()))
            .await
            .unwrap();
    }

    /// Sends a message with a fresh `req` and returns it.
    pub async fn ask(&mut self, mut message: Value) -> String {
        self.next_req += 1;
        let req = format!("r{}", self.next_req);
        message["req"] = json!(req);
        self.send(message).await;
        req
    }

    /// The next frame, applied to the views. None once the socket closed.
    pub async fn recv(&mut self) -> Option<Value> {
        loop {
            let frame = tokio::time::timeout(WAIT, self.ws.next())
                .await
                .expect("Reef went quiet")?;
            match frame.ok()? {
                Message::Text(text) => {
                    let message: Value = serde_json::from_str(text.as_str()).unwrap();
                    self.apply(&message);
                    return Some(message);
                }
                Message::Close(_) => return None,
                _ => {}
            }
        }
    }

    /// Reads until `done` holds.
    pub async fn until(&mut self, what: &str, done: impl Fn(&Phone) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, self.recv()).await {
                Ok(frame) => frame.expect("socket closed"),
                Err(_) => panic!("Reef went quiet waiting for {what}"),
            };
        }
    }

    pub fn view(&self, id: &str) -> &View {
        self.views.get(id).unwrap_or_else(|| panic!("no view {id}"))
    }

    pub fn keys(&self, id: &str) -> Vec<String> {
        self.views
            .get(id)
            .map(|v| v.parts.iter().map(|p| key_of(p).to_string()).collect())
            .unwrap_or_default()
    }

    pub fn apply(&mut self, message: &Value) {
        match message["type"].as_str().unwrap() {
            "snapshot" => {
                let id = message["id"].as_str().unwrap().to_string();
                let view = self.views.entry(id).or_default();
                view.seq = message["seq"].as_u64().unwrap();
                view.snapshots += 1;
                view.fields = message.as_object().unwrap().clone();
                let parts = (message.get("rows"))
                    .or(message.get("turns"))
                    .or(message.get("tiles"))
                    .unwrap();
                view.parts = parts.as_array().unwrap().clone();
                view.cards = message["cards"].as_array().cloned().unwrap_or_default();
            }
            "delta" => {
                let id = message["id"].as_str().unwrap().to_string();
                let view = self
                    .views
                    .get_mut(&id)
                    .expect("a delta before its snapshot");
                let seq = message["seq"].as_u64().unwrap();
                assert_eq!(seq, view.seq + 1, "deltas come in sequence");
                view.seq = seq;
                view.deltas += 1;
                for (field, value) in message.as_object().unwrap() {
                    match field.as_str() {
                        "type" | "id" | "seq" | "upsert" | "remove" | "order" | "turns"
                        | "tiles" | "cards" => {}
                        _ => {
                            view.fields.insert(field.clone(), value.clone());
                        }
                    }
                }
                upsert_rows(&mut view.parts, message);
                // A map nests its two lists.
                if message["tiles"].is_object() {
                    apply_rows(&mut view.parts, &message["tiles"]);
                }
                if message["cards"].is_object() {
                    apply_rows(&mut view.cards, &message["cards"]);
                }
                for patch in message["turns"].as_array().into_iter().flatten() {
                    view.appends += patch["items"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|i| i.get("append").is_some())
                        .count();
                    match view.parts.iter_mut().find(|t| t["key"] == patch["key"]) {
                        Some(turn) => apply_turn_patch(turn, patch),
                        None => view.parts.push(patch.clone()),
                    }
                }
                if let Some(order) = message.get("order") {
                    reorder(&mut view.parts, order);
                }
            }
            "result" => {
                self.results.insert(
                    message["req"].as_str().unwrap_or_default().to_string(),
                    message.clone(),
                );
            }
            "hosts" => {
                self.hosts = message["hosts"].clone();
                self.labels = message["labels"].clone();
            }
            "welcome" => {
                self.hosts = message["hosts"].clone();
                self.settings = message["settings"].clone();
                self.labels = message["labels"].clone();
            }
            "settings" => self.settings = message["settings"].clone(),
            "notification" => self.notifications.push(message.clone()),
            "pong" => self.pongs += 1,
            "error" => self.errors.push(message.clone()),
            _ => {}
        }
    }
}

pub fn texts(turn: &Value) -> Vec<String> {
    turn["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            format!(
                "{}:{}",
                i["kind"].as_str().unwrap(),
                i["text"].as_str().or(i["summary"].as_str()).unwrap_or("")
            )
        })
        .collect()
}
