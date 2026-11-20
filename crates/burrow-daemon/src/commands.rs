//! Burrow's agent commands (`burrow ls`, `inspect`, `send`, `rename`, `label`,
//! `stop`, `archive`): the common operations, over the same protocol Paseo's
//! CLI uses, so they work on hosts without Paseo's CLI and against any Burrow
//! with `--host`. Each returns what it prints; [`Failed`] carries the message
//! and exit code of one that did not do what was asked.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::client::{Client, WAIT};

/// A command that did not do what was asked: what to print, and the exit code.
#[derive(Debug, PartialEq, Eq)]
pub struct Failed {
    pub message: String,
    pub code: i32,
}

impl From<String> for Failed {
    fn from(message: String) -> Self {
        Failed { message, code: 1 }
    }
}

pub type Outcome = Result<String, Failed>;

/// Every agent, archived and background ones too, most recently updated first.
pub async fn agents(client: &mut Client) -> Result<Vec<Value>, String> {
    let reply = client
        .request(
            json!({"type": "fetch_agents_request",
                   "filter": {"includeArchived": true, "includeBackground": true},
                   "sort": [{"key": "updated_at", "direction": "desc"}]}),
            Some(WAIT),
        )
        .await?;
    Ok((reply["entries"].as_array().into_iter().flatten())
        .map(|entry| entry["agent"].clone())
        .filter(|agent| agent["id"].is_string())
        .collect())
}

/// `burrow ls --fleet`: every connected host's agents, asked of this
/// Burrow's Reef, each with its `host`; and the hosts that did not answer,
/// with why.
pub async fn fleet(
    client: &mut Client,
    all: bool,
    filter: &LabelFilter,
) -> Result<(Vec<Value>, Vec<String>), String> {
    // Full snapshots, whatever the host's default: the CLI's columns and
    // --json read them, and --brief compacts them here.
    let mut args = json!({"includeArchived": all, "includeBackground": all, "limit": 200,
                          "sinceHours": 720, "verbose": true});
    if !filter.workspace.is_empty() {
        args["workspaceLabels"] = json!(filter.workspace);
    }
    let reply = client
        .request(
            json!({"type": burrow_protocol::routing::FLEET_AGENTS_REQUEST, "args": args}),
            Some(Duration::from_secs(40)),
        )
        .await?;
    if reply["ok"] != true {
        return Err(reply["error"]
            .as_str()
            .unwrap_or("this Burrow could not list the fleet's agents")
            .to_string());
    }
    let result = &reply["result"];
    let agents: Vec<Value> = (result["agents"].as_array().into_iter().flatten())
        .filter(|a| filter.keeps_listed(a))
        .cloned()
        .collect();
    let missing: Vec<String> = (result["hosts"].as_array().into_iter().flatten())
        .filter(|h| h["answered"] != true)
        .map(|h| {
            format!(
                "{}: {}",
                h["host"].as_str().unwrap_or("?"),
                h["error"].as_str().unwrap_or("did not answer")
            )
        })
        .collect();
    Ok((agents, missing))
}

/// The agent `key` names, on this host first, then in the fleet through
/// this host's Reef, whichever one has it. A fleet answer is a full
/// snapshot with the agent's `host`, so its home can be read off it; when
/// the fleet cannot be asked, the error says so in the words it gives.
pub async fn find_fleet(client: &mut Client, agents: &[Value], key: &str) -> Result<Value, String> {
    match find(agents, key) {
        Ok(agent) => Ok(agent.clone()),
        Err(local) => {
            let (fleet, _missing) = match fleet(client, true, &LabelFilter::default()).await {
                Ok(listed) => listed,
                Err(fleet_error) => return Err(format!("{local}\n{fleet_error}")),
            };
            match find(&fleet, key) {
                Ok(agent) => Ok(agent.clone()),
                Err(_) => Err(local),
            }
        }
    }
}

/// One of the fleet's tools on the agent's host, through this host's Reef:
/// `burrow logs` and `wait` for an agent that is not here.
pub async fn fleet_tool(
    client: &mut Client,
    tool: &str,
    target: &str,
    args: Value,
    within: Duration,
) -> Result<Value, Failed> {
    let ask = json!({"type": burrow_protocol::routing::FLEET_TOOL_REQUEST,
                     "requestId": uuid::Uuid::new_v4().to_string(),
                     "tool": tool, "args": args, "targetAgentId": target,
                     "timeoutMs": within.as_millis().min(u64::MAX as u128) as u64});
    let reply = client
        .request(ask, Some(within + Duration::from_secs(10)))
        .await?;
    if reply["ok"] != true {
        let code = reply["code"].as_str().unwrap_or("refused").to_string();
        let error = reply["error"]
            .as_str()
            .unwrap_or("the Reef refused")
            .to_string();
        // The Reef itself cannot be reached, or is not routing: its words
        // already say what to do.
        if code == "unreachable" || code == burrow_protocol::routing::code::OFF {
            return Err(Failed {
                message: error,
                code: 1,
            });
        }
        let refused = crate::router::Refused {
            code,
            error,
            detail: reply["detail"].clone(),
        };
        return Err(Failed {
            message: crate::router::not_here(target, &crate::router::Reach::On, Some(&refused)),
            code: 1,
        });
    }
    Ok(reply["result"].clone())
}

/// The one agent `key` names: its whole id, a unique prefix of it, or its
/// name (exactly, ignoring case). Archived agents count only when nothing
/// else matches.
pub fn find<'a>(agents: &'a [Value], key: &str) -> Result<&'a Value, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("no agent named: give an id, an id prefix or a name".into());
    }
    let id = |agent: &Value| agent["id"].as_str().unwrap_or("").to_string();
    if let Some(agent) = agents.iter().find(|agent| id(agent) == key) {
        return Ok(agent);
    }
    let by_prefix: Vec<&Value> = agents.iter().filter(|a| id(a).starts_with(key)).collect();
    let by_name: Vec<&Value> = (agents.iter())
        .filter(|a| {
            a["title"]
                .as_str()
                .is_some_and(|t| t.eq_ignore_ascii_case(key))
        })
        .collect();
    for matched in [by_prefix, by_name] {
        let live: Vec<&Value> = matched
            .iter()
            .copied()
            .filter(|a| a["archivedAt"].is_null())
            .collect();
        let pick = if live.is_empty() { matched } else { live };
        match pick.as_slice() {
            [] => continue,
            [one] => return Ok(one),
            many => {
                let names: Vec<String> = (many.iter().take(8))
                    .map(|a| format!("  {}  {}", short(&id(a)), title(a)))
                    .collect();
                return Err(format!(
                    "\"{key}\" matches {} agents; give more of the id:\n{}",
                    many.len(),
                    names.join("\n")
                ));
            }
        }
    }
    Err(format!(
        "no agent matches \"{key}\" (`burrow ls -a` lists them all)"
    ))
}

/// The states `burrow ls` shows in its STATE column, in the order they are
/// worth acting on: `--state` takes them, and `--group-by` prints them so.
pub const STATES: &[&str] = &[
    "asking",
    "running",
    "initializing",
    "idle",
    "review",
    "error",
    "closed",
    "archived",
];

/// What `burrow ls --group-by` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Host,
    Label,
    State,
}

/// What `burrow ls` shows: which rows, in which columns, or counts instead.
#[derive(Debug, Default)]
pub struct ListOpts {
    /// `-a`: closed and archived agents too.
    pub all: bool,
    /// `--state`: only these states, the STATE column's words; given, they
    /// are shown, closed and archived included.
    pub states: Vec<String>,
    /// `--on`: only these hosts (a fleet listing's), any case.
    pub hosts: Vec<String>,
    /// Each workspace's label names, by id, for a listing whose agents do
    /// not carry their `workspaceLabels`.
    pub workspaces: BTreeMap<String, Vec<String>>,
    /// `--group-by`: counts only, no rows.
    pub group_by: Option<GroupBy>,
}

