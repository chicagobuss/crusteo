//! The agent lists a phone sees: rows, the needs-you list, and the agent
//! list filtered, sorted and grouped on Reef, so a phone only ever
//! receives the window it shows. Pure, like the rest of the views.

use std::collections::{BTreeMap, HashMap};

use fleet::{Agent, Bucket, Fleet, Host};
use serde_json::{Map, Value, json};

use crate::config::host_id;
use crate::proto::{AgentFilter, Grouping, ListSpec, Sorting};
use crate::settings::Settings;

/// The first line of an agent's latest reply, and when it came.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LastReply {
    pub line: String,
    pub at: String,
}

/// What rows show besides the directory: settings and the reply cache.
pub struct Context<'a> {
    pub settings: &'a Settings,
    pub replies: &'a HashMap<(String, String), LastReply>,
    /// The fleet's label catalog: rows show and group by agents' own labels.
    /// None shows workspaces' Paseo labels, as before the catalog.
    pub labels: Option<&'a fleet::labels::Catalog>,
}

/// A row's key: host and agent id, unique across the fleet.
pub fn agent_key(host: &str, agent: &str) -> String {
    format!("{host}/{agent}")
}

/// When the agent last changed, to sort by.
fn activity_at(agent: &Agent) -> &str {
    agent.updated_at.as_deref().unwrap_or("")
}

/// The agent's state as Reef shows it: Paseo's bucket, except that
/// "ready to review" is not a state here. Nobody clears it from the
/// Reef, so a finished agent is simply done.
pub fn state(agent: &Agent) -> Bucket {
    match agent.bucket() {
        Bucket::Attention => Bucket::Done,
        bucket => bucket,
    }
}

fn state_rank(agent: &Agent) -> usize {
    let bucket = state(agent);
    Bucket::GROUP_ORDER
        .iter()
        .position(|b| *b == bucket)
        .unwrap_or(9)
}

/// The question or permission an agent waits on, in a line.
fn ask_summary(agent: &Agent) -> Option<Value> {
    let request = agent.pending_permissions.first()?;
    let kind = request["kind"].as_str().unwrap_or("tool");
    let title = request["title"]
        .as_str()
        .or(request["name"].as_str())
        .unwrap_or("Permission");
    let mut ask = json!({
        "id": request["id"].as_str().unwrap_or_default(),
        "kind": kind,
        "title": title,
    });
    if fleet::questions::is_question(request)
        && let Some(first) = fleet::questions::parse(&request["input"])
            .and_then(|questions| questions.into_iter().next())
    {
        ask["question"] = json!(first.question);
    }
    if agent.pending_permissions.len() > 1 {
        ask["more"] = json!(agent.pending_permissions.len() - 1);
    }
    Some(ask)
}

fn workspace_of<'a>(host: &'a Host, agent: &Agent) -> Option<&'a fleet::Workspace> {
    agent
        .workspace_id
        .as_deref()
        .and_then(|id| host.directory.workspaces.get(id))
}

pub fn workspace_name(workspace: &fleet::Workspace) -> &str {
    workspace
        .title
        .as_deref()
        .filter(|title| !title.is_empty())
        .unwrap_or(&workspace.name)
}

/// One agent, as lists show it. Fields that would be empty are left out.
pub fn agent_row(host: &Host, agent: &Agent, cx: &Context) -> Value {
    let directory = &host.directory;
    let workspace = workspace_of(host, agent);
    let key = agent_key(host.name(), &agent.id);
    let mut row = Map::new();
    let mut put = |name: &str, value: Value| {
        let empty = match &value {
            Value::Null => true,
            Value::String(text) => text.is_empty(),
            Value::Array(items) => items.is_empty(),
            _ => false,
        };
        if !empty {
            row.insert(name.to_string(), value);
        }
    };
    put("key", json!(key));
    put("host", json!(host.name()));
    put("id", json!(agent.id));
    put("title", json!(agent.display_title()));
    put("state", json!(state(agent).as_str()));
    put("notify", json!(cx.settings.notify_for(&key)));
    put("provider", json!(agent.provider_id()));
    put("model", json!(agent.model_label()));
    // The effort chosen, else the one the provider resolved its default to.
    let thinking = ["thinkingOptionId", "effectiveThinkingOptionId"]
        .iter()
        .find_map(|key| agent.raw[*key].as_str());
    put("thinking", json!(thinking));
    if let Some(workspace) = workspace {
        put("workspace", json!(workspace_name(workspace)));
        let project = directory
            .projects
            .get(&workspace.project_id)
            .map(|project| project.name())
            .unwrap_or_else(|| workspace.project_display_name.clone());
        put("project", json!(project));
        if cx.labels.is_none() {
            put("labels", json!(workspace.labels));
        }
        if workspace.background {
            put("background", json!(true));
        }
    }
    if let Some(catalog) = cx.labels {
        let ids = fleet::labels::effective(agent, workspace, catalog);
        let names: Vec<&str> = (ids.iter())
            .filter_map(|id| catalog.get(id))
            .map(|label| label.name.as_str())
            .collect();
        put("labels", json!(names));
        put("label_ids", json!(ids));
    }
    put("at", json!(activity_at(agent)));
    if let Some(reply) = cx.replies.get(&(host.name().to_string(), agent.id.clone())) {
        put("reply", json!(reply.line));
        put("reply_at", json!(reply.at));
    }
    put("parent", json!(agent.parent_agent_id));
    if let Some(ask) = ask_summary(agent) {
        put("ask", ask);
    }
    Value::Object(row)
}

