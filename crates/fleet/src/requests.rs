//! Request bodies, so callers do not spell wire type strings.

use serde_json::{Value, json};

use paseo_client::proto::Cursor;

pub const PAGE_LIMIT: u32 = 200;

fn sync_field(cursor: Option<&Cursor>) -> Value {
    match cursor {
        Some(cursor) => json!({ "generation": cursor.generation, "afterSeq": cursor.after_seq }),
        None => json!({}),
    }
}

/// First page of workspaces, subscribed, with a sync cursor when the daemon
/// supports directory sync (sync ignores paging and returns everything).
pub fn fetch_workspaces(directory_sync: bool, cursor: Option<&Cursor>) -> Value {
    let mut request = json!({
        "type": "fetch_workspaces_request",
        "sort": [{ "key": "activity_at", "direction": "desc" }],
        "page": { "limit": PAGE_LIMIT },
        "subscribe": {},
    });
    if directory_sync {
        request["sync"] = sync_field(cursor);
    }
    request
}

pub fn fetch_workspaces_page(cursor: &str) -> Value {
    json!({
        "type": "fetch_workspaces_request",
        "sort": [{ "key": "activity_at", "direction": "desc" }],
        "page": { "limit": PAGE_LIMIT, "cursor": cursor },
    })
}

pub fn fetch_agents(directory_sync: bool, cursor: Option<&Cursor>) -> Value {
    let mut request = json!({
        "type": "fetch_agents_request",
        "scope": "active",
        "sort": [{ "key": "updated_at", "direction": "desc" }],
        "page": { "limit": PAGE_LIMIT },
        "subscribe": {},
    });
    if directory_sync {
        request["sync"] = sync_field(cursor);
    }
    request
}

pub fn fetch_agents_page(cursor: &str) -> Value {
    json!({
        "type": "fetch_agents_request",
        "scope": "active",
        "sort": [{ "key": "updated_at", "direction": "desc" }],
        "page": { "limit": PAGE_LIMIT, "cursor": cursor },
    })
}

pub fn list_projects(directory_sync: bool, cursor: Option<&Cursor>) -> Value {
    let mut request = json!({ "type": "project.list.request" });
    if directory_sync {
        request["sync"] = sync_field(cursor);
    }
    request
}

pub fn list_labels() -> Value {
    json!({ "type": "workspace.label.list.request", "subscribe": {} })
}

pub fn session_events(events: &[&str]) -> Value {
    json!({ "type": "session.events.set_subscription.request", "events": events })
}

pub fn release(subscription_id: &str) -> Value {
    json!({ "type": "subscription.release.request", "subscriptionId": subscription_id })
}

pub fn pin_workspace(workspace_id: &str, pinned: bool) -> Value {
    json!({ "type": "workspace.pin.set.request", "workspaceId": workspace_id, "pinned": pinned })
}

pub fn rename_workspace(workspace_id: &str, title: Option<&str>) -> Value {
    json!({ "type": "workspace.title.set.request", "workspaceId": workspace_id, "title": title })
}

pub fn archive_workspace(workspace_id: &str) -> Value {
    json!({ "type": "archive_workspace_request", "workspaceId": workspace_id })
}

pub fn clear_attention(workspace_id: &str) -> Value {
    json!({ "type": "workspace.clear_attention.request", "workspaceId": workspace_id })
}

pub fn mark_unread(workspace_id: &str) -> Value {
    json!({ "type": "workspace.mark_unread.request", "workspaceId": workspace_id })
}

pub fn set_label(workspace_id: &str, name: &str, color: &str, assigned: bool) -> Value {
    json!({
        "type": "workspace.label.assignment.set.request",
        "workspaceId": workspace_id,
        "label": { "name": name, "color": color },
        "assigned": assigned,
    })
}

pub fn archive_agent(agent_id: &str) -> Value {
    json!({ "type": "archive_agent_request", "agentId": agent_id })
}

pub fn rename_agent(agent_id: &str, name: &str) -> Value {
    json!({ "type": "update_agent_request", "agentId": agent_id, "name": name })
}

/// Marks whether this client shows a subagent in a tab. Paseo keeps a
/// subagent while any client has it open; closing its tab only says this
/// one no longer does.
pub fn set_open_agent_tab(agent_id: &str, client_id: &str, open: bool) -> Value {
    let label = format!("paseo.open-agent-tab.{client_id}");
    json!({
        "type": "update_agent_request",
        "agentId": agent_id,
        "labels": { label: if open { "true" } else { "false" } },
    })
}