/// One agent as a list should show it: the few fields an agent needs to
/// find and choose it, the rest behind the full snapshots (`list_agents`
/// with `verbose: true`, `burrow ls --json`). `shortId` addresses the
/// agent with every tool and command, so the full id stays out; `paseo.*`
/// and `crusteo.*` labels are state other software keeps, not the agent's
/// labels; the host, when the list knows it (a fleet list does); the age,
/// short; null and empty fields, out.
pub fn compact_agent(agent: &Value, now: OffsetDateTime) -> Value {
    let mut item = json!({
        "shortId": (agent["id"].as_str().unwrap_or_default()).chars().take(7).collect::<String>()
    });
    if let Some(host) = agent["host"].as_str().filter(|host| !host.is_empty()) {
        item["host"] = host.into();
    }
    if let Some(title) = agent["title"]
        .as_str()
        .filter(|title| !title.trim().is_empty())
    {
        item["title"] = title.into();
    }
    if let Some(status) = agent["status"].as_str().filter(|status| !status.is_empty()) {
        item["status"] = status.into();
    }
    if let Some(attention) = agent["attentionReason"].as_str().filter(|a| !a.is_empty()) {
        item["attention"] = attention.into();
    }
    let provider = agent["provider"].as_str().unwrap_or_default();
    let model = agent["model"].as_str().filter(|m| !m.is_empty());
    if !provider.is_empty() || model.is_some() {
        item["model"] = match model {
            Some(model) => format!("{provider}/{model}"),
            None => provider.to_string(),
        }
        .into();
    }
    if let Some(workspace) = agent["workspaceLabels"]
        .as_array()
        .filter(|w| !w.is_empty())
    {
        item["workspaceLabels"] = workspace.clone().into();
    }
    let labels = (agent["labels"].as_object())
        .map(|labels| {
            (labels.iter())
                .filter(|(key, _)| !key.starts_with("paseo.") && !key.starts_with("crusteo."))
                .filter(|(_, value)| value.as_str().is_some_and(|v| !v.is_empty()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Map<String, Value>>()
        })
        .unwrap_or_default();
    if !labels.is_empty() {
        item["labels"] = Value::Object(labels);
    }
    if let Some(updated) = agent["updatedAt"].as_str() {
        item["updated"] = age(Some(updated), now).into();
    }
    item
}

/// `burrow ls`: one line per agent, the hidden count, and `--group-by`
/// instead of rows. Without `all` and without a state filter, closed and
/// archived agents are hidden: the list is for acting, and neither needs it.
/// `--on` picks hosts (only a fleet listing has them), and `--state`
/// applies to the rows and the `--group-by` counts alike.
pub fn filtered<'a>(agents: &'a [Value], opts: &ListOpts) -> Vec<&'a Value> {
    (agents.iter())
        .filter(|a| {
            (opts.hosts.is_empty()
                || (a["host"].as_str())
                    .is_some_and(|host| (opts.hosts.iter()).any(|h| h.eq_ignore_ascii_case(host))))
                && (opts.states.is_empty() || (opts.states.iter()).any(|s| s.as_str() == state(a)))
        })
        .collect()
}

/// The rows a listing shows: without `all` and without a state filter, the
/// closed and archived agents are hidden, and how many of each. The text
/// rows and the JSON must list the same agents, so they share this.
pub fn visible<'a>(agents: &'a [&'a Value], opts: &ListOpts) -> (Vec<&'a Value>, usize, usize) {
    let mut hidden_closed = 0usize;
    let mut hidden_archived = 0usize;
    let shown: Vec<&Value> = (agents.iter())
        .copied()
        .filter(|a| {
            if !opts.all && opts.states.is_empty() {
                if !a["archivedAt"].is_null() {
                    hidden_archived += 1;
                    return false;
                }
                if a["status"].as_str() == Some("closed") {
                    hidden_closed += 1;
                    return false;
                }
            }
            true
        })
        .collect();
    (shown, hidden_closed, hidden_archived)
}

pub fn list(agents: &[Value], opts: &ListOpts, now: OffsetDateTime) -> String {
    let agents = filtered(agents, opts);
    if let Some(group) = opts.group_by {
        return group_counts(&agents, group, opts);
    }
    let (shown, hidden_closed, hidden_archived) = visible(&agents, opts);
    let hidden = hidden_closed + hidden_archived;
    if shown.is_empty() {
        return if !opts.states.is_empty() {
            format!("No agents in state: {}.", opts.states.join(", "))
        } else if !opts.hosts.is_empty() {
            format!("No agents on host: {}.", opts.hosts.join(", "))
        } else if hidden > 0 {
            format!("No agents ({} hidden; -a shows them).", hidden)
        } else if opts.all {
            "No agents.".into()
        } else {
            "No agents (`burrow ls -a` adds closed and archived ones).".into()
        } + "\n";
    }
    // Agents from the fleet (`--fleet`) say their host.
    let hosts = shown.iter().any(|a| a["host"].is_string());
    let rows: Vec<Vec<String>> = (shown.iter())
        .map(|a| {
            let mut row = vec![short(a["id"].as_str().unwrap_or(""))];
            if hosts {
                row.push(clip(a["host"].as_str().unwrap_or("-"), 24));
            }
            // NAME and LABELS are cut to fit a line: one long entry in the
            // fleet must not widen every row.
            row.extend([
                state(a),
                clip(&title(a), 32),
                clip(&model(a), 32),
                clip(&labels_of(a, &opts.workspaces), 24),
                age(a["updatedAt"].as_str(), now),
                clip(&home_relative(a["cwd"].as_str().unwrap_or("")), 40),
            ]);
            row
        })
        .collect();
    let mut head = vec!["ID"];
    if hosts {
        head.push("HOST");
    }
    head.extend(["STATE", "NAME", "MODEL", "LABELS", "UPDATED", "CWD"]);
    let mut widths: Vec<usize> = head.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                out.push_str(cell);
            } else {
                out.push_str(cell);
                out.push_str(&" ".repeat(widths[i] - cell.chars().count() + 2));
            }
        }
        out.trim_end().to_string() + "\n"
    };
    let head: Vec<String> = head.into_iter().map(String::from).collect();
    let mut out = line(&head);
    for row in &rows {
        out.push_str(&line(row));
    }
    if hidden > 0 {
        let which = if hidden_closed > 0 && hidden_archived > 0 {
            format!("{hidden_closed} closed, {hidden_archived} archived")
        } else if hidden_archived > 0 {
            "archived".into()
        } else {
            "closed".into()
        };
        out.push_str(&format!("{hidden} hidden ({which}); -a shows them\n"));
    }
    out
}

/// `burrow ls --group-by`: one line per group with its total and its
/// per-state counts, no agent rows; closed and archived are counted, which
/// is the point of a summary. A state filter and `--on` still apply.
fn group_counts(agents: &[&Value], group: GroupBy, opts: &ListOpts) -> String {
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    match group {
        GroupBy::State => {
            for a in agents {
                groups.entry(state(a)).or_default().push(a);
            }
        }
        GroupBy::Host => {
            for a in agents {
                let host = a["host"].as_str().unwrap_or("(unknown)").to_string();
                groups.entry(host).or_default().push(a);
            }
        }
        GroupBy::Label => {
            for a in agents {
                for label in label_names(a, &opts.workspaces) {
                    groups.entry(label).or_default().push(a);
                }
            }
        }
    }
    let mut lines: Vec<(String, Vec<&Value>)> = groups.into_iter().collect();
    match group {
        // In the order the states are worth acting on; the others, the
        // biggest first, by name where they tie.
        GroupBy::State => lines.sort_by(|(a, _), (b, _)| {
            STATES
                .iter()
                .position(|s| *s == a.as_str())
                .unwrap_or(usize::MAX)
                .cmp(
                    &STATES
                        .iter()
                        .position(|s| *s == b.as_str())
                        .unwrap_or(usize::MAX),
                )
        }),
        _ => lines.sort_by(|(a, am), (b, bm)| bm.len().cmp(&am.len()).then(a.cmp(b))),
    }
    if lines.is_empty() {
        return "No agents.\n".into();
    }
    let name_width = lines
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0)
        .max("total".chars().count());
    let count_width = lines
        .iter()
        .map(|(_, members)| members.len().to_string().len())
        .max()
        .unwrap_or(1)
        .max("total".chars().count());
    let mut out = String::new();
    for (name, members) in &lines {
        match group {
            GroupBy::State => out.push_str(&format!(
                "{:name_width$}  {:>count_width$}\n",
                name,
                members.len()
            )),
            _ => out.push_str(&format!(
                "{:name_width$}  {:>count_width$}  ({})\n",
                name,
                members.len(),
                state_counts(members)
            )),
        }
    }
    out.push_str(&format!(
        "{:name_width$}  {:>count_width$}\n",
        "total",
        agents.len()
    ));
    out
}

