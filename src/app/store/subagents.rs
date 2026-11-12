//! Provider subagents: the conversations a provider runs inside an agent,
//! listed under their parent and opened read-only, and the tabs this client
//! tells each host it has open.

use std::collections::{BTreeSet, HashMap};

use fleet::sync::Page;
use fleet::{Fleet, HostId, requests};
use gpui_kit::gpui::{AppContext, Context, Entity};
use serde_json::Value;

use super::{AgentSession, FleetStore, now_millis};

impl FleetStore {
    // ---- provider subagents --------------------------------------------

    /// The provider subagents of `parent`, oldest first, once an open
    /// conversation of it has listed them.
    pub fn provider_subagents(&self, host: &HostId, parent: &str) -> &[fleet::subagents::Subagent] {
        self.provider_subagents
            .get(&(host.clone(), parent.to_string()))
            .map_or(&[], Vec::as_slice)
    }

    /// Takes provider subagents off `parent`'s track. They are the
    /// provider's, not Paseo agents, so there is nothing to archive.
    pub fn hide_provider_subagents(&mut self, host: &HostId, parent: &str, ids: &[String]) {
        for id in ids {
            self.hidden_provider_subagents
                .insert((host.clone(), parent.to_string(), id.clone()));
        }
    }

    pub fn provider_subagent_hidden(&self, host: &HostId, parent: &str, id: &str) -> bool {
        self.hidden_provider_subagents
            .contains(&(host.clone(), parent.to_string(), id.to_string()))
    }

    /// Lists `parent`'s provider subagents; updates keep the list current.
    pub(super) fn list_subagents(&mut self, host: &HostId, parent: &str, cx: &mut Context<Self>) {
        if !self
            .fleet
            .host(host)
            .is_some_and(|record| record.has("providerSubagents"))
        {
            return;
        }
        let task = self.request(host, requests::list_provider_subagents(parent), cx);
        let (host, parent) = (host.clone(), parent.to_string());
        cx.spawn(async move |this, cx| {
            let Ok(payload) = task.await else {
                return;
            };
            if payload["error"].is_string() {
                return;
            }
            let _ = this.update(cx, |store, cx| {
                if store.session(&host, &parent).is_none() {
                    return;
                }
                let list = fleet::subagents::list(&payload);
                for subagent in &list {
                    store.sync_subagent_status(&host, &parent, subagent, cx);
                }
                store
                    .provider_subagents
                    .insert((host.clone(), parent.clone()), list);
                store.subagents_revision += 1;
                cx.notify();
            });
        })
        .detach();
    }

    /// After a reconnect: relist the open conversations' subagents and
    /// reload the open subagent conversations, whose epochs may be new.
    pub(super) fn reload_subagents(&mut self, host: &HostId, cx: &mut Context<Self>) {
        let parents: Vec<String> = self
            .sessions
            .keys()
            .filter(|(h, _)| h == host)
            .map(|(_, agent)| agent.clone())
            .collect();
        for parent in parents {
            self.list_subagents(host, &parent, cx);
        }
        let open: Vec<(String, String)> = self
            .subagent_sessions
            .keys()
            .filter(|(h, ..)| h == host)
            .map(|(_, parent, sub)| (parent.clone(), sub.clone()))
            .collect();
        for (parent, sub) in open {
            self.fetch_subagent(host, &parent, &sub, Page::Tail, cx);
        }
    }

    /// Brings an open subagent conversation's turn in line with its status.
    fn sync_subagent_status(
        &mut self,
        host: &HostId,
        parent: &str,
        subagent: &fleet::subagents::Subagent,
        cx: &mut Context<Self>,
    ) {
        let key = (host.clone(), parent.to_string(), subagent.id.clone());
        if let Some(session) = self.subagent_sessions.get(&key) {
            session.update(cx, |session, cx| {
                if session.timeline.is_running() != subagent.running() {
                    let event =
                        fleet::subagents::status_event(subagent.status, &subagent.updated_at);
                    session.timeline.apply_live(&event);
                    cx.notify();
                }
            });
        }
    }

