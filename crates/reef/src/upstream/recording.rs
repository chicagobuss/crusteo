//! The daemons' side of recording ([`crate::record`]): following every
//! agent, and reading the pages the recorder asks for.

use serde_json::json;

use super::{Upstream, guarded};
use crate::config::host_id;
use crate::record::{self, Key, Read, event::Context};

/// Entries per page when catching an agent up.
const PAGE: u32 = 200;

impl Upstream {
    /// About once a second: `record.json` again if it changed, the reads
    /// due, the sinks' next batches, the cursors they moved.
    pub fn record_tick(&mut self) {
        // New settings: follow every agent, or stop, and start any new
        // sink at each agent's end.
        if self.record.reload() {
            let hosts: Vec<String> = self.links.keys().cloned().collect();
            for host in hosts {
                self.resubscribe_timelines(&host, false);
                self.record_host_ready(&host);
            }
        }
        if !self.record.following() {
            return;
        }
        self.record_reads();
        self.record.pump();
        self.record.save();
    }

    /// Starts the reads due, keeping [`record::READS`] in flight.
    fn record_reads(&mut self) {
        let room = record::READS.saturating_sub(self.record.reads);
        for ((host, agent), read) in self.record.due(room) {
            self.record_read(&host, &agent, read);
        }
    }

    /// The agents whose timelines Reef streams on `host`: those phones
    /// have open, and every one when recording.
    pub(super) fn timeline_agents(&self, host: &str) -> Vec<String> {
        let mut agents = (self.links.get(host))
            .map(|link| link.watched.clone())
            .unwrap_or_default();
        if self.record.following()
            && let Some(host) = self.fleet.host(&host_id(host))
        {
            agents.extend(host.directory.agents.keys().cloned());
        }
        agents.into_iter().collect()
    }

    /// A host's agent list is complete: follow them all, and read those
    /// that changed while Reef was away.
    pub(super) fn record_host_ready(&mut self, host: &str) {
        if !self.record.following() {
            return;
        }
        let Some(listed) = self.fleet.host(&host_id(host)) else {
            return;
        };
        let agents: Vec<(String, Option<String>)> = (listed.directory.agents.values())
            .map(|agent| (agent.id.clone(), agent.updated_at.clone()))
            .collect();
        for (agent, updated) in agents {
            let key = (host.to_string(), agent);
            if self.record.stale(&key, updated.as_deref()) {
                self.record.touch(key, false);
            }
        }
    }

    fn record_read(&mut self, host: &str, agent: &str, read: Read) {
        let key = (host.to_string(), agent.to_string());
        let Some(link) = self.links.get(host).filter(|link| link.online) else {
            self.record.unread(&key);
            return;
        };
        let message = match read {
            Read::Tail => json!({
                "type": "fetch_agent_timeline_request", "agentId": agent, "direction": "tail",
                "limit": 1, "projection": "projected",
            }),
            Read::After(cursor) => json!({
                "type": "fetch_agent_timeline_request", "agentId": agent, "direction": "after",
                "cursor": { "epoch": cursor.epoch, "seq": cursor.seq }, "limit": PAGE,
                "projection": "projected",
            }),
        };
        let reply = guarded(&link.client, message);
        let apply = self.apply.clone();
        tokio::spawn(async move {
            let result = reply
                .await
                .and_then(|payload| match payload["error"].as_str() {
                    Some(error) => Err(error.to_string()),
                    None => Ok(payload),
                });
            let _ = apply.send(Box::new(move |up: &mut Upstream| {
                up.record_page(key, result)
            }));
        });
    }

    fn record_page(&mut self, key: Key, result: Result<serde_json::Value, String>) {
        let Some(context) = self.record_context(&key) else {
            // Gone meanwhile.
            self.record.unread(&key);
            self.record.forget(&key);
            return;
        };
        let running = self.running(&key.0, &key.1);
        let updated =
            (self.fleet.agent(&host_id(&key.0), &key.1)).and_then(|agent| agent.updated_at.clone());
        if let Some(cursor) = self
            .record
            .on_page(&key, &context, running, updated, result)
        {
            self.record.read_on(&key, &cursor);
            self.record_read(&key.0, &key.1, Read::After(cursor));
        }
        // The next agent waiting goes now, not at the next tick.
        self.record_reads();
        self.record.pump();
    }

    /// Who an agent is, as records carry it.
    fn record_context(&self, key: &Key) -> Option<Context> {
        let host = host_id(&key.0);
        let agent = self.fleet.agent(&host, &key.1)?;
        let workspace =
            (agent.workspace_id.as_deref()).and_then(|id| self.fleet.workspace(&host, id));
        Some(Context {
            host: key.0.clone(),
            agent: json!({
                "id": agent.id, "title": agent.display_title(), "provider": agent.provider,
                "model": agent.model,
            }),
            workspace: json!({
                "id": agent.workspace_id,
                "title": workspace.map(|w| w.title.clone().unwrap_or_else(|| w.name.clone())),
                "labels": workspace.map(|w| w.labels.clone()).unwrap_or_default(),
                "cwd": agent.cwd,
                "branch": workspace.and_then(|w| w.branch()),
            }),
        })
    }
}
