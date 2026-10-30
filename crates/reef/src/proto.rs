//! The Skiff protocol, v0, Reef's downstream: what phones send Reef, read leniently,
//! and the limits that keep what Reef sends phone-sized. The full spec
//! is `docs/reef/design.md`.

use std::collections::BTreeMap;
use std::time::Duration;

use fleet::Bucket;
use serde_json::{Value, json};

/// The protocol version this Reef speaks. A phone says which it speaks in
/// `hello`; additions that old phones can ignore do not bump it.
pub const VERSION: u64 = 0;

/// Budgets. Nothing Reef sends unasked is bigger than one screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    /// Deltas are gathered for this long and sent together.
    pub flush: Duration,
    /// Frames queued for one phone before it counts as too slow: its views
    /// then get one fresh snapshot each when it catches up, not every delta.
    pub outbox: usize,
    pub max_subscriptions: usize,
    /// Rows in a list view when the phone does not say.
    pub list_default: usize,
    pub list_max: usize,
    /// Turns a conversation opens with.
    pub turns_default: usize,
    /// Items a conversation sends at once, at most.
    pub items_max: usize,
    /// Bytes of one message's text; longer text is cut and marked.
    pub text_max: usize,
    /// Bytes of one section of a tool call's detail.
    pub detail_max: usize,
    /// A phone must say hello within this.
    pub hello_timeout: Duration,
    /// A phone silent this long is dropped; phones ping well inside it.
    pub idle_timeout: Duration,
    /// A conversation no phone shows keeps streaming this long, so
    /// opening it again is instant.
    pub session_grace: Duration,
    /// How often each daemon is asked for its usage reports, and the usage
    /// feed read: as often as crusteo asks them.
    pub usage_ask: Duration,
    pub usage_feed: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            flush: Duration::from_millis(150),
            outbox: 256,
            max_subscriptions: 8,
            list_default: 30,
            list_max: 100,
            turns_default: 3,
            items_max: 60,
            text_max: 16 * 1024,
            detail_max: 64 * 1024,
            hello_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(90),
            session_grace: Duration::from_secs(300),
            usage_ask: usage_source::reports::ASK_EVERY,
            usage_feed: crate::usage::FEED_EVERY,
        }
    }
}

/// Error codes in `error` and failed `result` messages.
pub mod code {
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const VERSION: &str = "version";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const UNKNOWN_VIEW: &str = "unknown_view";
    pub const NOT_FOUND: &str = "not_found";
    pub const TOO_MANY: &str = "too_many_subscriptions";
    pub const NOT_YET: &str = "not_yet";
    pub const UPSTREAM: &str = "upstream";
}

/// Which agents a list shows. Empty fields do not filter.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentFilter {
    pub hosts: Vec<String>,
    /// Workspace labels, matched as crusteo matches them.
    pub labels: Vec<String>,
    pub states: Vec<Bucket>,
    /// Words that must all appear in the title, workspace or project.
    pub query: String,
    /// Show subagents too; off by default.
    pub subagents: bool,
    /// Show the agents of Paseo's background workspaces; `None` follows
    /// Reef's `show_background` setting.
    pub background: Option<bool>,
}

/// How the agent list is grouped: into sections on a phone, columns on a
/// wide screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    #[default]
    None,
    Host,
    Label,
    /// One group per agent, its labels together: "Billing + infra".
    Combination,
    State,
    Workspace,
}

impl Grouping {
    pub fn parse(text: &str) -> Self {
        match text {
            "host" => Grouping::Host,
            "label" => Grouping::Label,
            "combination" => Grouping::Combination,
            "state" => Grouping::State,
            "workspace" => Grouping::Workspace,
            _ => Grouping::None,
        }
    }
}

/// The agent list's order, within each group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sorting {
    /// The agent that did something last first.
    #[default]
    Recent,
    /// Needs input, then failed, running, done; newest first in each.
    Needs,
    Name,
}

impl Sorting {
    pub fn parse(text: &str) -> Self {
        match text {
            "needs" => Sorting::Needs,
            "name" => Sorting::Name,
            _ => Sorting::Recent,
        }
    }
}

/// Everything the agent list is asked to be.
#[derive(Debug, Clone, PartialEq)]
pub struct ListSpec {
    pub filter: AgentFilter,
    pub group: Grouping,
    pub sort: Sorting,
    /// Ungrouped: where the window starts.
    pub offset: usize,
    /// Rows in the window; per group when grouped.
    pub limit: usize,
    /// Groups the phone has folded: counted, but no rows sent.
    pub collapsed: Vec<String>,
    /// Groups shown longer than `limit`, by group key.
    pub more: BTreeMap<String, usize>,
}