    pub(super) fn on_subagent_update(
        &mut self,
        host: &HostId,
        payload: &Value,
        cx: &mut Context<Self>,
    ) {
        match payload["kind"].as_str() {
            Some("upsert") => {
                let value = &payload["subagent"];
                let Some(parent) = value["parentAgentId"].as_str() else {
                    return;
                };
                let key = (host.clone(), parent.to_string());
                // Only conversations that are open keep a list.
                let Some(list) = self.provider_subagents.get_mut(&key) else {
                    return;
                };
                if fleet::subagents::upsert(list, value).is_none() {
                    return;
                }
                let id = value["id"].as_str().unwrap_or_default();
                if let Some(subagent) = list.iter().find(|sub| sub.id == id).cloned() {
                    self.sync_subagent_status(host, parent, &subagent, cx);
                }
                self.subagents_revision += 1;
                cx.notify();
            }
            Some("remove") => {
                let parent = payload["parentAgentId"].as_str().unwrap_or_default();
                let id = payload["subagentId"].as_str().unwrap_or_default();
                if let Some(list) = self
                    .provider_subagents
                    .get_mut(&(host.clone(), parent.to_string()))
                {
                    list.retain(|sub| sub.id != id);
                    self.subagents_revision += 1;
                    cx.notify();
                }
            }
            Some("timeline") => {
                let parent = payload["parentAgentId"].as_str().unwrap_or_default();
                let id = payload["subagentId"].as_str().unwrap_or_default();
                let key = (host.clone(), parent.to_string(), id.to_string());
                let Some(session) = self.subagent_sessions.get(&key).cloned() else {
                    return;
                };
                let follow = session.update(cx, |session, cx| {
                    let follow = session
                        .timeline
                        .apply_live(&fleet::subagents::live(payload));
                    cx.notify();
                    follow
                });
                if let Some(page) = Page::following(follow) {
                    self.fetch_subagent(host, parent, id, page, cx);
                }
            }
            _ => {}
        }
    }

    /// Opens a provider subagent's conversation, read-only, streaming.
    pub fn open_subagent(
        &mut self,
        host: &HostId,
        parent: &str,
        subagent: &str,
        cx: &mut Context<Self>,
    ) -> Entity<AgentSession> {
        let key = (host.clone(), parent.to_string(), subagent.to_string());
        *self.subagent_viewers.entry(key.clone()).or_default() += 1;
        if let Some(session) = self.subagent_sessions.get(&key) {
            return session.clone();
        }
        let session = cx.new(|_| AgentSession::new(host, subagent));
        self.subagent_sessions.insert(key, session.clone());
        if let Some(found) = self
            .provider_subagents(host, parent)
            .iter()
            .find(|sub| sub.id == subagent)
            .cloned()
        {
            self.sync_subagent_status(host, parent, &found, cx);
        }
        self.fetch_subagent(host, parent, subagent, Page::Tail, cx);
        session
    }

    /// Stops keeping a subagent conversation no view shows.
    pub fn close_subagent(&mut self, host: &HostId, parent: &str, subagent: &str) {
        let key = (host.clone(), parent.to_string(), subagent.to_string());
        if release_viewer(&mut self.subagent_viewers, &key) {
            self.subagent_sessions.remove(&key);
            let parent_key = (host.clone(), parent.to_string());
            if !self.sessions.contains_key(&parent_key)
                && !self
                    .subagent_viewers
                    .keys()
                    .any(|(h, p, _)| h == host && p == parent)
            {
                self.provider_subagents.remove(&parent_key);
            }
        }
    }

    /// Loads the page before what a subagent conversation shows.
    pub fn fetch_older_subagent(
        &mut self,
        host: &HostId,
        parent: &str,
        subagent: &str,
        cx: &mut Context<Self>,
    ) {
        let key = (host.clone(), parent.to_string(), subagent.to_string());
        let Some(session) = self.subagent_sessions.get(&key) else {
            return;
        };
        let Some(page) = Page::older(&session.read(cx).timeline, fleet::sync::PAGE) else {
            return;
        };
        self.fetch_subagent(host, parent, subagent, page, cx);
    }