/// Agents waiting on a person on online hosts: questions and permissions
/// first, then failures; newest first in each.
pub fn needs_you(fleet: &Fleet, cx: &Context) -> Vec<(String, Value)> {
    let mut agents: Vec<(&Host, &Agent)> = fleet
        .agents()
        .filter(|(host, agent)| {
            host.connection.is_online()
                && agent.archived_at.is_none()
                && matches!(state(agent), Bucket::NeedsInput | Bucket::Failed)
        })
        .collect();
    sort(&mut agents, Sorting::Needs);
    agents
        .into_iter()
        .map(|(host, agent)| {
            (
                agent_key(host.name(), &agent.id),
                agent_row(host, agent, cx),
            )
        })
        .collect()
}

/// The label filter's Background choice: background workspaces' agents,
/// and no others. A control character, so no label is spelt the same.
pub const BACKGROUND: &str = fleet::lanes::BACKGROUND;

/// The label group background workspaces' agents are in, and only there.
pub const BACKGROUND_GROUP: &str = "background";

/// Whether a list shows background workspaces' agents: the phone's own
/// choice, else Reef's setting.
fn shows_background(filter: &AgentFilter, cx: &Context) -> bool {
    filter.background.unwrap_or(cx.settings.show_background)
}

/// The label keys a list filters by. Background is left out while
/// background workspaces are hidden: it would match nothing.
fn wanted_labels(filter: &AgentFilter, background: bool) -> Vec<String> {
    (filter.labels.iter())
        .map(|l| fleet::directory::label_key(l))
        .filter(|key| background || key != BACKGROUND)
        .collect()
}

fn passes(
    host: &Host,
    agent: &Agent,
    filter: &AgentFilter,
    background: bool,
    words: &[String],
    labels: &[String],
    catalog: Option<&fleet::labels::Catalog>,
) -> bool {
    if agent.archived_at.is_some() {
        return false;
    }
    if !background && host.directory.in_background(agent) {
        return false;
    }
    if !filter.subagents && agent.parent_agent_id.is_some() {
        return false;
    }
    if !filter.hosts.is_empty() && !filter.hosts.iter().any(|h| h == host.name()) {
        return false;
    }
    if !filter.states.is_empty() && !filter.states.contains(&state(agent)) {
        return false;
    }
    let workspace = workspace_of(host, agent);
    if !labels.is_empty() {
        // A background workspace's agent matches Background, not its labels,
        // as it is grouped.
        let has = match (workspace, catalog) {
            (Some(w), _) if w.background => labels.iter().any(|l| l == BACKGROUND),
            // With the catalog, the agent's own labels, by id.
            (_, Some(catalog)) => fleet::labels::effective(agent, workspace, catalog)
                .iter()
                .any(|id| labels.iter().any(|l| l == id.as_str())),
            (Some(w), None) => w
                .labels
                .iter()
                .any(|l| labels.contains(&fleet::directory::label_key(l))),
            (None, None) => false,
        };
        if !has {
            return false;
        }
    }
    if !words.is_empty() {
        let mut haystack = agent.display_title().to_lowercase();
        if let Some(w) = workspace {
            haystack.push(' ');
            haystack.push_str(&w.name.to_lowercase());
            haystack.push(' ');
            haystack.push_str(&w.project_display_name.to_lowercase());
        }
        if !words.iter().all(|word| haystack.contains(word.as_str())) {
            return false;
        }
    }
    true
}