/// The per-state counts in [`STATES`]'s order, the zero ones left out:
/// "4 running, 6 idle, 2 review".
fn state_counts(agents: &[&Value]) -> String {
    (STATES.iter())
        .filter_map(|word| {
            let n = (agents.iter()).filter(|a| state(a) == *word).count();
            (n > 0).then(|| format!("{n} {word}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The LABELS column: the workspace's label names, then the agent's own
/// human labels as key=value. `paseo.*` and `crusteo.*` are other
/// software's state, not the agent's labels.
fn labels_of(agent: &Value, workspaces: &BTreeMap<String, Vec<String>>) -> String {
    let mut parts: Vec<String> = (label_names(agent, workspaces).iter())
        .filter(|name| !own_label(agent, name))
        .cloned()
        .collect();
    for (key, value) in own_labels(agent) {
        let value = value.as_str().unwrap_or("");
        parts.push(if value.is_empty() {
            key.clone()
        } else {
            format!("{key}={value}")
        });
    }
    parts.join(", ")
}

/// The label names an agent is counted under, for `--group-by label`: the
/// workspace's label names, then the agent's own human label keys.
fn label_names(agent: &Value, workspaces: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut names: Vec<String> = if let Some(names) = agent["workspaceLabels"].as_array() {
        (names.iter())
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    } else {
        (agent["workspaceId"].as_str())
            .and_then(|id| workspaces.get(id).cloned())
            .unwrap_or_default()
    };
    for (key, _) in own_labels(agent) {
        if !names.contains(key) {
            names.push(key.clone());
        }
    }
    names
}

/// The agent's own human labels, by key. `paseo.*` and `crusteo.*` are other
/// software's state. Sorted: a JSON object's order depends on how
/// `serde_json` was built (`preserve_order` in a workspace build), and a
/// listing should read the same either way.
fn own_labels(agent: &Value) -> Vec<(&String, &Value)> {
    let mut labels: Vec<_> = (agent["labels"].as_object().into_iter().flatten())
        .filter(|(key, _)| !key.starts_with("paseo.") && !key.starts_with("crusteo."))
        .collect();
    labels.sort_by(|a, b| a.0.cmp(b.0));
    labels
}

/// Whether the name is the agent's own label, shown as key=value in the
/// LABELS column rather than a bare name.
fn own_label(agent: &Value, name: &str) -> bool {
    (agent["labels"].as_object()).is_some_and(|labels| labels.contains_key(name))
}

/// `burrow inspect`: the agent's fields, one per line.
pub fn inspect(agent: &Value) -> String {
    let text = |key: &str| {
        agent[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut lines: Vec<(&str, String)> = vec![
        ("ID", text("id").unwrap_or_default()),
        ("Name", title(agent)),
        ("State", state(agent)),
        ("Model", model(agent)),
    ];
    let optional = [
        // A fleet snapshot says its host; a local one has no such field.
        ("Host", text("host")),
        (
            "Thinking",
            text("effectiveThinkingOptionId").or_else(|| text("thinkingOptionId")),
        ),
        ("Mode", text("currentModeId")),
        ("Cwd", text("cwd")),
        ("Workspace", text("workspaceId")),
        ("Created", text("createdAt")),
        ("Updated", text("updatedAt")),
        ("Last prompt", text("lastUserMessageAt")),
        ("Archived", text("archivedAt")),
        ("Last error", text("lastError")),
    ];
    lines.extend(optional.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))));
    if let Some(labels) = agent["labels"].as_object().filter(|l| !l.is_empty()) {
        // By key, as `own_labels` gives them: the same with any serde_json.
        let mut labels: Vec<String> = (labels.iter())
            .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
            .collect();
        labels.sort();
        lines.push(("Labels", labels.join(", ")));
    }
    if let Some(asking) = agent["pendingPermissions"]
        .as_array()
        .filter(|p| !p.is_empty())
    {
        lines.push(("Permissions", format!("{} waiting", asking.len())));
    }
    if agent["requiresAttention"] == true {
        let why = text("attentionReason").unwrap_or_else(|| "finished".into());
        lines.push(("Attention", format!("ready to review ({why})")));
    }
    let width = lines.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    lines
        .into_iter()
        .map(|(k, v)| format!("{k:<width$}  {v}\n"))
        .collect()
}

/// Who a `burrow send` is from.
#[derive(Clone, Copy, Debug)]
pub enum Sender<'a> {
    /// The agent's user: a person at a terminal.
    User,
    /// An agent, by id (`PASEO_AGENT_ID`).
    Agent(&'a str),
    /// Someone outside Burrow, by name (`--from`).
    Named(&'a str),
}

/// `burrow send`: sends `prompt` and, unless `wait` is `None`, waits for the
/// turn to end and returns the agent's last message. While it waits, the
/// receipt is on standard error, so a timeout is not read as a failure to
/// deliver. `host` is the `--host` it was sent with, for the hint to wait.
pub async fn send(
    client: &mut Client,
    agent: &Value,
    prompt: &str,
    sender: Sender<'_>,
    behavior: &str,
    wait: Option<Option<Duration>>,
    host: Option<&str>,
) -> Outcome {
    let id = agent["id"].as_str().unwrap_or_default();
    // A daemon that does not know `sourceName` would take the prompt as its
    // user's, and say it was delivered.
    let named = burrow_protocol::server_info::feature::MESSAGE_SOURCE_NAME;
    if matches!(sender, Sender::Named(_)) && client.info["features"][named] != true {
        return Err(
            "--from needs a Burrow from 0.2.2 on; this daemon would show the prompt as its \
             user's"
                .to_string()
                .into(),
        );
    }
    let mut message = json!({"type": "send_agent_message_request", "agentId": id,
                             "text": prompt, "activeTurnBehavior": behavior,
                             "messageId": uuid::Uuid::new_v4().to_string()});
    match sender {
        Sender::Agent(source) => message["sourceAgentId"] = source.into(),
        Sender::Named(name) => message["sourceName"] = name.into(),
        Sender::User => {}
    }
    // Replacing a turn waits for it to end.
    let reply = client
        .request(message, Some(WAIT + Duration::from_secs(15)))
        .await?;
    if reply["accepted"] == false {
        let why = reply["error"]
            .as_str()
            .unwrap_or("the agent did not take it");
        return Err(format!("{} refused the prompt: {why}", title(agent)).into());
    }
    let queued = (reply["disposition"] == "queued").then(|| reply["queuePosition"].as_u64());
    let receipt = receipt(agent, &reply);
    let Some(limit) = wait else {
        return Ok(receipt + "\n");
    };
    eprintln!("{receipt}");
    // Its own turn's end, not whichever turn runs then.
    let mut ask = json!({"type": "wait_for_finish_request", "agentId": id});
    if queued.is_some() {
        ask["messageId"] = reply["messageId"].clone();
    } else if reply["turnId"].is_string() {
        ask["turnId"] = reply["turnId"].clone();
    }
    finish(client, agent, ask, limit).await.map_err(|failed| {
        if failed.code != 3 {
            return failed;
        }
        let within = limit.map_or_else(String::new, |l| format!(" within {}", seconds(l)));
        let host = host.map_or_else(String::new, |host| format!("--host {host} "));
        let wait = format!("`burrow wait {host}{}`", short(id));
        // `burrow wait` waits for whichever turn runs then, not this prompt's:
        // for a queued prompt that may still be the turn before it.
        let message = match queued {
            Some(_) => format!(
                "Delivered; queued, no reply{within}. {wait} waits for the turn it is running, which may still be the one before this prompt."
            ),
            None => format!(
                "Delivered; no reply{within} (it is still working). {wait} waits for the turn it is running."
            ),
        };
        Failed { message, code: 3 }
    })
}

/// What became of a sent prompt, in a line: "Delivered to Builder (aa11bb22):
/// started."
fn receipt(agent: &Value, reply: &Value) -> String {
    let how = match reply["disposition"].as_str() {
        Some("steered") => "steered into its running turn".to_string(),
        Some("interrupted") => "its running turn was replaced".to_string(),
        Some("queued") => match reply["queuePosition"].as_u64() {
            Some(1) | None => "queued (next) behind its running turn".to_string(),
            Some(n) => format!("queued (#{n}) behind its running turn"),
        },
        _ => "started".to_string(),
    };
    let id = agent["id"].as_str().unwrap_or_default();
    format!("Delivered to {} ({}): {how}.", title(agent), short(id))
}

/// "5 s", "90 s", "1.5 s".
fn seconds(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s.fract() == 0.0 {
        format!("{} s", s as u64)
    } else {
        format!("{s:.1} s")
    }
}

/// Waits as `ask` (a `wait_for_finish_request`) says, at most `limit`, and
/// says how the turn ended: its last message, or why there is none yet.
async fn finish(
    client: &mut Client,
    agent: &Value,
    mut ask: Value,
    limit: Option<Duration>,
) -> Outcome {
    if let Some(limit) = limit {
        ask["timeoutMs"] = (limit.as_millis() as u64).into();
    }
    let done = client.request(ask, None).await?;
    wait_result(agent, &done)
}

/// How a `wait_for_finish` answer ends, local or through the Reef: its last
/// message, or why there is none yet.
pub fn wait_result(agent: &Value, done: &Value) -> Outcome {
    let id = agent["id"].as_str().unwrap_or_default();
    let last = done["lastMessage"]
        .as_str()
        .unwrap_or("")
        .trim_end()
        .to_string();
    let with_last = |text: String| {
        if last.is_empty() {
            text
        } else {
            format!("{last}\n\n{text}")
        }
    };
    match done["status"].as_str().unwrap_or("") {
        "idle" => Ok(if last.is_empty() {
            "(no reply text)\n".into()
        } else {
            format!("{last}\n")
        }),
        "permission" => Err(Failed {
            message: with_last(format!(
                "{} is waiting on a permission: answer it in Skiff, crusteo or Paseo's app.",
                title(agent)
            )),
            code: 2,
        }),
        "timeout" => Err(Failed {
            message: with_last(format!(
                "{} is still working; `burrow inspect {}` to check.",
                title(agent),
                short(id)
            )),
            code: 3,
        }),
        _ => {
            let why = done["error"].as_str().unwrap_or("the turn failed");
            Err(Failed {
                message: with_last(format!("{} failed: {why}", title(agent))),
                code: 1,
            })
        }
    }
}

/// What `burrow run` starts, with `paseo run`'s options.
#[derive(Debug, Default)]
pub struct Run {
    pub prompt: String,
    /// `claude`, or `codex/gpt-5.4` for a provider and its model.
    pub provider: String,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub mode: Option<String>,
    pub title: Option<String>,
    pub cwd: String,
    pub workspace: Option<String>,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    /// The agent running the command (`PASEO_AGENT_ID`): the new one is its
    /// child, in its workspace, as with `paseo run` from an agent.
    pub caller: Option<String>,
}

impl Run {
    /// `resolveProviderAndModel`: `--provider p/m` names the model too, which
    /// `--model` may repeat but not contradict.
    pub fn provider_and_model(&self) -> Result<(String, Option<String>), String> {
        let given = self.provider.trim();
        let model = self.model.as_deref().map(str::trim);
        if model == Some("") {
            return Err("--model cannot be empty".into());
        }
        let Some((provider, from_provider)) = given.split_once('/') else {
            if given.is_empty() {
                return Err(
                    "a provider is needed: --provider claude, or --provider codex/<model>".into(),
                );
            }
            return Ok((given.to_string(), model.map(str::to_string)));
        };
        let (provider, from_provider) = (provider.trim(), from_provider.trim());
        if provider.is_empty() || from_provider.is_empty() {
            return Err("use --provider <provider> or --provider <provider>/<model>".into());
        }
        if let Some(model) = model.filter(|m| *m != from_provider) {
            return Err(format!(
                "--provider names model {from_provider}, but --model says {model}"
            ));
        }
        Ok((provider.to_string(), Some(from_provider.to_string())))
    }

    /// The `agent.create.request` that starts it.
    pub fn request(&self) -> Result<Value, String> {
        let (provider, model) = self.provider_and_model()?;
        if self.prompt.trim().is_empty() {
            return Err("no prompt given".into());
        }
        let mut config = json!({"provider": provider, "cwd": self.cwd});
        let set = |config: &mut Value, key: &str, value: &Option<String>| {
            if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                config[key] = value.into();
            }
        };
        set(&mut config, "model", &model);
        set(&mut config, "modeId", &self.mode);
        set(&mut config, "thinkingOptionId", &self.thinking);
        set(&mut config, "title", &self.title);
        if !self.env.is_empty() {
            config["env"] = json!(self.env);
        }
        let mut message = json!({"type": "agent.create.request", "config": config,
                                 "initialPrompt": self.prompt, "labels": self.labels});
        if let Some(workspace) = self.workspace.as_deref().filter(|w| !w.trim().is_empty()) {
            message["workspaceId"] = workspace.trim().into();
        }
        if let Some(caller) = self.caller.as_deref().filter(|c| !c.is_empty()) {
            message["callerAgentId"] = caller.into();
        }
        Ok(message)
    }
}

/// `burrow run`: starts an agent on `run.prompt`. With `wait`, waits (at
/// most its limit) for that first turn and gives its last message, as
/// `burrow send` does; without, says what started.
pub async fn run(client: &mut Client, run: &Run, wait: Option<Option<Duration>>) -> Outcome {
    let reply = client.request(run.request()?, Some(WAIT * 3)).await?;
    let agent = reply["agent"].clone();
    let Some(id) = agent["id"].as_str().map(str::to_string) else {
        let why = reply["error"].as_str().unwrap_or("no agent came back");
        return Err(format!("could not start the agent: {why}").into());
    };
    let started = format!(
        "Started {} ({}), {} in {}.",
        title(&agent),
        short(&id),
        model(&agent),
        home_relative(agent["cwd"].as_str().unwrap_or(""))
    );
    let Some(limit) = wait else {
        return Ok(format!(
            "{started}\n`burrow wait {}` waits for it; `burrow logs {}` shows what it did.\n",
            short(&id),
            short(&id)
        ));
    };
    eprintln!("{started}");
    let ask = json!({"type": "wait_for_finish_request", "agentId": id});
    finish(client, &agent, ask, limit).await
}

/// `burrow wait`: until the agent's turn ends (at once when it has none),
/// with its last message.
pub async fn wait(client: &mut Client, agent: &Value, limit: Option<Duration>) -> Outcome {
    let id = agent["id"].as_str().unwrap_or_default();
    let ask = json!({"type": "wait_for_finish_request", "agentId": id});
    finish(client, agent, ask, limit).await
}

/// Timeline rows `burrow logs` reads, at most.
pub const LOG_ROWS: u64 = 5_000;

/// `burrow logs`: what the agent did, as the agent tool `get_agent_activity`
/// shows it; the last `tail` entries only, when given.
pub async fn logs(client: &mut Client, agent: &Value, tail: Option<usize>) -> Outcome {
    let id = agent["id"].as_str().unwrap_or_default();
    let page = client
        .request(
            json!({"type": "fetch_agent_timeline_request", "agentId": id,
                   "direction": "tail", "limit": LOG_ROWS, "projection": "projected"}),
            Some(WAIT),
        )
        .await?;
    if let Some(error) = page["error"].as_str() {
        return Err(format!("could not read {}'s timeline: {error}", title(agent)).into());
    }
    let mut timeline = fleet::timeline::Timeline::default();
    timeline.apply_page(&page);
    let items: Vec<&fleet::timeline::Item> = timeline.items().collect();
    let from = tail.map_or(0, |tail| items.len().saturating_sub(tail));
    let lines = crate::mcp::activity_lines(&items[from..], agent["cwd"].as_str().unwrap_or(""));
    if lines.is_empty() {
        return Ok(format!("{} has no activity yet.\n", title(agent)));
    }
    Ok(lines.join("\n") + "\n")
}

/// `burrow rename` and `burrow label`: one `update_agent_request`.
pub async fn update(
    client: &mut Client,
    agent: &Value,
    name: Option<&str>,
    labels: &BTreeMap<String, String>,
) -> Outcome {
    let id = agent["id"].as_str().unwrap_or_default();
    let mut message = json!({"type": "update_agent_request", "agentId": id});
    if let Some(name) = name {
        let name = name.trim();
        if name.is_empty() {
            return Err("a name cannot be blank".to_string().into());
        }
        message["name"] = name.into();
    }
    if !labels.is_empty() {
        message["labels"] = json!(labels);
    }
    let reply = client.request(message, Some(WAIT)).await?;
    if reply["accepted"] == false {
        let why = reply["error"].as_str().unwrap_or("not accepted");
        return Err(format!("could not update {}: {why}", title(agent)).into());
    }
    Ok(match name {
        Some(name) => format!("Renamed {} to {}.\n", short(id), name.trim()),
        None => {
            let set: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!(
                "Labelled {} ({}): {}.\n",
                title(agent),
                short(id),
                set.join(", ")
            )
        }
    })
}

/// `key=value` pairs for `burrow label`.
pub fn parse_labels(pairs: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut labels = BTreeMap::new();
    for pair in pairs.iter().flat_map(|p| p.split(',')) {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((key, value)) = pair.split_once('=') else {
            return Err(format!("a label is key=value, not \"{pair}\""));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("a label needs a key: \"{pair}\""));
        }
        labels.insert(key.to_string(), value.trim().to_string());
    }
    if labels.is_empty() {
        return Err("no labels given (key=value)".into());
    }
    Ok(labels)
}

/// `key=value` flags, each whole (a value may hold commas), as `paseo run`
/// reads `--env` and `--label`. None given is none.
pub fn parse_pairs(flag: &str, given: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut pairs = BTreeMap::new();
    for pair in given {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(format!("{flag} is key=value, not \"{pair}\""));
        };
        if key.trim().is_empty() {
            return Err(format!("{flag} needs a key: \"{pair}\""));
        }
        pairs.insert(key.trim().to_string(), value.to_string());
    }
    Ok(pairs)
}

/// What `burrow ls --label` keeps: agents in a workspace with every one of
/// `workspace` (label names, as Paseo's app shows them) whose own labels
/// also hold every `key=value` of `agent`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LabelFilter {
    pub workspace: Vec<String>,
    pub agent: BTreeMap<String, String>,
}

impl LabelFilter {
    /// Each `--label`: `key=value` is an agent's own label, anything else
    /// the name of a workspace label.
    pub fn parse(given: &[String]) -> Result<Self, String> {
        let mut filter = Self::default();
        for label in given {
            match label.split_once('=') {
                Some((key, value)) => {
                    let key = key.trim();
                    if key.is_empty() {
                        return Err(format!("a label needs a key: \"{label}\""));
                    }
                    filter
                        .agent
                        .insert(key.to_string(), value.trim().to_string());
                }
                None => {
                    let name = burrow_protocol::labels::normalize_name(label);
                    if name.is_empty() {
                        return Err("a label name cannot be blank".into());
                    }
                    filter.workspace.push(name);
                }
            }
        }
        Ok(filter)
    }

    pub fn is_empty(&self) -> bool {
        self.workspace.is_empty() && self.agent.is_empty()
    }

    /// Whether `agent` passes, given each workspace's label names by id.
    pub fn keeps(&self, agent: &Value, workspaces: &BTreeMap<String, Vec<String>>) -> bool {
        use burrow_protocol::labels::key;
        let own = |k: &String, v: &String| agent["labels"][k.as_str()].as_str() == Some(v);
        let theirs = (agent["workspaceId"].as_str())
            .and_then(|id| workspaces.get(id))
            .map(Vec::as_slice)
            .unwrap_or_default();
        self.agent.iter().all(|(k, v)| own(k, v))
            && (self.workspace.iter()).all(|name| theirs.iter().any(|l| key(l) == key(name)))
    }

    /// [`Self::keeps`] for an agent as `list_agents` lists it, with its
    /// `workspaceLabels`.
    pub fn keeps_listed(&self, agent: &Value) -> bool {
        use burrow_protocol::labels::key;
        let own = |k: &String, v: &String| agent["labels"][k.as_str()].as_str() == Some(v);
        let theirs: Vec<&str> = (agent["workspaceLabels"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .collect();
        self.agent.iter().all(|(k, v)| own(k, v))
            && (self.workspace.iter()).all(|name| theirs.iter().any(|l| key(l) == key(name)))
    }

    /// What `burrow ls` says when nothing passes.
    pub fn none_shown(&self) -> String {
        let mut named: Vec<String> = self.workspace.clone();
        named.extend(self.agent.iter().map(|(k, v)| format!("{k}={v}")));
        format!(
            "No agents labelled {} (`burrow ls -a` adds archived ones).\n",
            named.join(" and ")
        )
    }
}

/// Every workspace's label names, by workspace id, background ones too.
pub async fn workspace_labels(
    client: &mut Client,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let reply = client
        .request(
            json!({"type": "fetch_workspaces_request", "filter": {"includeBackground": true}}),
            Some(WAIT),
        )
        .await?;
    Ok((reply["entries"].as_array().into_iter().flatten())
        .filter_map(|workspace| {
            let id = workspace["id"].as_str()?.to_string();
            let labels = (workspace["labels"].as_array().into_iter().flatten())
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect();
            Some((id, labels))
        })
        .collect())
}

/// `burrow stop`: interrupts the agent's turn, if it has one.
pub async fn stop(client: &mut Client, agent: &Value) -> Outcome {
    let id = agent["id"].as_str().unwrap_or_default();
    let reply = client
        .request(
            json!({"type": "cancel_agent_request", "agentId": id}),
            Some(WAIT),
        )
        .await?;
    if let Some(error) = reply["error"].as_str() {
        return Err(format!("could not stop {}: {error}", title(agent)).into());
    }
    Ok(format!("Stopped {} ({}).\n", title(agent), short(id)))
}

/// `burrow archive`: stops a running agent first (archiving one mid-turn does
/// not stick), then archives it.
pub async fn archive(client: &mut Client, agent: &Value) -> Outcome {
    let id = agent["id"].as_str().unwrap_or_default();
    if !agent["archivedAt"].is_null() {
        return Ok(format!(
            "{} ({}) is already archived.\n",
            title(agent),
            short(id)
        ));
    }
    if busy(agent) {
        stop(client, agent).await?;
    }
    client
        .request(
            json!({"type": "archive_agent_request", "agentId": id}),
            Some(WAIT),
        )
        .await
        .map_err(|err| format!("could not archive {}: {err}", title(agent)))?;
    Ok(format!("Archived {} ({}).\n", title(agent), short(id)))
}

fn busy(agent: &Value) -> bool {
    matches!(agent["status"].as_str(), Some("running" | "initializing"))
}

/// The first eight characters of an id, as `burrow ls` shows it.
fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

pub fn title(agent: &Value) -> String {
    agent["title"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "(untitled)".into())
}

/// The state worth acting on: a waiting permission, then the status, then
/// "ready to review".
fn state(agent: &Value) -> String {
    let status = agent["status"].as_str().unwrap_or("unknown");
    if !agent["archivedAt"].is_null() {
        "archived".into()
    } else if agent["pendingPermissions"]
        .as_array()
        .is_some_and(|p| !p.is_empty())
    {
        "asking".into()
    } else if busy(agent) || status == "error" {
        status.into()
    } else if agent["requiresAttention"] == true {
        "review".into()
    } else {
        status.into()
    }
}

fn model(agent: &Value) -> String {
    let provider = agent["provider"].as_str().unwrap_or("");
    match agent["model"].as_str().filter(|m| !m.is_empty()) {
        Some(model) => format!("{provider}/{model}"),
        None => provider.to_string(),
    }
}

fn clip(text: &str, max: usize) -> String {
    let text = text.replace(['\n', '\r', '\t'], " ");
    if text.chars().count() <= max {
        text
    } else {
        text.chars().take(max - 1).collect::<String>() + "…"
    }
}

fn home_relative(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => path.to_string(),
    }
}

/// How long ago `at` was: `now`, `5m`, `3h`, `2d`.
fn age(at: Option<&str>, now: OffsetDateTime) -> String {
    let Some(then) = at.and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok()) else {
        return "-".into();
    };
    let seconds = (now - then).whole_seconds().max(0);
    match seconds {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, title: &str, status: &str) -> Value {
        json!({"id": id, "title": title, "status": status, "provider": "claude",
               "model": "opus", "cwd": "/work", "updatedAt": "2026-11-08T08:00:00Z",
               "pendingPermissions": [], "requiresAttention": false, "archivedAt": null})
    }

    #[test]
    fn the_compact_agent_keeps_what_a_list_needs_and_hides_the_rest() {
        let now = OffsetDateTime::parse("2026-11-08T10:00:00Z", &Rfc3339).unwrap();
        let agent = json!({
            "id": "cc33dd44-1234-5678-9abc-def012345678", "title": "Fix billing",
            "status": "idle", "provider": "claude", "model": "opus",
            "cwd": "/work", "updatedAt": "2026-11-08T09:55:00Z",
            "requiresAttention": false, "attentionReason": null, "archivedAt": null,
            "labels": {"team": "core", "paseo.open-agent-tab.cid_123": "true",
                       "crusteo.labels": "v1:lbl_9, lbl_8", "paseo.parent-agent-id": "agt_1",
                       "empty": ""},
            "workspaceLabels": ["billing", "ops team"], "host": "devbox",
        });
        let compact = compact_agent(&agent, now);
        // The full id is not in it: the shortId addresses the agent.
        assert_eq!(
            compact,
            json!({"shortId": "cc33dd4",
                   "host": "devbox", "title": "Fix billing", "status": "idle",
                   "model": "claude/opus", "workspaceLabels": ["billing", "ops team"],
                   "labels": {"team": "core"}, "updated": "5m"})
        );
        assert!(!compact.get("id").is_some(), "{compact}");

        // Null and empty fields are left out; the age buckets as `ls` says.
        let quiet = json!({"id": "a1b2c3d4-0000-0000-0000-000000000000", "title": "",
                           "status": "running", "provider": "codex", "model": null,
                           "updatedAt": null, "labels": {"paseo.parent-agent-id": "agt_1"}});
        assert_eq!(
            compact_agent(&quiet, now),
            json!({"shortId": "a1b2c3d", "status": "running", "model": "codex"})
        );
        let ages = |at: &str| {
            let agent = json!({"id": "a1b2c3d4-0000-0000-0000-000000000000",
                               "status": "idle", "updatedAt": at});
            compact_agent(&agent, now)["updated"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(ages("2026-11-08T09:59:30Z"), "now");
        assert_eq!(ages("2026-11-08T09:00:00Z"), "1h");
        assert_eq!(ages("2026-11-06T10:00:00Z"), "2d");
    }

    #[test]
    fn an_agent_is_found_by_id_prefix_or_name() {
        let mut archived = agent("cc33", "Old", "closed");
        archived["archivedAt"] = "2026-11-01T00:00:00Z".into();
        let agents = vec![
            agent("aa11-x", "Builder", "idle"),
            agent("aa22-y", "Reviewer", "running"),
            agent("bb11", "builder two", "idle"),
            archived,
        ];
        assert_eq!(find(&agents, "aa11-x").unwrap()["id"], "aa11-x");
        assert_eq!(find(&agents, "aa2").unwrap()["id"], "aa22-y");
        assert_eq!(find(&agents, "reviewer").unwrap()["id"], "aa22-y");
        assert_eq!(
            find(&agents, "cc").unwrap()["id"],
            "cc33",
            "archived ones still resolve"
        );
        let both = find(&agents, "aa").unwrap_err();
        assert!(both.starts_with("\"aa\" matches 2 agents"), "{both}");
        assert!(
            find(&agents, "zz")
                .unwrap_err()
                .starts_with("no agent matches")
        );
    }

    #[test]
    fn the_list_hides_closed_and_archived_and_says_how_many() {
        let now = OffsetDateTime::parse("2026-11-08T10:00:00Z", &Rfc3339).unwrap();
        let mut asking = agent("aa11", "Asks", "running");
        asking["pendingPermissions"] = json!([{"id": "p"}]);
        let mut done = agent("bb22", "Done", "idle");
        done["requiresAttention"] = true.into();
        let mut old = agent("cc33", "Old", "closed");
        old["archivedAt"] = "2026-11-01T00:00:00Z".into();
        let closed = agent("dd44", "Bye", "closed");
        let agents = vec![asking, done, old, closed];

        // Without -a: the hidden count, saying which, and no other word of it.
        let shown = list(&agents, &Default::default(), now);
        let lines: Vec<&str> = shown.lines().collect();
        assert!(lines[0].starts_with("ID"), "{shown}");
        assert!(
            lines[1].contains("asking") && lines[1].contains("claude/opus"),
            "{shown}"
        );
        assert!(
            lines[2].contains("review") && lines[2].contains("2h"),
            "{shown}"
        );
        assert_eq!(
            lines.get(3).copied(),
            Some("2 hidden (1 closed, 1 archived); -a shows them"),
            "{shown}"
        );
        assert_eq!(lines.len(), 4, "the hidden ones have no row: {shown}");
        let all = list(
            &agents,
            &ListOpts {
                all: true,
                ..Default::default()
            },
            now,
        );
        assert!(all.contains("archived") && all.contains("Bye"), "{all}");
        assert!(!all.contains("hidden"), "{all}");

        // Asking for a state shows it, closed and archived included, and no
        // footer: the filter said what it would show.
        let closed = list(
            &agents,
            &ListOpts {
                states: vec!["closed".into()],
                ..Default::default()
            },
            now,
        );
        assert!(
            closed.contains("Bye") && !closed.contains("Asks"),
            "{closed}"
        );
        assert!(!closed.contains("hidden"), "{closed}");
        let archived = list(
            &agents,
            &ListOpts {
                states: vec!["archived".into()],
                ..Default::default()
            },
            now,
        );
        assert!(
            archived.contains("Old") && !archived.contains("Bye"),
            "{archived}"
        );
        let none = list(
            &agents,
            &ListOpts {
                states: vec!["error".into()],
                ..Default::default()
            },
            now,
        );
        assert_eq!(none, "No agents in state: error.\n", "{none}");
    }

    #[test]
    fn the_labels_column_is_the_workspaces_names_and_the_agents_own_labels() {
        let now = OffsetDateTime::parse("2026-11-08T10:00:00Z", &Rfc3339).unwrap();
        let mut with_labels = agent("aa11", "Builder", "idle");
        with_labels["labels"] = json!({"team": "core", "stage": "", 
            "paseo.open-agent-tab.cid_123": "true", "crusteo.labels": "v1:lbl_1, lbl_2",
            "paseo.parent-agent-id": "agt_1"});
        let bare = agent("bb22", "Plain", "idle");
        let agents = vec![with_labels.clone(), bare];

        let shown = list(&agents, &Default::default(), now);
        let lines: Vec<&str> = shown.lines().collect();
        assert!(
            lines[0].starts_with("ID") && lines[0].contains("LABELS"),
            "{shown}"
        );
        // Its own labels, key=value where they have a value; the other
        // software's keys and the empty one stay out.
        assert!(
            lines[1].contains("stage, team=core"),
            "own labels, values and all: {shown}"
        );
        assert!(
            !shown.contains("paseo.") && !shown.contains("crusteo."),
            "{shown}"
        );

        // A listing that only knows the workspaces by id, as fetch_agents
        // gives them: their label names still show, first.
        let mut by_id = with_labels.clone();
        by_id["workspaceId"] = "w1".into();
        let shown = list(
            &[by_id],
            &ListOpts {
                workspaces: BTreeMap::from([("w1".to_string(), vec!["billing".to_string()])]),
                ..Default::default()
            },
            now,
        );
        // At 25 names it is cut to the column's 24, as one long entry must
        // not widen every row.
        assert!(
            (shown.lines()).any(|line| line.contains("billing, stage, team=co…")),
            "{shown}"
        );
    }

    #[test]
    fn the_list_filters_by_host_and_groups_by_host_label_and_state() {
        let now = OffsetDateTime::parse("2026-11-08T10:00:00Z", &Rfc3339).unwrap();
        let on = |host: &str, title: &str, status: &str| {
            let mut a = agent(title, title, status);
            a["host"] = host.into();
            a
        };
        let mut a1 = on("alpha", "One", "running");
        a1["labels"] = json!({"team": "core"});
        let a2 = on("alpha", "Two", "idle");
        let mut a3 = on("beta", "Three", "idle");
        a3["labels"] = json!({"team": "core"});
        let mut a4 = on("beta", "Four", "closed");
        a4["archivedAt"] = "2026-11-01T00:00:00Z".into();
        let agents = vec![a1, a2, a3, a4];

        // --on: that host's rows only, any case; the others not even counted.
        let alpha = list(
            &agents,
            &ListOpts {
                hosts: vec!["ALPHA".into()],
                ..Default::default()
            },
            now,
        );
        assert!(alpha.contains("One") && alpha.contains("Two"), "{alpha}");
        assert!(
            !alpha.contains("Three") && !alpha.contains("Four"),
            "{alpha}"
        );
        let missing = list(
            &agents,
            &ListOpts {
                hosts: vec!["gamma".into()],
                ..Default::default()
            },
            now,
        );
        assert_eq!(missing, "No agents on host: gamma.\n", "{missing}");

        // --group-by: one line per group, its total and its per-state
        // counts, no agent rows; the column padding is not what is under
        // test, so the lines are compared with it squashed.
        let squashed = |text: &str| {
            (text.lines())
                .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
        };
        // By host: the biggest first, by name where they tie; the closed
        // and archived are counted, which is the point of a summary.
        let by_host = list(
            &agents,
            &ListOpts {
                group_by: Some(GroupBy::Host),
                ..Default::default()
            },
            now,
        );
        assert_eq!(
            squashed(&by_host),
            vec![
                "alpha 2 (1 running, 1 idle)",
                "beta 2 (1 idle, 1 archived)",
                "total 4",
            ],
            "{by_host}"
        );
        // A state filter applies to the counts too.
        let by_host = list(
            &agents,
            &ListOpts {
                group_by: Some(GroupBy::Host),
                states: vec!["idle".into()],
                ..Default::default()
            },
            now,
        );
        assert_eq!(
            squashed(&by_host),
            vec!["alpha 1 (1 idle)", "beta 1 (1 idle)", "total 2"],
            "{by_host}"
        );

        // By state: the states in the order they are worth acting on.
        let by_state = list(
            &agents,
            &ListOpts {
                group_by: Some(GroupBy::State),
                ..Default::default()
            },
            now,
        );
        assert_eq!(
            squashed(&by_state),
            vec!["running 1", "idle 2", "archived 1", "total 4"],
            "{by_state}"
        );

        // By label: the labels' agents, the biggest first; agents with no
        // labels are in the total, not in a group.
        let by_label = list(
            &agents,
            &ListOpts {
                group_by: Some(GroupBy::Label),
                ..Default::default()
            },
            now,
        );
        assert_eq!(
            squashed(&by_label),
            vec!["team 2 (1 running, 1 idle)", "total 4"],
            "{by_label}"
        );
    }

    #[test]
    fn the_text_rows_are_the_rows_the_json_lists_for_each_filter() {
        // A small fleet: two hosts, every state, one label. The text rows
        // and the JSON list the same agents, in the same order, whatever
        // the filters: that is the agreement the JSON path is built on.
        let now = OffsetDateTime::parse("2026-11-08T10:00:00Z", &Rfc3339).unwrap();
        let make = |id: &str,
                    title: &str,
                    status: &str,
                    host: &str,
                    archived: bool,
                    asking: bool,
                    attention: bool|
         -> Value {
            let mut a = agent(id, title, status);
            a["host"] = host.into();
            if archived {
                a["archivedAt"] = "2026-11-07T00:00:00Z".into();
            }
            if asking {
                a["pendingPermissions"] = json!([{"id": "p"}]);
            }
            if attention {
                a["requiresAttention"] = true.into();
            }
            a
        };
        let mut one = make(
            "aa010001-0000-0000-0000-000000000001",
            "one",
            "idle",
            "alpha",
            false,
            false,
            false,
        );
        one["labels"] = json!({"team": "core"});
        let agents = vec![
            one,
            make(
                "aa020002-0000-0000-0000-000000000002",
                "two",
                "running",
                "alpha",
                false,
                false,
                false,
            ),
            make(
                "aa030003-0000-0000-0000-000000000003",
                "three",
                "closed",
                "alpha",
                false,
                false,
                false,
            ),
            make(
                "aa040004-0000-0000-0000-000000000004",
                "four",
                "idle",
                "alpha",
                true,
                false,
                false,
            ),
            make(
                "aa050005-0000-0000-0000-000000000005",
                "five",
                "idle",
                "alpha",
                false,
                true,
                false,
            ),
            make(
                "aa060006-0000-0000-0000-000000000006",
                "six",
                "idle",
                "alpha",
                false,
                false,
                true,
            ),
            make(
                "aa070007-0000-0000-0000-000000000007",
                "seven",
                "idle",
                "beta",
                false,
                false,
                false,
            ),
            make(
                "aa080008-0000-0000-0000-000000000008",
                "eight",
                "idle",
                "beta",
                false,
                false,
                false,
            ),
            make(
                "aa090009-0000-0000-0000-000000000009",
                "nine",
                "closed",
                "beta",
                false,
                false,
                false,
            ),
            make(
                "aa0a000a-0000-0000-0000-00000000000a",
                "ten",
                "error",
                "beta",
                false,
                false,
                false,
            ),
        ];
        let cases: Vec<(ListOpts, Vec<&str>)> = vec![
            (
                ListOpts::default(),
                vec![
                    "aa010001", "aa020002", "aa050005", "aa060006", "aa070007", "aa080008",
                    "aa0a000a",
                ],
            ),
            (
                ListOpts {
                    states: vec!["running".into()],
                    ..Default::default()
                },
                vec!["aa020002"],
            ),
            (
                ListOpts {
                    states: vec!["closed".into()],
                    ..Default::default()
                },
                vec!["aa030003", "aa090009"],
            ),
            (
                ListOpts {
                    states: vec!["archived".into()],
                    ..Default::default()
                },
                vec!["aa040004"],
            ),
            (
                ListOpts {
                    hosts: vec!["ALPHA".into()],
                    ..Default::default()
                },
                vec!["aa010001", "aa020002", "aa050005", "aa060006"],
            ),
            (
                ListOpts {
                    hosts: vec!["beta".into()],
                    states: vec!["idle".into()],
                    ..Default::default()
                },
                vec!["aa070007", "aa080008"],
            ),
            (
                ListOpts {
                    hosts: vec!["alpha".into()],
                    states: vec!["review".into()],
                    ..Default::default()
                },
                vec!["aa060006"],
            ),
            (
                ListOpts {
                    all: true,
                    ..Default::default()
                },
                vec![
                    "aa010001", "aa020002", "aa030003", "aa040004", "aa050005", "aa060006",
                    "aa070007", "aa080008", "aa090009", "aa0a000a",
                ],
            ),
            (
                ListOpts {
                    all: true,
                    hosts: vec!["alpha".into()],
                    ..Default::default()
                },
                vec![
                    "aa010001", "aa020002", "aa030003", "aa040004", "aa050005", "aa060006",
                ],
            ),
            (
                ListOpts {
                    hosts: vec!["gamma".into()],
                    ..Default::default()
                },
                vec![],
            ),
            (
                ListOpts {
                    hosts: vec!["beta".into()],
                    states: vec!["running".into()],
                    ..Default::default()
                },
                vec![],
            ),
        ];
        for (opts, want) in cases {
            let text = list(&agents, &opts, now);
            let picked = filtered(&agents, &opts);
            let (rows, hidden_closed, hidden_archived) = visible(&picked, &opts);
            // The JSON's agents, in order, by the id the text shows.
            let json_ids: Vec<&str> = (rows.iter())
                .map(|a| a["id"].as_str().unwrap())
                .map(|id| &id[..8])
                .collect();
            assert_eq!(json_ids, want, "opts: {opts:?}");
            // The text's rows, in order: its data lines start with the id.
            let text_ids: Vec<&str> = (text.lines())
                .skip(1)
                .filter_map(|line| line.split_whitespace().next())
                .filter(|t| t.len() == 8 && t.chars().all(|c| c.is_ascii_hexdigit()))
                .collect();
            assert_eq!(text_ids, want, "opts: {opts:?}\n{text}");
            // The footer counts the hidden ones, and it is text-only.
            if !opts.all && opts.states.is_empty() {
                let hidden = hidden_closed + hidden_archived;
                if hidden > 0 {
                    let which = if hidden_closed > 0 && hidden_archived > 0 {
                        format!("{hidden_closed} closed, {hidden_archived} archived")
                    } else if hidden_archived > 0 {
                        "archived".into()
                    } else {
                        "closed".into()
                    };
                    let footer = format!("{hidden} hidden ({which}); -a shows them");
                    assert!(text.contains(&footer), "opts: {opts:?}\n{text}");
                } else {
                    assert!(!text.contains("hidden"), "opts: {opts:?}\n{text}");
                }
            }
        }
    }

    #[test]
    fn a_receipt_says_it_was_delivered_and_how() {
        let agent = agent("aa11bb22-cccc-dddd-eeee-ffff00001111", "Builder", "running");
        let said = |reply: Value| receipt(&agent, &reply);
        assert_eq!(
            said(json!({"disposition": "started"})),
            "Delivered to Builder (aa11bb22): started."
        );
        assert_eq!(
            said(json!({"disposition": "steered"})),
            "Delivered to Builder (aa11bb22): steered into its running turn."
        );
        assert_eq!(
            said(json!({"disposition": "queued", "queuePosition": 1})),
            "Delivered to Builder (aa11bb22): queued (next) behind its running turn."
        );
        assert_eq!(
            said(json!({"disposition": "queued", "queuePosition": 3})),
            "Delivered to Builder (aa11bb22): queued (#3) behind its running turn."
        );
        assert_eq!(seconds(Duration::from_secs(5)), "5 s");
        assert_eq!(seconds(Duration::from_millis(1500)), "1.5 s");
    }

    #[test]
    fn a_wait_answer_says_how_the_turn_ended() {
        let agent = agent("aa11bb22-cccc-dddd-eeee-ffff00001111", "Builder", "idle");
        let done = wait_result(
            &agent,
            &json!({"status": "idle", "lastMessage": "It is done."}),
        );
        assert_eq!(done.unwrap(), "It is done.\n");
        assert_eq!(
            wait_result(&agent, &json!({"status": "idle"})).unwrap(),
            "(no reply text)\n"
        );
        let permission = wait_result(
            &agent,
            &json!({"status": "permission", "lastMessage": "May I run it?"}),
        )
        .unwrap_err();
        assert_eq!(permission.code, 2);
        assert!(
            permission.message.contains("May I run it?")
                && permission.message.contains("waiting on a permission"),
            "{}",
            permission.message
        );
        let timed_out = wait_result(&agent, &json!({"status": "timeout"})).unwrap_err();
        assert_eq!(timed_out.code, 3);
        assert!(
            timed_out.message.contains("still working"),
            "{}",
            timed_out.message
        );
        let failed = wait_result(
            &agent,
            &json!({"status": "error", "error": "the provider stopped"}),
        )
        .unwrap_err();
        assert_eq!(failed.code, 1);
        assert!(
            failed.message.contains("the provider stopped"),
            "{}",
            failed.message
        );
    }

    #[test]
    fn inspect_says_the_host_of_a_fleet_snapshot() {
        let mut agent = agent("aa11bb22-cccc-dddd-eeee-ffff00001111", "Builder", "idle");
        let local = inspect(&agent);
        assert!(!local.contains("Host"), "{local}");
        agent["host"] = "devbox".into();
        let fleet = inspect(&agent);
        assert!(
            (fleet.lines()).any(|line| line.starts_with("Host") && line.contains("devbox")),
            "{fleet}"
        );
    }

    #[test]
    fn ls_labels_are_workspace_label_names_or_an_agents_own_pairs() {
        let filter =
            LabelFilter::parse(&["Billing".into(), " ops  team ".into(), "role=review".into()])
                .unwrap();
        assert_eq!(filter.workspace, ["Billing", "ops team"]);
        assert_eq!(filter.agent["role"], "review");
        assert!(LabelFilter::parse(&["=x".into()]).is_err());
        assert!(LabelFilter::parse(&["  ".into()]).is_err());

        let workspaces = BTreeMap::from([
            (
                "w1".to_string(),
                vec!["billing".to_string(), "Ops Team".to_string()],
            ),
            ("w2".to_string(), vec!["billing".to_string()]),
        ]);
        let agent = |workspace: Option<&str>, labels: Value| json!({"id": "a", "workspaceId": workspace, "labels": labels});
        let both = agent(Some("w1"), json!({"role": "review"}));
        assert!(filter.keeps(&both, &workspaces));
        // Missing one workspace label, or its own label, or any workspace.
        assert!(!filter.keeps(&agent(Some("w2"), json!({"role": "review"})), &workspaces));
        assert!(!filter.keeps(&agent(Some("w1"), json!({})), &workspaces));
        assert!(!filter.keeps(&agent(None, json!({"role": "review"})), &workspaces));
        let billing = LabelFilter::parse(&["BILLING".into()]).unwrap();
        assert!(billing.keeps(&agent(Some("w2"), json!({})), &workspaces));
        assert!(!billing.keeps(&agent(Some("w9"), json!({})), &workspaces));
        assert_eq!(
            filter.none_shown(),
            "No agents labelled Billing and ops team and role=review (`burrow ls -a` adds archived ones).\n"
        );
    }

    #[test]
    fn run_reads_provider_and_model_as_paseo_run_does() {
        let run = |provider: &str, model: Option<&str>| Run {
            prompt: "go".into(),
            provider: provider.into(),
            model: model.map(str::to_string),
            cwd: "/w".into(),
            ..Run::default()
        };
        let ok = |provider, model| run(provider, model).provider_and_model().unwrap();
        assert_eq!(ok("claude", None), ("claude".into(), None));
        assert_eq!(
            ok("codex/gpt-5.4", None),
            ("codex".into(), Some("gpt-5.4".into()))
        );
        assert_eq!(
            ok("codex/gpt-5.4", Some("gpt-5.4")).1.as_deref(),
            Some("gpt-5.4")
        );
        assert_eq!(ok("codex", Some("gpt-5.4")).1.as_deref(), Some("gpt-5.4"));
        assert!(
            run("codex/gpt-5.4", Some("other"))
                .provider_and_model()
                .is_err()
        );
        assert!(run("", None).provider_and_model().is_err());
        assert!(run("codex/", None).provider_and_model().is_err());
        assert!(run("claude", Some(" ")).provider_and_model().is_err());

        let mut full = run("codex/gpt-5.4", None);
        full.mode = Some("plan".into());
        full.title = Some("Reviewer".into());
        full.env = BTreeMap::from([("A".into(), "1,2".into())]);
        full.labels = BTreeMap::from([("team".into(), "core".into())]);
        full.caller = Some("parent".into());
        full.workspace = Some("ws1".into());
        assert_eq!(
            full.request().unwrap(),
            json!({"type": "agent.create.request", "initialPrompt": "go",
                   "labels": {"team": "core"}, "workspaceId": "ws1", "callerAgentId": "parent",
                   "config": {"provider": "codex", "cwd": "/w", "model": "gpt-5.4",
                              "modeId": "plan", "title": "Reviewer", "env": {"A": "1,2"}}})
        );
        let mut blank = run("claude", None);
        blank.prompt = "  ".into();
        assert!(blank.request().is_err());
        assert_eq!(
            parse_pairs("--env", &["A=1,2".into(), "B=".into()]).unwrap(),
            BTreeMap::from([("A".into(), "1,2".into()), ("B".into(), String::new())])
        );
        assert!(parse_pairs("--env", &["A".into()]).is_err());
    }

    #[test]
    fn labels_are_key_value_pairs() {
        let labels = parse_labels(&["team=core, role=review".into(), "x=".into()]).unwrap();
        assert_eq!(labels["team"], "core");
        assert_eq!(labels["role"], "review");
        assert_eq!(labels["x"], "");
        assert!(parse_labels(&["nope".into()]).is_err());
        assert!(parse_labels(&["=v".into()]).is_err());
        assert!(parse_labels(&[]).is_err());
    }
}
