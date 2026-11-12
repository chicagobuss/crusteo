//! Reef's allowlist: every request to a daemon goes through [`guarded`],
//! which refuses any type not in [`READ_REQUESTS`] or [`WRITE_REQUESTS`]
//! (or one of the two checked `update_agent_request`s). The writes are the
//! phone's actions (`actions.rs`, and `agents.rs` for creating, deleting and
//! relabelling), each added on purpose. `clear_agent_attention` is one:
//! opening an agent in Skiff marks it seen, on its host, so a desktop sees
//! it seen too, as one opened there clears it for Skiff. Never on the
//! list: anything about terminals, and workspace changes other than
//! assigning a label.

use std::time::Duration;

use paseo_client::Client;
use serde_json::Value;

/// The requests Reef sends a daemon to keep its picture current.
pub const READ_REQUESTS: &[&str] = &[
    "session.events.set_subscription.request",
    "project.list.request",
    "fetch_workspaces_request",
    "fetch_agents_request",
    "workspace.label.list.request",
    "agent.timeline.set_subscription.request",
    "fetch_agent_timeline_request",
    // One agent by id, archived ones too: recording asks it of an agent
    // that left the active list, to tell archived from deleted.
    "fetch_agent_request",
    "subscription.release.request",
    // Each account's usage, for the `usage` view.
    usage_source::reports::REPORTS_REQUEST,
    // Providers, models and modes, for a phone's new-agent form; asked
    // only when a phone opens one.
    "get_providers_snapshot_request",
    // An image a conversation Reef holds keeps in a file: one a prompt
    // lists or a reply's Markdown names, never a path a phone gives
    // (`Upstream::read_image`).
    "file_explorer_request",
    // How a prompt Reef routed from another host ends, to tell its sender
    // (`routing.rs`); only while routing is on, or for routes under way.
    "wait_for_finish_request",
];

/// The only writes: what the phone's actions send, and nothing else.
pub const WRITE_REQUESTS: &[&str] = &[
    // Allow or deny a permission; answer or dismiss a question.
    "agent_permission_response",
    // Stop the running turn.
    "cancel_agent_request",
    // Send a message: to an idle agent, from the queue, or steering.
    "send_agent_message_request",
    // Archive an agent; the page asks first.
    "archive_agent_request",
    // Create an agent, in a workspace or a directory.
    "agent.create.request",
    // Delete an agent for good; the page asks first.
    "delete_agent_request",
    // Mark an agent seen: its "ready to review" goes, everywhere.
    "clear_agent_attention",
    // Put a label on an agent's workspace, or take it off: the labels a
    // row shows are its workspace's, as in crusteo.
    "workspace.label.assignment.set.request",
    // Tell an agent how a prompt it sent to another host, which Reef
    // routed, ended (Burrows only; `routing.rs`). Routed prompts
    // themselves go as `send_agent_message_request`.
    paseo_client::routing::FINISHED,
];

/// The one `update_agent_request` Reef may send: an agent's labels and
/// nothing else ([`fleet::labels::set_request`]), checked field by field, so
/// a name, or any other label key, never rides along.
pub fn labels_write(message: &Value) -> bool {
    let Some(fields) = message.as_object() else {
        return false;
    };
    let only_labels = (message["labels"].as_object()).is_some_and(|labels| {
        labels.len() == 1 && labels.get(fleet::labels::KEY).is_some_and(Value::is_string)
    });
    message["type"] == "update_agent_request"
        && message["agentId"].is_string()
        && only_labels
        && (fields.keys())
            .all(|key| matches!(key.as_str(), "type" | "agentId" | "labels" | "requestId"))
}

/// The longest agent name a phone may give.
pub const NAME_MAX: usize = 200;

/// The other `update_agent_request` Reef may send: an agent's new name
/// (crusteo's rename) and nothing else, checked field by field, so labels
/// never ride along.
pub fn name_write(message: &Value) -> bool {
    let Some(fields) = message.as_object() else {
        return false;
    };
    let name_ok = message["name"]
        .as_str()
        .is_some_and(|name| !name.trim().is_empty() && name.chars().count() <= NAME_MAX);
    message["type"] == "update_agent_request"
        && message["agentId"].is_string()
        && name_ok
        && (fields.keys())
            .all(|key| matches!(key.as_str(), "type" | "agentId" | "name" | "requestId"))
}

/// The one tool request Reef may send a Burrow: one of
/// [`paseo_client::routing::TOOLS`] (reads, stopping a turn, answering a
/// permission) for an agent on another host, while routing is on
/// (`routing.rs`), checked field by field.
pub fn tool_write(message: &Value) -> bool {
    use paseo_client::routing::{TOOL_REQUEST, TOOLS};
    let Some(fields) = message.as_object() else {
        return false;
    };
    message["type"] == TOOL_REQUEST
        && (message["tool"].as_str()).is_some_and(|tool| TOOLS.contains(&tool))
        && message["args"].is_object()
        && (fields.keys()).all(|key| matches!(key.as_str(), "type" | "tool" | "args" | "requestId"))
}

/// Sends `message` if it is a known read or one of the phone's writes;
/// anything else is refused here.
pub fn guarded(
    client: &Client,
    message: Value,
) -> impl std::future::Future<Output = Result<Value, String>> + Send + 'static {
    guarded_with_timeout(client, message, paseo_client::DEFAULT_TIMEOUT)
}

