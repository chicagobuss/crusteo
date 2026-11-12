//! Reef as the fleet's router: an agent on one Burrow prompts an agent on
//! another, through the connections Reef already holds
//! ([`paseo_client::routing`]). Off until the owner turns it on
//! (`agent_routing` in Reef's settings); Reef tells every Burrow which, on
//! connect and whenever it changes, and refuses what comes anyway while
//! it is off.
//!
//! A routed prompt whose sender wants to hear how it ends becomes a
//! [`Route`], kept in `routes.json` in the data dir until the sender's
//! Burrow has the notice, so a restart of either Burrow, or of Reef, loses
//! none. Routes already under way still end in a notice after routing is
//! turned off.
//!
//! Not a lock: Burrow has no passwords, so this adds no reach beyond what
//! anything that reaches Reef's hosts already has.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use paseo_client::Client;
use paseo_client::routing::{FEATURE, FINISHED, RESPONSE, ROUTER, TOOL_REQUEST, TOOLS, code, op};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::{Apply, Upstream, guarded_with_timeout};
use crate::config::host_id;

/// Where routes under way are kept, in the data dir.
pub const FILE: &str = "routes.json";
/// How long one wait on a target's turn lasts before it is asked again.
const WAIT_MS: u64 = 10 * 60 * 1000;
/// How long to wait before trying a host again.
const RETRY: Duration = Duration::from_secs(5);
/// How often a target that waits on a permission already told of is
/// looked at again.
const PERMISSION_POLL: Duration = Duration::from_secs(3);
/// How many times a notice the sender's Burrow refuses is tried.
const DELIVERY_TRIES: usize = 20;
/// Routed prompts one agent may send another in [`RATE_WINDOW`].
const RATE_LIMIT: usize = 20;
const RATE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Agents listed when a name matches several.
const CANDIDATES: usize = 10;
/// How long a lookup waits for hosts still loading their agents.
const LOOKUP_TRIES: u64 = 20;
const LOOKUP_PAUSE: Duration = Duration::from_millis(250);
/// How long a host has to run a tool for an agent elsewhere.
const TOOL_WITHIN: Duration = Duration::from_secs(20);

/// A routed prompt whose sender is to be told how its turn ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    pub route_id: String,
    pub source_host: String,
    pub caller_agent_id: String,
    pub target_host: String,
    pub target_agent_id: String,
    #[serde(default)]
    pub target_title: String,
    #[serde(default)]
    pub target_server_id: String,
    /// The prompt's message id on the target, for a wait on its own turn.
    #[serde(default)]
    pub message_id: Option<String>,
    /// The turn it started or joined, when the target said.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Permission requests already told of.
    #[serde(default)]
    pub told: Vec<String>,
}

/// Routes under way and how often each pair of agents routed lately.
#[derive(Default)]
pub struct Routes {
    pub on: bool,
    path: Option<PathBuf>,
    pub under_way: Vec<Route>,
    sent: HashMap<(String, String), VecDeque<Instant>>,
}

impl Routes {
    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Err(err) = crate::config::write_json(path, &self.under_way) {
            eprintln!("reef: saving routes: {err}");
        }
    }

    /// Whether `from` may route another prompt to `to` now; counts it if so.
    fn allow(&mut self, from: &str, to: &str) -> bool {
        let now = Instant::now();
        let sent = (self.sent)
            .entry((from.to_string(), to.to_string()))
            .or_default();
        while sent.front().is_some_and(|at| now - *at > RATE_WINDOW) {
            sent.pop_front();
        }
        if sent.len() >= RATE_LIMIT {
            return false;
        }
        sent.push_back(now);
        true
    }
}

/// A refusal for the asking Burrow: one of [`code`]'s, in words.
struct Refusal {
    code: &'static str,
    error: String,
    detail: Value,
}

impl Refusal {
    fn new(code: &'static str, error: impl Into<String>) -> Self {
        Self {
            code,
            error: error.into(),
            detail: Value::Null,
        }
    }
}

