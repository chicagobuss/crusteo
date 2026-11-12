//! What the user asks of a host's directory: labels, images, archiving,
//! providers, and creating workspaces and agents.

use fleet::prompt::{ImageAttachment, Prompt};
use fleet::{HostId, requests};
use gpui_kit::gpui::{App, AppContext, Context, Task};
use paseo_client::Client;
use serde_json::{Value, json};

use super::{
    Created, FleetStore, LabelBatch, LabelChange, ProvidersState, WRITE_TIMEOUT, reply_error,
    send_request, uuid_v4,
};
use crate::app::images;

impl FleetStore {
    /// The text of an agent's newest reply: from its conversation when one
    /// is open and loaded, else read from the end of its timeline
    /// ([`fleet::requests::last_reply`]). A read only: unlike opening the
    /// agent, it leaves "ready to review" as it is. A synthetic fleet
    /// answers from its made-up conversation, after a moment.
    pub fn last_reply(
        &self,
        host: &HostId,
        agent_id: &str,
        cx: &App,
    ) -> Task<Result<Option<String>, String>> {
        if let Some(session) = self.session(host, agent_id) {
            let timeline = &session.read(cx).timeline;
            if timeline.baseline {
                return Task::ready(Ok(timeline.newest_reply().map(str::to_string)));
            }
        }
        if self.stress {
            let page = crate::app::stress::exchange(agent_id);
            let timer = cx
                .background_executor()
                .timer(std::time::Duration::from_millis(120));
            return cx.background_spawn(async move {
                timer.await;
                Ok(fleet::preview::newest_reply(&page).map(|(text, _)| text))
            });
        }
        let task = self.request(host, fleet::requests::last_reply(agent_id), cx);
        cx.background_spawn(async move {
            let payload = task.await?;
            if let Some(error) = payload["error"].as_str() {
                return Err(error.to_string());
            }
            Ok(fleet::preview::newest_reply(&payload).map(|(text, _)| text))
        })
    }

    /// In reef mode, a new agent's `labels` for its create request: those
    /// given, else those the workspace's agents share
    /// ([`fleet::labels::workspace_default`]). None outside reef mode, or
    /// for a workspace not made yet with none given: it inherits.
    fn new_agent_labels(
        &self,
        host: &HostId,
        workspace: Option<&str>,
        labels: Option<Vec<fleet::labels::LabelId>>,
    ) -> Option<Value> {
        let catalog = self.fleet.labels.as_ref()?;
        let ids = match labels {
            Some(ids) => ids,
            None => {
                let directory = &self.fleet.host(host)?.directory;
                let workspace = directory.workspaces.get(workspace?)?;
                let agents = (directory.agents.values())
                    .filter(|agent| agent.workspace_id.as_deref() == Some(workspace.id.as_str()));
                fleet::labels::workspace_default(agents, workspace, catalog)
            }
        };
        Some(json!({ fleet::labels::KEY: fleet::labels::encode(&ids) }))
    }

    /// Reef's label catalog, when it is newer than the one there is: Lanes
    /// place agents by it, so everything is looked at again.
    pub fn set_catalog(&mut self, catalog: fleet::labels::Catalog, cx: &mut Context<Self>) {
        let current = self.fleet.labels.as_ref();
        if current.is_some_and(|c| c.revision == catalog.revision && *c == catalog) {
            return;
        }
        self.fleet.labels = Some(catalog);
        self.publish(true, cx);
    }