/// [`guarded`] for a read the daemon answers in several messages, the last
/// of type `until`: the earlier messages' payloads, and the last's.
pub fn guarded_until(
    client: &Client,
    message: Value,
    until: &str,
) -> impl std::future::Future<Output = Result<(Vec<Value>, Value), String>> + Send + 'static {
    let kind = message["type"].as_str().unwrap_or_default().to_string();
    let allowed = READ_REQUESTS.contains(&kind.as_str());
    let reply =
        allowed.then(|| client.request_until(message, until, paseo_client::DEFAULT_TIMEOUT));
    async move {
        match reply {
            Some(reply) => reply.await.map_err(|err| err.to_string()),
            None => {
                eprintln!("reef: refused to send {kind:?}: not on Reef's list");
                Err(format!("{kind} is not on Reef's list"))
            }
        }
    }
}

pub fn guarded_with_timeout(
    client: &Client,
    message: Value,
    timeout: Duration,
) -> impl std::future::Future<Output = Result<Value, String>> + Send + 'static {
    let kind = message["type"].as_str().unwrap_or_default().to_string();
    let allowed = READ_REQUESTS.contains(&kind.as_str())
        || WRITE_REQUESTS.contains(&kind.as_str())
        || labels_write(&message)
        || name_write(&message)
        || tool_write(&message);
    let reply = allowed.then(|| client.request_with_timeout(message, timeout));
    async move {
        match reply {
            Some(reply) => reply.await.map_err(|err| err.to_string()),
            None => {
                eprintln!("reef: refused to send {kind:?}: not on Reef's list");
                Err(format!("{kind} is not on Reef's list"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_client::Config;
    use serde_json::json;

    #[test]
    fn the_one_usage_read_is_on_the_list() {
        // Reports, not the older provider usage or anything that sets up
        // or changes a usage source.
        let usage: Vec<&&str> = (READ_REQUESTS.iter())
            .filter(|kind| kind.contains("usage"))
            .collect();
        assert_eq!(usage, [&"usage.list_reports.request"]);
        let client = Client::connect(Config::new("127.0.0.1:9", "cid_test"));
        let older = json!({"type": "provider.usage.list.request"});
        let result = paseo_client::block_on(guarded_until(&client, older, "x"));
        assert!(result.unwrap_err().contains("not on Reef's list"));
    }

    #[test]
    fn the_only_agent_update_is_its_labels_key() {
        let id = fleet::labels::LabelId::new("lbl_a");
        assert!(labels_write(&fleet::labels::set_request("a", &[id])));
        for refused in [
            json!({"type": "update_agent_request", "agentId": "a", "name": "x",
                   "labels": {fleet::labels::KEY: "v1:"}}),
            json!({"type": "update_agent_request", "agentId": "a",
                   "labels": {fleet::labels::KEY: "v1:", "paseo.parent-agent-id": "b"}}),
            json!({"type": "update_agent_request", "agentId": "a", "labels": {"other": "x"}}),
            json!({"type": "update_agent_request", "agentId": "a", "labels": {fleet::labels::KEY: 1}}),
            json!({"type": "send_agent_message_request", "agentId": "a",
                   "labels": {fleet::labels::KEY: "v1:"}}),
        ] {
            assert!(!labels_write(&refused), "{refused}");
        }
    }

    #[test]
    fn a_name_goes_alone_and_is_a_real_name() {
        assert!(name_write(
            &json!({"type": "update_agent_request", "agentId": "a", "name": "Fix the build"})
        ));
        for refused in [
            json!({"type": "update_agent_request", "agentId": "a", "name": "x",
                   "labels": {fleet::labels::KEY: "v1:"}}),
            json!({"type": "update_agent_request", "agentId": "a", "name": "   "}),
            json!({"type": "update_agent_request", "agentId": "a", "name": 3}),
            json!({"type": "update_agent_request", "agentId": "a",
                   "name": "x".repeat(NAME_MAX + 1)}),
            json!({"type": "update_agent_request", "name": "x"}),
            json!({"type": "send_agent_message_request", "agentId": "a", "name": "x"}),
        ] {
            assert!(!name_write(&refused), "{refused}");
        }
    }

    #[test]
    fn a_tool_for_another_host_is_one_of_the_few() {
        use paseo_client::routing::TOOL_REQUEST;
        assert!(tool_write(
            &json!({"type": TOOL_REQUEST, "tool": "get_agent_activity", "args": {"agentId": "a"}})
        ));
        for refused in [
            json!({"type": TOOL_REQUEST, "tool": "create_agent", "args": {}}),
            json!({"type": TOOL_REQUEST, "tool": "archive_agent", "args": {"agentId": "a"}}),
            json!({"type": TOOL_REQUEST, "tool": "send_agent_prompt", "args": {}}),
            json!({"type": TOOL_REQUEST, "tool": "list_agents"}),
            json!({"type": TOOL_REQUEST, "tool": "list_agents", "args": {}, "extra": 1}),
            json!({"type": "send_agent_message_request", "tool": "list_agents", "args": {}}),
        ] {
            assert!(!tool_write(&refused), "{refused}");
        }
    }

    #[test]
    fn writes_off_the_list_are_refused_before_they_leave() {
        // A client that never connects: a refused request must fail at
        // once, without being queued for the socket.
        let client = Client::connect(Config::new("127.0.0.1:9", "cid_test"));
        for write in [
            json!({"type": "workspace.clear_attention.request", "workspaceId": "w"}),
            json!({"type": "update_agent_request", "agentId": "a", "name": "x",
                   "labels": {"other": "x"}}),
            json!({"type": "create_terminal_request", "cwd": "/"}),
            json!({"type": "create_agent_request", "config": {}}),
            json!({"type": "workspace.label.delete.request", "name": "x"}),
            json!({"type": "archive_workspace_request", "workspaceId": "w"}),
        ] {
            let result = paseo_client::block_on(guarded(&client, write));
            assert!(result.unwrap_err().contains("not on Reef's list"));
        }
    }
}