fn response(request_id: &str, outcome: Result<Value, Refusal>) -> Value {
    match outcome {
        Ok(result) => {
            json!({"type": RESPONSE, "requestId": request_id, "ok": true, "result": result})
        }
        Err(refusal) => json!({"type": RESPONSE, "requestId": request_id, "ok": false,
                               "code": refusal.code, "error": refusal.error,
                               "detail": refusal.detail}),
    }
}

/// A host's connection as a route's task finds it.
enum LinkNow {
    Up(Client),
    Down,
    /// Removed from the fleet.
    Gone,
}

/// The host's client now, asked of Reef's state through `apply`.
async fn link(apply: &mpsc::UnboundedSender<Apply>, host: &str) -> LinkNow {
    let (tx, rx) = oneshot::channel();
    let host = host.to_string();
    let asked = apply.send(Box::new(move |up: &mut Upstream| {
        let now = match (up.fleet.host(&host_id(&host)), up.links.get(&host)) {
            (Some(h), Some(l)) if h.connection.is_online() => LinkNow::Up(l.client.clone()),
            (Some(_), Some(_)) => LinkNow::Down,
            _ => LinkNow::Gone,
        };
        let _ = tx.send(now);
    }));
    if asked.is_err() {
        return LinkNow::Gone;
    }
    rx.await.unwrap_or(LinkNow::Gone)
}

impl Upstream {
    /// Loads the routes a stop left under way and follows them again.
    pub fn open_routes(&mut self, data_dir: Option<&Path>) {
        let path = data_dir.map(|dir| dir.join(FILE));
        let saved: Vec<Route> = (path.as_deref())
            .filter(|p| p.exists())
            .and_then(|p| crate::config::read_json(p).ok())
            .unwrap_or_default();
        self.routes.path = path;
        for route in saved {
            self.follow_route(route, false);
        }
    }

    /// Turns routing on or off, and tells every Burrow.
    pub fn set_routing(&mut self, on: bool) {
        if self.routes.on == on {
            return;
        }
        self.routes.on = on;
        let names: Vec<String> = self.links.keys().cloned().collect();
        for name in names {
            self.announce_routing(&name);
        }
    }

    /// Tells a Burrow whether Reef routes for it. Other daemons get nothing.
    pub(super) fn announce_routing(&self, name: &str) {
        let speaks = (self.fleet.host(&host_id(name)))
            .is_some_and(|host| host.connection.is_online() && host.has(FEATURE));
        let Some(link) = self.links.get(name).filter(|_| speaks) else {
            return;
        };
        let routing = if self.routes.on { "on" } else { "off" };
        link.client
            .send(json!({"type": ROUTER, "reefId": self.client_id, "routing": routing}));
    }

