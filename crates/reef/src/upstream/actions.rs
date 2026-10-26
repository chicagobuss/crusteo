//! The phone's actions, as daemon writes: answering permissions and
//! questions, stopping a turn, sending (or queueing) a message, and
//! archiving. Each goes through the guard like every other request; a
//! type not on [`super::WRITE_REQUESTS`] is refused before it leaves.
//!
//! Also the text for turn-finished notifications, which only reads.

use std::time::Duration;

use serde_json::{Value, json};

use super::{Upstream, guarded, guarded_with_timeout};

/// Writes can take a while on a busy daemon (archive waits for the
/// provider to shut down); the reference client allows a minute.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Messages one agent may have waiting on Reef.
pub const QUEUE_MAX: usize = 20;
/// The Paseo app waits this long for a permission to be confirmed.
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(15);

/// Where an action's answer goes: the phone's connection and its `req`.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub conn: u64,
    pub req: Option<String>,
}

/// A message waiting for its agent's turn to end.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    /// The message id the phone made; the daemon dedupes on it.
    pub id: String,
    pub prompt: Prompt,
    /// Why sending it failed; it then waits for the phone to retry or
    /// drop it.
    pub error: Option<String>,
}

/// A reply that says no in its own way: an `error`, or `accepted: false`.
fn rejected(payload: &Value) -> Option<String> {
    payload["error"]
        .as_str()
        .map(str::to_string)
        .or_else(|| (payload["accepted"] == false).then(|| "The host declined".to_string()))
}

pub use crate::preview::first_line;
use fleet::prompt::Prompt;

use crate::config::host_id;

impl Upstream {
    /// Queues an action's answer for its phone.
    pub(super) fn answer(&mut self, reply: &Reply, result: Result<Value, String>) {
        match result {
            Ok(extra) => {
                let message = crate::proto::result_ok(reply.req.as_deref(), extra);
                self.outbox.push((reply.conn, message));
            }
            Err(error) => self.refuse(reply, crate::proto::code::UPSTREAM, &error),
        }
    }

    /// Queues a failed answer with its own code.
    pub(super) fn refuse(&mut self, reply: &Reply, code: &str, error: &str) {
        let message = crate::proto::result_err(reply.req.as_deref(), code, error);
        self.outbox.push((reply.conn, message));
    }

    /// Sends a write and hands its outcome to `done` on Reef's task.
    pub(super) fn write(
        &mut self,
        host: &str,
        message: Value,
        timeout: Duration,
        done: impl FnOnce(&mut Upstream, Result<Value, String>) + Send + 'static,
    ) {
        let Some(link) = self.links.get(host).filter(|link| link.online) else {
            done(self, Err("The host is offline".into()));
            return;
        };
        let about = (message["agentId"].as_str())
            .or(message["workspaceId"].as_str())
            .or(message["config"]["cwd"].as_str())
            .unwrap_or_default();
        eprintln!(
            "reef: {host}: {} for {about}",
            message["type"].as_str().unwrap_or_default(),
        );
        let reply = guarded_with_timeout(&link.client, message, timeout);
        let apply = self.apply.clone();
        tokio::spawn(async move {
            let result = reply.await.and_then(|payload| match rejected(&payload) {
                Some(error) => Err(error),
                None => Ok(payload),
            });
            let _ = apply.send(Box::new(move |up: &mut Upstream| done(up, result)));
        });
    }

