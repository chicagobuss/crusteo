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

/// How a host's directory is read: with sync cursors when the daemon has
/// `directorySync`, and background workspaces included when it has
/// `backgroundWorkspaces`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirectoryRead {
    pub sync: bool,
    pub background: bool,
}

impl DirectoryRead {
    pub fn for_host(host: &crate::Host) -> Self {
        Self {
            sync: host.has("directorySync"),
            background: host.has(paseo_client::proto::BACKGROUND_WORKSPACES),
        }
    }
}

/// Asks for background workspaces and their agents. Paseo 0.12 takes the
/// flag at the top level of every directory read: in `filter` it would
/// refuse a sequenced read, and the top-level flag also reaches the
/// subscription, so pushes include them too. Daemons before 0.12 drop the
/// unknown field, but it is only sent to those that advertise it.
fn include_background(request: &mut Value, read: DirectoryRead) {
    if read.background {
        request["includeBackground"] = json!(true);
    }
}

/// First page of workspaces, subscribed, with a sync cursor when the daemon
/// supports directory sync (sync ignores paging and returns everything).
pub fn fetch_workspaces(read: DirectoryRead, cursor: Option<&Cursor>) -> Value {
    let mut request = json!({
        "type": "fetch_workspaces_request",
        "sort": [{ "key": "activity_at", "direction": "desc" }],
        "page": { "limit": PAGE_LIMIT },
        "subscribe": {},
    });
    if read.sync {
        request["sync"] = sync_field(cursor);
    }
    include_background(&mut request, read);
    request
}

pub fn fetch_workspaces_page(read: DirectoryRead, cursor: &str) -> Value {
    let mut request = json!({
        "type": "fetch_workspaces_request",
        "sort": [{ "key": "activity_at", "direction": "desc" }],
        "page": { "limit": PAGE_LIMIT, "cursor": cursor },
    });
    include_background(&mut request, read);
    request
}

pub fn fetch_agents(read: DirectoryRead, cursor: Option<&Cursor>) -> Value {
    let mut request = json!({
        "type": "fetch_agents_request",
        "scope": "active",
        "sort": [{ "key": "updated_at", "direction": "desc" }],
        "page": { "limit": PAGE_LIMIT },
        "subscribe": {},
    });
    if read.sync {
        request["sync"] = sync_field(cursor);
    }
    include_background(&mut request, read);
    request
}