    /// A Burrow asks Reef to route ([`REQUEST`]).
    pub(super) fn on_fleet_request(&mut self, name: &str, payload: &Value) {
        let Some(request_id) = payload["requestId"].as_str().map(str::to_string) else {
            return;
        };
        let Some(source) = self.links.get(name).map(|l| l.client.clone()) else {
            return;
        };
        let refuse = |refusal: Refusal| source.send(response(&request_id, Err(refusal)));
        if !self.routes.on {
            return refuse(Refusal::new(
                code::OFF,
                "Messaging agents on other hosts is off in this fleet's Reef.",
            ));
        }
        match payload["op"].as_str() {
            Some(op::ROUTE_PROMPT) => {}
            Some(op::LIST_HOSTS) => {
                return source.send(response(&request_id, Ok(self.hosts_for(name))));
            }
            Some(op::LIST_AGENTS) => {
                return self.fleet_agents(&request_id, payload, source.clone());
            }
            Some(op::TOOL) => return self.fleet_tool(name, &request_id, payload, source.clone()),
            _ => return refuse(Refusal::new(code::REFUSED, "Reef does not do that.")),
        }
        let key = payload["targetAgentId"].as_str().unwrap_or_default().trim();
        let prompt = payload["prompt"].as_str().unwrap_or_default().to_string();
        if key.is_empty() || prompt.trim().is_empty() {
            return refuse(Refusal::new(
                code::REFUSED,
                "A prompt needs an agent and text.",
            ));
        }
        let (target_host, agent) = match self.find_agent(key) {
            Ok(found) => found,
            Err(refusal) => {
                if !self.look_again(name, payload, &refusal) {
                    refuse(refusal);
                }
                return;
            }
        };
        let caller = payload["sourceAgentId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let pair_from = caller.clone().unwrap_or_else(|| name.to_string());
        if !self.routes.allow(&pair_from, &agent.id) {
            return refuse(Refusal::new(
                code::RATE_LIMITED,
                "Too many prompts to that agent from this one in the last ten minutes; wait, or work through fewer messages.",
            ));
        }
        let Some(target) = (self.links.get(&target_host))
            .filter(|_| {
                self.fleet
                    .host(&host_id(&target_host))
                    .is_some_and(|h| h.connection.is_online())
            })
            .map(|l| l.client.clone())
        else {
            return refuse(Refusal::new(
                code::HOST_OFFLINE,
                format!("Its host, {target_host}, is not connected to the Reef now."),
            ));
        };
        let server_id = (self.fleet.host(&host_id(&target_host)))
            .and_then(|h| h.server.as_ref())
            .map(|s| s.server_id.clone())
            .unwrap_or_default();
        let title = agent.title.clone().unwrap_or_else(|| agent.id.clone());
        let behavior = payload["activeTurnBehavior"]
            .as_str()
            .unwrap_or("auto")
            .to_string();
        let notify = payload["notifyOnFinish"] == true && caller.is_some();
        let message_id = format!(
            "route-{}",
            crate::config::new_token().unwrap_or_else(|_| request_id.clone())
        );
        let mut send = json!({"type": "send_agent_message_request", "agentId": agent.id,
                              "text": prompt, "messageId": message_id,
                              "activeTurnBehavior": behavior});
        if let Some(caller) = &caller {
            send["sourceAgentId"] = caller.clone().into();
        }
        let route = notify.then(|| Route {
            route_id: message_id.clone(),
            source_host: name.to_string(),
            caller_agent_id: caller.clone().unwrap_or_default(),
            target_host: target_host.clone(),
            target_agent_id: agent.id.clone(),
            target_title: title.clone(),
            target_server_id: server_id,
            message_id: Some(message_id.clone()),
            turn_id: None,
            told: Vec::new(),
        });
        let receipt = json!({
            "target": {"agentId": agent.id, "title": title, "host": target_host},
            "messageId": message_id, "status": agent.status,
        });
        let apply = self.apply.clone();
        // Replacing a turn waits for it to end: the sender hears at once.
        let interrupting = behavior == "interrupt";
        if interrupting {
            let mut receipt = receipt.clone();
            receipt["disposition"] = "interrupting".into();
            source.send(response(&request_id, Ok(receipt)));
        }
        tokio::spawn(async move {
            let sent = guarded_with_timeout(&target, send, Duration::from_secs(25)).await;
            let outcome = match sent {
                Ok(reply) if reply["accepted"] != false && reply["error"].is_null() => Ok(reply),
                Ok(reply) => Err(Refusal::new(
                    code::REFUSED,
                    reply["error"]
                        .as_str()
                        .unwrap_or("Its host refused the prompt.")
                        .to_string(),
                )),
                Err(error) => Err(Refusal::new(code::REFUSED, error)),
            };
            let turn = (outcome.as_ref().ok())
                .and_then(|r| r["turnId"].as_str())
                .map(str::to_string);
            if !interrupting {
                let answer = outcome.map(|reply| {
                    let mut receipt = receipt;
                    receipt["disposition"] =
                        (reply["disposition"].as_str()).unwrap_or("started").into();
                    if let Some(position) = reply["queuePosition"].as_u64() {
                        receipt["queuePosition"] = position.into();
                    }
                    receipt
                });
                let delivered = answer.is_ok();
                source.send(response(&request_id, answer));
                if !delivered {
                    return;
                }
            } else if let Err(refusal) = outcome {
                eprintln!(
                    "reef: a routed interrupt was not delivered: {}",
                    refusal.error
                );
                return;
            }
            if let Some(mut route) = route {
                route.turn_id = turn;
                let _ = apply.send(Box::new(move |up: &mut Upstream| {
                    up.follow_route(route, true)
                }));
            }
        });
    }

    /// A lookup that found no agent while a host still loads its agents:
    /// asks `payload` again soon, for a few seconds, before saying no agent
    /// has it. `false` when the refusal stands.
    fn look_again(&self, name: &str, payload: &Value, refusal: &Refusal) -> bool {
        let tries = payload["_tries"].as_u64().unwrap_or(0);
        if refusal.code != code::NOT_FOUND || self.connected_synced() || tries >= LOOKUP_TRIES {
            return false;
        }
        let mut payload = payload.clone();
        payload["_tries"] = (tries + 1).into();
        let (apply, name) = (self.apply.clone(), name.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(LOOKUP_PAUSE).await;
            let _ = apply.send(Box::new(move |up: &mut Upstream| {
                up.on_fleet_request(&name, &payload)
            }));
        });
        true
    }

