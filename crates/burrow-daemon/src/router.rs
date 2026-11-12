//! The Reef that routes for this Burrow: how its agents reach agents on
//! other hosts ([`burrow_protocol::routing`]). A Reef connects as a client
//! like any other and says whether its owner lets it route; Burrow asks the
//! most recent one that does. Without one, an agent elsewhere is out of
//! reach, and [`Reach`] says why in words an agent can act on.
//!
//! The switch is not a lock: Burrow has no passwords, so anything that
//! reaches this Burrow can prompt its agents, routed or not.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use burrow_protocol::envelope::session;
use burrow_protocol::routing::{REQUEST, code};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::agent_requests::Late;

/// How long a Reef has to answer.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(30);

struct Link {
    connection: u64,
    tx: Late,
    reef: String,
    on: bool,
    /// When it last said so: the latest wins.
    order: u64,
}

struct Waiting {
    connection: u64,
    reply: oneshot::Sender<Result<Value, Refused>>,
}

#[derive(Default)]
struct Inner {
    links: Vec<Link>,
    waiting: HashMap<String, Waiting>,
    next: u64,
}

#[derive(Default)]
pub struct Router {
    inner: Mutex<Inner>,
}

/// Whether agents here can reach other hosts now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// No Reef is connected.
    NoReef,
    /// A Reef is, and its owner has not turned routing on.
    Off,
    On,
}

/// A Reef's refusal, or why it could not be asked.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    /// One of [`code`]'s, or `unreachable`.
    pub code: String,
    pub error: String,
    pub detail: Value,
}

impl Refused {
    fn unreachable(error: &str) -> Self {
        Self {
            code: "unreachable".into(),
            error: error.into(),
            detail: Value::Null,
        }
    }
}

impl Router {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A Reef on `connection` says whether it routes.
    pub fn announce(&self, connection: u64, tx: &Late, reef: &str, on: bool) {
        let mut inner = self.lock();
        inner.next += 1;
        let order = inner.next;
        inner.links.retain(|l| l.connection != connection);
        inner.links.push(Link {
            connection,
            tx: tx.clone(),
            reef: reef.to_string(),
            on,
            order,
        });
        tracing::info!(reef, on, "a Reef says whether it routes for this host");
    }

    /// The connection closed: its Reef no longer routes, and what waited
    /// on it is told so.
    pub fn gone(&self, connection: u64) {
        let mut inner = self.lock();
        inner.links.retain(|l| l.connection != connection);
        let dropped: Vec<String> = (inner.waiting.iter())
            .filter(|(_, w)| w.connection == connection)
            .map(|(id, _)| id.clone())
            .collect();
        for id in dropped {
            if let Some(waiting) = inner.waiting.remove(&id) {
                let _ = (waiting.reply).send(Err(Refused::unreachable(
                    "The Reef's connection to this host closed before it answered.",
                )));
            }
        }
    }

    /// Whether the client on `connection` is a Reef that routes now.
    pub fn routes_for(&self, connection: u64) -> bool {
        (self.lock().links.iter()).any(|l| l.connection == connection && l.on)
    }

    pub fn reach(&self) -> Reach {
        let inner = self.lock();
        if inner.links.iter().any(|l| l.on) {
            Reach::On
        } else if inner.links.is_empty() {
            Reach::NoReef
        } else {
            Reach::Off
        }
    }

    /// Asks the latest Reef that routes to do `op`; its result, or why not.
    pub async fn ask(&self, op: &str, mut args: Value) -> Result<Value, Refused> {
        let request_id = format!("route_{}", uuid::Uuid::new_v4());
        let rx = {
            let mut inner = self.lock();
            let Some(link) = (inner.links.iter().filter(|l| l.on)).max_by_key(|l| l.order) else {
                return Err(match inner.links.is_empty() {
                    true => Refused::unreachable("No Reef is connected."),
                    false => Refused {
                        code: code::OFF.into(),
                        error: "Routing is off in this fleet's Reef.".into(),
                        detail: Value::Null,
                    },
                });
            };
            args["requestId"] = request_id.clone().into();
            args["op"] = op.into();
            let connection = link.connection;
            tracing::debug!(reef = %link.reef, op, "asking the Reef");
            if link.tx.send(session(REQUEST, &args)).is_err() {
                return Err(Refused::unreachable("The Reef's connection is closing."));
            }
            let (reply, rx) = oneshot::channel();
            inner
                .waiting
                .insert(request_id.clone(), Waiting { connection, reply });
            rx
        };
        let answer = tokio::time::timeout(ANSWER_WITHIN, rx).await;
        self.lock().waiting.remove(&request_id);
        match answer {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(Refused::unreachable(
                "The Reef went away before it answered.",
            )),
            Err(_) => Err(Refused::unreachable(
                "The Reef did not answer within 30 seconds.",
            )),
        }
    }

    /// A Reef's answer (`burrow.fleet.response`).
    pub fn answered(&self, message: &Value) {
        let Some(id) = message["requestId"].as_str() else {
            return;
        };
        let Some(waiting) = self.lock().waiting.remove(id) else {
            return;
        };
        let answer = if message["ok"] == true {
            Ok(message["result"].clone())
        } else {
            Err(Refused {
                code: (message["code"].as_str()).unwrap_or("refused").to_string(),
                error: (message["error"].as_str())
                    .unwrap_or("The Reef refused.")
                    .to_string(),
                detail: message["detail"].clone(),
            })
        };
        let _ = waiting.reply.send(answer);
    }
}

