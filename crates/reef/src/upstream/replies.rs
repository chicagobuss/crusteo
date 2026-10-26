//! The first line of each agent's latest reply, for list rows. Read from
//! an open conversation when there is one; otherwise fetched with a short
//! tail read, but only for agents some phone's window shows, a few at a
//! time, and again only when a turn ends. A fleet of hundreds of agents
//! never causes a burst of reads.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use fleet::preview::newest_reply;
use fleet::requests;

use super::actions::first_line;
use super::{Upstream, guarded};
use crate::config::host_id;
use crate::lists::LastReply;

/// Tail reads in flight at once, across the fleet.
const IN_FLIGHT: usize = 4;
/// An agent whose read failed or found no reply is not asked again
/// sooner than this, unless a turn of it ends.
const RETRY: Duration = Duration::from_secs(300);

type Key = (String, String);

#[derive(Default)]
pub struct Replies {
    pub known: HashMap<Key, LastReply>,
    queue: VecDeque<Key>,
    queued: HashSet<Key>,
    in_flight: usize,
    tried: HashMap<Key, Instant>,
}

impl Upstream {
    /// Asks for the replies of agents a phone shows, if not known yet.
    pub fn want_replies(&mut self, keys: impl IntoIterator<Item = Key>) {
        let now = Instant::now();
        for key in keys {
            let r = &mut self.replies;
            if r.known.contains_key(&key)
                || r.queued.contains(&key)
                || r.tried.get(&key).is_some_and(|at| now - *at < RETRY)
            {
                continue;
            }
            r.queued.insert(key.clone());
            r.queue.push_back(key);
        }
    }

    /// A turn ended: the reply is worth reading again, before others.
    pub fn refresh_reply(&mut self, host: &str, agent: &str) {
        let key = (host.to_string(), agent.to_string());
        if self.sessions.contains_key(&key) {
            self.reply_from_session(host, agent);
            return;
        }
        let r = &mut self.replies;
        r.tried.remove(&key);
        if r.queued.insert(key.clone()) {
            r.queue.push_front(key);
        }
    }

    /// The reply as an open conversation has it.
    pub fn reply_from_session(&mut self, host: &str, agent: &str) {
        let key = (host.to_string(), agent.to_string());
        let Some(session) = self.sessions.get(&key) else {
            return;
        };
        let timeline = &session.timeline;
        let mut newest_first = timeline.head.iter().rev().chain(timeline.tail.iter().rev());
        let found = newest_first.find_map(|item| match &item.kind {
            fleet::timeline::ItemKind::Assistant { text, .. } if !text.trim().is_empty() => {
                Some((text.clone(), item.timestamp.clone()))
            }
            _ => None,
        });
        if let Some((text, at)) = found {
            self.set_reply(key, &text, at);
        }
    }

    /// Stores a reply; the lists show it at the next flush.
    pub fn set_reply(&mut self, key: Key, text: &str, at: String) {
        let reply = LastReply {
            line: first_line(text),
            at,
        };
        if self.replies.known.get(&key) != Some(&reply) {
            self.replies.known.insert(key, reply);
            self.dirty_fleet = true;
        }
    }

    /// Forgets a removed host's replies. Reads in flight still count
    /// down when they land.
    pub(super) fn forget_host(&mut self, host: &str) {
        self.replies.known.retain(|key, _| key.0 != host);
        self.replies.queue.retain(|key| key.0 != host);
        self.replies.queued.retain(|key| key.0 != host);
        self.replies.tried.retain(|key, _| key.0 != host);
    }

    /// Forgets one agent's reply, for an agent that is gone.
    pub(super) fn forget_agent_reply(&mut self, key: &Key) {
        self.replies.known.remove(key);
        self.replies.queue.retain(|k| k != key);
        self.replies.queued.remove(key);
        self.replies.tried.remove(key);
    }

    /// Starts tail reads from the queue, a few at a time.
    pub fn pump_replies(&mut self) {
        while self.replies.in_flight < IN_FLIGHT {
            let Some(key) = self.replies.queue.pop_front() else {
                return;
            };
            self.replies.queued.remove(&key);
            if self.sessions.contains_key(&key) {
                self.reply_from_session(&key.0, &key.1);
                continue;
            }
            let Some(link) = self.links.get(&key.0).filter(|link| link.online) else {
                continue;
            };
            if self.fleet.agent(&host_id(&key.0), &key.1).is_none() {
                continue;
            }
            self.replies.tried.insert(key.clone(), Instant::now());
            self.replies.in_flight += 1;
            let reply = guarded(&link.client, requests::last_reply(&key.1));
            let apply = self.apply.clone();
            tokio::spawn(async move {
                let found = reply.await.ok().as_ref().and_then(newest_reply);
                let _ = apply.send(Box::new(move |up: &mut Upstream| {
                    up.replies.in_flight -= 1;
                    if let Some((text, at)) = found {
                        up.set_reply(key, &text, at);
                    }
                }));
            });
        }
    }
}