impl Default for ListSpec {
    fn default() -> Self {
        Self {
            filter: AgentFilter::default(),
            group: Grouping::None,
            sort: Sorting::Recent,
            offset: 0,
            limit: 30,
            collapsed: Vec::new(),
            more: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ViewSpec {
    NeedsYou {
        limit: usize,
    },
    Agents {
        spec: ListSpec,
    },
    /// Every agent as a small tile, by lane, and full cards only for the
    /// agents in ocus: what a zoomable map shows.
    Map {
        spec: ListSpec,
        focus: Vec<String>,
    },
    Conversation {
        host: String,
        agent: String,
        turns: usize,
    },
    /// Each account's usage limits.
    Usage,
}

/// The actions about one agent a phone can ask for. The others concern the
/// Reef (`settings`, `host_add`, `host_remove`) or a host (`create`).
pub const ACTIONS: &[&str] = &[
    "permission",
    "answer",
    "stop",
    "send",
    "steer",
    "unqueue",
    "archive",
    "delete",
    "relabel",
    "notify",
];

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMsg {
    Hello {
        version: u64,
        token: String,
        device: Option<String>,
    },
    Subscribe {
        id: String,
        view: ViewSpec,
    },
    Unsubscribe {
        id: String,
    },
    /// Move or grow a list's window; answered with a fresh snapshot.
    Window {
        id: String,
        offset: usize,
        limit: usize,
    },
    /// The agents whose cards a map shows up close, as host/agent.
    Focus {
        id: String,
        agents: Vec<String>,
    },
    /// The phone lost track of a view; answered with a fresh snapshot.
    Resync {
        id: String,
    },
    /// Show `turns` more turns before what a conversation shows.
    Older {
        req: Option<String>,
        id: String,
        turns: usize,
    },
    /// The full text of one tool call or thought.
    Detail {
        req: Option<String>,
        id: String,
        item: String,
    },
    Action {
        req: Option<String>,
        action: String,
        body: Value,
    },
    /// What a new agent on `host` can be: its providers and workspaces.
    CreateOptions {
        req: Option<String>,
        host: String,
    },
    Ping {
        t: Value,
    },
    /// A frame that did not parse; answered as a bad request.
    Invalid {
        req: Option<String>,
        problem: String,
    },
}

/// Reads a frame, turning what does not parse into [`ClientMsg::Invalid`].
pub fn parse_or_invalid(frame: &str, limits: &Limits) -> ClientMsg {
    parse(frame, limits).unwrap_or_else(|problem| ClientMsg::Invalid {
        req: serde_json::from_str::<Value>(frame)
            .ok()
            .and_then(|v| v["req"].as_str().map(str::to_string)),
        problem,
    })
}

fn text(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_string)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn count(value: &Value, default: usize, max: usize) -> usize {
    value
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or(default)
        .clamp(1, max.max(1))
}

/// Reads one frame from a phone. `Err` carries what was wrong with it.
pub fn parse(frame: &str, limits: &Limits) -> Result<ClientMsg, String> {
    let value: Value = serde_json::from_str(frame).map_err(|err| format!("not JSON: {err}"))?;
    let req = text(&value, "req");
    let id = || {
        text(&value, "id")
            .filter(|id| !id.is_empty())
            .ok_or("no id")
    };
    let message = match value["type"].as_str() {
        Some("hello") => ClientMsg::Hello {
            version: value["v"].as_u64().unwrap_or(0),
            token: text(&value, "token").unwrap_or_default(),
            device: text(&value, "device"),
        },
        Some("subscribe") => ClientMsg::Subscribe {
            id: id()?,
            view: parse_view(&value, limits)?,
        },
        Some("unsubscribe") => ClientMsg::Unsubscribe { id: id()? },
        Some("window") => ClientMsg::Window {
            id: id()?,
            offset: value["offset"].as_u64().unwrap_or(0) as usize,
            limit: count(&value["limit"], limits.list_default, limits.list_max),
        },
        Some("resync") => ClientMsg::Resync { id: id()? },
        Some("focus") => ClientMsg::Focus {
            id: id()?,
            agents: focus(&value["agents"]),
        },
        Some("older") => ClientMsg::Older {
            req,
            id: id()?,
            turns: count(&value["turns"], limits.turns_default, 50),
        },
        Some("detail") => ClientMsg::Detail {
            req,
            id: id()?,
            item: text(&value, "item").ok_or("no item")?,
        },
        Some("action") => ClientMsg::Action {
            req,
            action: text(&value, "action").unwrap_or_default(),
            body: value,
        },
        Some("create_options") => ClientMsg::CreateOptions {
            req,
            host: text(&value, "host").ok_or("no host")?,
        },
        Some("ping") => ClientMsg::Ping {
            t: value["t"].clone(),
        },
        Some(other) => return Err(format!("unknown message type {other}")),
        None => return Err("no type".into()),
    };
    Ok(message)
}

/// Cards a map shows up close, at most; a phone screen holds far fewer.
pub const FOCUS_MAX: usize = 80;

fn focus(value: &Value) -> Vec<String> {
    let mut agents = strings(value);
    agents.truncate(FOCUS_MAX);
    agents
}

fn list_spec(value: &Value, limits: &Limits) -> ListSpec {
    let filter = &value["filter"];
    let more = value["more"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(group, n)| {
            let n = n.as_u64()? as usize;
            Some((group.clone(), n.clamp(1, limits.list_max)))
        })
        .collect();
    ListSpec {
        filter: AgentFilter {
            hosts: strings(&filter["hosts"]),
            labels: strings(&filter["labels"]),
            states: strings(&filter["states"])
                .iter()
                .map(|s| Bucket::parse(s))
                .collect(),
            query: text(filter, "query").unwrap_or_default(),
            subagents: filter["subagents"] == true,
            background: filter["background"].as_bool(),
        },
        group: Grouping::parse(value["group"].as_str().unwrap_or_default()),
        sort: Sorting::parse(value["sort"].as_str().unwrap_or_default()),
        offset: value["offset"].as_u64().unwrap_or(0) as usize,
        limit: count(&value["limit"], limits.list_default, limits.list_max),
        collapsed: strings(&value["collapsed"]),
        more,
    }
}

fn parse_view(value: &Value, limits: &Limits) -> Result<ViewSpec, String> {
    match value["view"].as_str() {
        Some("needs_you") => Ok(ViewSpec::NeedsYou {
            limit: count(&value["limit"], limits.list_default, limits.list_max),
        }),
        Some("agents") => Ok(ViewSpec::Agents {
            spec: list_spec(value, limits),
        }),
        Some("map") => Ok(ViewSpec::Map {
            spec: list_spec(value, limits),
            focus: focus(&value["focus"]),
        }),
        Some("conversation") => Ok(ViewSpec::Conversation {
            host: text(value, "host").ok_or("no host")?,
            agent: text(value, "agent").ok_or("no agent")?,
            turns: count(&value["turns"], limits.turns_default, 50),
        }),
        Some("usage") => Ok(ViewSpec::Usage),
        Some(other) => Err(format!("unknown view {other}")),
        None => Err("no view".into()),
    }
}

pub fn error(id: Option<&str>, code: &str, message: &str) -> Value {
    let mut error = json!({ "type": "error", "code": code, "message": message });
    if let Some(id) = id {
        error["id"] = json!(id);
    }
    error
}

pub fn result_ok(req: Option<&str>, extra: Value) -> Value {
    let mut result = json!({ "type": "result", "req": req, "ok": true });
    if let (Some(target), Value::Object(fields)) = (result.as_object_mut(), extra) {
        target.extend(fields);
    }
    result
}

pub fn result_err(req: Option<&str>, code: &str, message: &str) -> Value {
    json!({ "type": "result", "req": req, "ok": false, "error": { "code": code, "message": message } })
}

/// Cuts `text` to at most `max` bytes on a character boundary.
pub fn clip(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_parse_leniently_and_counts_are_clamped() {
        let limits = Limits::default();
        let hello = parse(
            r#"{"type":"hello","v":0,"token":"t","device":"phone"}"#,
            &limits,
        );
        assert_eq!(
            hello.unwrap(),
            ClientMsg::Hello {
                version: 0,
                token: "t".into(),
                device: Some("phone".into())
            }
        );
        let list = parse(
            r#"{"type":"subscribe","id":"s","view":"agents","limit":5000,"filter":{"states":["running"],"extra":1}}"#,
            &limits,
        )
        .unwrap();
        let ClientMsg::Subscribe {
            view: ViewSpec::Agents { spec },
            ..
        } = list
        else {
            panic!("{list:?}")
        };
        assert_eq!(spec.limit, limits.list_max);
        assert_eq!(spec.filter.states, [Bucket::Running]);
        assert_eq!(spec.filter.background, None, "Reef's setting decides");
        let hidden = parse(
            r#"{"type":"subscribe","id":"s","view":"agents","filter":{"background":false}}"#,
            &limits,
        );
        let Ok(ClientMsg::Subscribe {
            view: ViewSpec::Agents { spec: chosen },
            ..
        }) = hidden
        else {
            panic!("{hidden:?}")
        };
        assert_eq!(chosen.filter.background, Some(false));
        assert_eq!(spec.sort, Sorting::Recent, "recent activity by default");
        assert_eq!(spec.group, Grouping::None);
        let grouped = parse(
            r#"{"type":"subscribe","id":"s","view":"agents","group":"label","sort":"name","collapsed":["label:"],"more":{"label:x":50}}"#,
            &limits,
        )
        .unwrap();
        let ClientMsg::Subscribe {
            view: ViewSpec::Agents { spec },
            ..
        } = grouped
        else {
            panic!()
        };
        assert_eq!((spec.group, spec.sort), (Grouping::Label, Sorting::Name));
        assert_eq!(spec.collapsed, ["label:"]);
        assert_eq!(spec.more["label:x"], 50);
        assert!(parse(r#"{"type":"subscribe","view":"agents"}"#, &limits).is_err());
        assert!(parse(r#"{"type":"subscribe","id":"s","view":"bogus"}"#, &limits).is_err());
        assert!(parse("not json", &limits).is_err());
    }

    #[test]
    fn clipping_keeps_whole_characters() {
        assert_eq!(clip("héllo", 2), ("h", true));
        assert_eq!(clip("hi", 10), ("hi", false));
    }
}
