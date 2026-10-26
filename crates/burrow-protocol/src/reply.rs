//! How a request's reply type is named.

/// The `type` of the message that answers `request` (Paseo's `replies.ts`).
pub fn reply_type(request: &str) -> String {
    match request {
        "ping" => return "pong".into(),
        "archive_agent_request" => return "agent_archived".into(),
        "delete_agent_request" => return "agent_deleted".into(),
        "agent_permission_response" => return "agent_permission_resolved".into(),
        "subscribe_terminals_request" => return "terminals_changed".into(),
        "daemon.update.request" => return "daemon.update.progress".into(),
        _ => {}
    }
    if let Some(stem) = request.strip_suffix("_request") {
        format!("{stem}_response")
    } else if let Some(stem) = request.strip_suffix(".request") {
        format!("{stem}.response")
    } else if request.contains('/') {
        format!("{request}/response")
    } else {
        format!("{request}_response")
    }
}

/// Requests whose reply is a `status` message rather than a named response.
pub fn status_reply(request: &str) -> Option<&'static [&'static str]> {
    Some(match request {
        "create_agent_request" => &["agent_created", "agent_create_failed"],
        "resume_agent_request" | "import_agent_request" => {
            &["agent_resumed", "agent_create_failed"]
        }
        "refresh_agent_request" => &["agent_refreshed"],
        "restart_server_request" => &["restart_requested"],
        "shutdown_server_request" => &["shutdown_requested"],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_paseo() {
        assert_eq!(reply_type("fetch_agents_request"), "fetch_agents_response");
        assert_eq!(
            reply_type("subscription.release.request"),
            "subscription.release.response"
        );
        assert_eq!(reply_type("a/b"), "a/b/response");
        assert_eq!(
            reply_type("clear_agent_attention"),
            "clear_agent_attention_response"
        );
        assert_eq!(reply_type("ping"), "pong");
        assert_eq!(reply_type("archive_agent_request"), "agent_archived");
        assert!(status_reply("refresh_agent_request").is_some());
        assert!(status_reply("fetch_agents_request").is_none());
    }
}