/// Makes a subagent a standalone agent.
pub fn detach_agent(agent_id: &str) -> Value {
    json!({ "type": "agent.detach.request", "agentId": agent_id })
}

/// The subagents an agent's provider runs inside it.
pub fn list_provider_subagents(parent_agent_id: &str) -> Value {
    json!({ "type": "agent.provider_subagents.list.request", "parentAgentId": parent_agent_id })
}

/// A page of a provider subagent's timeline: the tail, or the page before
/// `before`, a cursor `(epoch, seq)`, or after `after`.
pub fn provider_subagent_timeline(
    parent_agent_id: &str,
    subagent_id: &str,
    direction: &str,
    cursor: Option<(&str, u64)>,
    limit: u32,
) -> Value {
    let mut request = json!({
        "type": "agent.provider_subagents.timeline.get.request",
        "parentAgentId": parent_agent_id,
        "subagentId": subagent_id,
        "direction": direction,
        "limit": limit,
    });
    if let Some((epoch, seq)) = cursor {
        request["cursor"] = json!({ "epoch": epoch, "seq": seq });
    }
    request
}

pub fn subscribe_terminals(cwd: &str, workspace_id: &str) -> Value {
    json!({
        "type": "subscribe_terminals_request",
        "cwd": cwd,
        "workspaceId": workspace_id,
    })
}

pub fn create_terminal(cwd: &str, workspace_id: &str, rows: u32, cols: u32) -> Value {
    let mut message = json!({
        "type": "create_terminal_request",
        "cwd": cwd,
        "workspaceId": workspace_id,
    });
    if rows > 0 && cols > 0 {
        message["size"] = json!({ "rows": rows, "cols": cols });
    }
    message
}

/// Attach to a PTY. `restore` is the reference client's visible snapshot
/// when the daemon advertises `terminal-restore-modes`.
pub fn subscribe_terminal(terminal_id: &str, restore: Option<Value>) -> Value {
    let mut message = json!({
        "type": "subscribe_terminal_request",
        "terminalId": terminal_id,
    });
    if let Some(restore) = restore {
        message["restore"] = restore;
    }
    message
}

pub fn kill_terminal(terminal_id: &str) -> Value {
    json!({ "type": "kill_terminal_request", "terminalId": terminal_id })
}

/// Keystrokes. The reference app sends these as JSON, not binary Input frames.
pub fn terminal_input(terminal_id: &str, data: &str) -> Value {
    json!({
        "type": "terminal_input",
        "terminalId": terminal_id,
        "message": { "type": "input", "data": data },
    })
}

pub fn terminal_resize(terminal_id: &str, rows: u32, cols: u32, intent: &str) -> Value {
    json!({
        "type": "terminal_input",
        "terminalId": terminal_id,
        "message": { "type": "resize", "rows": rows, "cols": cols, "intent": intent },
    })
}

pub fn send_message(
    agent_id: &str,
    message_id: &str,
    prompt: &crate::prompt::Prompt,
    behavior: Option<&str>,
) -> Value {
    let mut message = json!({"type":"send_agent_message_request", "agentId":agent_id, "text":prompt.text, "messageId":message_id});
    add_images(&mut message, prompt);
    if let Some(behavior) = behavior {
        message["activeTurnBehavior"] = json!(behavior);
    }
    message
}

pub fn create_agent(
    workspace_id: &str,
    config: Value,
    prompt: Option<crate::prompt::Prompt>,
    message_id: &str,
) -> Value {
    let mut message = json!({"type":"agent.create.request", "workspaceId":workspace_id, "config":config, "labels":{}});
    if let Some(prompt) = prompt.filter(|p| !p.is_empty()) {
        message["initialPrompt"] = json!(prompt.text);
        add_images(&mut message, &prompt);
        message["clientMessageId"] = json!(message_id);
    }
    message
}

fn add_images(message: &mut Value, prompt: &crate::prompt::Prompt) {
    if !prompt.images.is_empty() {
        message["images"] = json!(prompt.images);
    }
}

pub fn prompt_index(agent_id: &str) -> Value {
    json!({"type":"agent.timeline.list_prompts.request", "agentId":agent_id})
}