    /// The fleet's hosts but `asking`, in Paseo's host directory shape
    /// (`HostSummary`), with how many agents each has and whether it takes
    /// routed prompts and follow-ups (`reachable`).
    fn hosts_for(&self, asking: &str) -> Value {
        let hosts: Vec<Value> = (self.fleet.hosts.iter())
            .filter(|host| host.name() != asking)
            .map(|host| {
                let online = host.connection.is_online();
                let server = host.server.as_ref().map(|s| s.server_id.clone());
                json!({
                    "registrationId": format!("reef:{}", host.name()),
                    "serverId": server, "label": host.name(),
                    "source": {"id": "reef", "key": self.client_id},
                    "metadata": {}, "enabled": true,
                    "status": if online { "online" } else { "offline" },
                    "targetDescription": "through the fleet's Reef",
                    "lastOnlineAt": null, "nextRetryAt": null, "error": null,
                    "agents": host.directory.agents.len(),
                    "reachable": online && host.has(FEATURE),
                })
            })
            .collect();
        json!({"hosts": hosts})
    }

    /// The hosts a request to all of them goes to: each one's name, server
    /// id and client, or why it cannot be asked.
    fn every_host(&self) -> Vec<(String, String, Result<Client, &'static str>)> {
        (self.fleet.hosts.iter())
            .map(|host| {
                let name = host.name().to_string();
                let server = (host.server.as_ref())
                    .map(|s| s.server_id.clone())
                    .unwrap_or_default();
                let client = match self.links.get(&name) {
                    _ if !host.connection.is_online() => Err("offline"),
                    _ if !host.has(FEATURE) => Err("its daemon is not a Burrow that answers this"),
                    Some(link) => Ok(link.client.clone()),
                    None => Err("offline"),
                };
                (name, server, client)
            })
            .collect()
    }

    /// [`op::LIST_AGENTS`]: `list_agents` on every connected Burrow at once,
    /// each agent with its host, merged in Paseo's order and cut to the
    /// limit; and each host with whether it answered.
    fn fleet_agents(&self, request_id: &str, payload: &Value, source: Client) {
        let args = (payload.get("args").cloned())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let limit = args["limit"].as_u64().unwrap_or(50) as usize;
        let hosts = self.every_host();
        let request_id = request_id.to_string();
        tokio::spawn(async move {
            let asks = hosts.into_iter().map(|(name, server, client)| {
                let args = args.clone();
                async move {
                    let answer = match client {
                        Err(why) => Err(why.to_string()),
                        Ok(client) => {
                            let ask = json!({"type": TOOL_REQUEST, "tool": "list_agents",
                                             "args": args});
                            tool_answer(guarded_with_timeout(&client, ask, TOOL_WITHIN).await)
                        }
                    };
                    (name, server, answer)
                }
            });
            let answers = futures_util::future::join_all(asks).await;
            let mut agents: Vec<Value> = Vec::new();
            let mut hosts: Vec<Value> = Vec::new();
            for (name, server, answer) in answers {
                match answer {
                    Ok(listed) => {
                        for mut agent in (listed["agents"].as_array().cloned()).unwrap_or_default()
                        {
                            agent["host"] = name.clone().into();
                            agent["serverId"] = server.clone().into();
                            agents.push(agent);
                        }
                        hosts.push(json!({"host": name, "serverId": server, "answered": true}));
                    }
                    Err(error) => hosts.push(json!({"host": name, "serverId": server,
                                                    "answered": false, "error": error})),
                }
            }
            sort_listed(&mut agents);
            agents.truncate(limit);
            source.send(response(
                &request_id,
                Ok(json!({"agents": agents, "hosts": hosts})),
            ));
        });
    }

