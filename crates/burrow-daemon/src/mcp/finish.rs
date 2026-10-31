//! Finish notices (Paseo's `finish-subscriptions`): an agent that made or
//! prompted another through its tools with `notifyOnFinish` is told when
//! that agent finishes, errors, needs permission or goes away, in an
//! `agent-notification` envelope with Paseo's wording. One subscription per
//! caller and child: a newer one replaces the older. Kept in
//! `<home>/finish-subscriptions.json`, so a restart keeps the intent. A
//! caller that is gone or archived is told nothing.
//!
//! Paseo's version also follows children on other hosts, with retries and
//! acknowledged dispatch; Burrow has no connected hosts, so a child is
//! always local and its daemon is this one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::agents;
use crate::daemon::Daemon;
use crate::store::StoreEvent;

/// How much of the child's last reply a notice carries.
const MESSAGE_LIMIT: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscription {
    id: String,
    caller_agent_id: String,
    child_agent_id: String,
}

/// The subscriptions waiting on a child.
pub struct Finish {
    path: Option<PathBuf>,
    subscriptions: Mutex<Vec<Subscription>>,
}

impl Finish {
    pub fn open(home: Option<&Path>) -> Self {
        let path = home.map(|home| home.join("finish-subscriptions.json"));
        let subscriptions = (path.as_deref())
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            subscriptions: Mutex::new(subscriptions),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Subscription>> {
        self.subscriptions.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self, all: &[Subscription]) {
        let Some(path) = &self.path else {
            return;
        };
        let written = serde_json::to_string_pretty(all)
            .map_err(std::io::Error::other)
            .and_then(|text| std::fs::write(path, text + "\n"));
        if let Err(err) = written {
            warn!(%err, "could not save finish subscriptions");
        }
    }

    /// Whether `id` still stands (nothing replaced or ended it).
    fn current(&self, id: &str) -> bool {
        self.lock().iter().any(|s| s.id == id)
    }

    fn remove(&self, id: &str) {
        let mut all = self.lock();
        let before = all.len();
        all.retain(|s| s.id != id);
        if all.len() != before {
            self.save(&all);
        }
    }
}

/// `caller` is to be told how `child`'s work ends. Register once the work
/// has started: a child that is idle now counts as finished.
pub fn register(daemon: &Arc<Daemon>, caller: &str, child: &str) {
    let subscription = Subscription {
        id: uuid::Uuid::new_v4().to_string(),
        caller_agent_id: caller.to_string(),
        child_agent_id: child.to_string(),
    };
    {
        let mut all = daemon.finish.lock();
        all.retain(|s| !(s.caller_agent_id == caller && s.child_agent_id == child));
        all.push(subscription.clone());
        daemon.finish.save(&all);
    }
    tokio::spawn(watch(daemon.clone(), subscription));
}

/// At start: the subscriptions a stop left waiting.
pub fn resume(daemon: &Arc<Daemon>) {
    let all = daemon.finish.lock().clone();
    for subscription in all {
        tokio::spawn(watch(daemon.clone(), subscription));
    }
}

async fn watch(daemon: Arc<Daemon>, subscription: Subscription) {
    let Subscription {
        id,
        caller_agent_id: caller,
        child_agent_id: child,
    } = &subscription;
    // Permission requests already told of, so each is told once.
    let mut told: BTreeSet<String> = BTreeSet::new();
    loop {
        if !daemon.finish.current(id) {
            return;
        }
        if !daemon.read(|store| store.agents.get(caller).is_some()) {
            daemon.finish.remove(id);
            return;
        }
        let answer = agents::wait_for_finish(&daemon, child, None).await;
        if !daemon.finish.current(id) {
            return;
        }
        let reason = reason(&answer);
        if reason == "needs permission" {
            let pending: Vec<Value> = (answer["final"]["pendingPermissions"].as_array())
                .cloned()
                .unwrap_or_default();
            for request in &pending {
                let request_id = request["id"].as_str().unwrap_or_default().to_string();
                if told.insert(request_id) {
                    tell(&daemon, caller, child, &answer, reason, Some(request)).await;
                }
            }
            told.retain(|t| pending.iter().any(|r| r["id"] == t.as_str()));
            permissions_change(&daemon, child, &told).await;
            continue;
        }
        tell(&daemon, caller, child, &answer, reason, None).await;
        daemon.finish.remove(id);
        return;
    }
}

/// Paseo's `reasonFor`.
fn reason(answer: &Value) -> &'static str {
    let last = &answer["final"];
    if last.is_null() {
        return "is unavailable";
    }
    if last["status"] == "closed" {
        return "was closed";
    }
    match answer["status"].as_str() {
        Some("error") => "errored",
        Some("permission") => "needs permission",
        _ => "finished",
    }
}

