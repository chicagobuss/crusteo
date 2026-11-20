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
/// The most the Reef waits for a tool that waits: the daemon's own longest
/// wait (a wait_for_finish without its own timeout), the cap on a client's.
const TOOL_WITHIN_MAX: Duration = Duration::from_secs(24 * 3600);

/// How long the Reef waits for this tool request: the 20 seconds for the
/// reads, the client's own for a waiting tool, capped at [`TOOL_WITHIN_MAX`].
fn tool_within(payload: &Value) -> Duration {
    (payload["timeoutMs"].as_u64())
        .map(Duration::from_millis)
        .unwrap_or(TOOL_WITHIN)
        .min(TOOL_WITHIN_MAX)
}

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

impl Route {
    /// The same sender and the same target: one route at a time between
    /// them, as Burrow keeps one finish subscription per caller and child.
    fn same_pair(&self, other: &Route) -> bool {
        self.pair() == other.pair()
    }

    fn pair(&self) -> Pair {
        (
            self.source_host.clone(),
            self.caller_agent_id.clone(),
            self.target_host.clone(),
            self.target_agent_id.clone(),
        )
    }
}

/// Sender host, sender, target host, target.
type Pair = (String, String, String, String);

/// Routes under way and how often each pair of agents routed lately.
#[derive(Default)]
pub struct Routes {
    pub on: bool,
    path: Option<PathBuf>,
    pub under_way: Vec<Route>,
    sent: HashMap<(String, String), VecDeque<Instant>>,
    /// The turn each pair was last told the end of: a prompt steered into
    /// a turn that ends at once can leave two routes waiting on it.
    told_turn: HashMap<Pair, String>,
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

/// Has Reef's state keep `route`, saved, and follow it; returns once it is
/// in `routes.json`, or Reef is stopping. `replaces` drops the pair's route
/// under way: the new prompt joined or cut off the turn that one waits for.
async fn follow_recorded(apply: &mpsc::UnboundedSender<Apply>, route: Route, replaces: bool) {
    let (tx, rx) = oneshot::channel();
    let asked = apply.send(Box::new(move |up: &mut Upstream| {
        if replaces {
            up.keep_route(route.clone(), true);
        }
        up.follow_route(route, !replaces);
        let _ = tx.send(());
    }));
    if asked.is_ok() {
        let _ = rx.await;
    }
}

/// Whether `route` is still the one to tell: nothing replaced or ended it,
/// and, for its turn's end (`ending`), no route between the same two agents
/// was told that turn's end already. Claims that turn for it when so.
async fn still_to_tell(apply: &mpsc::UnboundedSender<Apply>, route: &Route, ending: bool) -> bool {
    let (tx, rx) = oneshot::channel();
    let route = route.clone();
    let asked = apply.send(Box::new(move |up: &mut Upstream| {
        let _ = tx.send(up.claim(&route, ending));
    }));
    asked.is_ok() && rx.await.unwrap_or(false)
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
        // Under way again, so the next save keeps them and each is told.
        self.routes.under_way = saved.clone();
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
        // A caller that named a host wants its agent from there only.
        let server = (payload["serverId"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (target_host, agent) = match self.find_agent(key, server) {
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
        // Replacing a turn waits for it to end: the sender hears at once,
        // once the route is kept, so a Reef stopped now still tells it.
        let interrupting = behavior == "interrupt";
        if interrupting {
            // The turn the pair's earlier route waits for is being cut off:
            // its end is not the sender's news.
            if let Some(route) = &route {
                self.keep_route(route.clone(), true);
            }
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
            if interrupting {
                match (outcome, route) {
                    (Ok(_), Some(mut route)) => {
                        route.turn_id = turn;
                        follow_recorded(&apply, route, false).await;
                    }
                    (Ok(_), None) => {}
                    (Err(refusal), route) => {
                        eprintln!(
                            "reef: a routed interrupt was not delivered: {}",
                            refusal.error
                        );
                        if let Some(route) = route {
                            // The sender was told "interrupting" and waits
                            // for a notice: it hears this one instead.
                            let notice = json!({
                                "type": FINISHED, "noticeId": format!("{}:end", route.route_id),
                                "callerAgentId": route.caller_agent_id,
                                "agentId": route.target_agent_id, "title": route.target_title,
                                "serverId": route.target_server_id, "host": route.target_host,
                                "reason": "errored", "lastMessage": Value::Null,
                                "permission": Value::Null, "error": refusal.error,
                            });
                            let _ = tell(&apply, &route.source_host, notice).await;
                            let _ = apply.send(Box::new(move |up: &mut Upstream| {
                                up.route_done(&route.route_id)
                            }));
                        }
                    }
                }
                return;
            }
            let answer = outcome.map(|reply| {
                let mut receipt = receipt;
                receipt["disposition"] =
                    (reply["disposition"].as_str()).unwrap_or("started").into();
                if let Some(position) = reply["queuePosition"].as_u64() {
                    receipt["queuePosition"] = position.into();
                }
                receipt
            });
            // The route is on disk before the sender hears: a Reef stopped
            // in between would otherwise lose the notice it is owed. One that
            // started a turn or steered into one replaces the pair's earlier
            // route; a queued one waits behind it, and each turn is told.
            if let (Ok(receipt), Some(mut route)) = (&answer, route) {
                route.turn_id = turn;
                let replaces = receipt["disposition"] != "queued";
                follow_recorded(&apply, route, replaces).await;
            }
            source.send(response(&request_id, answer));
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
            let mut lists: Vec<Vec<Value>> = Vec::new();
            let mut hosts: Vec<Value> = Vec::new();
            for (name, server, answer) in answers {
                match answer {
                    Ok(listed) => {
                        let mut list: Vec<Value> =
                            (listed["agents"].as_array().cloned()).unwrap_or_default();
                        for agent in &mut list {
                            agent["host"] = name.clone().into();
                            // A full snapshot keeps its host's server id; a
                            // compact one stays compact.
                            if agent["updatedAt"].is_string() {
                                agent["serverId"] = server.clone().into();
                            }
                        }
                        if !list.is_empty() {
                            lists.push(list);
                        }
                        hosts.push(json!({"host": name, "serverId": server, "answered": true}));
                    }
                    Err(error) => hosts.push(json!({"host": name, "serverId": server,
                                                    "answered": false, "error": error})),
                }
            }
            let mut agents = merge_listed(lists);
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
            match self.find_agent(key.trim(), None) {
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
            match self.hosts_named(server).first() {
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
        let within = tool_within(payload);
        tokio::spawn(async move {
            let ask = json!({"type": TOOL_REQUEST, "tool": tool, "args": args});
            let outcome = tool_answer(guarded_with_timeout(&client, ask, within).await)
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

    /// The hosts `key` names, in any case: by the name Reef has for one or
    /// its server id, else by the hostname its server reports. These are
    /// the ways a Burrow takes a `serverId` for itself
    /// (`Config::names_this_host`).
    fn hosts_named(&self, key: &str) -> Vec<&fleet::Host> {
        let key = key.trim();
        if key.is_empty() {
            return Vec::new();
        }
        let hosts = || self.fleet.hosts.iter();
        let named: Vec<_> = hosts()
            .filter(|host| {
                host.name().eq_ignore_ascii_case(key)
                    || (host.server.as_ref()).is_some_and(|s| s.server_id.eq_ignore_ascii_case(key))
            })
            .collect();
        if !named.is_empty() {
            return named;
        }
        hosts()
            .filter(|host| {
                (host.server.as_ref()).is_some_and(|s| s.hostname.eq_ignore_ascii_case(key))
            })
            .collect()
    }

    /// The agent `key` names, by id, a unique id prefix (seven or more
    /// characters) or its name, on any host, or only on the host `server`
    /// names ([`Self::hosts_named`]) when it is given.
    fn find_agent(
        &self,
        key: &str,
        server: Option<&str>,
    ) -> Result<(String, paseo_client::proto::Agent), Refusal> {
        let hosts = match server {
            Some(server) => {
                let named = self.hosts_named(server);
                if named.is_empty() {
                    return Err(Refusal::new(
                        code::NOT_FOUND,
                        format!("No host in the fleet is {server}."),
                    ));
                }
                named
            }
            None => self.fleet.hosts.iter().collect(),
        };
        let all = || {
            (hosts.iter()).flat_map(|host| {
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
                match server {
                    Some(server) => format!("No agent on {server} goes by {key}."),
                    None => format!("No agent in the fleet goes by {key}."),
                },
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
            self.keep_route(route.clone(), false);
        }
        tokio::spawn(follow(self.apply.clone(), route));
    }

    /// Keeps `route` among those under way, in `routes.json` by the time
    /// this returns; one with its id already there is replaced, and with
    /// `replace_pair` so is the route between the same two agents that waits
    /// for a turn (the one joined or cut off). A queued prompt's route, still
    /// waiting by its message, is its own turn's and stays.
    fn keep_route(&mut self, route: Route, replace_pair: bool) {
        self.routes.under_way.retain(|r| {
            r.route_id != route.route_id
                && !(replace_pair && r.turn_id.is_some() && r.same_pair(&route))
        });
        self.routes.under_way.push(route);
        self.routes.save();
    }

    fn route_told(&mut self, route_id: &str, told: Vec<String>) {
        if let Some(route) = (self.routes.under_way.iter_mut()).find(|r| r.route_id == route_id) {
            route.told = told;
            self.routes.save();
        }
    }

    /// See [`still_to_tell`]. A route whose turn's end was told already is
    /// done.
    fn claim(&mut self, route: &Route, ending: bool) -> bool {
        if !(self.routes.under_way.iter()).any(|r| r.route_id == route.route_id) {
            return false;
        }
        if let (true, Some(turn)) = (ending, &route.turn_id) {
            let pair = route.pair();
            if self.routes.told_turn.get(&pair) == Some(turn) {
                self.route_done(&route.route_id);
                return false;
            }
            self.routes.told_turn.insert(pair, turn.clone());
        }
        true
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

/// Each host's list is already in Paseo's `selectAgentListItems` order —
/// attention first, then the status, then the most recently updated — so
/// the fleet's list is the merge of them. A compact list has no times, so
/// its own order stands; equal ones take turns, host by host.
fn merge_listed(lists: Vec<Vec<Value>>) -> Vec<Value> {
    let count = lists.len();
    let mut lists = lists;
    let mut merged = Vec::new();
    let mut turn = 0usize;
    loop {
        let mut best: Option<usize> = None;
        for offset in 0..count {
            let i = (turn + offset) % count;
            let Some(head) = lists[i].first() else {
                continue;
            };
            match best {
                None => best = Some(i),
                Some(b) if before(head, &lists[b][0]) => best = Some(i),
                _ => {}
            }
        }
        let Some(i) = best else {
            break;
        };
        let item = lists[i].remove(0);
        turn = (i + 1) % count;
        merged.push(item);
    }
    merged
}

/// `a` sorts strictly before `b` in Paseo's list order. Burrows write every
/// time the same way (RFC 3339, UTC), so times compare as text; a missing
/// time (a compact item) compares equal.
fn before(a: &Value, b: &Value) -> bool {
    let key = |v: &Value| {
        (
            v["requiresAttention"] == true,
            match v["status"].as_str() {
                Some("running") => 0,
                Some("initializing") => 1,
                Some("idle") => 2,
                Some("error") => 3,
                _ => 4,
            },
            v["updatedAt"].as_str().unwrap_or_default().to_string(),
        )
    };
    let (a_attention, a_rank, a_updated) = key(a);
    let (b_attention, b_rank, b_updated) = key(b);
    if a_attention != b_attention {
        return a_attention;
    }
    if a_rank != b_rank {
        return a_rank < b_rank;
    }
    a_updated > b_updated
}

/// Why the prompt the wait is for never ran, when the answer says so: a
/// queued prompt that a stop dropped has an `error` and the agent, but no
/// turn. A turn that failed has an `error` too, but also its turn, and its
/// prompt was delivered; an agent that is gone has neither agent nor turn.
fn never_ran(answer: &Value) -> Option<&str> {
    (answer["error"].as_str()).map(str::trim).filter(|error| {
        !error.is_empty() && !answer["final"].is_null() && answer["turnId"].is_null()
    })
}

/// Paseo's `reasonFor`, as Burrow's finish notices say it.
fn reason(answer: &Value) -> &'static str {
    let last = &answer["final"];
    if last.is_null() {
        return "is unavailable";
    }
    // Before the agent's own state: a stop that dropped the prompt closed it.
    if never_ran(answer).is_some() {
        return "errored";
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
        // A queued prompt's turn, once known: a turn told by another route
        // (one steered into it) is not told again.
        if route.turn_id.is_none() {
            route.turn_id = answer["turnId"].as_str().map(str::to_string);
        }
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
        // A newer prompt between the same two agents took this one's place
        // while it waited, or another route already told this turn's end:
        // the notice is not this one's to give.
        if !still_to_tell(&apply, &route, reason != "needs permission").await {
            return;
        }
        let title = (answer["final"]["title"].as_str())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(&route.target_title)
            .to_string();
        for (notice_id, permission) in notices {
            // `error` tells the sender its prompt was not delivered.
            let notice = json!({
                "type": FINISHED, "noticeId": notice_id,
                "callerAgentId": route.caller_agent_id, "agentId": route.target_agent_id,
                "title": title, "serverId": route.target_server_id, "host": route.target_host,
                "reason": reason, "lastMessage": answer["lastMessage"], "permission": permission,
                "error": never_ran(&answer),
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
    fn a_clients_tool_timeout_is_capped_at_the_daemons_longest_wait() {
        // No timeout, or one the client did not give as milliseconds: the
        // Reef's 20 seconds.
        assert_eq!(tool_within(&json!({})), TOOL_WITHIN);
        assert_eq!(tool_within(&json!({"timeoutMs": "soon"})), TOOL_WITHIN);
        // A waiting tool passes its own, up to the daemon's longest wait.
        assert_eq!(
            tool_within(&json!({"timeoutMs": 5000})),
            Duration::from_millis(5000)
        );
        assert_eq!(
            tool_within(&json!({"timeoutMs": 86_400_000})),
            TOOL_WITHIN_MAX
        );
        assert_eq!(
            tool_within(&json!({"timeoutMs": 86_400_000 * 10})),
            TOOL_WITHIN_MAX
        );
    }

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

    #[test]
    fn a_prompt_a_stop_dropped_is_told_as_not_delivered_not_as_a_closed_agent() {
        // What Burrow answers a wait on a queued prompt that never got a turn:
        // an error, and the agent, closed by the stop that dropped it.
        let dropped = json!({
            "status": "error", "final": {"status": "closed"}, "lastMessage": null,
            "turnId": null, "error": "The queued prompt was dropped: the agent was stopped",
        });
        assert_eq!(reason(&dropped), "errored");
        assert_eq!(
            never_ran(&dropped),
            Some("The queued prompt was dropped: the agent was stopped")
        );
        // A turn that failed was delivered: its error is not news of that, and
        // an agent closed after it is still reported as closed.
        let failed = json!({
            "status": "error", "final": {"status": "closed"}, "lastMessage": "partial",
            "turnId": "t1", "error": "The provider crashed",
        });
        assert_eq!(never_ran(&failed), None);
        assert_eq!(reason(&failed), "was closed");
        // An agent that is gone says nothing of its prompt either.
        let gone = json!({"status": "error", "final": null, "turnId": null,
                          "error": "Agent not found: a1"});
        assert_eq!(never_ran(&gone), None);
        assert_eq!(reason(&gone), "is unavailable");
        assert_eq!(never_ran(&json!({"status": "idle", "final": {}})), None);
    }

    /// A host for [`fleet_of`]: its name, server id, reported hostname, and
    /// agents as (id, title).
    type Seed<'a> = (&'a str, &'a str, &'a str, &'a [(&'a str, &'a str)]);

    /// An upstream with `hosts`.
    async fn fleet_of(hosts: &[Seed<'_>]) -> Upstream {
        let (events, _events) = mpsc::channel(8);
        let (apply, _apply) = mpsc::unbounded_channel();
        let configs = (hosts.iter())
            .map(|(name, ..)| fleet::HostConfig {
                id: host_id(name),
                name: name.to_string(),
                endpoint: None,
                password: None,
            })
            .collect();
        let mut up = Upstream::new(configs, "cid_test".into(), events, apply);
        for (host, (_, server_id, hostname, agents)) in up.fleet.hosts.iter_mut().zip(hosts) {
            host.server = Some(paseo_client::ServerInfo {
                server_id: server_id.to_string(),
                hostname: hostname.to_string(),
                ..Default::default()
            });
            for (id, title) in *agents {
                host.directory.agents.insert(
                    id.to_string(),
                    paseo_client::proto::Agent {
                        id: id.to_string(),
                        title: Some(title.to_string()),
                        ..Default::default()
                    },
                );
            }
        }
        up
    }

    #[test]
    fn the_fleets_list_is_the_merge_of_the_hosts_lists() {
        let item = |id: &str, status: &str, updated: Option<&str>, attention: bool| {
            let mut item = json!({"id": id, "status": status, "requiresAttention": attention});
            if let Some(updated) = updated {
                item["updatedAt"] = updated.into();
            }
            item
        };
        // With times: attention first, then the status, then the newest —
        // whichever host it is on.
        let a = vec![
            item("a-ask", "running", Some("2026-11-08T10:00:00Z"), true),
            item("a-run", "running", Some("2026-11-08T09:00:00Z"), false),
            item("a-idle", "idle", Some("2026-11-08T09:30:00Z"), false),
        ];
        let b = vec![
            item("b-run", "running", Some("2026-11-08T09:45:00Z"), false),
            item("b-idle", "idle", Some("2026-11-08T09:50:00Z"), false),
        ];
        let merged = merge_listed(vec![a, b]);
        let ids = |merged: &[Value]| -> Vec<String> {
            (merged.iter())
                .map(|v| v["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            ids(&merged),
            vec!["a-ask", "b-run", "a-run", "b-idle", "a-idle"]
        );
        // Compact items have no times: their own order stands, and the
        // equal ones take turns, host by host.
        let a = vec![
            item("a-1", "idle", None, true),
            item("a-2", "running", None, false),
        ];
        let b = vec![
            item("b-1", "idle", None, true),
            item("b-2", "running", None, false),
            item("b-3", "idle", None, false),
        ];
        assert_eq!(
            ids(&merge_listed(vec![a, b])),
            vec!["a-1", "b-1", "a-2", "b-2", "b-3"]
        );
        // One host alone is its own list, in the order its host gave it.
        let one = vec![
            item("x-1", "running", Some("2026-11-08T09:00:00Z"), false),
            item("x-2", "idle", Some("2026-11-08T08:00:00Z"), false),
        ];
        assert_eq!(ids(&merge_listed(vec![one])), vec!["x-1", "x-2"]);
        assert_eq!(merge_listed(Vec::new()), Vec::<Value>::new());
    }

    #[tokio::test]
    async fn a_host_is_found_by_name_server_id_or_reported_hostname_in_any_case() {
        let up = fleet_of(&[
            ("buildbox", "srv_build", "ci.example.com", &[]),
            ("devbox", "srv_dev", "Sam-Laptop", &[]),
            // A reported hostname that is another host's name does not win.
            ("spare", "srv_spare", "buildbox", &[]),
        ])
        .await;
        let names = |key: &str| -> Vec<String> {
            (up.hosts_named(key).iter())
                .map(|host| host.name().to_string())
                .collect()
        };
        assert_eq!(names("devbox"), ["devbox"]);
        assert_eq!(names("DevBox"), ["devbox"]);
        assert_eq!(names("SRV_DEV"), ["devbox"]);
        assert_eq!(names(" sam-laptop "), ["devbox"]);
        assert_eq!(names("ci.example.com"), ["buildbox"]);
        assert_eq!(names("buildbox"), ["buildbox"]);
        assert!(names("nowhere").is_empty());
        assert!(names("").is_empty() && names("  ").is_empty());
    }

    #[tokio::test]
    async fn an_agent_is_looked_for_only_on_the_host_a_caller_names() {
        let up = fleet_of(&[
            (
                "buildbox",
                "srv_build",
                "ci.example.com",
                &[("agent-1111111", "worker"), ("agent-2222222", "docs")],
            ),
            (
                "devbox",
                "srv_dev",
                "sam-laptop",
                &[("agent-3333333", "Worker")],
            ),
            ("spare", "srv_spare", "spare", &[]),
        ])
        .await;
        let found = |key: &str, server: Option<&str>| {
            up.find_agent(key, server)
                .ok()
                .map(|(host, agent)| (host, agent.id))
        };
        let refused = |key: &str, server: Option<&str>| {
            let refusal = up.find_agent(key, server).unwrap_err();
            (refusal.code, refusal.error)
        };

        // On no host named, the name is on two: which one is not guessed.
        assert_eq!(refused("worker", None).0, code::AMBIGUOUS);
        // Where it is named, only that host is looked at, whichever way.
        for server in ["devbox", "SRV_DEV", "Sam-Laptop"] {
            assert_eq!(
                found("worker", Some(server)),
                Some(("devbox".into(), "agent-3333333".into())),
                "{server}"
            );
        }
        assert_eq!(
            found("WORKER", Some("srv_build")),
            Some(("buildbox".into(), "agent-1111111".into()))
        );
        // By id too, but only on that host: another's agent is not found there.
        assert_eq!(
            found("agent-2222222", Some("buildbox")),
            Some(("buildbox".into(), "agent-2222222".into()))
        );
        assert_eq!(
            refused("agent-2222222", Some("devbox")),
            (
                code::NOT_FOUND,
                "No agent on devbox goes by agent-2222222.".to_string()
            )
        );
        assert_eq!(
            refused("worker", Some("spare")),
            (
                code::NOT_FOUND,
                "No agent on spare goes by worker.".to_string()
            )
        );
        // A host that is not one of the fleet's.
        assert_eq!(
            refused("worker", Some("srv_nowhere")),
            (
                code::NOT_FOUND,
                "No host in the fleet is srv_nowhere.".to_string()
            )
        );
        // As before without a host: an agent found anywhere.
        assert_eq!(
            found("docs", None),
            Some(("buildbox".into(), "agent-2222222".into()))
        );
        assert_eq!(
            refused("nobody", None),
            (
                code::NOT_FOUND,
                "No agent in the fleet goes by nobody.".to_string()
            )
        );
    }
}
