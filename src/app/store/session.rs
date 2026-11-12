//! Open conversations: one [`AgentSession`] per streamed agent, its
//! timeline reads, the prompt index, and the composer's send and queue
//! actions.

use std::collections::{BTreeMap, BTreeSet};

use fleet::prompt::Prompt;
use fleet::prompt_index::{JumpState, PageOutcome, PromptMark};
use fleet::sync::{Fetch, HostSync, Page};
use fleet::timeline::{Follow, Timeline};
use fleet::{HostId, requests};
use gpui_kit::gpui::{App, AppContext, Context, Entity};
use serde_json::{Value, json};

use super::{FleetEvent, FleetStore, now_iso, uuid_v4};

/// Daemon prompt metadata and history loading belong to the session driver.
#[derive(Default)]
pub struct SessionPromptIndex {
    marks: Vec<PromptMark>,
    key: Option<(String, Option<u64>)>,
    generation: u64,
    jump: JumpState,
    revision: u64,
}
impl SessionPromptIndex {
    pub fn marks(&self) -> &[PromptMark] {
        &self.marks
    }
    pub fn jump(&self) -> &JumpState {
        &self.jump
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn loading(&self) -> bool {
        matches!(self.jump, JumpState::Loading { .. })
    }
    pub fn error(&self) -> Option<&str> {
        match &self.jump {
            JumpState::Failed(error) => Some(error),
            _ => None,
        }
    }
}

/// One open conversation.
pub struct AgentSession {
    pub host: HostId,
    pub agent_id: String,
    pub timeline: Timeline,
    pub loading: bool,
    pub error: Option<String>,
    /// Messages typed while a turn ran, sent one per idle edge.
    pub queue: Vec<Prompt>,
    pub failed_prompts: BTreeMap<String, Prompt>,
    inflight_sends: BTreeSet<String>,
    pub prompt_index: SessionPromptIndex,
    /// One timeline read at a time; a reload asked for meanwhile waits.
    pub(super) fetch: Fetch,
}

impl AgentSession {
    pub fn display_error(&self) -> Option<String> {
        self.prompt_index
            .error()
            .map(str::to_string)
            .or_else(|| self.error.clone())
    }
    pub fn unsent_count(&self) -> usize {
        self.queue.len() + self.failed_prompts.len() + self.inflight_sends.len()
    }
    pub(super) fn new(host: &HostId, agent_id: &str) -> Self {
        Self {
            host: host.clone(),
            agent_id: agent_id.to_string(),
            timeline: Timeline::default(),
            loading: true,
            error: None,
            queue: Vec::new(),
            failed_prompts: BTreeMap::new(),
            inflight_sends: BTreeSet::new(),
            prompt_index: SessionPromptIndex::default(),
            fetch: Fetch::default(),
        }
    }
}

impl FleetStore {
    // ---- conversations ----------------------------------------------------

    /// Opens (or returns) a conversation and starts streaming it.
    pub fn open_session(
        &mut self,
        host: &HostId,
        agent_id: &str,
        cx: &mut Context<Self>,
    ) -> Entity<AgentSession> {
        let key = (host.clone(), agent_id.to_string());
        if let Some(session) = self.sessions.get(&key) {
            return session.clone();
        }
        let session = cx.new(|_| AgentSession::new(host, agent_id));
        if let Some(agent) = self.fleet.agent(host, agent_id) {
            let raw = agent.raw.clone();
            session.update(cx, |session, _| session.timeline.sync_snapshot(&raw));
        }
        if self.stress {
            session.update(cx, |session, _| {
                session
                    .timeline
                    .apply_page(&crate::app::stress::exchange(agent_id));
            });
        }
        self.sessions.insert(key, session.clone());
        self.sessions_revision += 1;
        // Membership affects previews, but is not a directory change: do
        // not bump the directory revision or publish a Bulk projection reset.
        cx.notify();
        if let Some(link) = self.links.get_mut(host) {
            link.sync.watched.insert(agent_id.to_string());
        }
        self.resubscribe_timelines(host, false, cx);
        self.fetch(host, agent_id, Page::Tail, cx);
        self.list_subagents(host, agent_id, cx);
        session
    }

    pub fn sessions_revision(&self) -> u64 {
        self.sessions_revision
    }

