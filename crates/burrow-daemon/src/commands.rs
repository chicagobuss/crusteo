//! Burrow's agent commands (`burrow ls`, `inspect`, `send`, `rename`, `label`,
//! `stop`, `archive`): the common operations, over the same protocol Paseo's
//! CLI uses, so they work on hosts without Paseo's CLI and against any Burrow
//! with `--host`. Each returns what it prints; [`Failed`] carries the message
//! and exit code of one that did not do what was asked.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};
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
    let mut args = json!({"includeArchived": all, "includeBackground": all, "limit": 200,
                          "sinceHours": 720});
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

/// `burrow ls`: one line per agent. `all` adds archived and background ones.
pub fn list(agents: &[Value], all: bool, now: OffsetDateTime) -> String {
    let shown: Vec<&Value> = (agents.iter())
        .filter(|a| all || a["archivedAt"].is_null())
        .collect();
    if shown.is_empty() {
        return if all {
            "No agents.\n".into()
        } else {
            "No agents (`burrow ls -a` adds archived ones).\n".into()
        };
    }
    // Agents from the fleet (`--fleet`) say their host.
    let hosts = shown.iter().any(|a| a["host"].is_string());
    let rows: Vec<Vec<String>> = (shown.iter())
        .map(|a| {
            let mut row = vec![short(a["id"].as_str().unwrap_or(""))];
            if hosts {
                row.push(clip(a["host"].as_str().unwrap_or("-"), 24));
            }
            row.extend([
                state(a),
                clip(&title(a), 48),
                clip(&model(a), 32),
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
    head.extend(["STATE", "NAME", "MODEL", "UPDATED", "CWD"]);
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
    out
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
        let labels: Vec<String> = (labels.iter())
            .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
            .collect();
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

/// `burrow send`: sends `prompt` and, unless `wait` is `None`, waits for the
/// turn to end and returns the agent's last message. `source` is the sending
/// agent's id when an agent sends it (`PASEO_AGENT_ID`).
pub async fn send(
    client: &mut Client,
    agent: &Value,
    prompt: &str,
    source: Option<&str>,
    behavior: &str,
    wait: Option<Option<Duration>>,
) -> Outcome {
    let id = agent["id"].as_str().unwrap_or_default();
    let mut message = json!({"type": "send_agent_message_request", "agentId": id,
                             "text": prompt, "activeTurnBehavior": behavior,
                             "messageId": uuid::Uuid::new_v4().to_string()});
    if let Some(source) = source {
        message["sourceAgentId"] = source.into();
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
    let Some(limit) = wait else {
        let how = match (reply["disposition"].as_str(), queued) {
            (Some("steered"), _) => "Sent into the running turn of",
            (Some("interrupted"), _) => "Replaced the running turn of",
            (_, Some(Some(1))) => "Queued (next) behind the running turn of",
            (_, Some(_)) => "Queued behind the running turn of",
            _ => "Sent to",
        };
        return Ok(format!("{how} {} ({}).\n", title(agent), short(id)));
    };
    if let Some(position) = queued {
        let place = match position {
            Some(1) | None => "next".to_string(),
            Some(n) => format!("number {n}"),
        };
        eprintln!(
            "{} is mid-turn; this waits behind it ({place} in line).",
            title(agent)
        );
    }
    // Its own turn's end, not whichever turn runs then.
    let mut ask = json!({"type": "wait_for_finish_request", "agentId": id});
    if queued.is_some() {
        ask["messageId"] = reply["messageId"].clone();
    } else if reply["turnId"].is_string() {
        ask["turnId"] = reply["turnId"].clone();
    }
    finish(client, agent, ask, limit).await
}

/// Waits as `ask` (a `wait_for_finish_request`) says, at most `limit`, and
/// says how the turn ended: its last message, or why there is none yet.
async fn finish(
    client: &mut Client,
    agent: &Value,
    mut ask: Value,
    limit: Option<Duration>,
) -> Outcome {
    let id = agent["id"].as_str().unwrap_or_default();
    if let Some(limit) = limit {
        ask["timeoutMs"] = (limit.as_millis() as u64).into();
    }
    let done = client.request(ask, None).await?;
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
const LOG_ROWS: u64 = 5_000;

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

fn title(agent: &Value) -> String {
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
    fn the_list_shows_state_and_hides_archived_unless_asked() {
        let now = OffsetDateTime::parse("2026-11-08T10:00:00Z", &Rfc3339).unwrap();
        let mut asking = agent("aa11", "Asks", "running");
        asking["pendingPermissions"] = json!([{"id": "p"}]);
        let mut done = agent("bb22", "Done", "idle");
        done["requiresAttention"] = true.into();
        let mut old = agent("cc33", "Old", "closed");
        old["archivedAt"] = "2026-11-01T00:00:00Z".into();
        let agents = vec![asking, done, old];
        let shown = list(&agents, false, now);
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
        assert_eq!(lines.len(), 3, "archived left out: {shown}");
        assert!(list(&agents, true, now).contains("archived"));
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