    /// [`op::TOOL`]: one of [`TOOLS`] on the host of the agent
    /// `targetAgentId` names, or of `serverId`; its answer, with the host.
    fn fleet_tool(&self, name: &str, request_id: &str, payload: &Value, source: Client) {
        let refuse = |refusal: Refusal| source.send(response(request_id, Err(refusal)));
        let tool = payload["tool"].as_str().unwrap_or_default().to_string();
        if !TOOLS.contains(&tool.as_str()) {
            return refuse(Refusal::new(
                code::REFUSED,
                format!("{tool} does not reach other hosts."),
            ));
        }
        let mut args = (payload.get("args").cloned())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let host = if let Some(key) = payload["targetAgentId"].as_str() {
            match self.find_agent(key.trim()) {
                Ok((host, agent)) => {
                    args["agentId"] = agent.id.into();
                    host
                }
                Err(refusal) => {
                    if !self.look_again(name, payload, &refusal) {
                        refuse(refusal);
                    }
                    return;
                }
            }
        } else {
            let server = payload["serverId"].as_str().unwrap_or_default();
            let found = (self.fleet.hosts.iter()).find(|host| {
                host.name() == server || host.server.as_ref().is_some_and(|s| s.server_id == server)
            });
            match found {
                Some(host) => host.name().to_string(),
                None => {
                    return refuse(Refusal::new(
                        code::NOT_FOUND,
                        format!("No host in the fleet is {server}."),
                    ));
                }
            }
        };
        let client = match (self.every_host().into_iter()).find(|(n, _, _)| *n == host) {
            Some((_, _, Ok(client))) => client,
            Some((_, _, Err(why))) => {
                return refuse(Refusal::new(
                    code::HOST_OFFLINE,
                    format!("Its host, {host}, cannot be asked now: {why}."),
                ));
            }
            None => {
                return refuse(Refusal::new(
                    code::HOST_OFFLINE,
                    format!("Its host, {host}, is not connected to the Reef now."),
                ));
            }
        };
        let request_id = request_id.to_string();
        tokio::spawn(async move {
            let ask = json!({"type": TOOL_REQUEST, "tool": tool, "args": args});
            let outcome = tool_answer(guarded_with_timeout(&client, ask, TOOL_WITHIN).await)
                .map(|mut result| {
                    if result.is_object() {
                        result["host"] = host.clone().into();
                    }
                    result
                })
                .map_err(|error| Refusal::new(code::REFUSED, error));
            source.send(response(&request_id, outcome));
        });
    }

    /// The agent `key` names, by id, a unique id prefix (seven or more
    /// characters) or its name, on any host.
    fn find_agent(&self, key: &str) -> Result<(String, paseo_client::proto::Agent), Refusal> {
        let all = || {
            (self.fleet.hosts.iter()).flat_map(|host| {
                (host.directory.agents.values()).map(move |agent| (host.name(), agent))
            })
        };
        let exact: Vec<_> = all().filter(|(_, a)| a.id == key).collect();
        let mut found = exact;
        if found.is_empty() && key.len() >= 7 {
            found = all().filter(|(_, a)| a.id.starts_with(key)).collect();
        }
        if found.is_empty() {
            found = all()
                .filter(|(_, a)| {
                    (a.title.as_deref()).is_some_and(|t| t.trim().eq_ignore_ascii_case(key))
                })
                .collect();
        }
        match found.as_slice() {
            [] => Err(Refusal::new(
                code::NOT_FOUND,
                format!("No agent in the fleet goes by {key}."),
            )),
            [(host, agent)] => Ok((host.to_string(), (*agent).clone())),
            many => {
                let candidates: Vec<Value> = (many.iter().take(CANDIDATES))
                    .map(|(host, a)| {
                        json!({"agentId": a.id, "title": a.title, "host": host,
                                            "status": a.status})
                    })
                    .collect();
                Err(Refusal {
                    code: code::AMBIGUOUS,
                    error: format!("More than one agent goes by {key}."),
                    detail: json!({"candidates": candidates}),
                })
            }
        }
    }