    /// Reef mode: changes the labels of `agents` on `host` by `changes`,
    /// named as the picker names them. Each agent keeps its own labels but
    /// those taken off, plus those put on; a name the catalog lacks is made
    /// at Reef first. The batch names each change's label by its id, as
    /// reef-mode lanes key them, so it lands when the agents' labels have
    /// those ids.
    pub fn relabel_agents(
        &mut self,
        host: &HostId,
        agents: Vec<String>,
        changes: Vec<LabelChange>,
        cx: &mut Context<Self>,
    ) -> Task<LabelBatch> {
        let failed = |error: String| {
            Task::ready(LabelBatch {
                applied: vec![],
                error: Some(error),
            })
        };
        let Some(link) = self.reef.as_ref().map(|reef| reef.link.clone()) else {
            return failed("Labels are Reef's, and this desktop is not on a Reef".into());
        };
        let Some(catalog) = &self.fleet.labels else {
            return failed("Reef's labels have not arrived yet".into());
        };
        let missing: Vec<String> = (changes.iter())
            .filter(|change| change.assigned && catalog.by_name(&change.name).is_none())
            .map(|change| change.name.trim().to_string())
            .collect();
        let host = host.clone();
        cx.spawn(async move |this, cx| {
            let failed = |error: String| LabelBatch {
                applied: vec![],
                error: Some(error),
            };
            for name in missing {
                let link = link.clone();
                let edit = json!({ "op": "create", "name": name });
                let answer = cx
                    .background_spawn(async move { link.edit_labels(&edit) })
                    .await;
                let catalog = match answer {
                    Ok((_, catalog)) => catalog,
                    Err(error) => return failed(error),
                };
                if this
                    .update(cx, |store, cx| store.set_catalog(catalog, cx))
                    .is_err()
                {
                    return failed("crusteo is closing".into());
                }
            }
            let plan = this.update(cx, |store, _| store.relabel_plan(&host, &agents, &changes));
            let (client, writes, applied) = match plan {
                Ok(Ok(plan)) => plan,
                Ok(Err(error)) => return failed(error),
                Err(_) => return failed("crusteo is closing".into()),
            };
            for (agent, ids) in writes {
                let message = fleet::labels::set_request(&agent, &ids);
                let failure = match send_request(&client, message, WRITE_TIMEOUT).await {
                    Err(error) => Some(error),
                    Ok(payload) => reply_error(&payload),
                };
                if let Some(error) = failure {
                    return failed(error);
                }
            }
            LabelBatch {
                applied,
                error: None,
            }
        })
    }

