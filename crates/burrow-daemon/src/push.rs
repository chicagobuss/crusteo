//! Notifications to Paseo's apps, as Paseo sends them.
//!
//! When an agent needs attention, at most one client that asked for
//! notifications (`session.events.set_subscription.request` with
//! `notifications: true`) is told to show one: the one most recently
//! active, by its `client_heartbeat`s, within three minutes. A client that
//! is looking at that agent stops it altogether. When no such client is
//! there, the phones that registered a push token get a push through
//! Expo's push service, as from Paseo's own daemon; an error never
//! pushes. Paseo's `agent-attention-policy.ts` and `push/`.
//!
//! Tokens are leased: a registration or a heartbeat from the phone that
//! registered keeps one for 48 hours, and Expo's "this device is gone"
//! drops it. They are kept in `<home>/push-tokens.json`, in Paseo's
//! format, so `burrow import-paseo` can carry Paseo's over.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::clock;

/// Where pushes go: Expo's push service, which Paseo's apps register with.
pub const EXPO_PUSH_URL: &str = "https://exp.host/--/api/v2/push/send";

/// How long a token is kept after its phone was last heard from.
pub const TOKEN_LEASE_MS: i64 = 48 * 60 * 60 * 1000;

/// How recent a heartbeat makes a client present.
pub const PRESENCE_MS: i64 = 180_000;

/// Expo takes at most this many messages a request.
const BATCH: usize = 100;

/// The push tokens, each with the time its lease ends. Clones share them.
#[derive(Clone)]
pub struct Tokens {
    path: Option<PathBuf>,
    leases: Arc<Mutex<BTreeMap<String, i64>>>,
}

impl Tokens {
    /// The tokens kept in `home`; none without a home.
    pub fn open(home: Option<&Path>) -> Self {
        let path = home.map(|home| home.join("push-tokens.json"));
        let leases = path.as_deref().map(read_leases).unwrap_or_default();
        Self {
            path,
            leases: Arc::new(Mutex::new(leases)),
        }
    }

    /// Starts or extends `token`'s lease. A lease with more than half its
    /// time left is not rewritten.
    pub fn renew(&self, token: &str) {
        let token = token.trim();
        if token.is_empty() {
            return;
        }
        let now = clock::now_ms();
        let mut leases = self.lock();
        if leases
            .get(token)
            .is_some_and(|ends| ends - now > TOKEN_LEASE_MS / 2)
        {
            return;
        }
        leases.insert(token.to_string(), now + TOKEN_LEASE_MS);
        self.save(&leases);
    }

    pub fn revoke(&self, token: &str) {
        let mut leases = self.lock();
        if leases.remove(token.trim()).is_some() {
            self.save(&leases);
        }
    }

    /// The tokens whose lease has not ended; ended ones are dropped.
    pub fn active(&self) -> Vec<String> {
        let now = clock::now_ms();
        let mut leases = self.lock();
        let before = leases.len();
        leases.retain(|_, ends| *ends > now);
        if leases.len() != before {
            self.save(&leases);
        }
        leases.keys().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, i64>> {
        self.leases.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self, leases: &BTreeMap<String, i64>) {
        let Some(path) = &self.path else {
            return;
        };
        if let Err(err) = write_leases(path, leases) {
            warn!(%err, path = %path.display(), "could not save push tokens");
        }
    }
}

/// `{"subscriptions": [{"token", "expiresAt"}]}`, or the older `{"tokens":
/// [...]}`, whose tokens get a fresh lease. A missing or broken file holds
/// none.
pub fn read_leases(path: &Path) -> BTreeMap<String, i64> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Lease {
        token: String,
        expires_at: String,
    }
    #[derive(Deserialize, Default)]
    struct File {
        #[serde(default)]
        subscriptions: Vec<Value>,
        #[serde(default)]
        tokens: Vec<Value>,
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let file: File = serde_json::from_str(&text).unwrap_or_default();
    let mut leases = BTreeMap::new();
    for lease in file.subscriptions {
        let Ok(lease) = serde_json::from_value::<Lease>(lease) else {
            continue;
        };
        let token = lease.token.trim();
        if let Some(ends) = clock::parse_iso_ms(&lease.expires_at).filter(|_| !token.is_empty()) {
            leases.insert(token.to_string(), ends);
        }
    }
    let fresh = clock::now_ms() + TOKEN_LEASE_MS;
    for token in file.tokens.iter().filter_map(Value::as_str).map(str::trim) {
        if !token.is_empty() {
            leases.insert(token.to_string(), fresh);
        }
    }
    leases
}

/// Writes `leases` in Paseo's format, owner-only, through a temporary file.
pub fn write_leases(path: &Path, leases: &BTreeMap<String, i64>) -> std::io::Result<()> {
    let subscriptions: Vec<Value> = leases
        .iter()
        .map(|(token, ends)| json!({"token": token, "expiresAt": clock::iso_from_ms(*ends)}))
        .collect();
    let text = serde_json::to_string_pretty(&json!({"subscriptions": subscriptions}))? + "\n";
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text)?;
    private(&temp)?;
    std::fs::rename(&temp, path)
}