    fn fetch_subagent(
        &mut self,
        host: &HostId,
        parent: &str,
        subagent: &str,
        page: Page,
        cx: &mut Context<Self>,
    ) {
        let key = (host.clone(), parent.to_string(), subagent.to_string());
        let Some(session) = self.subagent_sessions.get(&key).cloned() else {
            return;
        };
        if !session.update(cx, |session, _| session.fetch.begin(&page)) {
            return;
        }
        let (direction, cursor, limit) = match &page {
            Page::Tail => ("tail", None, fleet::sync::PAGE),
            Page::After { epoch, seq } => ("after", Some((epoch.as_str(), *seq)), 0),
            Page::Before { epoch, seq, .. } => {
                ("before", Some((epoch.as_str(), *seq)), fleet::sync::PAGE)
            }
        };
        let message =
            requests::provider_subagent_timeline(parent, subagent, direction, cursor, limit);
        let task = self.request(host, message, cx);
        let (host, parent, subagent) = (host.clone(), parent.to_string(), subagent.to_string());
        cx.spawn(async move |this, cx| {
            let result = task
                .await
                .and_then(|payload| match payload["error"].as_str() {
                    Some(error) => Err(error.to_string()),
                    None => Ok(fleet::subagents::page(&payload)),
                });
            let _ = this.update(cx, |store, cx| {
                let key = (host.clone(), parent.clone(), subagent.clone());
                let Some(session) = store.subagent_sessions.get(&key).cloned() else {
                    return;
                };
                let next = session.update(cx, |session, cx| {
                    session.loading = false;
                    let landed = session.fetch.land(&mut session.timeline, &result);
                    session.error = landed.error;
                    cx.notify();
                    landed.next
                });
                if let Some(page) = next {
                    store.fetch_subagent(&host, &parent, &subagent, page, cx);
                }
            });
        })
        .detach();
    }

    /// Says to each host which subagents this client has open in tabs, as
    /// Paseo does, so the daemon keeps them when their parent is archived.
    pub fn mark_open_subagent_tabs(
        &mut self,
        tabs: &BTreeSet<(HostId, String)>,
        cx: &mut Context<Self>,
    ) {
        let label = format!("paseo.open-agent-tab.{}", self.client_id);
        let desired: BTreeSet<_> = tabs
            .iter()
            .filter(|(host, agent)| {
                self.fleet.agent(host, agent).is_some_and(|record| {
                    record.parent_agent_id.is_some() && record.archived_at.is_none()
                })
            })
            .cloned()
            .collect();
        let marked: BTreeSet<_> = desired
            .iter()
            .filter(|(host, agent)| {
                self.fleet.agent(host, agent).is_some_and(|record| {
                    record.labels.get(&label).and_then(Value::as_str) == Some("true")
                })
            })
            .cloned()
            .collect();
        let online: BTreeSet<_> = self
            .links
            .iter()
            .filter(|(_, link)| link.sync.online())
            .map(|(host, _)| host.clone())
            .collect();
        self.subagent_tabs = desired.clone();
        prune_tab_marks(
            &mut self.open_tab_marks,
            &self.fleet,
            &self.links.keys().cloned().collect(),
        );
        self.tab_mark_retry_at.retain(|(host, agent, _), _| {
            self.open_tab_marks.contains(&(host.clone(), agent.clone()))
        });
        let actions = subagent_tab_actions(&mut self.open_tab_marks, &desired, &marked, &online);
        for (host, agent, open) in actions {
            let attempt = (host.clone(), agent.clone(), open);
            if self
                .tab_mark_retry_at
                .get(&attempt)
                .is_some_and(|at| now_millis() < *at)
                || !self.tab_mark_pending.insert(attempt.clone())
            {
                continue;
            }
            let task = self.request(
                &host,
                requests::set_open_agent_tab(&agent, &self.client_id, open),
                cx,
            );
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |store, cx| {
                    store.tab_mark_pending.remove(&attempt);
                    let delivered = result.as_ref().is_ok_and(|payload| {
                        let error = payload["error"].as_str().unwrap_or("").to_ascii_lowercase();
                        error.is_empty()
                            || (!open
                                && (error.contains("no such agent")
                                    || error.contains("agent not found")))
                    });
                    if delivered {
                        store.tab_mark_retry_at.remove(&attempt);
                    } else {
                        store
                            .tab_mark_retry_at
                            .insert(attempt.clone(), now_millis() + 30_000);
                    }
                    if delivered && !open {
                        let key = (host.clone(), agent.clone());
                        if store.subagent_tabs.contains(&key) {
                            // The tab reopened while its close was in flight.
                            store.act(
                                &host,
                                requests::set_open_agent_tab(&agent, &store.client_id, true),
                                cx,
                            );
                        } else {
                            store.open_tab_marks.remove(&key);
                        }
                    }
                });
            })
            .detach();
        }
    }
}