pub fn fetch_agents_page(read: DirectoryRead, cursor: &str) -> Value {
    let mut request = json!({
        "type": "fetch_agents_request",
        "scope": "active",
        "sort": [{ "key": "updated_at", "direction": "desc" }],
        "page": { "limit": PAGE_LIMIT, "cursor": cursor },
    });
    include_background(&mut request, read);
    request
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

/// Deletes an agent for good: unlike archiving, it leaves no history.
pub fn delete_agent(agent_id: &str) -> Value {
    json!({ "type": "delete_agent_request", "agentId": agent_id })
}

pub fn providers_snapshot() -> Value {
    json!({ "type": "get_providers_snapshot_request" })
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

/// Creates an agent in a workspace, or, with no workspace, in `config`'s
/// `cwd`, where the daemon finds or makes the workspace for that directory.
pub fn create_agent(
    workspace_id: Option<&str>,
    config: Value,
    prompt: Option<crate::prompt::Prompt>,
    message_id: &str,
) -> Value {
    let mut message = json!({"type":"agent.create.request", "config":config, "labels":{}});
    if let Some(workspace_id) = workspace_id {
        message["workspaceId"] = json!(workspace_id);
    }
    if let Some(prompt) = prompt.filter(|p| !p.is_empty()) {
        message["initialPrompt"] = json!(prompt.text);
        add_images(&mut message, &prompt);
        message["clientMessageId"] = json!(message_id);
    }
    message
}

/// Folders on the host matching `query`: a name searched for under the
/// daemon's home, or a path (`~/code/ap`, `/srv/ap`) completed. Paseo's
/// Add Project asks the same, without a `cwd`.
pub fn directory_suggestions(query: &str, limit: u32) -> Value {
    json!({
        "type": "directory_suggestions_request",
        "query": query,
        "includeDirectories": true,
        "includeFiles": false,
        "limit": limit,
    })
}

/// Registers a folder as a project, without making a workspace in it.
pub fn add_project(cwd: &str) -> Value {
    json!({ "type": "project.add.request", "cwd": cwd })
}

/// Removes a project from the host's list. The host archives its open
/// workspaces first; nothing on disk changes ([`crate::project_remove`]).
pub fn remove_project(project_id: &str) -> Value {
    json!({ "type": "project.remove.request", "projectId": project_id })
}

/// A repository's branches, those matching `query` when it is not empty.
pub fn branch_suggestions(cwd: &str, query: &str, limit: u32) -> Value {
    let mut message = json!({ "type": "branch_suggestions_request", "cwd": cwd, "limit": limit });
    if !query.trim().is_empty() {
        message["query"] = json!(query.trim());
    }
    message
}

/// Open pull and merge requests of a repository's forge.
pub fn forge_change_requests(cwd: &str, query: &str, limit: u32) -> Value {
    json!({
        "type": "forge.search.request",
        "cwd": cwd,
        "query": query.trim(),
        "limit": limit,
        "kinds": ["change_request"],
    })
}

fn add_images(message: &mut Value, prompt: &crate::prompt::Prompt) {
    if !prompt.images.is_empty() {
        message["images"] = json!(prompt.images);
    }
}

/// Entries [`last_reply`] asks for; a reply is usually among the last few.
pub const LAST_REPLY_TAIL: u32 = 12;

/// The end of an agent's timeline, short, for the first line of its latest
/// reply ([`crate::preview::newest_reply`]). Only a read: it subscribes to
/// nothing and leaves "ready to review" as it is, unlike opening the agent.
pub fn last_reply(agent_id: &str) -> Value {
    json!({
        "type": "fetch_agent_timeline_request",
        "agentId": agent_id,
        "direction": "tail",
        "limit": LAST_REPLY_TAIL,
        "projection": "projected",
    })
}

pub fn prompt_index(agent_id: &str) -> Value {
    json!({"type":"agent.timeline.list_prompts.request", "agentId":agent_id})
}

/// Reads one file, as Paseo's app reads an image a reply shows. Without
/// `acceptBinary` the content comes back base64 in the JSON reply; a file
/// over `max_bytes` is refused rather than sent.
pub fn read_file(cwd: &str, path: &str, max_bytes: u64) -> Value {
    json!({
        "type": "file_explorer_request",
        "cwd": cwd,
        "path": path,
        "mode": "file",
        "maxBytes": max_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_is_asked_for_only_where_advertised() {
        let cursor = Cursor {
            generation: "g:background".into(),
            after_seq: 4,
        };
        let old = DirectoryRead {
            sync: true,
            background: false,
        };
        let new = DirectoryRead {
            sync: true,
            background: true,
        };
        for request in [
            fetch_workspaces(old, None),
            fetch_workspaces_page(old, "c"),
            fetch_agents(old, Some(&cursor)),
            fetch_agents_page(old, "c"),
        ] {
            assert!(request.get("includeBackground").is_none(), "{request}");
        }
        for request in [
            fetch_workspaces(new, None),
            fetch_workspaces_page(new, "c"),
            fetch_agents(new, Some(&cursor)),
            fetch_agents_page(new, "c"),
        ] {
            // Top level, never in `filter`: Paseo refuses a filtered sync read.
            assert_eq!(request["includeBackground"], true, "{request}");
            assert!(request.get("filter").is_none());
        }
        let agents = fetch_agents(new, Some(&cursor));
        assert_eq!(agents["sync"]["generation"], "g:background");
        assert_eq!(agents["scope"], "active");
        assert!(fetch_workspaces(new, None)["subscribe"].is_object());
    }

    #[test]
    fn project_and_ref_requests_have_paseos_shapes() {
        let search = directory_suggestions("~/code/ap", 30);
        assert_eq!(search["type"], "directory_suggestions_request");
        assert_eq!(search["includeFiles"], false);
        // Without a cwd the daemon searches its home, as for Add Project.
        assert!(search.get("cwd").is_none());
        assert_eq!(add_project("/srv/app")["cwd"], "/srv/app");
        // Paseo's `ProjectRemoveRequestSchema`: the id and nothing else.
        assert_eq!(
            remove_project("prj_1"),
            json!({"type": "project.remove.request", "projectId": "prj_1"})
        );
        let branches = branch_suggestions("/srv/app", "  ", 50);
        assert!(branches.get("query").is_none());
        assert_eq!(branch_suggestions("/srv/app", "fe", 50)["query"], "fe");
        let prs = forge_change_requests("/srv/app", "", 20);
        assert_eq!(prs["type"], "forge.search.request");
        assert_eq!(prs["kinds"], json!(["change_request"]));
    }

    #[test]
    fn directory_read_follows_server_features() {
        let mut host = crate::Host::default();
        assert_eq!(DirectoryRead::for_host(&host), DirectoryRead::default());
        host.server = paseo_client::ServerInfo::from_status(&json!({"type": "status",
            "payload": {"status": "server_info", "version": "0.12.0",
                        "features": {"directorySync": true, "backgroundWorkspaces": true}}}));
        assert_eq!(
            DirectoryRead::for_host(&host),
            DirectoryRead {
                sync: true,
                background: true
            }
        );
    }
}