#[cfg(unix)]
fn private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn private(_: &Path) -> std::io::Result<()> {
    Ok(())
}

/// What a client's last `client_heartbeat` said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Presence {
    pub app_visible: bool,
    pub focused_agent_id: Option<String>,
    /// When the person last did something in the app, in ms.
    pub last_activity_ms: Option<i64>,
}

/// One connected client, as notifications see it.
#[derive(Debug, Default)]
struct Client {
    presence: Presence,
    /// The push token this client registered, renewed by its heartbeats.
    token: Option<String>,
    /// Its event subscriptions to `agent_attention_required` that asked
    /// for notifications.
    notifying: Vec<String>,
}

/// Every connected client, by connection.
#[derive(Default)]
pub struct Clients {
    by_connection: Mutex<HashMap<u64, Client>>,
}

/// Who hears about an agent that needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// The connection that shows a notification, if any.
    pub in_app: Option<u64>,
    /// Whether the registered phones get a push.
    pub push: bool,
}

impl Clients {
    fn with<R>(&self, connection: u64, f: impl FnOnce(&mut Client) -> R) -> R {
        let mut clients = self.by_connection.lock().unwrap_or_else(|p| p.into_inner());
        f(clients.entry(connection).or_default())
    }

    /// A `client_heartbeat`. Returns the client's push token, whose lease
    /// it renews.
    pub fn heartbeat(&self, connection: u64, presence: Presence) -> Option<String> {
        self.with(connection, |client| {
            client.presence = presence;
            client.token.clone()
        })
    }

    pub fn set_token(&self, connection: u64, token: Option<String>) {
        self.with(connection, |client| client.token = token);
    }

    /// Forgets `token` on whichever client registered it.
    pub fn clear_token(&self, token: &str) {
        let mut clients = self.by_connection.lock().unwrap_or_else(|p| p.into_inner());
        for client in clients.values_mut() {
            if client.token.as_deref().map(str::trim) == Some(token.trim()) {
                client.token = None;
            }
        }
    }

    /// An event subscription opened: `notifying` when it covers
    /// `agent_attention_required` and asked for notifications.
    pub fn subscribed(&self, connection: u64, subscription: &str, notifying: bool) {
        if notifying {
            self.with(connection, |client| {
                client.notifying.push(subscription.to_string())
            });
        }
    }

    pub fn released(&self, connection: u64, subscription: &str) {
        self.with(connection, |client| {
            client.notifying.retain(|id| id != subscription)
        });
    }

    /// The connection closed.
    pub fn gone(&self, connection: u64) {
        let mut clients = self.by_connection.lock().unwrap_or_else(|p| p.into_inner());
        clients.remove(&connection);
    }

    /// Who hears that `agent_id` needs attention, now. `pushable` is false
    /// for an error, which never pushes.
    pub fn plan(&self, agent_id: &str, pushable: bool, now_ms: i64) -> Plan {
        let clients = self.by_connection.lock().unwrap_or_else(|p| p.into_inner());
        let mut states: Vec<(u64, &Presence)> = (clients.iter())
            .filter(|(_, client)| !client.notifying.is_empty())
            .map(|(connection, client)| (*connection, &client.presence))
            .collect();
        // Ties go to the connection opened first, as Paseo's goes to the
        // first in its list.
        states.sort_by_key(|(connection, _)| *connection);
        plan(&states, agent_id, pushable, now_ms)
    }
}