    /// Keeps `route` (saved, when `new`) and follows it to its notice.
    fn follow_route(&mut self, route: Route, new: bool) {
        if new {
            self.routes
                .under_way
                .retain(|r| r.route_id != route.route_id);
            self.routes.under_way.push(route.clone());
            self.routes.save();
        }
        tokio::spawn(follow(self.apply.clone(), route));
    }

    fn route_told(&mut self, route_id: &str, told: Vec<String>) {
        if let Some(route) = (self.routes.under_way.iter_mut()).find(|r| r.route_id == route_id) {
            route.told = told;
            self.routes.save();
        }
    }

    fn route_done(&mut self, route_id: &str) {
        let before = self.routes.under_way.len();
        self.routes.under_way.retain(|r| r.route_id != route_id);
        if self.routes.under_way.len() != before {
            self.routes.save();
        }
    }
}

/// A Burrow's answer to a [`TOOL_REQUEST`]: the tool's result, or its error
/// in words (a Burrow from before these says it does not know the request).
fn tool_answer(answer: Result<Value, String>) -> Result<Value, String> {
    match answer {
        Ok(answer) if answer["ok"] == true => Ok(answer["result"].clone()),
        Ok(answer) => Err((answer["error"].as_str())
            .unwrap_or("Its host refused.")
            .to_string()),
        Err(error) if error.contains("upgrading the daemon") => {
            Err("its Burrow is too old for this and needs upgrading".to_string())
        }
        Err(error) => Err(error),
    }
}

/// Paseo's `selectAgentListItems` order across hosts: attention first,
/// then running, then by latest update.
fn sort_listed(agents: &mut [Value]) {
    let rank = |a: &Value| match a["status"].as_str() {
        Some("running") => 0,
        Some("initializing") => 1,
        Some("idle") => 2,
        Some("error") => 3,
        _ => 4,
    };
    // Burrows write every time the same way (RFC 3339, UTC), so as text.
    let updated = |a: &Value| a["updatedAt"].as_str().unwrap_or_default().to_string();
    agents.sort_by(|a, b| {
        (b["requiresAttention"] == true)
            .cmp(&(a["requiresAttention"] == true))
            .then(rank(a).cmp(&rank(b)))
            .then(updated(b).cmp(&updated(a)))
    });
}

/// Paseo's `reasonFor`, as Burrow's finish notices say it.
fn reason(answer: &Value) -> &'static str {
    let last = &answer["final"];
    if last.is_null() {
        return "is unavailable";
    }
    if last["status"] == "closed" {
        return "was closed";
    }
    match answer["status"].as_str() {
        Some("error") => "errored",
        Some("permission") => "needs permission",
        _ => "finished",
    }
}