/// The envelope's `event` for a reason (Paseo's `finishNotificationEvents`).
fn event(reason: &str) -> &'static str {
    match reason {
        "finished" => "finished",
        "needs permission" => "permission-required",
        "was closed" | "is unavailable" => "closed",
        _ => "errored",
    }
}

/// Waits until `child` asks for a permission not yet told of, or has none
/// pending.
async fn permissions_change(daemon: &Daemon, child: &str, told: &BTreeSet<String>) {
    let mut changes = daemon.changes();
    loop {
        let settled = daemon.read(|store| match store.agents.get(child) {
            None => true,
            Some(agent) => {
                let pending: Vec<&str> = (agent.pending_permissions.iter())
                    .filter_map(|p| p["id"].as_str())
                    .collect();
                pending.is_empty() || pending.iter().any(|p| !told.contains(*p))
            }
        });
        if settled {
            return;
        }
        match changes.recv().await {
            Ok(event) if matches!(*event, StoreEvent::Agent { .. }) => {}
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn tell(
    daemon: &Arc<Daemon>,
    caller: &str,
    child: &str,
    answer: &Value,
    reason: &str,
    permission: Option<&Value>,
) {
    let title = (answer["final"]["title"].as_str())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(child);
    let body = body(
        child,
        &daemon.config.server_id,
        title,
        reason,
        answer["lastMessage"].as_str(),
        permission,
    );
    if let Err(error) = agents::send_notice(daemon, caller, child, event(reason), &body).await {
        warn!(caller, child, %error, "finish notice not delivered");
    }
}

/// Paseo's `formatFinishNotificationBody`, for a local child.
fn body(
    child: &str,
    server_id: &str,
    title: &str,
    reason: &str,
    last_message: Option<&str>,
    permission: Option<&Value>,
) -> String {
    let mut sections = vec![
        format!("Agent {child} ({title}) {reason}."),
        format!("Server: {server_id}"),
    ];
    if let Some(request) = permission.filter(|_| reason == "needs permission") {
        let detail = json!({
            "agentId": child, "serverId": server_id,
            "requestId": request["id"], "request": request,
        });
        sections.push(
            "Respond with `respond_to_permission` using the `agentId` and `requestId` below."
                .into(),
        );
        let detail = serde_json::to_string_pretty(&detail).unwrap_or_default();
        sections.push(format!(
            "<permission-request>\n{detail}\n</permission-request>"
        ));
    }
    if let Some(message) = last_message.map(str::trim).filter(|m| !m.is_empty()) {
        let chars = message.chars().count();
        let message = if chars > MESSAGE_LIMIT {
            let kept: String = message.chars().take(MESSAGE_LIMIT).collect();
            format!(
                "{kept}\n[truncated {} chars; use get_agent_activity for the full response]",
                chars - MESSAGE_LIMIT
            )
        } else {
            message.to_string()
        };
        sections.push(format!("<agent-response>\n{message}\n</agent-response>"));
    }
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_reads_as_paseos() {
        let text = body(
            "child-1",
            "srv_x",
            "Fix the build",
            "finished",
            Some(" Done. "),
            None,
        );
        assert_eq!(
            text,
            "Agent child-1 (Fix the build) finished.\n\nServer: srv_x\n\n<agent-response>\nDone.\n</agent-response>"
        );
        let request = json!({"id": "p1", "name": "Bash", "kind": "tool"});
        let asking = body(
            "child-1",
            "srv_x",
            "t",
            "needs permission",
            None,
            Some(&request),
        );
        assert!(asking.contains("Respond with `respond_to_permission`"));
        assert!(asking.contains("\"requestId\": \"p1\""));
        let long = "x".repeat(MESSAGE_LIMIT + 5);
        assert!(
            body("c", "s", "t", "finished", Some(&long), None)
                .contains("[truncated 5 chars; use get_agent_activity for the full response]")
        );
    }

    #[test]
    fn the_reason_and_event_follow_how_the_wait_ended() {
        let wait = |status: &str, last: Value| json!({"status": status, "final": last});
        assert_eq!(reason(&wait("idle", json!({"status": "idle"}))), "finished");
        assert_eq!(
            reason(&wait("error", json!({"status": "error"}))),
            "errored"
        );
        assert_eq!(
            reason(&wait("permission", json!({"status": "running"}))),
            "needs permission"
        );
        assert_eq!(
            reason(&wait("idle", json!({"status": "closed"}))),
            "was closed"
        );
        assert_eq!(reason(&wait("error", Value::Null)), "is unavailable");
        assert_eq!(event("needs permission"), "permission-required");
        assert_eq!(event("is unavailable"), "closed");
    }
}