    pub fn session(&self, host: &HostId, agent_id: &str) -> Option<Entity<AgentSession>> {
        self.sessions
            .get(&(host.clone(), agent_id.to_string()))
            .cloned()
    }

    /// Stops streaming conversations no tab shows any more.
    pub fn retain_sessions(&mut self, keep: &BTreeSet<(HostId, String)>, cx: &mut Context<Self>) {
        let before = self.sessions.len();
        self.sessions.retain(|key, _| keep.contains(key));
        // A parent's subagents go with its conversation.
        // Provider sessions are owned by their live Conversation views, not
        // by the parent's navigation tab. Two views may share one session.
        self.provider_subagents.retain(|key, _| {
            keep.contains(key)
                || self
                    .subagent_viewers
                    .keys()
                    .any(|(host, parent, _)| key == &(host.clone(), parent.clone()))
        });
        if self.sessions.len() == before {
            return;
        }
        self.sessions_revision += 1;
        // Membership affects previews, but is not a directory change: do
        // not bump the directory revision or publish a Bulk projection reset.
        cx.notify();
        let hosts: Vec<HostId> = self.links.keys().cloned().collect();
        for host in hosts {
            let wanted: BTreeSet<String> = keep
                .iter()
                .filter(|(h, _)| *h == host)
                .map(|(_, a)| a.clone())
                .collect();
            if let Some(link) = self.links.get_mut(&host)
                && link.sync.watched != wanted
            {
                link.sync.watched = wanted;
                self.resubscribe_timelines(&host, false, cx);
            }
        }
    }