/// `computeNotificationPlan`: a present client looking at the agent stops
/// everything; else the most recently active present client is told; else
/// the phones get a push, if it may push.
pub fn plan(states: &[(u64, &Presence)], agent_id: &str, pushable: bool, now_ms: i64) -> Plan {
    let mut chosen: Option<(u64, i64)> = None;
    for (connection, presence) in states {
        let Some(active) = presence.last_activity_ms.map(|at| at.min(now_ms)) else {
            continue;
        };
        if now_ms - active > PRESENCE_MS {
            continue;
        }
        if presence.app_visible && presence.focused_agent_id.as_deref() == Some(agent_id) {
            return Plan {
                in_app: None,
                push: false,
            };
        }
        if chosen.is_none_or(|(_, at)| active > at) {
            chosen = Some((*connection, active));
        }
    }
    match chosen {
        Some((connection, _)) => Plan {
            in_app: Some(connection),
            push: false,
        },
        None => Plan {
            in_app: None,
            push: pushable,
        },
    }
}

/// [`send`] on a thread of its own, so a slow or unreachable push service
/// holds nothing up.
pub fn send_later(url: String, tokens: Tokens, notification: Value) {
    let spawned = std::thread::Builder::new()
        .name("burrow-push".into())
        .spawn(move || send(&url, &tokens, &notification));
    if let Err(err) = spawned {
        warn!(%err, "push notification not sent");
    }
}

/// Sends `notification` (`{title, body, data}`) to every token through
/// `url`, a hundred to a request, and revokes the tokens Expo says are no
/// longer valid. Blocking: run it off the async runtime.
pub fn send(url: &str, tokens: &Tokens, notification: &Value) {
    let active = tokens.active();
    if active.is_empty() {
        return;
    }
    info!(tokens = active.len(), "sending push notification");
    for batch in active.chunks(BATCH) {
        let messages: Vec<Value> = batch
            .iter()
            .map(|token| {
                json!({
                    "to": token,
                    "title": notification["title"],
                    "body": notification["body"],
                    "data": notification["data"],
                    "sound": "default",
                })
            })
            .collect();
        match post(url, &Value::Array(messages)) {
            Ok(reply) => {
                for token in rejected(batch, &reply) {
                    warn!(token, "push token no longer valid; dropped");
                    tokens.revoke(token);
                }
            }
            Err(err) => warn!(%err, "push notification not sent"),
        }
    }
}

fn post(url: &str, body: &Value) -> Result<Value, String> {
    let mut response = ureq::post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|err| err.to_string())?;
    let text = (response.body_mut())
        .read_to_string()
        .map_err(|err| err.to_string())?;
    serde_json::from_str(&text).map_err(|err| err.to_string())
}

