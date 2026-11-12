//! Permissions and attention: the requests an agent raises, the answers
//! clients give, the attention they latch, and the notifications they send.

use std::sync::Arc;

use burrow_protocol::directory::{AgentSnapshot, AttentionReason};
use burrow_provider::Command;
use serde_json::{Value, json};

use super::{last_assistant_text, preview, publish, resolve, stream, update};
use crate::clock;
use crate::daemon::Daemon;
use crate::store::StoreEvent;

/// `agent_attention_required`, for event subscribers.
pub(super) fn attention(daemon: &Daemon, agent: &AgentSnapshot, reason: AttentionReason) {
    // A child agent, and an agent a schedule started (`notifyOnFinish:
    // false` in Paseo), raise none.
    if agent.labels.contains_key("paseo.parent-agent-id")
        || agent.labels.contains_key(crate::schedules::SCHEDULE_LABEL)
    {
        return;
    }
    let (reason_name, title, fallback) = match reason {
        AttentionReason::Finished => ("finished", "Agent finished", "Finished working."),
        AttentionReason::Permission => (
            "permission",
            "Agent needs permission",
            "Permission requested.",
        ),
        AttentionReason::Error => ("error", "Agent needs attention", "Encountered an error."),
    };
    let body = match reason {
        AttentionReason::Finished => {
            preview(&daemon.read(|store| last_assistant_text(store, &agent.id)))
        }
        AttentionReason::Permission => agent
            .pending_permissions
            .last()
            .map(permission_summary)
            .map(|s| preview(&s))
            .unwrap_or_default(),
        AttentionReason::Error => String::new(),
    };
    let body = if body.is_empty() {
        fallback.to_string()
    } else {
        body
    };
    let timestamp = clock::now_iso();
    let notification = json!({
        "title": title,
        "body": body,
        "data": {
            "serverId": daemon.config.server_id,
            "workspaceId": agent.workspace_id,
            "agentId": agent.id,
            "reason": reason_name,
        },
    });
    // One present client shows it, or else the phones get a push; an
    // error is never pushed.
    let pushable = !matches!(reason, AttentionReason::Error);
    let plan = daemon.clients.plan(&agent.id, pushable, clock::now_ms());
    let mut payload = json!({
        "agentId": agent.id,
        "reason": reason_name,
        "timestamp": timestamp,
        "shouldNotify": false,
        "notification": notification,
    });
    if let Some(connection) = plan.in_app {
        payload[crate::subscriptions::NOTIFY_CONNECTION] = connection.into();
    }
    publish(
        daemon,
        vec![StoreEvent::Session {
            kind: "agent_attention_required",
            payload,
        }],
    );
    if let (true, Some(url)) = (plan.push, &daemon.config.push_url) {
        crate::push::send_later(url.clone(), daemon.push_tokens.clone(), notification);
    }
}

fn permission_summary(request: &Value) -> String {
    let parts: Vec<&str> = ["title", "description"]
        .iter()
        .filter_map(|key| request[*key].as_str())
        .filter(|s| !s.is_empty())
        .collect();
    if !parts.is_empty() {
        return parts.join(" - ");
    }
    for key in ["input", "metadata"] {
        if !request[key].is_null() {
            return request[key].to_string();
        }
    }
    request["name"]
        .as_str()
        .or(request["kind"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// Publishes a resolution: the stream event and the session event.
pub(super) fn resolved(
    daemon: &Daemon,
    agent_id: &str,
    provider: &str,
    request_id: &str,
    resolution: Value,
) {
    update(daemon, agent_id, |agent| {
        agent
            .pending_permissions
            .retain(|p| p["id"].as_str() != Some(request_id));
    });
    stream(
        daemon,
        agent_id,
        json!({"type": "permission_resolved", "provider": provider,
               "requestId": request_id, "resolution": resolution}),
    );
    publish(
        daemon,
        vec![StoreEvent::Session {
            kind: "agent_permission_resolved",
            payload: json!({"agentId": agent_id, "requestId": request_id, "resolution": resolution}),
        }],
    );
}

/// `agent_permission_response`. Returns the resolution.
pub fn respond_permission(
    daemon: &Arc<Daemon>,
    key: &str,
    request_id: &str,
    response: Value,
) -> Result<Value, String> {
    let agent_id = resolve(daemon, key)?;
    let live = daemon
        .agents
        .live(&agent_id)
        .ok_or_else(|| format!("Agent {agent_id} has no pending permission {request_id}"))?;
    let provider_id = {
        let mut state = live.state.lock().unwrap_or_else(|p| p.into_inner());
        let at = state
            .permissions
            .iter()
            .position(|(client, _, _)| client == request_id)
            .ok_or_else(|| format!("Permission request {request_id} not found"))?;
        if !question_answer_deliverable(&state.permissions[at].2, &response) {
            // Paseo 0.12: the question stays pending and can be answered again.
            return Err("question_answer_required: answer at least one question".into());
        }
        state.permissions.remove(at).1
    };
    let _ = live.commands.send(Command::RespondPermission {
        id: provider_id,
        response: response.clone(),
    });
    resolved(
        daemon,
        &agent_id,
        &live.info.id,
        request_id,
        response.clone(),
    );
    Ok(response)
}

/// An `allow` answer to a question must carry at least one non-empty
/// answer, keyed by a question's header or its full text.
fn question_answer_deliverable(request: &Value, response: &Value) -> bool {
    if request["kind"] != "question" || response["behavior"] != "allow" {
        return true;
    }
    let answers = &response["updatedInput"]["answers"];
    request["input"]["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|q| {
            ["header", "question"].iter().any(|key| {
                q[*key]
                    .as_str()
                    .and_then(|k| answers[k].as_str())
                    .is_some_and(|a| !a.is_empty())
            })
        })
}

/// `clear_agent_attention`. Returns the snapshots of the agents it names.
pub fn clear_attention(daemon: &Arc<Daemon>, keys: &[String]) -> Vec<AgentSnapshot> {
    let mut agents = Vec::new();
    for key in keys {
        let Ok(agent_id) = resolve(daemon, key) else {
            continue;
        };
        let requires =
            daemon.read(|store| store.agents.get(&agent_id).map(|a| a.requires_attention));
        let snapshot = if requires == Some(true) {
            update(daemon, &agent_id, |agent| {
                agent.requires_attention = false;
                agent.attention_reason = None;
                agent.attention_timestamp = None;
            })
        } else {
            daemon.read(|store| store.agent(&agent_id).cloned())
        };
        agents.extend(snapshot);
    }
    agents
}