    pub(super) fn resubscribe_timelines(
        &mut self,
        host: &HostId,
        catch_up: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(link) = self.links.get(host) else {
            return;
        };
        if !link.sync.online() {
            return;
        }
        let epoch = link.sync.epoch();
        let message = HostSync::timeline_subscription_request(&link.sync.watched);
        let task = self.request(host, message, cx);
        let host = host.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(payload) = task.await {
                let id = payload["subscriptionId"].as_str().map(str::to_string);
                let _ = this.update(cx, |store, cx| {
                    if !store.is_current(&host, epoch) {
                        return;
                    }
                    // Each request makes a new subscription on the daemon;
                    // release the one it replaces so frames are not doubled.
                    let previous = (store.links.get_mut(&host))
                        .and_then(|link| link.sync.swap_timeline_subscription(id));
                    if let Some(previous) = previous {
                        store
                            .request(&host, requests::release(&previous), cx)
                            .detach();
                    }
                    if catch_up {
                        let agents: Vec<String> = (store.links.get(&host))
                            .map(|l| l.sync.watched.iter().cloned().collect())
                            .unwrap_or_default();
                        for agent in agents {
                            store.catch_up(&host, &agent, cx);
                        }
                    }
                });
            }
        })
        .detach();
    }

    fn catch_up(&mut self, host: &HostId, agent_id: &str, cx: &mut Context<Self>) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        let page = Page::catch_up(&session.read(cx).timeline);
        self.fetch(host, agent_id, page, cx);
    }

    pub(super) fn follow(
        &mut self,
        host: &HostId,
        agent_id: &str,
        follow: Follow,
        cx: &mut Context<Self>,
    ) {
        if let Some(page) = Page::following(follow) {
            self.fetch(host, agent_id, page, cx);
        }
    }

    /// Loads the page before what is shown, for scrolling up.
    pub fn fetch_older(&mut self, host: &HostId, agent_id: &str, cx: &mut Context<Self>) {
        self.fetch_before(host, agent_id, fleet::sync::PAGE, cx);
    }

    fn fetch_before(&mut self, host: &HostId, agent_id: &str, limit: u32, cx: &mut Context<Self>) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        if let Some(page) = Page::older(&session.read(cx).timeline, limit) {
            self.fetch(host, agent_id, page, cx);
        }
    }

    /// Reads `page` of an open conversation ([`Fetch`] allows one read at a
    /// time).
    pub(super) fn fetch(
        &mut self,
        host: &HostId,
        agent_id: &str,
        page: Page,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        if !session.update(cx, |session, _| session.fetch.begin(&page)) {
            return;
        }
        let older = page.is_older();
        let task = self.request(host, page.request(agent_id), cx);
        let host = host.clone();
        let agent_id = agent_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = task.await.and_then(|payload| {
                if let Some(error) = payload["error"].as_str() {
                    Err(error.to_string())
                } else {
                    Ok(payload)
                }
            });
            let _ = this.update(cx, |store, cx| {
                let Some(session) = store.session(&host, &agent_id) else {
                    return;
                };
                let (next, outcome) = session.update(cx, |session, cx| {
                    session.loading = false;
                    let landed = session.fetch.land(&mut session.timeline, &result);
                    session.error = landed.error;
                    cx.notify();
                    let outcome = match &result {
                        Ok(_) => PageOutcome::Success {
                            older,
                            progressed: landed.progressed,
                        },
                        Err(error) => PageOutcome::Failure(error.clone()),
                    };
                    (landed.next, outcome)
                });
                if let Some(page) = next {
                    store.fetch(&host, &agent_id, page, cx);
                }
                store.continue_prompt_load(&host, &agent_id, Some(&outcome), cx);
                store.ensure_prompt_index(&host, &agent_id, cx);
            });
        })
        .detach();
    }

    pub(super) fn ensure_prompt_index(
        &mut self,
        host: &HostId,
        agent_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        let timeline = &session.read(cx).timeline;
        let Some(cursor) = &timeline.cursor else {
            return;
        };
        let latest = timeline
            .items()
            .filter_map(|item| match &item.kind {
                fleet::timeline::ItemKind::User { seq, .. } => *seq,
                _ => None,
            })
            .max();
        let key = (cursor.epoch.clone(), latest);
        if session.read(cx).prompt_index.key.as_ref() == Some(&key) {
            return;
        }
        let generation = session.update(cx, |s, cx| {
            let index = &mut s.prompt_index;
            if index.key.as_ref().is_some_and(|(epoch, _)| epoch != &key.0) {
                index.marks.clear();
            }
            index.key = Some(key.clone());
            index.generation += 1;
            index.revision += 1;
            cx.notify();
            index.generation
        });
        if !self
            .fleet
            .host(host)
            .is_some_and(|h| h.has("agentTimelinePromptIndex"))
        {
            return;
        }
        let task = self.request(host, requests::prompt_index(agent_id), cx);
        cx.spawn(async move |_, cx| {
            let result = task.await;
            session.update(cx, |s, cx| {
                if s.prompt_index.generation != generation {
                    return;
                }
                if let Ok(payload) = result
                    && payload["epoch"].as_str() == Some(&key.0)
                    && payload["error"].as_str().is_none()
                    && s.timeline.cursor.as_ref().is_some_and(|c| c.epoch == key.0)
                    && let Ok(mut marks) =
                        serde_json::from_value::<Vec<PromptMark>>(payload["prompts"].clone())
                {
                    marks.sort_by_key(|p| p.seq);
                    s.prompt_index.marks = marks;
                    s.prompt_index.revision += 1;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Prepend contiguous history through the normal paging gate, keeping
    /// the live tail, held events and optimistic sends intact.
    pub fn load_prompt(&mut self, host: &HostId, agent_id: &str, seq: u64, cx: &mut Context<Self>) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        session.update(cx, |s, cx| {
            s.prompt_index.jump = match &s.timeline.cursor {
                Some(cursor) => JumpState::Loading {
                    epoch: cursor.epoch.clone(),
                    seq,
                },
                None => JumpState::Failed("Conversation is still loading; try again.".into()),
            };
            s.prompt_index.revision += 1;
            cx.notify();
        });
        self.continue_prompt_load(host, agent_id, None, cx);
    }

    fn continue_prompt_load(
        &mut self,
        host: &HostId,
        agent_id: &str,
        page: Option<&PageOutcome>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(host, agent_id) else {
            return;
        };
        session.update(cx, |s, cx| {
            let previous = &s.prompt_index.jump;
            let next = previous.advance(&s.timeline, s.fetch.in_flight(), page);
            if &next != previous {
                s.prompt_index.jump = next;
                s.prompt_index.revision += 1;
                cx.notify();
            }
        });
        let state = session.read(cx);
        if let JumpState::Loading { epoch, seq } = state.prompt_index.jump()
            && fleet::prompt_index::history_load(
                &state.timeline,
                epoch,
                *seq,
                state.fetch.in_flight(),
            ) == fleet::prompt_index::HistoryLoad::FetchOlder
        {
            self.fetch_before(host, agent_id, 200, cx);
        }
    }

    // ---- composer actions ---------------------------------------------------

    pub fn can_submit(&self, host: &HostId, agent_id: &str, cx: &App) -> Result<(), String> {
        let Some(session) = self.session(host, agent_id) else {
            return Err("Conversation is not ready; try again.".into());
        };
        if session.read(cx).unsent_count() >= 32 {
            return Err(
                "Restore failed messages or wait for queued messages before sending more.".into(),
            );
        }
        Ok(())
    }

    pub fn send_prompt(
        &mut self,
        host: &HostId,
        agent_id: &str,
        prompt: Prompt,
        behavior: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.can_submit(host, agent_id, cx)?;
        let message_id = uuid_v4();
        let now = now_iso();
        if let Some(session) = self.session(host, agent_id) {
            session.update(cx, |session, cx| {
                session.inflight_sends.insert(message_id.clone());
                session
                    .timeline
                    .add_local_prompt(&message_id, &prompt, &now);
                cx.notify();
            });
        }
        let message = requests::send_message(agent_id, &message_id, &prompt, behavior);
        let task = self.write(host, message, cx);
        let host = host.clone();
        let agent_id = agent_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let failure = match result {
                Err(err) => Some(err),
                Ok(payload) if payload["accepted"] == false || payload["error"].is_string() => {
                    Some(
                        payload["error"]
                            .as_str()
                            .unwrap_or("Message was not accepted")
                            .to_string(),
                    )
                }
                Ok(_) => None,
            };
            let _ = this.update(cx, |store, cx| {
                if let Some(session) = store.session(&host, &agent_id) {
                    session.update(cx, |session, cx| {
                        session.inflight_sends.remove(&message_id);
                        if failure.is_some() {
                            session.timeline.remove_local_user(&message_id);
                            session.failed_prompts.insert(message_id, prompt);
                        }
                        cx.notify();
                    });
                }
                if let Some(error) = failure {
                    cx.emit(FleetEvent::Toast(error));
                }
            });
        })
        .detach();
        Ok(())
    }

    pub fn failed_prompt(
        &self,
        host: &HostId,
        agent_id: &str,
        id: &str,
        cx: &App,
    ) -> Option<Prompt> {
        self.session(host, agent_id)?
            .read(cx)
            .failed_prompts
            .get(id)
            .cloned()
    }
    pub fn take_failed(
        &mut self,
        host: &HostId,
        agent_id: &str,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Option<Prompt> {
        self.session(host, agent_id)?.update(cx, |s, cx| {
            let prompt = s.failed_prompts.remove(id);
            if prompt.is_some() {
                cx.notify();
            }
            prompt
        })
    }

    pub fn queue_message(
        &mut self,
        host: &HostId,
        agent_id: &str,
        prompt: Prompt,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.can_submit(host, agent_id, cx)?;
        if let Some(session) = self.session(host, agent_id) {
            session.update(cx, |session, cx| {
                session.queue.push(prompt);
                cx.notify();
            });
        }
        Ok(())
    }

    pub fn cancel(&mut self, host: &HostId, agent_id: &str, cx: &mut Context<Self>) {
        self.act(
            host,
            json!({ "type": "cancel_agent_request", "agentId": agent_id }),
            cx,
        );
    }

    pub fn answer_permission(
        &mut self,
        host: &HostId,
        agent_id: &str,
        permission_id: &str,
        response: Value,
        cx: &mut Context<Self>,
    ) {
        self.send_raw(
            host,
            json!({
                "type": "agent_permission_response",
                "agentId": agent_id,
                "requestId": permission_id,
                "response": response,
            }),
        );
        // The card stays until the daemon confirms with
        // agent_permission_resolved, so an answer lost to a reconnect can be
        // given again.
        let _ = cx;
    }

    pub fn clear_attention(&mut self, host: &HostId, agent_id: &str, cx: &mut Context<Self>) {
        let needs = self.fleet.agent(host, agent_id).is_some_and(|a| {
            a.requires_attention && a.attention_reason.as_deref() != Some("permission")
        });
        if needs {
            let task = self.request(
                host,
                json!({ "type": "clear_agent_attention", "agentId": agent_id }),
                cx,
            );
            task.detach();
        }
    }
}