/// The tokens of `batch` whose ticket in Expo's `reply` says the device
/// is gone or the credentials are wrong.
pub fn rejected<'a>(batch: &'a [String], reply: &Value) -> Vec<&'a str> {
    let tickets = reply["data"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    (batch.iter().zip(tickets))
        .filter(|(_, ticket)| {
            ticket["status"] == "error"
                && matches!(
                    ticket["details"]["error"].as_str(),
                    Some("DeviceNotRegistered" | "InvalidCredentials")
                )
        })
        .map(|(token, _)| token.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(at: i64, visible: bool, focused: Option<&str>) -> Presence {
        Presence {
            app_visible: visible,
            focused_agent_id: focused.map(str::to_string),
            last_activity_ms: Some(at),
        }
    }

    #[test]
    fn the_most_recent_present_client_is_told_and_nobody_is_pushed() {
        let now = 1_000_000;
        let (a, b) = (
            present(now - 5_000, true, None),
            present(now - 1_000, false, None),
        );
        let plan = plan(&[(1, &a), (2, &b)], "agent", true, now);
        assert_eq!(
            plan,
            Plan {
                in_app: Some(2),
                push: false
            }
        );
    }

    #[test]
    fn a_client_looking_at_the_agent_stops_it_all() {
        let now = 1_000_000;
        let (a, b) = (
            present(now - 1_000, false, None),
            present(now - 9_000, true, Some("agent")),
        );
        assert_eq!(
            plan(&[(1, &a), (2, &b)], "agent", true, now),
            Plan {
                in_app: None,
                push: false
            }
        );
        // Not visible, so it does not count as looking.
        let hidden = present(now - 1_000, false, Some("agent"));
        assert_eq!(plan(&[(1, &hidden)], "agent", true, now).in_app, Some(1));
    }

    #[test]
    fn with_nobody_present_the_phones_are_pushed_but_never_for_an_error() {
        let now = 1_000_000;
        let stale = present(now - PRESENCE_MS - 1, true, None);
        let silent = Presence::default();
        let states = [(1, &stale), (2, &silent)];
        assert_eq!(
            plan(&states, "agent", true, now),
            Plan {
                in_app: None,
                push: true
            }
        );
        assert_eq!(
            plan(&states, "agent", false, now),
            Plan {
                in_app: None,
                push: false
            }
        );
        // An activity time in the future counts as now.
        let ahead = present(now + 60_000, false, None);
        assert_eq!(plan(&[(3, &ahead)], "agent", true, now).in_app, Some(3));
    }

    #[test]
    fn only_clients_that_asked_for_notifications_count() {
        let clients = Clients::default();
        let now = clock::now_ms();
        clients.heartbeat(1, present(now, true, None));
        assert!(clients.plan("agent", true, now).push, "1 never asked");
        clients.subscribed(1, "sub-1", true);
        assert_eq!(clients.plan("agent", true, now).in_app, Some(1));
        clients.released(1, "sub-1");
        assert!(clients.plan("agent", true, now).push);
        clients.subscribed(1, "sub-2", true);
        clients.gone(1);
        assert!(clients.plan("agent", true, now).push);
    }

    #[test]
    fn tokens_are_leased_kept_in_paseos_format_and_revoked() {
        let dir = std::env::temp_dir().join(format!("burrow-push-{}", crate::ids::agent_id()));
        let tokens = Tokens::open(Some(&dir));
        tokens.renew(" ExponentPushToken[abc] ");
        tokens.renew("ExponentPushToken[def]");
        assert_eq!(
            tokens.active(),
            ["ExponentPushToken[abc]", "ExponentPushToken[def]"]
        );
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("push-tokens.json")).unwrap())
                .unwrap();
        assert_eq!(saved["subscriptions"][0]["token"], "ExponentPushToken[abc]");
        assert!(
            saved["subscriptions"][0]["expiresAt"]
                .as_str()
                .unwrap()
                .ends_with('Z')
        );
        tokens.revoke("ExponentPushToken[abc]");
        assert_eq!(
            Tokens::open(Some(&dir)).active(),
            ["ExponentPushToken[def]"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ended_leases_go_and_old_token_lists_get_a_lease() {
        let dir = std::env::temp_dir().join(format!("burrow-push-{}", crate::ids::agent_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("push-tokens.json");
        std::fs::write(
            &path,
            r#"{"subscriptions": [{"token": "old", "expiresAt": "2020-01-01T00:00:00.000Z"},
                                  {"token": "", "expiresAt": "2099-01-01T00:00:00.000Z"},
                                  {"token": "new", "expiresAt": "2099-01-01T00:00:00.000Z"}],
                "tokens": ["legacy"]}"#,
        )
        .unwrap();
        assert_eq!(Tokens::open(Some(&dir)).active(), ["legacy", "new"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expo_rejections_name_the_tokens_to_drop() {
        let batch = ["a".to_string(), "b".to_string(), "c".to_string()];
        let reply = json!({"data": [
            {"status": "ok", "id": "1"},
            {"status": "error", "details": {"error": "DeviceNotRegistered"}},
            {"status": "error", "details": {"error": "MessageRateExceeded"}},
        ]});
        assert_eq!(rejected(&batch, &reply), ["b"]);
    }
}