fn sort(agents: &mut [(&Host, &Agent)], sorting: Sorting) {
    agents.sort_by(|(ha, a), (hb, b)| {
        let first = match sorting {
            // The agent that did something last comes first, as Paseo's
            // history view has it.
            Sorting::Recent => activity_at(b).cmp(activity_at(a)),
            Sorting::Needs => state_rank(a)
                .cmp(&state_rank(b))
                .then_with(|| activity_at(b).cmp(activity_at(a))),
            Sorting::Name => a
                .display_title()
                .to_lowercase()
                .cmp(&b.display_title().to_lowercase()),
        };
        first
            .then_with(|| ha.name().cmp(hb.name()))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// The agent list as sent: every match counted, the groups, and the rows
/// of the window (per group when grouped).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ListOut {
    pub total: usize,
    pub groups: Vec<Value>,
    pub rows: Vec<(String, Value)>,
}

/// The groups an agent falls in, as (key, label, order). An agent with
/// several labels is in several groups.
fn groups_of(
    fleet: &Fleet,
    host: &Host,
    agent: &Agent,
    grouping: Grouping,
) -> Vec<(String, String, String)> {
    match grouping {
        Grouping::None => Vec::new(),
        Grouping::State => {
            let bucket = state(agent);
            vec![(
                format!("state:{}", bucket.as_str()),
                bucket.label().to_string(),
                format!("{:02}", state_rank(agent)),
            )]
        }
        Grouping::Host => {
            let position = fleet
                .hosts
                .iter()
                .position(|h| h.name() == host.name())
                .unwrap_or(0);
            vec![(
                format!("host:{}", host.name()),
                host.name().to_string(),
                format!("{position:04}"),
            )]
        }
        Grouping::Label => {
            let workspace = workspace_of(host, agent);
            if workspace.is_some_and(|w| w.background) {
                // Only here, not in its labels' groups too; after No label.
                return vec![(
                    BACKGROUND_GROUP.into(),
                    "Background".into(),
                    "\u{10FFFF}\u{10FFFF}".into(),
                )];
            }
            if let Some(catalog) = &fleet.labels {
                // The agent's own labels, by id, sorted by name.
                let ids = fleet::labels::effective(agent, workspace, catalog);
                let groups: Vec<(String, String, String)> = (ids.iter())
                    .filter_map(|id| catalog.get(id))
                    .map(|label| {
                        let order = fleet::directory::label_key(&label.name);
                        (format!("label:{}", label.id), label.name.clone(), order)
                    })
                    .collect();
                if groups.is_empty() {
                    return vec![("label:".into(), "No label".into(), "\u{10FFFF}".into())];
                }
                return groups;
            }
            let labels = workspace.map(|w| w.labels.clone()).unwrap_or_default();
            if labels.is_empty() {
                // After every label.
                return vec![("label:".into(), "No label".into(), "\u{10FFFF}".into())];
            }
            labels
                .iter()
                .map(|label| {
                    let key = fleet::directory::label_key(label);
                    (format!("label:{key}"), label.clone(), key)
                })
                .collect()
        }
        Grouping::Workspace => {
            let (id, name) = match workspace_of(host, agent) {
                Some(w) => (w.id.clone(), workspace_name(w).to_string()),
                None => (String::new(), "No workspace".to_string()),
            };
            let label = if fleet.hosts.len() > 1 {
                format!("{name} · {}", host.name())
            } else {
                name.clone()
            };
            // Workspaces order by their newest agent; set below.
            vec![(
                format!("ws:{}/{id}", host.name()),
                label,
                name.to_lowercase(),
            )]
        }
    }
}

/// The agent list a spec asks for: filtered, sorted, grouped and windowed.
pub fn agent_list(fleet: &Fleet, spec: &ListSpec, cx: &Context) -> ListOut {
    let filter = &spec.filter;
    let words: Vec<String> = filter
        .query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let background = shows_background(filter, cx);
    let labels = wanted_labels(filter, background);
    let mut agents: Vec<(&Host, &Agent)> = fleet
        .agents()
        .filter(|(host, agent)| {
            passes(
                host,
                agent,
                filter,
                background,
                &words,
                &labels,
                fleet.labels.as_ref(),
            )
        })
        .collect();
    sort(&mut agents, spec.sort);
    let total = agents.len();
    let row = |host: &Host, agent: &Agent| agent_row(host, agent, cx);

    if spec.group == Grouping::None {
        let rows = agents
            .iter()
            .skip(spec.offset)
            .take(spec.limit)
            .map(|(host, agent)| (agent_key(host.name(), &agent.id), row(host, agent)))
            .collect();
        return ListOut {
            total,
            groups: Vec::new(),
            rows,
        };
    }

    // Members of each group, in the list's order.
    struct Group<'a> {
        label: String,
        order: String,
        newest: &'a str,
        members: Vec<(&'a Host, &'a Agent)>,
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for &(host, agent) in &agents {
        for (key, label, order) in groups_of(fleet, host, agent, spec.group) {
            let group = groups.entry(key).or_insert_with(|| Group {
                label,
                order,
                newest: "",
                members: Vec::new(),
            });
            group.newest = group.newest.max(activity_at(agent));
            group.members.push((host, agent));
        }
    }
    let mut ordered: Vec<(String, Group)> = groups.into_iter().collect();
    if spec.group == Grouping::Workspace {
        ordered.sort_by(|(ka, a), (kb, b)| b.newest.cmp(a.newest).then_with(|| ka.cmp(kb)));
    } else {
        ordered.sort_by(|(ka, a), (kb, b)| a.order.cmp(&b.order).then_with(|| ka.cmp(kb)));
    }
    let mut out = ListOut {
        total,
        ..Default::default()
    };
    for (key, group) in ordered {
        let collapsed = spec.collapsed.contains(&key);
        out.groups.push(json!({
            "key": key, "label": group.label, "total": group.members.len(),
        }));
        if collapsed {
            continue;
        }
        let limit = spec.more.get(&key).copied().unwrap_or(spec.limit);
        for (host, agent) in group.members.iter().take(limit) {
            let mut value = row(host, agent);
            let row_key = format!("{key}|{}", agent_key(host.name(), &agent.id));
            value["key"] = json!(row_key);
            value["group"] = json!(key);
            out.rows.push((row_key, value));
        }
    }
    out
}

/// What a map shows: lanes, a small tile for every agent, and full cards
/// for the agents it looks at up close.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MapOut {
    pub lanes: Vec<Value>,
    pub tiles: Vec<(String, Value)>,
    pub cards: Vec<(String, Value)>,
}

/// The map of the agents a spec lets through. Lanes come in the
/// grouping's own order (workspaces by name, not by activity), and tiles
/// in each lane by when the agent was created, so nothing moves when an
/// agent is busy: a phone remembers where things are. A tile carries
/// only its state, whether it waits on a question or permission, and the
/// minute it last changed, which is what a far view draws.
pub fn map_view(fleet: &Fleet, spec: &ListSpec, focus: &[String], cx: &Context) -> MapOut {
    let filter = &spec.filter;
    let words: Vec<String> = filter
        .query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let background = shows_background(filter, cx);
    let labels = wanted_labels(filter, background);
    let mut agents: Vec<(&Host, &Agent)> = fleet
        .agents()
        .filter(|(host, agent)| {
            passes(
                host,
                agent,
                filter,
                background,
                &words,
                &labels,
                fleet.labels.as_ref(),
            )
        })
        .collect();
    agents.sort_by(|(ha, a), (hb, b)| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| ha.name().cmp(hb.name()))
            .then_with(|| a.id.cmp(&b.id))
    });
    struct Lane<'a> {
        label: String,
        order: String,
        members: Vec<(&'a Host, &'a Agent)>,
    }
    let mut lanes: BTreeMap<String, Lane> = BTreeMap::new();
    for &(host, agent) in &agents {
        let groups = match spec.group {
            Grouping::None => vec![("all".to_string(), "All agents".to_string(), String::new())],
            grouping => groups_of(fleet, host, agent, grouping),
        };
        for (key, label, order) in groups {
            lanes
                .entry(key)
                .or_insert_with(|| Lane {
                    label,
                    order,
                    members: Vec::new(),
                })
                .members
                .push((host, agent));
        }
    }
    let mut ordered: Vec<(String, Lane)> = lanes.into_iter().collect();
    ordered.sort_by(|(ka, a), (kb, b)| a.order.cmp(&b.order).then_with(|| ka.cmp(kb)));
    let mut out = MapOut::default();
    for (lane, group) in ordered {
        out.lanes.push(json!({
            "key": lane, "label": group.label, "total": group.members.len(),
        }));
        for (host, agent) in group.members {
            let agent_key = agent_key(host.name(), &agent.id);
            let key = format!("{lane}|{agent_key}");
            // To the minute: enough for a glow, and quiet while busy.
            let minute: String = activity_at(agent).chars().take(16).collect();
            let mut tile = json!({
                "key": key, "agent": agent_key, "lane": lane,
                "state": state(agent).as_str(), "at": minute,
            });
            if !agent.pending_permissions.is_empty() {
                tile["ask"] = json!(true);
            }
            out.tiles.push((key, tile));
        }
    }
    for wanted in focus {
        let Some((host_name, id)) = wanted.split_once('/') else {
            continue;
        };
        if let Some(host) = fleet.host(&host_id(host_name))
            && let Some(agent) = host.directory.agents.get(id)
        {
            out.cards.push((wanted.clone(), agent_row(host, agent, cx)));
        }
    }
    out
}