/// What an agent is told when `key` is not on this host and it cannot be
/// routed, or the Reef could not route it.
pub fn not_here(key: &str, reach: &Reach, refused: Option<&Refused>) -> String {
    match (reach, refused) {
        (Reach::NoReef, _) => format!(
            "Agent {key} is not on this host, and no Reef is connected to route it: give its host (`burrow send --host`), or connect this host to a Reef."
        ),
        (Reach::Off, _) => off(key),
        (Reach::On, Some(refused)) if refused.code == code::OFF => off(key),
        (Reach::On, Some(refused)) if refused.code == code::AMBIGUOUS => {
            let lines: Vec<String> = (refused.detail["candidates"].as_array().into_iter())
                .flatten()
                .map(|c| {
                    format!(
                        "- {} ({}) on {}, {}",
                        c["agentId"].as_str().unwrap_or_default(),
                        c["title"].as_str().unwrap_or_default(),
                        c["host"].as_str().unwrap_or_default(),
                        c["status"].as_str().unwrap_or_default()
                    )
                })
                .collect();
            format!(
                "More than one agent in the fleet goes by \"{key}\"; send again with one of these ids:\n{}",
                lines.join("\n")
            )
        }
        (Reach::On, Some(refused)) if refused.code == code::NOT_FOUND => {
            format!("Agent {key} not found on this host or any other in the fleet.")
        }
        (Reach::On, Some(refused)) => format!("Agent {key} is on another host: {}", refused.error),
        (Reach::On, None) => format!("Agent {key} not found"),
    }
}

/// What an agent is told when a host tool names `server`, which is not
/// this host (`this`), and the Reef cannot, or did not, run it there.
pub fn host_not_here(server: &str, this: &str, reach: &Reach, refused: Option<&Refused>) -> String {
    match (reach, refused) {
        (Reach::On, Some(refused)) if refused.code == code::NOT_FOUND => format!(
            "Host {server} is not one of this fleet's (list_hosts lists them); this host is {this}."
        ),
        (Reach::On, Some(refused)) if refused.code != code::OFF => {
            format!("Host {server}: {}", refused.error)
        }
        (Reach::On, _) => format!("Host {server}: {}", fleet_unreachable(&Reach::Off)),
        _ => format!(
            "Host {server} is not this host ({this}). {}",
            fleet_unreachable(reach)
        ),
    }
}

/// Why other hosts' agents are out of reach, and what would help.
pub fn fleet_unreachable(reach: &Reach) -> String {
    match reach {
        Reach::NoReef => "Other hosts are reached through a Reef, and none is connected to this host: connect it to a Reef, or give the host (`burrow --host`).".into(),
        _ => "Messaging agents on other hosts is off in this fleet's Reef, so other hosts' agents are out of reach from here: turn it on in crusteo's or Skiff's settings, or give the host (`burrow --host`).".into(),
    }
}

fn off(key: &str) -> String {
    format!(
        "Agent {key} is not on this host. Messaging agents on other hosts is off in this fleet's Reef: turn it on in crusteo's or Skiff's settings, or give its host (`burrow send --host`)."
    )
}

/// The answer to a [`burrow_protocol::routing::FINISHED`] request.
pub fn finished_answer(
    request_id: &str,
    delivered: bool,
    gone: bool,
    error: Option<String>,
) -> Value {
    session(
        burrow_protocol::routing::FINISHED_RESPONSE,
        json!({"requestId": request_id, "delivered": delivered, "gone": gone, "error": error}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn the_latest_reef_that_routes_is_asked_and_its_answer_returned() {
        let router = Router::default();
        assert_eq!(router.reach(), Reach::NoReef);
        let (off_tx, _off_rx) = mpsc::unbounded_channel();
        router.announce(1, &off_tx, "reef-a", false);
        assert_eq!(router.reach(), Reach::Off);
        let refused = router.ask("route_prompt", json!({})).await.unwrap_err();
        assert_eq!(refused.code, code::OFF);
        assert!(not_here("abc1234", &router.reach(), None).contains("off in this fleet's Reef"));

        let (tx, mut rx) = mpsc::unbounded_channel();
        router.announce(2, &tx, "reef-b", true);
        assert_eq!(router.reach(), Reach::On);
        let ask = router.ask("route_prompt", json!({"targetAgentId": "x"}));
        let answer = async {
            let frame = rx.recv().await.unwrap();
            let payload = &frame["message"]["payload"];
            assert_eq!(frame["message"]["type"], REQUEST);
            assert_eq!(payload["op"], "route_prompt");
            router.answered(&json!({"requestId": payload["requestId"], "ok": true,
                                    "result": {"disposition": "started"}}));
        };
        let (asked, ()) = tokio::join!(ask, answer);
        assert_eq!(asked.unwrap()["disposition"], "started");

        // A connection that closes fails what waits on it.
        let ask = router.ask("route_prompt", json!({}));
        let close = async {
            rx.recv().await.unwrap();
            router.gone(2);
        };
        let (asked, ()) = tokio::join!(ask, close);
        assert_eq!(asked.unwrap_err().code, "unreachable");
        assert_eq!(router.reach(), Reach::Off);
        router.gone(1);
        assert_eq!(router.reach(), Reach::NoReef);
    }
}