    /// The pending permission or question `id` of an agent, as the daemon
    /// sent it.
    pub fn pending_request(&self, host: &str, agent: &str, id: &str) -> Option<Value> {
        let from_directory = self.fleet.agent(&host_id(host), agent).and_then(|a| {
            a.pending_permissions
                .iter()
                .find(|p| p["id"] == id)
                .cloned()
        });
        from_directory.or_else(|| {
            let session = self.sessions.get(&(host.to_string(), agent.to_string()))?;
            session
                .timeline
                .permissions
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.request.clone())
        })
    }

    /// Answers a permission or a question. The daemon confirms with
    /// `agent_permission_resolved`, which carries the permission id as
    /// its request id and so comes back as this request's reply.
    pub fn respond_permission(
        &mut self,
        host: &str,
        agent: &str,
        permission: &str,
        response: Value,
        reply: Reply,
    ) {
        let message = json!({
            "type": "agent_permission_response",
            "agentId": agent,
            "requestId": permission,
            "response": response,
        });
        let (h, a, p) = (host.to_string(), agent.to_string(), permission.to_string());
        self.write(host, message, PERMISSION_TIMEOUT, move |up, result| {
            if result.is_ok() {
                up.resolved(&h, &a, &p);
            }
            up.answer(&reply, result.map(|_| json!({})));
        });
    }

    /// A permission is settled: take it off the agent now rather than when
    /// the daemon's next snapshot comes.
    fn resolved(&mut self, host: &str, agent: &str, permission: &str) {
        let key = (host.to_string(), agent.to_string());
        if let Some(session) = self.sessions.get_mut(&key) {
            session.timeline.apply_permission_event(
                "agent_permission_resolved",
                &json!({ "requestId": permission }),
            );
            self.dirty_sessions.insert(key);
        }
        if let Some(found) = self
            .fleet
            .host_mut(&host_id(host))
            .and_then(|h| h.directory.agents.get_mut(agent))
        {
            found.pending_permissions.retain(|p| p["id"] != permission);
            if let Some(pending) = found.raw["pendingPermissions"].as_array_mut() {
                pending.retain(|p| p["id"] != permission);
            }
            self.dirty_fleet = true;
        }
    }

    /// Stops the agent's turn.
    pub fn stop(&mut self, host: &str, agent: &str, reply: Reply) {
        let message = json!({ "type": "cancel_agent_request", "agentId": agent });
        // The phone stopped it: the turn ending is no news to it. The end
        // may show before the daemon's reply does.
        let quiet = std::time::Instant::now() + Duration::from_secs(15);
        self.quiet_until
            .insert((host.to_string(), agent.to_string()), quiet);
        self.write(host, message, WRITE_TIMEOUT, move |up, result| {
            up.answer(&reply, result.map(|_| json!({})));
        });
    }

    /// Sends a message. While the agent is in a turn it waits on Reef
    /// and goes when the turn ends, as the Paseo app's queue does; with
    /// `steer` it goes now, into the running turn.
    pub fn send(
        &mut self,
        host: &str,
        agent: &str,
        prompt: Prompt,
        id: String,
        steer: bool,
        reply: Reply,
    ) {
        let key = (host.to_string(), agent.to_string());
        if self
            .queues
            .get(&key)
            .is_some_and(|queue| queue.iter().any(|m| m.id == id))
        {
            // The phone sent it twice; it is already waiting.
            self.answer(&reply, Ok(json!({ "queued": true })));
            return;
        }
        let running = self.running(host, agent);
        // A message on its way, or others waiting, also make it wait: a new
        // one never jumps the queue.
        let busy = running
            || self.sending.contains(&key)
            || self.queues.get(&key).is_some_and(|queue| !queue.is_empty());
        if busy && !steer {
            let queue = self.queues.entry(key.clone()).or_default();
            if queue.len() >= QUEUE_MAX {
                let error =
                    format!("{QUEUE_MAX} messages are waiting already; send or drop some first");
                self.answer(&reply, Err(error));
                return;
            }
            queue.push(Queued {
                id,
                prompt,
                error: None,
            });
            self.dirty_sessions.insert(key);
            self.answer(&reply, Ok(json!({ "queued": true })));
            // Waiting only behind messages that failed: it can go now.
            self.drain_queue(host, agent);
            return;
        }
        let behavior = (running && steer).then_some("steer");
        self.send_now(host, agent, prompt, id, behavior, Some(reply));
    }

    /// Sends a queued message now, into the running turn.
    pub fn steer_queued(&mut self, host: &str, agent: &str, id: &str, reply: Reply) {
        let key = (host.to_string(), agent.to_string());
        let Some(message) = self.take_queued(&key, id) else {
            self.answer(&reply, Err("That message is no longer waiting".into()));
            return;
        };
        let behavior = self.running(host, agent).then_some("steer");
        self.send_now(
            host,
            agent,
            message.prompt,
            message.id,
            behavior,
            Some(reply),
        );
    }

    /// Drops a queued message.
    pub fn unqueue(&mut self, host: &str, agent: &str, id: &str, reply: Reply) {
        let key = (host.to_string(), agent.to_string());
        let result = match self.take_queued(&key, id) {
            Some(_) => Ok(json!({})),
            None => Err("That message is no longer waiting".into()),
        };
        self.answer(&reply, result);
    }

    fn take_queued(&mut self, key: &(String, String), id: &str) -> Option<Queued> {
        let queue = self.queues.get_mut(key)?;
        let index = queue.iter().position(|m| m.id == id)?;
        let message = queue.remove(index);
        if queue.is_empty() {
            self.queues.remove(key);
        }
        self.dirty_sessions.insert(key.clone());
        Some(message)
    }

    /// Sends the first waiting message that has not failed, if the agent is
    /// idle and nothing is on its way already. Called on every idle edge,
    /// and again when a send finishes. A message that failed stays at the
    /// front, marked, for the phone to retry or drop; it is never resent
    /// on its own.
    pub(super) fn drain_queue(&mut self, host: &str, agent: &str) {
        let key = (host.to_string(), agent.to_string());
        if self.sending.contains(&key) {
            // Look again when that send finishes: this edge would be lost.
            self.missed_edges.insert(key);
            return;
        }
        if self.running(host, agent) {
            return;
        }
        let Some(queue) = self.queues.get_mut(&key) else {
            return;
        };
        let Some(index) = queue.iter().position(|m| m.error.is_none()) else {
            return;
        };
        let next = queue.remove(index);
        if queue.is_empty() {
            self.queues.remove(&key);
        }
        self.dirty_sessions.insert(key);
        self.send_now(host, agent, next.prompt, next.id, None, None);
    }

    fn send_now(
        &mut self,
        host: &str,
        agent: &str,
        prompt: Prompt,
        id: String,
        behavior: Option<&'static str>,
        reply: Option<Reply>,
    ) {
        let key = (host.to_string(), agent.to_string());
        if let Some(session) = self.sessions.get_mut(&key) {
            // Shown at once; the daemon's echo of the message replaces it.
            session
                .timeline
                .add_local_prompt(&id, &prompt, &fleet::clock::now_iso());
            self.dirty_sessions.insert(key.clone());
        }
        self.sending.insert(key.clone());
        let mut message = json!({
            "type": "send_agent_message_request",
            "agentId": agent,
            "text": prompt.text,
            "messageId": id,
        });
        if let Some(behavior) = behavior {
            message["activeTurnBehavior"] = json!(behavior);
        }
        if !prompt.images.is_empty() {
            message["images"] = json!(prompt.images);
        }
        self.write(host, message, WRITE_TIMEOUT, move |up, result| {
            up.sending.remove(&key);
            // A failed send leaves the agent as it was, so the next message
            // may go; after a sent one, only if a turn ended meanwhile.
            let missed = up.missed_edges.remove(&key);
            let drain = result.is_err() || missed;
            if let Err(error) = &result {
                if let Some(session) = up.sessions.get_mut(&key) {
                    session.timeline.remove_local_user(&id);
                }
                if reply.is_none() {
                    // From the queue: back to its front, marked, for the
                    // phone to retry or drop.
                    up.queues.entry(key.clone()).or_default().insert(
                        0,
                        Queued {
                            id,
                            prompt,
                            error: Some(error.clone()),
                        },
                    );
                }
                up.dirty_sessions.insert(key.clone());
            }
            if let Some(reply) = reply {
                up.answer(&reply, result.map(|_| json!({ "sent": true })));
            }
            if drain {
                up.drain_queue(&key.0, &key.1);
            }
        });
    }

    /// Archives the agent.
    pub fn archive(&mut self, host: &str, agent: &str, reply: Reply) {
        let message = json!({ "type": "archive_agent_request", "agentId": agent });
        let key = (host.to_string(), agent.to_string());
        self.write(host, message, WRITE_TIMEOUT, move |up, result| {
            if result.is_ok() {
                up.queues.remove(&key);
            }
            up.answer(&reply, result.map(|_| json!({})));
        });
    }

    // ---- notifications --------------------------------------------------

    /// Makes the notification for a turn that ended: the agent's title and
    /// the first line of its last reply, read from its open conversation
    /// or, failing that, from the end of its timeline.
    pub fn describe_turn(&mut self, host: &str, agent: &str, failed: bool) {
        let Some(found) = self.fleet.agent(&host_id(host), agent) else {
            return;
        };
        let title = found.display_title();
        let error = found.raw["lastError"].as_str().map(str::to_string);
        let key = (host.to_string(), agent.to_string());
        if self
            .sessions
            .get(&key)
            .is_some_and(|session| session.timeline.baseline)
        {
            self.reply_from_session(host, agent);
            let text = self.replies.known.get(&key).map(|r| r.line.clone());
            self.notice(host, agent, title, failed, text.or(error));
            return;
        }
        let Some(link) = self.links.get(host).filter(|link| link.online) else {
            self.notice(host, agent, title, failed, error);
            return;
        };
        let reply = guarded(&link.client, fleet::requests::last_reply(agent));
        let apply = self.apply.clone();
        tokio::spawn(async move {
            let found = reply
                .await
                .ok()
                .as_ref()
                .and_then(fleet::preview::newest_reply);
            let _ = apply.send(Box::new(move |up: &mut Upstream| {
                // The row's reply line comes from the same read.
                let text = found.map(|(text, at)| {
                    up.set_reply(key.clone(), &text, at);
                    text
                });
                up.notice(&key.0, &key.1, title, failed, text.or(error));
            }));
        });
    }
    fn notice(
        &mut self,
        host: &str,
        agent: &str,
        title: String,
        failed: bool,
        body: Option<String>,
    ) {
        self.next_notice += 1;
        self.notices.push(json!({
            "type": "notification",
            "id": format!("n{}", self.next_notice),
            "kind": "turn_finished",
            "state": if failed { "failed" } else { "done" },
            "key": crate::views::agent_key(host, agent),
            "host": host,
            "agent": agent,
            "title": title,
            "body": body.as_deref().map(first_line).unwrap_or_default(),
            "at": fleet::clock::now_iso(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notification_shows_the_first_line_worth_reading() {
        assert_eq!(first_line("\n\n## Done\n\nAll tests pass."), "Done");
        assert_eq!(first_line("  plain  "), "plain");
        assert_eq!(first_line(&"x".repeat(300)).chars().count(), 201);
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn a_declined_reply_is_an_error() {
        assert_eq!(rejected(&json!({"accepted": true, "error": null})), None);
        assert!(rejected(&json!({"accepted": false})).is_some());
        assert_eq!(rejected(&json!({"error": "nope"})).as_deref(), Some("nope"));
    }
}