/// Paseo's label and identity colours by name, as their foreground on a
/// dark background (crusteo's `theme::IDENTITY`).
const LABEL_COLORS: [(&str, &str); 10] = [
    ("violet", "#a392d5"),
    ("sky", "#6aa6ce"),
    ("emerald", "#6cae96"),
    ("orange", "#cc8f64"),
    ("pink", "#d87da3"),
    ("indigo", "#9299d5"),
    ("teal", "#6cacab"),
    ("red", "#d88381"),
    ("amber", "#b29d64"),
    ("blue", "#7ba1d5"),
];

/// A label's colour as CSS: its named colour, a hex colour as given, or,
/// for a label with none, one picked from its name with Paseo's identity
/// hash, so it is the same everywhere.
pub fn label_color(name: &str, color: Option<&str>) -> String {
    if let Some(color) = color {
        let color = color.trim();
        if let Some((_, hex)) = LABEL_COLORS
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(color))
        {
            return hex.to_string();
        }
        let hex = color.trim_start_matches('#');
        if (hex.len() == 6 || hex.len() == 3) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return format!("#{hex}");
        }
    }
    LABEL_COLORS[identity(name)].1.to_string()
}

/// Whether `name` is one of Paseo's label colours, the only ones a label
/// may be given.
pub fn is_label_color(name: &str) -> bool {
    LABEL_COLORS.iter().any(|(n, _)| *n == name)
}

/// The colour name Paseo's identity hash picks for `name`: what a label
/// with no definition shows in, so one made from a phone keeps it.
pub fn label_color_name(name: &str) -> &'static str {
    LABEL_COLORS[identity(name)].0
}

fn identity(name: &str) -> usize {
    let hash = name.encode_utf16().fold(0u32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(u32::from(unit))
    });
    (hash as i32).unsigned_abs() as usize % LABEL_COLORS.len()
}

/// Every label in the fleet with its colour: the catalog's, by label id;
/// without one, by matching key, the definitions the hosts have, then
/// labels workspaces use without one.
pub fn label_colors(fleet: &Fleet) -> Value {
    let mut labels = Map::new();
    if let Some(catalog) = &fleet.labels {
        for label in &catalog.labels {
            labels.insert(
                label.id.to_string(),
                json!({ "name": label.name, "color": label_color(&label.name, Some(&label.color)) }),
            );
        }
        return Value::Object(labels);
    }
    for host in &fleet.hosts {
        for def in &host.directory.labels {
            let key = fleet::directory::label_key(&def.name);
            labels.entry(key).or_insert_with(
                || json!({ "name": def.name, "color": label_color(&def.name, Some(&def.color)) }),
            );
        }
    }
    for host in &fleet.hosts {
        for workspace in host.directory.workspaces.values() {
            for name in &workspace.labels {
                let key = fleet::directory::label_key(name);
                labels
                    .entry(key)
                    .or_insert_with(|| json!({ "name": name, "color": label_color(name, None) }));
            }
        }
    }
    Value::Object(labels)
}