/// Waits for the routed prompt's turn to end on its host, and tells the
/// sender, through its Burrow, until it has been told or is gone.
async fn follow(apply: mpsc::UnboundedSender<Apply>, mut route: Route) {
    loop {
        let answer = loop {
            match link(&apply, &route.target_host).await {
                LinkNow::Gone => break json!({"status": "error", "final": null}),
                LinkNow::Down => tokio::time::sleep(RETRY).await,
                LinkNow::Up(client) => {
                    let mut ask = json!({"type": "wait_for_finish_request",
                                         "agentId": route.target_agent_id, "timeoutMs": WAIT_MS});
                    if let Some(turn) = &route.turn_id {
                        ask["turnId"] = turn.clone().into();
                    } else if let Some(message) = &route.message_id {
                        ask["messageId"] = message.clone().into();
                    }
                    let within = Duration::from_millis(WAIT_MS) + Duration::from_secs(30);
                    match guarded_with_timeout(&client, ask, within).await {
                        Ok(answer) if answer["status"] == "timeout" => {}
                        Ok(answer) => break answer,
                        Err(_) => tokio::time::sleep(RETRY).await,
                    }
                }
            }
        };
        let reason = reason(&answer);
        let mut notices: Vec<(String, Value)> = Vec::new();
        if reason == "needs permission" {
            let pending: Vec<Value> = (answer["final"]["pendingPermissions"].as_array())
                .cloned()
                .unwrap_or_default();
            for request in pending {
                let id = request["id"].as_str().unwrap_or_default().to_string();
                if !id.is_empty() && !route.told.contains(&id) {
                    notices.push((format!("{}:permission:{id}", route.route_id), request));
                }
            }
            if notices.is_empty() {
                tokio::time::sleep(PERMISSION_POLL).await;
                continue;
            }
        } else {
            notices.push((format!("{}:end", route.route_id), Value::Null));
        }
        let title = (answer["final"]["title"].as_str())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(&route.target_title)
            .to_string();
        for (notice_id, permission) in notices {
            let notice = json!({
                "type": FINISHED, "noticeId": notice_id,
                "callerAgentId": route.caller_agent_id, "agentId": route.target_agent_id,
                "title": title, "serverId": route.target_server_id, "host": route.target_host,
                "reason": reason, "lastMessage": answer["lastMessage"], "permission": permission,
            });
            match tell(&apply, &route.source_host, notice).await {
                Told::Delivered => {}
                Told::Gone => {
                    let id = route.route_id.clone();
                    let _ = apply.send(Box::new(move |up: &mut Upstream| up.route_done(&id)));
                    return;
                }
            }
            if let Some(id) = permission["id"].as_str() {
                route.told.push(id.to_string());
            }
        }
        if reason != "needs permission" {
            let id = route.route_id.clone();
            let _ = apply.send(Box::new(move |up: &mut Upstream| up.route_done(&id)));
            return;
        }
        let (id, told) = (route.route_id.clone(), route.told.clone());
        let _ = apply.send(Box::new(move |up: &mut Upstream| up.route_told(&id, told)));
    }
}

enum Told {
    Delivered,
    /// The sender, or its host, is gone: nothing more to tell.
    Gone,
}

/// Sends a notice to the sender's Burrow until it has it.
async fn tell(apply: &mpsc::UnboundedSender<Apply>, host: &str, notice: Value) -> Told {
    let mut refused = 0;
    loop {
        match link(apply, host).await {
            LinkNow::Gone => return Told::Gone,
            LinkNow::Down => {}
            LinkNow::Up(client) => {
                match guarded_with_timeout(&client, notice.clone(), Duration::from_secs(60)).await {
                    Ok(answer) if answer["delivered"] == true => return Told::Delivered,
                    Ok(answer) if answer["gone"] == true => return Told::Gone,
                    Ok(answer) => {
                        refused += 1;
                        if refused >= DELIVERY_TRIES {
                            eprintln!(
                                "reef: giving up on a routed notice for {host}: {}",
                                answer["error"]
                            );
                            return Told::Gone;
                        }
                    }
                    // Not connected, or an older Burrow: try again later.
                    Err(_) => {}
                }
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pair_of_agents_is_held_to_its_rate() {
        let mut routes = Routes::default();
        for _ in 0..RATE_LIMIT {
            assert!(routes.allow("a", "b"));
        }
        assert!(!routes.allow("a", "b"));
        assert!(routes.allow("a", "c"), "another pair has its own count");
    }

    #[test]
    fn the_reason_follows_how_the_wait_ended() {
        let wait = |status: &str, last: Value| json!({"status": status, "final": last});
        assert_eq!(reason(&wait("idle", json!({"status": "idle"}))), "finished");
        assert_eq!(
            reason(&wait("error", json!({"status": "error"}))),
            "errored"
        );
        assert_eq!(
            reason(&wait("permission", json!({"status": "running"}))),
            "needs permission"
        );
        assert_eq!(
            reason(&wait("idle", json!({"status": "closed"}))),
            "was closed"
        );
        assert_eq!(reason(&wait("error", Value::Null)), "is unavailable");
    }
}
