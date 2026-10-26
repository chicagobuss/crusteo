//! Making and unmaking agents from a phone: the new-agent form's choices
//! (each host's providers, read once and kept a while, and its
//! workspaces), creating an agent, deleting one, and relabelling one.
//!
//! With Reef's label catalog (always, outside tests) an agent's labels are
//! its own, kept under one key of Paseo's per-agent labels
//! ([`fleet::labels`]); an agent without that key inherits its workspace's
//! Paseo labels. Relabelling writes that key and nothing else, the one
//! `update_agent_request` the guard lets through. Without a catalog,
//! relabelling assigns the workspace's labels, one request per change.

use std::sync::Arc;
use std::time::{Duration, Instant};

use fleet::directory::label_key;
use fleet::labels::LabelId;
use fleet::prompt::Prompt;
use fleet::providers::Provider;
use fleet::requests;
use serde_json::{Value, json};

use super::actions::Reply;
use super::{Upstream, guarded};
use crate::config::host_id;
use crate::proto::code;

/// A providers snapshot is kept this long; models and modes rarely change.
const PROVIDERS_FRESH: Duration = Duration::from_secs(600);
/// While a provider is still starting up, the snapshot is read again
/// after this.
const PROVIDERS_LOADING: Duration = Duration::from_secs(5);
/// Writes can take a while on a busy daemon: creating an agent starts its
/// provider. The reference client allows a minute.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Workspaces a new-agent form offers, newest activity first.
const WORKSPACES_MAX: usize = 200;

type Waiter = Box<dyn FnOnce(&mut Upstream, Result<Arc<Vec<Provider>>, String>) + Send>;

/// One host's providers: being read, with phones waiting, or read.
pub(super) enum Providers {
    Fetching(Vec<Waiter>),
    Ready {
        list: Arc<Vec<Provider>>,
        at: Instant,
    },
}

/// An agent a phone asked for, checked against the directory; the
/// provider, model, mode and thinking are checked against the host's
/// providers before anything is sent.
#[derive(Debug, Clone, PartialEq)]
pub struct NewAgent {
    pub provider: String,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub thinking: Option<String>,
    /// A known workspace to create it in; without one, the daemon finds or
    /// makes the workspace for `cwd`.
    pub workspace: Option<String>,
    pub cwd: String,
    /// Its labels, by name; None takes its workspace's default. Without a
    /// catalog they are added to its workspace once it exists.
    pub labels: Option<Vec<String>>,
    pub prompt: Prompt,
    pub message_id: String,
}