/// Each host's connection, for the phone's status dots and host filter.
/// `since` holds when each host entered its current state.
pub fn hosts(fleet: &Fleet, since: &HashMap<String, String>) -> Value {
    let hosts: Vec<Value> = fleet
        .hosts
        .iter()
        .map(|host| {
            let (state, reason) = match &host.connection {
                fleet::Connection::Idle | fleet::Connection::Connecting => ("connecting", None),
                fleet::Connection::Online => ("online", None),
                fleet::Connection::Offline(reason) => ("offline", Some(reason.clone())),
                fleet::Connection::Refused(reason) => ("refused", Some(reason.clone())),
            };
            let mut labels: Vec<&str> = host
                .directory
                .labels
                .iter()
                .map(|label| label.name.as_str())
                .collect();
            // Labels in use but without a definition, as fake and old
            // daemons have them, count too.
            for workspace in host.directory.workspaces.values() {
                for label in &workspace.labels {
                    if !labels.iter().any(|l| l.eq_ignore_ascii_case(label)) {
                        labels.push(label);
                    }
                }
            }
            labels.sort_unstable_by_key(|l| l.to_lowercase());
            let mut entry = json!({
                "name": host.name(),
                "state": state,
                "agents": host.directory.agents.values().filter(|a| a.archived_at.is_none()).count(),
            "endpoint": host.config.endpoint,
                "labels": labels,
            });
            if let Some(reason) = reason {
                entry["reason"] = json!(reason);
            }
            if let Some(at) = since.get(host.name()) {
                entry["since"] = json!(at);
            }
            if let Some(version) = host.server.as_ref().map(|s| s.version.as_str())
                && !version.is_empty()
            {
                entry["version"] = json!(version);
            }
            entry
        })
        .collect();
    Value::Array(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet::HostConfig;

    fn host(name: &str, agents: Vec<Value>) -> Host {
        let mut host = Host::new(HostConfig {
            id: host_id(name),
            name: name.into(),
            endpoint: None,
            password: None,
        });
        host.connection = fleet::Connection::Online;
        host.directory.projects_from_workspaces = true;
        host.directory.apply_workspaces_reply(
            &json!({"entries": [
                {"id": "w1", "projectId": "p1", "projectDisplayName": "shop",
                 "name": "shop-fix", "status": "done", "labels": ["Billing", "infra"]},
                {"id": "w2", "projectId": "p1", "projectDisplayName": "shop",
                 "name": "shop-docs", "status": "done", "labels": []}]}),
            true,
        );
        let entries: Vec<Value> = agents.into_iter().map(|a| json!({ "agent": a })).collect();
        host.directory
            .apply_agents_reply(&json!({ "entries": entries }), true);
        host
    }

    fn agent(id: &str, workspace: &str, at: &str, extra: Value) -> Value {
        let mut agent = json!({"id": id, "title": format!("agent {id}"), "status": "idle",
            "workspaceId": workspace, "provider": "claude", "updatedAt": at});
        if let (Some(target), Value::Object(extra)) = (agent.as_object_mut(), extra) {
            target.extend(extra);
        }
        agent
    }

    fn fleet() -> Fleet {
        Fleet {
            labels: None,
            hosts: vec![
                host(
                    "example-a",
                    vec![
                        agent(
                            "old",
                            "w1",
                            "2026-10-17T08:00:00Z",
                            json!({"status": "running"}),
                        ),
                        agent("new", "w2", "2026-10-17T10:00:00Z", json!({})),
                        agent(
                            "zed",
                            "w1",
                            "2026-10-17T09:00:00Z",
                            json!({"status": "error"}),
                        ),
                    ],
                ),
                host(
                    "example-b",
                    vec![agent(
                        "mid",
                        "w2",
                        "2026-10-17T09:30:00Z",
                        json!({"title": "Alpha"}),
                    )],
                ),
            ],
        }
    }

    fn keys(out: &ListOut) -> Vec<&str> {
        out.rows.iter().map(|(k, _)| k.as_str()).collect()
    }

    fn list(spec: ListSpec) -> ListOut {
        let settings = Settings::default();
        let replies = HashMap::from([(
            ("example-a".to_string(), "new".to_string()),
            LastReply {
                line: "All done.".into(),
                at: "2026-10-17T10:00:00Z".into(),
            },
        )]);
        let cx = Context {
            settings: &settings,
            replies: &replies,
            labels: None,
        };
        agent_list(&fleet(), &spec, &cx)
    }

    fn one_host(agents: Vec<Value>) -> Fleet {
        Fleet {
            labels: None,
            hosts: vec![host("example-host", agents)],
        }
    }

    fn rows_of(fleet: &Fleet, filter: AgentFilter) -> Vec<String> {
        let settings = Settings::default();
        let replies = HashMap::new();
        let cx = Context {
            settings: &settings,
            replies: &replies,
            labels: None,
        };
        let spec = ListSpec {
            filter,
            ..Default::default()
        };
        agent_list(fleet, &spec, &cx)
            .rows
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    #[test]
    fn rows_say_the_model_and_the_effort_it_runs_at() {
        let host = host(
            "example-a",
            vec![
                agent(
                    "chosen",
                    "w1",
                    "2026-10-17T08:00:00Z",
                    json!({"model": "opus", "thinkingOptionId": "low", "effectiveThinkingOptionId": "low"}),
                ),
                agent(
                    "default",
                    "w1",
                    "2026-10-17T08:00:00Z",
                    json!({"thinkingOptionId": null, "effectiveThinkingOptionId": "high"}),
                ),
                agent("unknown", "w1", "2026-10-17T08:00:00Z", json!({})),
            ],
        );
        let (settings, replies) = (Settings::default(), HashMap::new());
        let cx = Context {
            settings: &settings,
            replies: &replies,
            labels: None,
        };
        let row = |id: &str| agent_row(&host, host.directory.agents.get(id).unwrap(), &cx);
        let chosen = row("chosen");
        assert_eq!(
            (&chosen["provider"], &chosen["model"], &chosen["thinking"]),
            (&json!("claude"), &json!("opus"), &json!("low"))
        );
        assert_eq!(
            row("default")["thinking"],
            "high",
            "the provider's resolved default"
        );
        assert!(row("unknown").get("thinking").is_none());
    }

    #[test]
    fn needs_you_puts_questions_first_then_failures() {
        let at = "2026-10-17T10:00:00Z";
        let fleet = one_host(vec![
            agent("done", "w1", at, json!({})),
            agent(
                "review",
                "w1",
                at,
                json!({"requiresAttention": true, "attentionReason": "finished"}),
            ),
            agent("failed", "w1", at, json!({"status": "error"})),
            agent(
                "asks",
                "w1",
                at,
                json!({"pendingPermissions": [{"id": "p1", "kind": "question", "name": "AskUserQuestion",
                "input": {"questions": [{"header": "Scope", "question": "Which part?", "options": [{"label": "All"}]}]}}]}),
            ),
            agent("running", "w1", at, json!({"status": "running"})),
        ]);
        let mut settings = Settings::default();
        settings.set_notify("example-host/failed", Some(false));
        let replies = HashMap::new();
        let cx = Context {
            settings: &settings,
            replies: &replies,
            labels: None,
        };
        let rows = needs_you(&fleet, &cx);
        // Finished work waiting for review is not Reef's business.
        let keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["example-host/asks", "example-host/failed"]);
        let ask = &rows[0].1["ask"];
        assert_eq!(ask["kind"], "question");
        assert_eq!(ask["question"], "Which part?");
        assert_eq!(rows[0].1["workspace"], "shop-fix");
        assert_eq!(rows[0].1["project"], "shop");
        assert_eq!(rows[1].1["labels"], json!(["Billing", "infra"]));
        assert_eq!(rows[0].1["notify"], true);
        assert_eq!(rows[1].1["notify"], false);
        let all = agent_list(&fleet, &ListSpec::default(), &cx);
        let review = all
            .rows
            .iter()
            .find(|(k, _)| k == "example-host/review")
            .unwrap();
        assert_eq!(review.1["state"], "done");
    }

    #[test]
    fn the_agent_list_filters_by_state_label_and_words() {
        let at = "2026-10-17T10:00:00Z";
        let fleet = one_host(vec![
            agent(
                "a",
                "w1",
                at,
                json!({"status": "running", "title": "Fix the flaky retry"}),
            ),
            agent("b", "w2", at, json!({"title": "Write docs"})),
            agent(
                "c",
                "w1",
                at,
                json!({"parentAgentId": "a", "status": "running"}),
            ),
        ]);
        assert_eq!(rows_of(&fleet, AgentFilter::default()).len(), 2);
        let running = AgentFilter {
            states: vec![Bucket::Running],
            subagents: true,
            ..Default::default()
        };
        assert_eq!(
            rows_of(&fleet, running),
            ["example-host/a", "example-host/c"]
        );
        let words = AgentFilter {
            query: "FLAKY fix".into(),
            ..Default::default()
        };
        assert_eq!(rows_of(&fleet, words), ["example-host/a"]);
        let label = AgentFilter {
            labels: vec!["billing".into()],
            ..Default::default()
        };
        assert_eq!(rows_of(&fleet, label), ["example-host/a"]);
        let other = AgentFilter {
            hosts: vec!["elsewhere".into()],
            ..Default::default()
        };
        assert!(rows_of(&fleet, other).is_empty());
    }

    #[test]
    fn background_agents_carry_the_flag_and_follow_the_phone_then_the_reef() {
        let at = "2026-10-17T10:00:00Z";
        let mut fleet = one_host(vec![
            agent("front", "w1", at, json!({})),
            agent("helper", "w3", at, json!({"status": "error"})),
        ]);
        fleet.hosts[0].directory.apply_push(&json!({"type": "workspace_update",
            "payload": {"kind": "upsert", "workspace": {"id": "w3", "projectId": "p1",
            "projectDisplayName": "shop", "name": "shop-bot", "status": "failed", "background": true}}}));
        let mut settings = Settings::default();
        let replies = HashMap::new();
        let keys_for = |settings: &Settings, background: Option<bool>| -> Vec<String> {
            let cx = Context {
                settings,
                replies: &replies,
                labels: None,
            };
            let spec = ListSpec {
                filter: AgentFilter {
                    background,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut keys: Vec<String> = agent_list(&fleet, &spec, &cx)
                .rows
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            keys.sort();
            let map: Vec<String> = map_view(&fleet, &spec, &[], &cx)
                .tiles
                .into_iter()
                .map(|(_, t)| t["agent"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(map.len(), keys.len(), "the map filters alike");
            keys
        };
        let both = ["example-host/front", "example-host/helper"];
        assert_eq!(keys_for(&settings, None), both, "shown by default");
        assert_eq!(keys_for(&settings, Some(false)), ["example-host/front"]);
        settings.show_background = false;
        assert_eq!(keys_for(&settings, None), ["example-host/front"]);
        assert_eq!(
            keys_for(&settings, Some(true)),
            both,
            "the phone's choice wins"
        );

        let cx = Context {
            settings: &settings,
            replies: &replies,
            labels: None,
        };
        let row = |id: &str| {
            agent_row(
                &fleet.hosts[0],
                fleet.hosts[0].directory.agents.get(id).unwrap(),
                &cx,
            )
        };
        assert_eq!(row("helper")["background"], true);
        assert!(row("front").get("background").is_none());
        // Waiting on a person is never hidden.
        let waiting: Vec<String> = needs_you(&fleet, &cx).into_iter().map(|(k, _)| k).collect();
        assert_eq!(waiting, ["example-host/helper"]);
    }

    #[test]
    fn background_agents_are_one_label_group_and_one_label_choice() {
        let at = "2026-10-17T10:00:00Z";
        let mut fleet = one_host(vec![
            agent("billing", "w1", at, json!({})),
            agent("loose", "w2", at, json!({})),
            agent("helper", "w3", at, json!({})),
        ]);
        // Labelled like w1, but background: only in Background.
        fleet.hosts[0]
            .directory
            .apply_push(&json!({"type": "workspace_update",
            "payload": {"kind": "upsert", "workspace": {"id": "w3", "projectId": "p1",
            "projectDisplayName": "shop", "name": "shop-bot", "status": "done",
            "labels": ["Billing"], "background": true}}}));
        let mut settings = Settings::default();
        let replies = HashMap::new();
        let by_label = |settings: &Settings, labels: Vec<String>| {
            let cx = Context {
                settings,
                replies: &replies,
                labels: None,
            };
            let spec = ListSpec {
                group: Grouping::Label,
                filter: AgentFilter {
                    labels,
                    ..Default::default()
                },
                ..Default::default()
            };
            let groups = |lanes: &[Value]| -> Vec<(String, u64)> {
                (lanes.iter())
                    .map(|g| {
                        (
                            g["key"].as_str().unwrap().into(),
                            g["total"].as_u64().unwrap(),
                        )
                    })
                    .collect()
            };
            let list = agent_list(&fleet, &spec, &cx);
            let map = map_view(&fleet, &spec, &[], &cx);
            assert_eq!(
                groups(&list.groups),
                groups(&map.lanes),
                "the map groups alike"
            );
            (groups(&list.groups), keys(&list).join(" "))
        };
        let (groups, rows) = by_label(&settings, vec![]);
        assert_eq!(
            groups,
            [
                ("label:billing".into(), 1),
                ("label:infra".into(), 1),
                ("label:".into(), 1),
                (BACKGROUND_GROUP.into(), 1),
            ]
        );
        assert!(rows.contains("background|example-host/helper"));
        assert_eq!(rows.matches("example-host/helper").count(), 1);

        // Background chooses exactly them; Billing no longer does.
        let (_, rows) = by_label(&settings, vec![BACKGROUND.into()]);
        assert_eq!(rows, "background|example-host/helper");
        let (_, rows) = by_label(&settings, vec!["billing".into()]);
        assert!(!rows.contains("helper"), "{rows}");

        // Hidden, there is no Background group, and choosing it is ignored.
        settings.show_background = false;
        let (groups, _) = by_label(&settings, vec![]);
        assert!(!groups.iter().any(|(key, _)| key == BACKGROUND_GROUP));
        let (groups, _) = by_label(&settings, vec![BACKGROUND.into()]);
        assert_eq!(groups.len(), 3);

        // Other groupings keep it where it was.
        let cx = Context {
            settings: &Settings::default(),
            replies: &replies,
            labels: None,
        };
        let by_workspace = agent_list(
            &fleet,
            &ListSpec {
                group: Grouping::Workspace,
                ..Default::default()
            },
            &cx,
        );
        assert!(keys(&by_workspace).contains(&"ws:example-host/w3|example-host/helper"));
    }

    #[test]
    fn the_newest_agent_comes_first_by_default_and_rows_carry_its_reply() {
        let out = list(ListSpec::default());
        assert_eq!(
            keys(&out),
            [
                "example-a/new",
                "example-b/mid",
                "example-a/zed",
                "example-a/old"
            ]
        );
        assert_eq!(out.rows[0].1["reply"], "All done.");
        let by_need = list(ListSpec {
            sort: Sorting::Needs,
            ..Default::default()
        });
        assert_eq!(keys(&by_need)[0], "example-a/zed");
        let by_name = list(ListSpec {
            sort: Sorting::Name,
            ..Default::default()
        });
        // "agent new", "agent old", "agent zed", then "Alpha": case aside.
        assert_eq!(keys(&by_name)[3], "example-b/mid");
    }

    #[test]
    fn a_map_keeps_tiles_where_they_were_made_and_cards_only_for_its_focus() {
        let fleet = one_host(vec![
            agent(
                "late",
                "w1",
                "2026-10-17T10:00:00.500Z",
                json!({"createdAt": "2026-10-17T09:00:00Z", "status": "running"}),
            ),
            agent(
                "early",
                "w1",
                "2026-10-17T08:00:00Z",
                json!({"createdAt": "2026-10-17T07:00:00Z",
                       "pendingPermissions": [{"id": "p", "kind": "tool"}]}),
            ),
            agent(
                "docs",
                "w2",
                "2026-10-17T11:00:00Z",
                json!({"createdAt": "2026-10-17T08:00:00Z"}),
            ),
        ]);
        let settings = Settings::default();
        let replies = HashMap::new();
        let cx = Context {
            settings: &settings,
            replies: &replies,
            labels: None,
        };
        let spec = ListSpec {
            group: Grouping::Workspace,
            ..Default::default()
        };
        let out = map_view(&fleet, &spec, &["example-host/docs".to_string()], &cx);
        // Workspaces by name on a map, not by activity.
        let lanes: Vec<&str> = out
            .lanes
            .iter()
            .map(|l| l["label"].as_str().unwrap())
            .collect();
        assert_eq!(lanes, ["shop-docs", "shop-fix"]);
        let tiles: Vec<&str> = out
            .tiles
            .iter()
            .map(|(_, t)| t["agent"].as_str().unwrap())
            .collect();
        // Oldest first in a lane, however recently it worked.
        assert_eq!(
            tiles,
            [
                "example-host/docs",
                "example-host/early",
                "example-host/late"
            ]
        );
        assert_eq!(out.tiles[1].1["ask"], true);
        assert_eq!(out.tiles[2].1["state"], "running");
        assert_eq!(out.tiles[2].1["at"], "2026-10-17T10:00");
        assert_eq!(out.cards.len(), 1);
        assert_eq!(out.cards[0].1["title"], "agent docs");
        // Ungrouped, everything is one lane.
        let one = map_view(&fleet, &ListSpec::default(), &[], &cx);
        assert_eq!(one.lanes.len(), 1);
        assert!(one.cards.is_empty());
    }

    #[test]
    fn labels_keep_their_colour_or_get_a_stable_one() {
        assert_eq!(label_color("billing", Some("orange")), "#cc8f64");
        assert_eq!(label_color("billing", Some("#12ab34")), "#12ab34");
        let picked = label_color("search", None);
        assert!(picked.starts_with('#'));
        assert_eq!(label_color("search", None), picked, "the same every time");
        assert_eq!(label_color("search", Some("not a colour")), picked);
        let mut fleet = one_host(vec![]);
        fleet.hosts[0]
            .directory
            .apply_labels_reply(&json!({"labels": [{"name": "Billing", "color": "sky"}]}));
        let colors = label_colors(&fleet);
        assert_eq!(
            colors["billing"],
            json!({"name": "Billing", "color": "#6aa6ce"})
        );
        // Used by a workspace but never defined: still coloured.
        assert_eq!(colors["infra"]["color"], label_color("infra", None));
    }

    #[test]
    fn grouping_counts_every_member_and_windows_each_group() {
        let by_label = list(ListSpec {
            group: Grouping::Label,
            limit: 1,
            ..Default::default()
        });
        let groups: Vec<(&str, u64)> = by_label
            .groups
            .iter()
            .map(|g| (g["label"].as_str().unwrap(), g["total"].as_u64().unwrap()))
            .collect();
        // Two agents carry both labels; the rest have none, shown last.
        assert_eq!(groups, [("Billing", 2), ("infra", 2), ("No label", 2)]);
        assert_eq!(
            keys(&by_label),
            [
                "label:billing|example-a/zed",
                "label:infra|example-a/zed",
                "label:|example-a/new"
            ]
        );
        assert_eq!(by_label.rows[0].1["group"], "label:billing");

        let by_state = list(ListSpec {
            group: Grouping::State,
            collapsed: vec!["state:done".into()],
            ..Default::default()
        });
        let labels: Vec<&str> = by_state
            .groups
            .iter()
            .map(|g| g["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, ["Failed", "Working", "Done"]);
        assert_eq!(by_state.rows.len(), 2, "a collapsed group sends no rows");

        let by_workspace = list(ListSpec {
            group: Grouping::Workspace,
            ..Default::default()
        });
        // The workspace with the newest agent leads.
        assert_eq!(by_workspace.groups[0]["label"], "shop-docs · example-a");
        let more = list(ListSpec {
            group: Grouping::Host,
            limit: 1,
            more: BTreeMap::from([("host:example-a".to_string(), 3)]),
            ..Default::default()
        });
        assert_eq!(more.rows.len(), 4);
        assert_eq!(more.total, 4);
    }
}