fn prune_tab_marks(
    tracked: &mut BTreeSet<(HostId, String)>,
    fleet: &Fleet,
    configured: &BTreeSet<HostId>,
) {
    tracked.retain(|(host, agent)| {
        configured.contains(host)
            && !fleet.host(host).is_some_and(|host| {
                host.connection.is_online()
                    && host.directory.agents_hydrated
                    && !host.directory.agents.contains_key(agent)
            })
    });
}

fn subagent_tab_actions(
    tracked: &mut BTreeSet<(HostId, String)>,
    desired: &BTreeSet<(HostId, String)>,
    marked: &BTreeSet<(HostId, String)>,
    online: &BTreeSet<HostId>,
) -> Vec<(HostId, String, bool)> {
    tracked.extend(desired.iter().cloned());
    desired
        .difference(marked)
        .filter(|(host, _)| online.contains(host))
        .map(|(host, agent)| (host.clone(), agent.clone(), true))
        .chain(
            tracked
                .difference(desired)
                .filter(|(host, _)| online.contains(host))
                .map(|(host, agent)| (host.clone(), agent.clone(), false)),
        )
        .collect()
}

fn release_viewer<K: std::hash::Hash + Eq>(viewers: &mut HashMap<K, usize>, key: &K) -> bool {
    let Some(count) = viewers.get_mut(key) else {
        return false;
    };
    if *count > 1 {
        *count -= 1;
        false
    } else {
        viewers.remove(key);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet::{Connection, HostConfig};

    #[test]
    fn tab_marks_wait_for_authoritative_directory_and_drop_removed_hosts() {
        let key = (HostId::new("mac"), "child".to_string());
        let configured = BTreeSet::from([HostId::new("mac")]);
        let mut fleet = Fleet::new([HostConfig {
            name: "mac".into(),
            ..Default::default()
        }]);
        let mut tracked = BTreeSet::from([key.clone(), (HostId::new("removed"), "child".into())]);
        prune_tab_marks(&mut tracked, &fleet, &configured);
        assert_eq!(tracked, BTreeSet::from([key.clone()]));
        fleet.hosts[0].connection = Connection::Online;
        prune_tab_marks(&mut tracked, &fleet, &configured);
        assert!(tracked.contains(&key));
        fleet.hosts[0].directory.agents_hydrated = true;
        prune_tab_marks(&mut tracked, &fleet, &configured);
        assert!(tracked.is_empty());
        tracked.insert(key.clone());
        fleet.hosts[0].connection = Connection::Offline("test".into());
        prune_tab_marks(&mut tracked, &fleet, &configured);
        assert!(tracked.contains(&key));
    }

    #[test]
    fn restored_subagent_tab_clears_its_existing_mark_after_offline_close() {
        let key = (HostId::new("mac"), "child".to_string());
        let mut tracked = BTreeSet::new();
        let desired = BTreeSet::from([key.clone()]);
        let online = BTreeSet::from([HostId::new("mac")]);
        assert!(subagent_tab_actions(&mut tracked, &desired, &desired, &online).is_empty());
        assert!(tracked.contains(&key));
        assert!(
            subagent_tab_actions(&mut tracked, &BTreeSet::new(), &desired, &BTreeSet::new())
                .is_empty()
        );
        assert!(tracked.contains(&key));
        assert_eq!(
            subagent_tab_actions(&mut tracked, &BTreeSet::new(), &desired, &online),
            vec![(HostId::new("mac"), "child".into(), false)]
        );
    }

    #[test]
    fn shared_subagent_survives_until_the_last_view_closes() {
        let mut viewers = HashMap::from([("subagent", 2)]);
        assert!(!release_viewer(&mut viewers, &"subagent"));
        assert_eq!(viewers["subagent"], 1);
        assert!(release_viewer(&mut viewers, &"subagent"));
        assert!(!release_viewer(&mut viewers, &"subagent"));
        assert!(viewers.is_empty());
    }
}