/// A failure to tell the phone: an error code and why.
type Refusal = (&'static str, String);

/// The agent's config as `agent.create.request` takes it, with the
/// provider's defaults for whatever the phone left out, as crusteo's
/// new-agent draft picks them.
fn config(providers: &[Provider], new: &NewAgent, host: &str) -> Result<Value, Refusal> {
    let provider = (providers.iter())
        .find(|p| p.id == new.provider)
        .ok_or_else(|| {
            let error = format!("{host} has no provider {}", new.provider);
            (code::NOT_FOUND, error)
        })?;
    let name = if provider.label.is_empty() {
        &provider.id
    } else {
        &provider.label
    };
    if !provider.ready() {
        let why = if provider.enabled {
            provider.status.as_str()
        } else {
            "disabled"
        };
        let error = format!("{name} is not ready on {host} ({why})");
        return Err((code::BAD_REQUEST, error));
    }
    let model = match &new.model {
        Some(id) => Some(
            (provider.models.iter())
                .find(|m| &m.id == id)
                .ok_or_else(|| (code::BAD_REQUEST, format!("{name} has no model {id}")))?,
        ),
        None => provider.default_model(),
    };
    let thinking = match (&new.thinking, model) {
        (Some(id), Some(model)) if model.thinking.iter().any(|c| &c.id == id) => Some(id.clone()),
        (Some(id), _) => {
            let error = format!("That model has no thinking option {id}");
            return Err((code::BAD_REQUEST, error));
        }
        (None, model) => model.and_then(|m| m.default_thinking.clone()),
    };
    let mode = match &new.mode {
        Some(id) if provider.modes.iter().any(|m| &m.id == id) => Some(id.clone()),
        Some(id) => return Err((code::BAD_REQUEST, format!("{name} has no mode {id}"))),
        None => provider.default_mode.clone(),
    };
    let mut config = json!({ "provider": provider.id, "cwd": new.cwd });
    if let Some(model) = model {
        config["model"] = json!(model.id);
    }
    if let Some(thinking) = thinking {
        config["thinkingOptionId"] = json!(thinking);
    }
    if let Some(mode) = mode {
        config["modeId"] = json!(mode);
    }
    Ok(config)
}

/// A host's providers as a new-agent form shows them: the ready ones with
/// their models, modes and thinking options, the others by name and why.
pub fn providers_json(providers: &[Provider]) -> Vec<Value> {
    let choices = |list: &[fleet::providers::Choice]| -> Vec<Value> {
        (list.iter())
            .map(|c| json!({ "id": c.id, "label": c.label }))
            .collect()
    };
    (providers.iter())
        .map(|p| {
            let label = if p.label.is_empty() { &p.id } else { &p.label };
            if !p.ready() {
                let status = if p.enabled {
                    p.status.as_str()
                } else {
                    "disabled"
                };
                return json!({ "id": p.id, "label": label, "ready": false, "status": status });
            }
            let models: Vec<Value> = (p.models.iter())
                .map(|m| {
                    let label = if m.label.is_empty() { &m.id } else { &m.label };
                    let mut model = json!({ "id": m.id, "label": label });
                    if m.is_default {
                        model["default"] = json!(true);
                    }
                    if !m.thinking.is_empty() {
                        model["thinking"] = json!(choices(&m.thinking));
                        model["thinking_default"] = json!(m.default_thinking);
                    }
                    model
                })
                .collect();
            json!({
                "id": p.id, "label": label, "ready": true,
                "modes": choices(&p.modes), "mode": p.default_mode, "models": models,
            })
        })
        .collect()
}

/// A host's workspaces as a new-agent form offers them, the most recently
/// active first: where its agents last did something, else never.
pub fn workspaces_json(host: &fleet::Host, catalog: Option<&fleet::labels::Catalog>) -> Vec<Value> {
    let directory = &host.directory;
    let mut newest: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for agent in directory.agents.values() {
        if let (Some(workspace), Some(at)) = (&agent.workspace_id, &agent.updated_at) {
            let slot = newest.entry(workspace.as_str()).or_default();
            if *slot < at.as_str() {
                *slot = at;
            }
        }
    }
    let mut workspaces: Vec<&fleet::Workspace> = (directory.workspaces.values())
        .filter(|w| w.archiving_at.is_none())
        .collect();
    workspaces.sort_by(|a, b| {
        let at = |w: &fleet::Workspace| newest.get(w.id.as_str()).copied().unwrap_or("");
        at(b)
            .cmp(at(a))
            .then_with(|| crate::lists::workspace_name(a).cmp(crate::lists::workspace_name(b)))
    });
    (workspaces.into_iter())
        .take(WORKSPACES_MAX)
        .map(|w| {
            let project = (directory.projects.get(&w.project_id))
                .map(|p| p.name())
                .unwrap_or_else(|| w.project_display_name.clone());
            // With a catalog, the labels a new agent there starts with.
            let labels: Vec<String> = match catalog {
                Some(catalog) => {
                    let agents = (directory.agents.values())
                        .filter(|agent| agent.workspace_id.as_deref() == Some(w.id.as_str()));
                    (fleet::labels::workspace_default(agents, w, catalog).iter())
                        .filter_map(|id| catalog.get(id).map(|label| label.name.clone()))
                        .collect()
                }
                None => w.labels.clone(),
            };
            json!({
                "id": w.id, "name": crate::lists::workspace_name(w), "project": project,
                "directory": w.directory(), "labels": labels,
            })
        })
        .collect()
}

impl Upstream {
    /// Hands `done` the host's providers: kept ones while fresh, else read
    /// once however many phones ask meanwhile.
    pub(super) fn with_providers(
        &mut self,
        host: &str,
        done: impl FnOnce(&mut Upstream, Result<Arc<Vec<Provider>>, String>) + Send + 'static,
    ) {
        match self.providers.get_mut(host) {
            Some(Providers::Ready { list, at }) => {
                let loading = list.iter().any(|p| p.enabled && p.status == "loading");
                let fresh = if loading {
                    PROVIDERS_LOADING
                } else {
                    PROVIDERS_FRESH
                };
                if at.elapsed() < fresh {
                    let list = list.clone();
                    return done(self, Ok(list));
                }
            }
            Some(Providers::Fetching(waiters)) => {
                waiters.push(Box::new(done));
                return;
            }
            None => {}
        }
        let Some(link) = self.links.get(host).filter(|link| link.online) else {
            return done(self, Err("The host is offline".into()));
        };
        let generation = link.generation;
        let reply = guarded(&link.client, requests::providers_snapshot());
        self.providers
            .insert(host.to_string(), Providers::Fetching(vec![Box::new(done)]));
        let apply = self.apply.clone();
        let host = host.to_string();
        tokio::spawn(async move {
            let result = reply.await.map(|payload| fleet::providers::parse(&payload));
            let _ = apply.send(Box::new(move |up: &mut Upstream| {
                let waiters = match up.providers.remove(&host) {
                    Some(Providers::Fetching(waiters)) => waiters,
                    Some(ready) => {
                        up.providers.insert(host.clone(), ready);
                        Vec::new()
                    }
                    None => Vec::new(),
                };
                let result = result.map(Arc::new);
                // Kept only for the connection that read them.
                let same = (up.links.get(&host)).is_some_and(|l| l.generation == generation);
                if let (Ok(list), true) = (&result, same) {
                    let at = Instant::now();
                    let list = list.clone();
                    up.providers.insert(host, Providers::Ready { list, at });
                }
                for waiter in waiters {
                    waiter(up, result.clone());
                }
            }));
        });
    }

    /// Forgets a host's providers, for a host that reconnected or went.
    /// A read in flight still answers the phones waiting on it.
    pub(super) fn forget_providers(&mut self, host: &str) {
        if matches!(self.providers.get(host), Some(Providers::Ready { .. })) {
            self.providers.remove(host);
        }
    }

    /// The new-agent form's choices for one host: its providers and its
    /// workspaces. The label set is the fleet's, which phones already have.
    pub fn create_options(&mut self, host: &str, reply: Reply) {
        let name = host.to_string();
        self.with_providers(host, move |up, result| match result {
            Ok(providers) => {
                let Some(host) = up.fleet.host(&host_id(&name)) else {
                    return up.refuse(&reply, code::NOT_FOUND, "no such host");
                };
                let options = json!({
                    "host": name,
                    "providers": providers_json(&providers),
                    "workspaces": workspaces_json(host, up.fleet.labels.as_ref()),
                    "labels": host.has("workspaceLabels"),
                });
                up.answer(&reply, Ok(options));
            }
            Err(error) => up.answer(&reply, Err(error)),
        });
    }

    /// Creates an agent, then gives its workspace the labels asked for. The
    /// answer names it, so the phone can open its conversation.
    pub fn create(&mut self, host: &str, new: NewAgent, reply: Reply) {
        let name = host.to_string();
        self.with_providers(host, move |up, result| {
            let config = match result {
                Ok(providers) => config(&providers, &new, &name),
                Err(error) => Err((code::UPSTREAM, error)),
            };
            match config {
                Ok(config) => up.create_now(&name, new, config, reply),
                Err((code, error)) => up.refuse(&reply, code, &error),
            }
        });
    }

    fn create_now(&mut self, host: &str, new: NewAgent, config: Value, reply: Reply) {
        // With a catalog the agent carries its labels from the start; the
        // workspace is left alone.
        let reef_mode = self.fleet.labels.is_some();
        let initial = match reef_mode {
            true => self.initial_labels(host, new.workspace.as_deref(), new.labels.as_deref()),
            false => None,
        };
        let labels = match reef_mode {
            true => Vec::new(),
            false => new.labels.unwrap_or_default(),
        };
        let prompt = Some(new.prompt).filter(|p| !p.is_empty());
        let mut message =
            requests::create_agent(new.workspace.as_deref(), config, prompt, &new.message_id);
        if let Some(ids) = &initial {
            message["labels"] = json!({ fleet::labels::KEY: fleet::labels::encode(ids) });
        }
        let name = host.to_string();
        self.write(host, message, WRITE_TIMEOUT, move |up, result| {
            let created = result.and_then(|payload| {
                let agent = payload["agent"].clone();
                match agent["id"].as_str() {
                    Some(_) => Ok(agent),
                    None => Err("The host did not return an agent".to_string()),
                }
            });
            let agent = match created {
                Ok(agent) => agent,
                Err(error) => return up.answer(&reply, Err(error)),
            };
            let id = agent["id"].as_str().unwrap_or_default().to_string();
            // The directory push may come after this answer; the phone
            // opens the conversation at once, so the agent must be there.
            if up.fleet.agent(&host_id(&name), &id).is_none() {
                let push =
                    json!({"type": "agent_update", "payload": {"kind": "upsert", "agent": agent}});
                up.on_push(&name, push);
            }
            let answer =
                json!({ "host": name, "agent": id, "key": crate::views::agent_key(&name, &id) });
            let workspace = agent["workspaceId"].as_str().map(str::to_string);
            let current = (workspace.as_deref())
                .and_then(|w| up.fleet.workspace(&host_id(&name), w))
                .map(|w| w.labels.clone())
                .unwrap_or_default();
            let mut wanted = current.clone();
            for label in labels {
                if !wanted.iter().any(|l| label_key(l) == label_key(&label)) {
                    wanted.push(label);
                }
            }
            let changes = up.label_changes(&name, &current, &wanted);
            match (workspace, changes.is_empty()) {
                (_, true) => up.answer(&reply, Ok(answer)),
                (None, false) => {
                    let mut answer = answer;
                    answer["labels_error"] = json!("The new agent has no workspace to label");
                    up.answer(&reply, Ok(answer));
                }
                (Some(workspace), false) => {
                    up.assign_labels(&name, &workspace, changes, move |up, result| {
                        let mut answer = answer;
                        if let Err(error) = result {
                            answer["labels_error"] = json!(error);
                        }
                        up.answer(&reply, Ok(answer));
                    });
                }
            }
        });
    }

    /// Deletes an agent for good. Unlike an archived agent it leaves no
    /// history, so the page asks first.
    pub fn delete(&mut self, host: &str, agent: &str, reply: Reply) {
        let (name, id) = (host.to_string(), agent.to_string());
        self.write(
            host,
            requests::delete_agent(agent),
            WRITE_TIMEOUT,
            move |up, result| {
                if result.is_ok() && up.fleet.agent(&host_id(&name), &id).is_some() {
                    // Gone now, not when the directory says so.
                    let push = json!({"type": "agent_update", "payload": {"kind": "remove", "agentId": id}});
                    up.on_push(&name, push);
                }
                up.answer(&reply, result.map(|_| json!({})));
            },
        );
    }

    /// A new agent's labels: those named, made in the catalog where it has
    /// none of that name, else its workspace's default
    /// ([`fleet::labels::workspace_default`]). None leaves them unset, for a
    /// workspace not made yet.
    fn initial_labels(
        &mut self,
        host: &str,
        workspace: Option<&str>,
        names: Option<&[String]>,
    ) -> Option<Vec<LabelId>> {
        if let Some(names) = names {
            let ids = crate::catalog::resolve_names(self.fleet.labels.as_mut()?, names);
            self.dirty_fleet = true;
            return Some(ids);
        }
        let catalog = self.fleet.labels.as_ref()?;
        let directory = &self.fleet.host(&host_id(host))?.directory;
        let workspace = directory.workspaces.get(workspace?)?;
        let agents = (directory.agents.values())
            .filter(|agent| agent.workspace_id.as_deref() == Some(workspace.id.as_str()));
        Some(fleet::labels::workspace_default(agents, workspace, catalog))
    }

    /// Gives an agent exactly `labels`. With a catalog they are the agent's
    /// own (one `update_agent_request` with only [`fleet::labels::KEY`]);
    /// without one, its workspace's.
    pub fn relabel(&mut self, host: &str, agent: &str, labels: Vec<String>, reply: Reply) {
        if self.fleet.labels.is_some() {
            return self.relabel_agent(host, agent, labels, reply);
        }
        self.relabel_workspace(host, agent, labels, reply);
    }

    fn relabel_agent(&mut self, host: &str, agent: &str, labels: Vec<String>, reply: Reply) {
        let Some(current) = self
            .fleet
            .agent(&host_id(host), agent)
            .map(|a| a.labels.clone())
        else {
            return self.refuse(&reply, code::NOT_FOUND, "no such agent");
        };
        let Some(catalog) = self.fleet.labels.as_mut() else {
            return self.refuse(&reply, code::BAD_REQUEST, "Reef keeps no labels");
        };
        let ids = crate::catalog::resolve_names(catalog, &labels);
        self.dirty_fleet = true;
        // The same labels, in any order: nothing to send.
        if let fleet::labels::Assignment::Labels(had) = fleet::labels::assignment(&current)
            && had.len() == ids.len()
            && ids.iter().all(|id| had.contains(id))
        {
            return self.answer(&reply, Ok(json!({})));
        }
        let (name, id) = (host.to_string(), agent.to_string());
        let message = fleet::labels::set_request(agent, &ids);
        self.write(host, message, WRITE_TIMEOUT, move |up, result| {
            if result.is_ok()
                && let Some(found) = (up.fleet.host_mut(&host_id(&name)))
                    .and_then(|h| h.directory.agents.get_mut(&id))
            {
                // Shown at once; the daemon's push agrees later.
                let value = Value::String(fleet::labels::encode(&ids));
                found.labels.insert(fleet::labels::KEY.to_string(), value);
                up.dirty_fleet = true;
            }
            up.answer(&reply, result.map(|_| json!({})));
        });
    }

    /// Gives an agent's workspace exactly `labels`: what is missing is
    /// assigned and what is extra taken off, one request each.
    fn relabel_workspace(&mut self, host: &str, agent: &str, labels: Vec<String>, reply: Reply) {
        let found = (self.fleet.agent(&host_id(host), agent))
            .and_then(|a| a.workspace_id.clone())
            .and_then(|w| {
                self.fleet
                    .workspace(&host_id(host), &w)
                    .map(|ws| (w, ws.labels.clone()))
            });
        let Some((workspace, current)) = found else {
            return self.refuse(
                &reply,
                code::NOT_FOUND,
                "This agent has no workspace to label",
            );
        };
        if !self
            .fleet
            .host(&host_id(host))
            .is_some_and(|h| h.has("workspaceLabels"))
        {
            return self.refuse(&reply, code::BAD_REQUEST, "This host has no labels");
        }
        let changes = self.label_changes(host, &current, &labels);
        if changes.is_empty() {
            return self.answer(&reply, Ok(json!({})));
        }
        self.assign_labels(host, &workspace, changes, move |up, result| {
            up.answer(&reply, result.map(|_| json!({})));
        });
    }

    /// What it takes to go from `current` labels to `wanted`, matched as
    /// crusteo matches them, each with the colour its definition has on
    /// this host or another, else the one its name picks.
    fn label_changes(&self, host: &str, current: &[String], wanted: &[String]) -> Vec<LabelChange> {
        let has =
            |list: &[String], name: &str| list.iter().any(|l| label_key(l) == label_key(name));
        let color = |name: &str| {
            let here = self.fleet.host(&host_id(host)).into_iter();
            let others = (self.fleet.hosts.iter()).filter(|h| h.name() != host);
            (here.chain(others))
                .flat_map(|h| h.directory.labels.iter())
                .find(|def| label_key(&def.name) == label_key(name))
                .map(|def| (def.name.clone(), def.color.clone()))
                .unwrap_or_else(|| {
                    let name = name.trim().to_string();
                    let color = crate::lists::label_color_name(&name).to_string();
                    (name, color)
                })
        };
        let added = (wanted.iter()).filter(|l| !has(current, l)).map(|l| {
            let (name, color) = color(l);
            LabelChange {
                name,
                color,
                assigned: true,
            }
        });
        let removed = (current.iter()).filter(|l| !has(wanted, l)).map(|l| {
            let (_, color) = color(l);
            LabelChange {
                name: l.clone(),
                color,
                assigned: false,
            }
        });
        added.chain(removed).collect()
    }

    /// Sends the label changes one after another, stopping at the first
    /// that fails, as crusteo does. Each one done shows on the workspace at
    /// once; the daemon's push agrees with it later.
    fn assign_labels(
        &mut self,
        host: &str,
        workspace: &str,
        mut changes: Vec<LabelChange>,
        done: impl FnOnce(&mut Upstream, Result<(), String>) + Send + 'static,
    ) {
        if changes.is_empty() {
            return done(self, Ok(()));
        }
        let change = changes.remove(0);
        let message = requests::set_label(workspace, &change.name, &change.color, change.assigned);
        let (name, id) = (host.to_string(), workspace.to_string());
        self.write(host, message, WRITE_TIMEOUT, move |up, result| {
            if let Err(error) = result {
                return done(up, Err(error));
            }
            if let Some(found) = (up.fleet.host_mut(&host_id(&name)))
                .and_then(|h| h.directory.workspaces.get_mut(&id))
            {
                found
                    .labels
                    .retain(|l| label_key(l) != label_key(&change.name));
                if change.assigned {
                    found.labels.push(change.name.clone());
                }
                up.dirty_fleet = true;
            }
            up.assign_labels(&name, &id, changes, done);
        });
    }
}

/// One label put on or taken off a workspace.
#[derive(Debug, Clone, PartialEq)]
struct LabelChange {
    name: String,
    color: String,
    assigned: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet::providers::{Choice, Model};

    fn claude() -> Provider {
        let choice = |id: &str| Choice {
            id: id.into(),
            label: id.into(),
        };
        Provider {
            id: "claude".into(),
            label: "Claude".into(),
            status: "ready".into(),
            enabled: true,
            default_mode: Some("default".into()),
            modes: vec![choice("default"), choice("plan")],
            models: vec![
                Model {
                    id: "opus".into(),
                    label: "Opus".into(),
                    is_default: true,
                    thinking: vec![choice("low"), choice("high")],
                    default_thinking: Some("low".into()),
                },
                Model {
                    id: "haiku".into(),
                    label: "Haiku".into(),
                    is_default: false,
                    thinking: Vec::new(),
                    default_thinking: None,
                },
            ],
        }
    }

    fn asked(provider: &str) -> NewAgent {
        NewAgent {
            provider: provider.into(),
            model: None,
            mode: None,
            thinking: None,
            workspace: None,
            cwd: "/home/dev/code/shop".into(),
            labels: None,
            prompt: Prompt::default(),
            message_id: "m".into(),
        }
    }

    #[test]
    fn a_config_takes_the_providers_defaults_for_what_is_left_out() {
        let made = config(&[claude()], &asked("claude"), "example-a").unwrap();
        assert_eq!(
            made,
            json!({"provider": "claude", "cwd": "/home/dev/code/shop", "model": "opus",
                   "thinkingOptionId": "low", "modeId": "default"})
        );
        let mut new = asked("claude");
        new.model = Some("haiku".into());
        new.mode = Some("plan".into());
        let made = config(&[claude()], &new, "example-a").unwrap();
        assert_eq!(made["model"], "haiku");
        assert_eq!(made["modeId"], "plan");
        assert!(made.get("thinkingOptionId").is_none());
    }

    #[test]
    fn unknown_or_unready_choices_are_refused() {
        let (code, error) = config(&[claude()], &asked("codex"), "example-a").unwrap_err();
        assert_eq!(code, code::NOT_FOUND);
        assert!(error.contains("codex"), "{error}");
        let mut new = asked("claude");
        new.model = Some("gpt".into());
        assert_eq!(
            config(&[claude()], &new, "h").unwrap_err().0,
            code::BAD_REQUEST
        );
        let mut new = asked("claude");
        new.thinking = Some("max".into());
        assert_eq!(
            config(&[claude()], &new, "h").unwrap_err().0,
            code::BAD_REQUEST
        );
        let mut off = claude();
        off.status = "unavailable".into();
        let (_, error) = config(&[off.clone()], &asked("claude"), "example-a").unwrap_err();
        assert!(error.contains("not ready"), "{error}");
        // Shown to the form by name only.
        assert_eq!(
            providers_json(&[off]),
            [json!({"id": "claude", "label": "Claude", "ready": false, "status": "unavailable"})]
        );
    }

    #[test]
    fn the_form_gets_ready_providers_whole_and_compact() {
        let shown = providers_json(&[claude()]);
        assert_eq!(shown[0]["mode"], "default");
        assert_eq!(shown[0]["models"][0]["default"], true);
        assert_eq!(shown[0]["models"][0]["thinking_default"], "low");
        assert!(shown[0]["models"][1].get("thinking").is_none());
    }
}
