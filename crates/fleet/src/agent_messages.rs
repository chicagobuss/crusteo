//! Messages one agent sends another, and the notices an agent gets about
//! agents it started, with who sent them.
//!
//! Paseo 0.12 wraps such a prompt in a tagged envelope on its way to the
//! provider and shows it as a synthetic `agent_message` tool call carrying
//! `agentMessage: {event, sender: {id, title?}, text}`
//! (server/agent/agent-messages, protocol/agent-message.ts). Older daemons
//! send no sender: a message arrives as a plain prompt, and 0.10/0.11 hide
//! their notices. A history replayed from the provider keeps the envelopes,
//! so they are read back here as 0.12 would show them.

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEvent {
    Message,
    Finished,
    Errored,
    PermissionRequired,
    Closed,
}

impl AgentEvent {
    pub fn parse(event: &str) -> Option<Self> {
        Some(match event {
            "message" => Self::Message,
            "finished" => Self::Finished,
            "errored" => Self::Errored,
            "permission-required" => Self::PermissionRequired,
            "closed" => Self::Closed,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Finished => "finished",
            Self::Errored => "errored",
            Self::PermissionRequired => "permission-required",
            Self::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentMessage {
    pub event: AgentEvent,
    pub sender_id: String,
    /// The sender's title when it was sent; the fleet may know a newer one.
    pub sender_title: Option<String>,
    pub text: String,
}

impl AgentMessage {
    /// A tool call's `agentMessage`.
    pub fn from_wire(value: &Value) -> Option<Self> {
        let sender = &value["sender"];
        Some(Self {
            event: AgentEvent::parse(value["event"].as_str()?)?,
            sender_id: sender["id"]
                .as_str()
                .filter(|id| !id.is_empty())?
                .to_string(),
            sender_title: sender["title"]
                .as_str()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_string),
            text: value["text"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// A notice about an agent rather than a message from one.
    pub fn is_notice(&self) -> bool {
        self.event != AgentEvent::Message
    }

    /// What the sender last said, from a notice's `<agent-response>`.
    pub fn response(&self) -> Option<&str> {
        let start = self.text.find("<agent-response>\n")? + "<agent-response>\n".len();
        let rest = &self.text[start..];
        let body = rest.split("\n</agent-response>").next().unwrap_or(rest);
        Some(body.trim()).filter(|body| !body.is_empty())
    }
}

/// What a user message holding an envelope stands for: the tool call 0.12
/// shows for an agent's message or notice, or the plain prompt a human
/// sent, unwrapped. None for any other prompt.
pub fn project_user_message(item: &Value) -> Option<Value> {
    let text = item["text"].as_str()?;
    if let Some(envelope) = parse_envelope(text) {
        let Some(message) = envelope.message else {
            let mut item = item.clone();
            item["text"] = json!(envelope.text);
            item["clientMessageId"] = json!(envelope.id);
            return Some(item);
        };
        return Some(tool_item(&envelope.id, &message));
    }
    // Before 0.11.1 a notice was untagged, and the daemon hid it.
    let message = parse_legacy_notice(text)?;
    let id = item["messageId"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("{:016x}", content_hash(text)));
    Some(tool_item(&id, &message))
}

/// The tool call Paseo 0.12 projects a message as.
fn tool_item(id: &str, message: &AgentMessage) -> Value {
    let mut sender = json!({ "id": message.sender_id });
    if let Some(title) = &message.sender_title {
        sender["title"] = json!(title);
    }
    json!({
        "type": "tool_call",
        "callId": format!("paseo-agent-message:{id}"),
        "name": "agent_message",
        "status": "completed",
        "error": null,
        "detail": { "type": "plain_text", "text": message.text, "icon": "bot" },
        "agentMessage": { "event": message.event.as_str(), "sender": sender, "text": message.text },
    })
}

struct Envelope {
    id: String,
    /// None for a human's prompt that merely looked like an envelope.
    message: Option<AgentMessage>,
    text: String,
}

/// `<paseo-system version="1" kind=… message-id=…>\n{body}\n</paseo-system>`,
/// as strictly as Paseo's own `parseAgentMessage` reads it.
fn parse_envelope(text: &str) -> Option<Envelope> {
    let rest = text.strip_prefix("<paseo-system")?;
    let (head, rest) = rest.split_once(">\n")?;
    let body = rest.strip_suffix("\n</paseo-system>")?;
    let mut attributes: Vec<(&str, String)> = Vec::new();
    let mut head = head;
    while !head.is_empty() {
        let trimmed = head.trim_start();
        if trimmed.len() == head.len() {
            return None;
        }
        let (name, after) = trimmed.split_once("=\"")?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
            return None;
        }
        let (value, after) = after.split_once('"')?;
        if attributes.iter().any(|(seen, _)| *seen == name) {
            return None;
        }
        attributes.push((name, unescape(value)?));
        head = after;
    }
    let get = |key: &str| {
        attributes
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    };
    if get("version") != Some("1") {
        return None;
    }
    let id = get("message-id").filter(|id| !id.is_empty())?.to_string();
    let text = unescape(body)?;
    let sender_id = get("source-agent-id").filter(|id| !id.is_empty());
    let event = match get("kind")? {
        "user-message" if sender_id.is_none() && get("event").is_none() => {
            return Some(Envelope {
                id,
                message: None,
                text,
            });
        }
        "agent-message" => AgentEvent::Message,
        "agent-notification" => {
            AgentEvent::parse(get("event")?).filter(|e| *e != AgentEvent::Message)?
        }
        _ => return None,
    };
    let message = AgentMessage {
        event,
        sender_id: sender_id?.to_string(),
        sender_title: get("source-agent-title")
            .filter(|title| !title.is_empty())
            .map(str::to_string),
        text: text.clone(),
    };
    Some(Envelope {
        id,
        message: Some(message),
        text,
    })
}

/// XML entities back to text; None for a raw `<`, `>` or a stray `&`,
/// which an envelope never holds.
fn unescape(value: &str) -> Option<String> {
    if value.contains(['<', '>']) {
        return None;
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let (entity, len) = [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&apos;", '\''),
        ]
        .into_iter()
        .find_map(|(name, ch)| rest.starts_with(name).then_some((ch, name.len())))?;
        out.push(entity);
        rest = &rest[len..];
    }
    out.push_str(rest);
    Some(out)
}

/// `<paseo-system>\nAgent {id} ({title}) {reason}.\n…\n</paseo-system>`: the
/// untagged notice of 0.10 and 0.11.0. Other untagged envelopes, such as a
/// schedule firing, are not an agent's.
fn parse_legacy_notice(text: &str) -> Option<AgentMessage> {
    let body = text
        .strip_prefix("<paseo-system>\n")?
        .strip_suffix("\n</paseo-system>")?;
    let line = body.lines().next()?.strip_prefix("Agent ")?;
    let (id, rest) = line.split_once(" (")?;
    let (event, title) = [
        (") finished.", AgentEvent::Finished),
        (") errored.", AgentEvent::Errored),
        (") needs permission.", AgentEvent::PermissionRequired),
        (") was closed.", AgentEvent::Closed),
        (") was canceled.", AgentEvent::Errored),
    ]
    .into_iter()
    .find_map(|(suffix, event)| rest.strip_suffix(suffix).map(|title| (event, title)))?;
    if id.is_empty() || id.contains(char::is_whitespace) {
        return None;
    }
    Some(AgentMessage {
        event,
        sender_id: id.to_string(),
        sender_title: Some(title.trim().to_string()).filter(|t| !t.is_empty()),
        text: body.to_string(),
    })
}

fn content_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_wire_sender() {
        let message = AgentMessage::from_wire(&json!({
            "event": "finished", "sender": {"id": "agt_1", "title": "  Check totals "},
            "text": "Agent agt_1 (Check totals) finished.\n\n<agent-response>\nAll match.\nTwo fixes.\n</agent-response>"
        }))
        .unwrap();
        assert_eq!(message.event, AgentEvent::Finished);
        assert_eq!(message.sender_title.as_deref(), Some("Check totals"));
        assert!(message.is_notice());
        assert_eq!(message.response(), Some("All match.\nTwo fixes."));
        assert!(
            AgentMessage::from_wire(&json!({"event": "message", "sender": {}, "text": ""}))
                .is_none()
        );
        assert!(
            AgentMessage::from_wire(&json!({"event": "shouted", "sender": {"id": "a"}})).is_none()
        );
    }

    #[test]
    fn envelopes_become_what_paseo_shows() {
        let tagged = json!({"type": "user_message", "messageId": "u1", "text":
            "<paseo-system version=\"1\" kind=\"agent-message\" source-agent-id=\"agt_9\" message-id=\"m1\" source-agent-title=\"Plan &amp; review\">\nUse &lt;b&gt; tags\n</paseo-system>"});
        let tool = project_user_message(&tagged).unwrap();
        assert_eq!(tool["type"], "tool_call");
        assert_eq!(tool["callId"], "paseo-agent-message:m1");
        assert_eq!(
            tool["agentMessage"],
            json!({"event": "message", "sender": {"id": "agt_9", "title": "Plan & review"}, "text": "Use <b> tags"})
        );

        let notice = json!({"type": "user_message", "text":
            "<paseo-system version=\"1\" kind=\"agent-notification\" source-agent-id=\"agt_9\" message-id=\"m2\" event=\"permission-required\">\nAgent agt_9 (x) needs permission.\n</paseo-system>"});
        assert_eq!(
            project_user_message(&notice).unwrap()["agentMessage"]["event"],
            "permission-required"
        );

        // A human's prompt that was quoted on its way in comes back as typed.
        let human = json!({"type": "user_message", "text":
            "<paseo-system version=\"1\" kind=\"user-message\" message-id=\"c1\">\n&lt;paseo-system&gt; is a tag\n</paseo-system>"});
        let user = project_user_message(&human).unwrap();
        assert_eq!(user["type"], "user_message");
        assert_eq!(user["text"], "<paseo-system> is a tag");
        assert_eq!(user["clientMessageId"], "c1");
    }

    #[test]
    fn anything_else_stays_a_prompt() {
        for text in [
            "Review the parser change",
            // A raw `<` cannot be in an envelope's body.
            "<paseo-system version=\"1\" kind=\"agent-message\" source-agent-id=\"a\" message-id=\"m\">\n1 < 2\n</paseo-system>",
            "<paseo-system version=\"2\" kind=\"agent-message\" source-agent-id=\"a\" message-id=\"m\">\nhi\n</paseo-system>",
            "<paseo-system version=\"1\" kind=\"agent-message\" message-id=\"m\">\nno sender\n</paseo-system>",
            "<paseo-system version=\"1\" version=\"1\" kind=\"user-message\" message-id=\"m\">\ntwice\n</paseo-system>",
            // A schedule firing is no agent's notice.
            "<paseo-system>\nScheduled run \"nightly\" fired.\n</paseo-system>",
        ] {
            assert_eq!(
                project_user_message(&json!({"type": "user_message", "text": text})),
                None,
                "{text}"
            );
        }
    }

    #[test]
    fn untagged_notices_of_older_daemons_are_read() {
        let item = json!({"type": "user_message", "messageId": "u7", "text":
            "<paseo-system>\nAgent agt_3 (Fix (flaky) tests) was canceled.\n</paseo-system>"});
        let tool = project_user_message(&item).unwrap();
        assert_eq!(tool["callId"], "paseo-agent-message:u7");
        assert_eq!(
            tool["agentMessage"]["sender"],
            json!({"id": "agt_3", "title": "Fix (flaky) tests"})
        );
        assert_eq!(tool["agentMessage"]["event"], "errored");
    }
}