    /// What [`Self::relabel_agents`] sends: each agent's new labels, and
    /// the changes by label id. Agents whose labels stay the same are left
    /// out.
    #[allow(clippy::type_complexity)]
    fn relabel_plan(
        &self,
        host: &HostId,
        agents: &[String],
        changes: &[LabelChange],
    ) -> Result<
        (
            Client,
            Vec<(String, Vec<fleet::labels::LabelId>)>,
            Vec<LabelChange>,
        ),
        String,
    > {
        let catalog = self
            .fleet
            .labels
            .as_ref()
            .ok_or("Reef's labels have not arrived yet")?;
        let link = self
            .links
            .get(host)
            .ok_or_else(|| self.not_connected(host))?;
        let by_id: Vec<(fleet::labels::LabelId, bool)> = (changes.iter())
            .filter_map(|change| {
                let label = catalog.by_name(&change.name)?;
                Some((label.id.clone(), change.assigned))
            })
            .collect();
        let found = self
            .fleet
            .host(host)
            .ok_or_else(|| self.not_connected(host))?;
        let mut writes = Vec::new();
        for agent in agents {
            let Some(current) = found.directory.agents.get(agent) else {
                continue;
            };
            let workspace =
                (current.workspace_id.as_deref()).and_then(|id| found.directory.workspaces.get(id));
            let had = fleet::labels::effective(current, workspace, catalog);
            let mut ids: Vec<_> = (had.iter())
                .filter(|id| !by_id.iter().any(|(off, on)| !on && off == *id))
                .cloned()
                .collect();
            for (id, on) in &by_id {
                if *on && !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
            // One that would keep its labels keeps inheriting them, if it does.
            if ids != had {
                writes.push((agent.clone(), ids));
            }
        }
        let applied = (by_id.into_iter())
            .map(|(id, assigned)| LabelChange {
                name: id.to_string(),
                color: String::new(),
                assigned,
            })
            .collect();
        Ok((link.client.clone(), writes, applied))
    }

    /// The protocol has one assignment per request. Preserve successful writes
    /// on partial failure so the view can reconcile against the actual labels.
    pub fn set_workspace_labels(
        &self,
        host: &HostId,
        workspace: &str,
        changes: Vec<LabelChange>,
        cx: &App,
    ) -> Task<LabelBatch> {
        let Some(link) = self.links.get(host) else {
            return Task::ready(LabelBatch {
                applied: vec![],
                error: Some(format!("{} is not connected", self.fleet.host_name(host))),
            });
        };
        let client = link.client.clone();
        let workspace = workspace.to_string();
        cx.background_spawn(async move {
            let mut batch = LabelBatch {
                applied: vec![],
                error: None,
            };
            for change in changes {
                let message =
                    requests::set_label(&workspace, &change.name, &change.color, change.assigned);
                let result = send_request(&client, message, WRITE_TIMEOUT).await;
                let failure = match result {
                    Err(error) => Some(error),
                    Ok(payload) => reply_error(&payload),
                };
                if let Some(error) = failure {
                    batch.error = Some(error);
                    break;
                }
                batch.applied.push(change);
            }
            batch
        })
    }

    // ---- images ---------------------------------------------------------

    /// What to show for images `host` keeps as files, reading each one not
    /// yet read. Views call it as they draw a row, so only what is on screen
    /// is read; when a read lands, `images.revision` moves.
    pub fn show_images(
        &mut self,
        host: &HostId,
        files: &[fleet::images::ImageFile],
        cx: &mut Context<Self>,
    ) -> Vec<images::Shown> {
        let now = std::time::Instant::now();
        files
            .iter()
            .map(|file| {
                let (shown, read) = self.images.show(host, file, now);
                if read {
                    self.load_image(host, file.clone(), cx);
                }
                shown
            })
            .collect()
    }

    fn load_image(
        &mut self,
        host: &HostId,
        file: fleet::images::ImageFile,
        cx: &mut Context<Self>,
    ) {
        let read = self.read_image(host, &file, cx);
        let host = host.clone();
        cx.spawn(async move |this, cx| {
            let result = match read.await {
                Ok(image) => cx
                    .background_executor()
                    .spawn(async move { images::prepare(&image) })
                    .await
                    .map_err(images::Failure::Refused),
                Err(failure) => Err(failure),
            };
            let _ = this.update(cx, |this, cx| {
                let now = std::time::Instant::now();
                for forgotten in this.images.finish(&host, &file, result, now) {
                    forgotten.remove_asset(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reads one image a daemon keeps as a file; see [`fleet::images`].
    pub fn read_image(
        &self,
        host: &HostId,
        file: &fleet::images::ImageFile,
        cx: &App,
    ) -> Task<Result<ImageAttachment, images::Failure>> {
        if self.stress {
            return crate::app::stress::read_image(file, cx);
        }
        let Some(link) = self.links.get(host) else {
            return Task::ready(Err(images::Failure::Unreachable));
        };
        let client = link.client.clone();
        let message = file.request();
        cx.background_spawn(async move {
            match client.request_with_timeout(message, WRITE_TIMEOUT).await {
                Ok(payload) => {
                    fleet::images::image_from_reply(&payload).map_err(images::Failure::Refused)
                }
                Err(paseo_client::Error::Disconnected | paseo_client::Error::Timeout) => {
                    Err(images::Failure::Unreachable)
                }
                // Among them `unknown_schema`, from a daemon that serves no files.
                Err(error) => Err(images::Failure::Refused(error.to_string())),
            }
        })
    }

    /// Keep duplicate archive requests out; directory pushes own view removal.
    pub fn workspace_archive_pending(&self, host: &HostId, workspace: &str) -> bool {
        self.workspace_archives.contains(host, workspace)
    }

    pub fn archive_workspace(&mut self, host: &HostId, workspace: &str, cx: &mut Context<Self>) {
        let key = (host.clone(), workspace.to_string());
        let Some(attempt) = self.workspace_archives.begin(host, workspace) else {
            return;
        };
        let task = self.write(host, requests::archive_workspace(workspace), cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let failure = match result {
                Err(error) => Some(error),
                Ok(payload) => reply_error(&payload),
            };
            let succeeded = failure.is_none();
            let _ = this.update(cx, |store, cx| {
                if let Some(error) = failure {
                    store.workspace_archives.release(&key.0, &key.1, attempt);
                    store.toast(error, cx);
                }
                cx.notify();
            });
            if succeeded {
                // One deadline per successful request, never a polling loop.
                // A late deadline must not release a newer attempt's guard.
                cx.background_executor().timer(std::time::Duration::from_secs(30)).await;
                let _ = this.update(cx, |store, cx| {
                    if store.workspace_archives.release(&key.0, &key.1, attempt) {
                        store.toast("Archive was acknowledged, but its directory update has not arrived. Reconnect the host to refresh its state.", cx);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// Loads a host's providers snapshot once; views observe the store.
    /// After a failure, the next call tries again.
    pub fn ensure_providers(&mut self, host: &HostId, cx: &mut Context<Self>) {
        if self.providers.contains_key(host) || !self.providers_loading.insert(host.clone()) {
            return;
        }
        self.providers_failed.remove(host);
        let task = self.request(host, requests::providers_snapshot(), cx);
        let host = host.clone();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |store, cx| {
                store.providers_loading.remove(&host);
                match result {
                    Ok(payload) => {
                        store.keep_providers(&host, &payload);
                        store.schedule_cli_flush(cx);
                    }
                    Err(error) => {
                        store.providers_failed.insert(host.clone(), error);
                    }
                }
                if store.providers_stale.remove(&host) {
                    store.forget_providers(&host);
                    store.ensure_providers(&host, cx);
                }
                store.changed(cx);
            });
        })
        .detach();
    }

    /// Keeps a full providers snapshot for `host`, with its hash.
    fn keep_providers(&mut self, host: &HostId, payload: &Value) {
        self.providers
            .insert(host.clone(), fleet::providers::parse(payload));
        if let (Some(name), Some(list)) = (
            self.fleet.host(host).map(|h| h.name().to_string()),
            self.providers.get(host),
        ) {
            self.cli_watch
                .observe(&name, list, std::time::Instant::now());
        }
        match payload["snapshotHash"].as_str() {
            Some(hash) => self.providers_hash.insert(host.clone(), hash.to_string()),
            None => self.providers_hash.remove(host),
        };
    }

    /// Waits until what the CLI watch has gathered is due, then tells the
    /// user in a toast: one per CLI version for the whole fleet, as hosts
    /// upgrade. The desktop's toasts are its notices.
    pub(super) fn schedule_cli_flush(&mut self, cx: &mut Context<Self>) {
        if self.cli_flush_scheduled {
            return;
        }
        let Some(due) = self.cli_watch.next_due() else {
            return;
        };
        self.cli_flush_scheduled = true;
        let wait = due.saturating_duration_since(std::time::Instant::now());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |store, cx| {
                store.cli_flush_scheduled = false;
                for notice in store.cli_watch.flush(std::time::Instant::now()) {
                    store.toast(format!("{}: {}", notice.title, notice.body), cx);
                }
                store.schedule_cli_flush(cx);
                store.changed(cx);
            });
        })
        .detach();
    }

    /// A host's CLIs for the hosts screen, with those behind the fleet's
    /// newest version marked.
    pub fn cli_rows(&self, host: &HostId) -> Vec<fleet::cli_watch::Row> {
        match self.fleet.host(host) {
            Some(host) => self.cli_watch.rows(host.name()),
            None => Vec::new(),
        }
    }

    /// Drops what `host` was known to offer; true if anything was known.
    pub(super) fn forget_providers(&mut self, host: &HostId) -> bool {
        self.providers_failed.remove(host);
        self.providers_hash.remove(host);
        self.providers.remove(host).is_some()
    }

    /// A daemon's providers changed, typically as a probe landed after it
    /// started. A client that takes snapshot references (we do) is sent
    /// only the new hash, so a changed hash means asking again; an older
    /// daemon sends the snapshot itself. A snapshot for one project's
    /// directory (`cwd`) is not the host-wide one kept here.
    pub(super) fn on_providers_update(
        &mut self,
        host: &HostId,
        payload: &Value,
        cx: &mut Context<Self>,
    ) {
        let known = self.providers_hash.get(host).map(String::as_str);
        match fleet::providers::update(payload, known) {
            fleet::providers::Update::Ignore => {}
            fleet::providers::Update::Keep => {
                self.providers_failed.remove(host);
                self.keep_providers(host, payload);
                self.schedule_cli_flush(cx);
                self.changed(cx);
            }
            fleet::providers::Update::Reload => {
                if self.providers_loading.contains(host) {
                    self.providers_stale.insert(host.clone());
                } else if self.forget_providers(host) {
                    self.ensure_providers(host, cx);
                    self.changed(cx);
                }
            }
        }
    }

    /// What a draft can say about `host`'s providers while it has none to
    /// offer, so it never sits on "Loading…" for a host that is gone.
    pub fn providers_state(&self, host: &HostId) -> ProvidersState {
        if self.providers.contains_key(host) {
            return ProvidersState::Ready;
        }
        if !self
            .fleet
            .host(host)
            .is_some_and(|record| record.connection.is_online())
        {
            return ProvidersState::Offline;
        }
        match self.providers_failed.get(host) {
            Some(error) if !self.providers_loading.contains(host) => {
                ProvidersState::Failed(error.clone())
            }
            _ => ProvidersState::Loading,
        }
    }

    /// Takes a project the daemon just described into its host's
    /// directory, before the push that would bring it, so a form can pick
    /// it at once.
    pub fn upsert_project(&mut self, host: &HostId, project: &Value, cx: &mut Context<Self>) {
        if project["projectId"].is_string() {
            let message = json!({"type": "project.update",
                "payload": {"kind": "upsert", "project": project}});
            self.on_push(host, message, cx);
        }
    }

    /// [`Self::upsert_project`] for a workspace the daemon just made.
    pub fn upsert_workspace(&mut self, host: &HostId, workspace: &Value, cx: &mut Context<Self>) {
        if workspace["id"].is_string() {
            let message = json!({"type": "workspace_update",
                "payload": {"kind": "upsert", "workspace": workspace}});
            self.on_push(host, message, cx);
        }
    }

    /// Makes a workspace as `plan` says, with a first agent when `agent`
    /// gives its config and prompt: in one request where the host takes
    /// both, else the workspace and then the agent in it.
    pub fn create_workspace(
        &mut self,
        plan: &fleet::creation::Plan,
        agent: Option<(Value, Prompt)>,
        labels: Option<Vec<fleet::labels::LabelId>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Created, String>> {
        // A new workspace has no agents to share labels from.
        let labels = self.new_agent_labels(&plan.host, None, labels);
        let support = self
            .fleet
            .host(&plan.host)
            .map(fleet::creation::Support::for_host)
            .unwrap_or_default();
        let one_step = plan.agent_in_create(support);
        let prompt_text = agent
            .as_ref()
            .map(|(_, prompt)| prompt.text.clone())
            .unwrap_or_default();
        let message_id = uuid_v4();
        let initial = agent.as_ref().filter(|_| one_step).map(|(config, prompt)| {
            let mut agent = fleet::creation::initial_agent(config.clone(), prompt, &message_id);
            if let Some(labels) = &labels {
                agent["labels"] = labels.clone();
            }
            agent
        });
        let message = plan.create_request(&prompt_text, initial, &uuid_v4(), support);
        let create = self.write(&plan.host, message, cx);
        // The agent's own request, for when it could not go in the first.
        let follow = (!one_step).then_some(agent).flatten();
        let host = plan.host.clone();
        cx.spawn(async move |this, cx| {
            let reply = create.await;
            let made = match &reply {
                Ok(payload) => payload["workspace"]["id"].as_str().map(str::to_string),
                Err(_) => None,
            };
            let Some(workspace) = made else {
                return Err(match reply {
                    Ok(payload) => reply_error(&payload)
                        .unwrap_or_else(|| "The host did not return the new workspace".into()),
                    Err(error) => error,
                });
            };
            let payload = reply.unwrap_or_default();
            let directory = payload["workspace"]["workspaceDirectory"]
                .as_str()
                .filter(|dir| !dir.is_empty())
                .or_else(|| payload["workspace"]["projectRootPath"].as_str())
                .unwrap_or_default()
                .to_string();
            let mut created = Created {
                workspace,
                directory,
                agent: Ok(payload["agent"]["id"].as_str().map(str::to_string)),
            };
            // The workspace is made but its agent failed in the same request.
            if let Some(error) = reply_error(&payload) {
                created.agent = Err(error);
                return Ok(created);
            }
            if let Some((mut config, prompt)) = follow {
                config["cwd"] = json!(created.directory);
                let mut message = requests::create_agent(
                    Some(&created.workspace),
                    config,
                    Some(prompt),
                    &message_id,
                );
                if let Some(labels) = labels {
                    message["labels"] = labels;
                }
                let reply = match this.update(cx, |store, cx| store.write(&host, message, cx)) {
                    Ok(task) => task.await,
                    Err(_) => Err("crusteo is closing".to_string()),
                };
                created.agent = match reply {
                    Ok(payload) => match reply_error(&payload) {
                        Some(error) => Err(error),
                        None => Ok(payload["agent"]["id"].as_str().map(str::to_string)),
                    },
                    Err(error) => Err(error),
                };
            }
            Ok(created)
        })
    }

    /// Creates an agent in a workspace and returns its id when ready. In
    /// reef mode it carries `labels`, else its workspace's default.
    pub fn create_agent(
        &mut self,
        host: &HostId,
        workspace_id: &str,
        config: Value,
        prompt: Option<Prompt>,
        labels: Option<Vec<fleet::labels::LabelId>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<String, String>> {
        let mut message = requests::create_agent(Some(workspace_id), config, prompt, &uuid_v4());
        if let Some(labels) = self.new_agent_labels(host, Some(workspace_id), labels) {
            message["labels"] = labels;
        }
        let task = self.write(host, message, cx);
        cx.background_spawn(async move {
            let payload = task.await?;
            if let Some(error) = payload["error"].as_str() {
                return Err(error.to_string());
            }
            payload["agent"]["id"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "The host did not return an agent".to_string())
        })
    }
}
