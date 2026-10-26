//! Frame-level envelopes in both directions.

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::hello::Hello;

/// A parsed inbound text frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    Hello(Box<Hello>),
    /// The bare liveness ping. Its first use claims the socket lease.
    Ping,
    /// Voice only; carries `isRecording`.
    RecordingState(Value),
    /// The inner `message` of a session frame, still unvalidated.
    Session(Value),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundError {
    /// Not JSON, or not an object with a string `type`.
    Malformed(String),
    /// A JSON object whose `type` names no frame kind.
    UnknownFrame(String),
    /// A `hello` that does not validate.
    InvalidHello(String),
}

impl std::fmt::Display for InboundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InboundError::Malformed(why) => f.write_str(why),
            InboundError::UnknownFrame(kind) => write!(f, "Unknown frame type '{kind}'"),
            InboundError::InvalidHello(why) => write!(f, "Invalid hello: {why}"),
        }
    }
}

impl std::error::Error for InboundError {}

/// Parses one inbound text frame.
pub fn parse_inbound(text: &str) -> Result<Inbound, InboundError> {
    let value: Value =
        serde_json::from_str(text).map_err(|err| InboundError::Malformed(err.to_string()))?;
    let Value::Object(mut frame) = value else {
        return Err(InboundError::Malformed("Expected a JSON object".into()));
    };
    let kind = match frame.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        _ => return Err(InboundError::Malformed("Missing frame type".into())),
    };
    match kind.as_str() {
        "hello" => Hello::from_frame(Value::Object(frame))
            .map(|hello| Inbound::Hello(Box::new(hello)))
            .map_err(InboundError::InvalidHello),
        "ping" => Ok(Inbound::Ping),
        "recording_state" => Ok(Inbound::RecordingState(Value::Object(frame))),
        "session" => match frame.remove("message") {
            Some(message @ Value::Object(_)) => Ok(Inbound::Session(message)),
            _ => Err(InboundError::Malformed(
                "Session frame without a message object".into(),
            )),
        },
        _ => Err(InboundError::UnknownFrame(kind)),
    }
}

/// `{"type":"session","message":{"type":kind,"payload":payload}}`.
pub fn session(kind: &str, payload: impl Serialize) -> Value {
    json!({
        "type": "session",
        "message": { "type": kind, "payload": payload },
    })
}

/// The bare reply to a bare `ping`.
pub fn pong() -> Value {
    json!({ "type": "pong" })
}

/// Why a hello was refused (`hello.rejected.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    PasswordRequired,
    IncorrectPassword,
    IncompatibleProtocol,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::PasswordRequired => "password_required",
            RejectReason::IncorrectPassword => "incorrect_password",
            RejectReason::IncompatibleProtocol => "incompatible_protocol",
        }
    }
}

/// The bare frame sent before closing a refused socket, when the hello asked
/// for it (`capabilities.hello_rejection` or an `auth` block).
pub fn hello_rejected(reason: RejectReason) -> Value {
    json!({ "type": "hello.rejected", "reason": reason.as_str(), "accepts": ["password"] })
}

/// A correlated failure: `rpc_error`.
pub fn rpc_error(request_id: &str, request_type: Option<&str>, error: &str, code: &str) -> Value {
    let mut payload = Map::new();
    payload.insert("requestId".into(), request_id.into());
    if let Some(request_type) = request_type {
        payload.insert("requestType".into(), request_type.into());
    }
    payload.insert("error".into(), error.into());
    payload.insert("code".into(), code.into());
    session("rpc_error", Value::Object(payload))
}

/// An uncorrelated failure: `status{status:"error"}`. It never closes the socket.
pub fn status_error(message: &str) -> Value {
    session("status", json!({ "status": "error", "message": message }))
}

/// The `requestId` of an inbound session message, if it has a string one.
pub fn request_id(message: &Value) -> Option<&str> {
    message.get("requestId").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_frame_kind() {
        assert_eq!(parse_inbound(r#"{"type":"ping"}"#), Ok(Inbound::Ping));
        let Ok(Inbound::Session(message)) = parse_inbound(
            r#"{"type":"session","message":{"type":"fetch_agents_request","requestId":"r1"}}"#,
        ) else {
            panic!("session frame")
        };
        assert_eq!(request_id(&message), Some("r1"));
        assert!(matches!(
            parse_inbound("this is not json"),
            Err(InboundError::Malformed(_))
        ));
        assert!(matches!(
            parse_inbound(r#"{"type":"nope"}"#),
            Err(InboundError::UnknownFrame(_))
        ));
    }

    #[test]
    fn outbound_session_messages_nest_the_payload() {
        let frame = session("pong", json!({"requestId": "r1"}));
        assert_eq!(frame["type"], "session");
        assert_eq!(frame["message"]["type"], "pong");
        assert_eq!(frame["message"]["payload"]["requestId"], "r1");
        let error = rpc_error("r2", Some("x"), "nope", "unknown_schema");
        assert_eq!(error["message"]["payload"]["code"], "unknown_schema");
    }
}
