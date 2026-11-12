//! Reef mode, early: with `reef.json` in the data dir, the desktop takes its
//! hosts from a Reef (`GET /api/hosts`) instead of its own `hosts.json`, and
//! keeps following that list. It still connects to every daemon itself.
//!
//! Reef gives each host's address as Reef reaches it. A loopback one
//! (`127.0.0.1`, `localhost`, `::1`) means Reef's own machine, so it becomes
//! the address the desktop reaches Reef at. `overrides` in `reef.json` give a
//! host another address, or a password, for this desktop only.
//!
//! Reef keeps the fleet's labels too (`/api/labels`); in reef mode they
//! belong to agents ([`fleet::labels`]), and Lanes places agents by them.

use std::collections::BTreeMap;
use std::time::Duration;

use fleet::labels::Catalog;
use fleet::{HostConfig, HostId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `reef.json`: which Reef, and what this desktop changes about its hosts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReefLink {
    /// Where Reef is, such as `http://192.0.2.10:6798`.
    pub url: String,
    /// A device token, unless Reef runs `--open`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// By host id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, HostOverride>,
}

/// One host as this desktop reaches it, where that differs from Reef.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// How often the list is asked for again.
pub const EVERY: Duration = Duration::from_secs(30);

impl ReefLink {
    /// The machine part of [`Self::url`]: `192.0.2.10` for
    /// `http://192.0.2.10:6798/`, `[2001:db8::1]` kept bracketed.
    pub fn machine(&self) -> Option<&str> {
        let rest = self
            .url
            .split_once("://")
            .map_or(self.url.as_str(), |(_, rest)| rest);
        let authority = rest.split('/').next()?;
        let machine = if authority.starts_with('[') {
            &authority[..=authority.find(']')?]
        } else {
            authority.split(':').next()?
        };
        (!machine.is_empty()).then_some(machine)
    }

    /// The hosts in Reef's answer to `/api/hosts`, as this desktop connects
    /// to them.
    pub fn hosts(&self, answer: &Value) -> Result<Vec<HostConfig>, String> {
        let list = answer["hosts"].as_array().ok_or("Reef sent no host list")?;
        let mut hosts = Vec::new();
        for entry in list {
            let (Some(id), Some(name)) = (entry["id"].as_str(), entry["name"].as_str()) else {
                continue;
            };
            let mut endpoint = entry["endpoint"]
                .as_str()
                .map(|endpoint| self.reachable(endpoint));
            let mut password = None;
            if let Some(local) = self.overrides.get(id) {
                endpoint = local.endpoint.clone().or(endpoint);
                password = local.password.clone();
            }
            hosts.push(HostConfig {
                id: HostId::new(id),
                name: name.to_string(),
                endpoint,
                password,
            });
        }
        Ok(hosts)
    }

    /// `endpoint` as this desktop reaches it: on Reef's machine for a
    /// loopback one, else as it is. An `ssh://` one goes through this
    /// desktop's own ssh, its keys and `~/.ssh/config`.
    fn reachable(&self, endpoint: &str) -> String {
        if let Some(target) = paseo_client::ssh::Target::parse(endpoint) {
            return match (target, self.machine()) {
                (Ok(target), Some(reef)) => ssh_on(&target, reef).unwrap_or(endpoint.to_string()),
                _ => endpoint.to_string(),
            };
        }
        let (machine, port) = match endpoint.rsplit_once(':') {
            Some((machine, port)) if !machine.ends_with(':') => (machine, Some(port)),
            _ => (endpoint, None),
        };
        let loopback = matches!(machine, "127.0.0.1" | "localhost" | "[::1]" | "::1");
        match (loopback, self.machine(), port) {
            (true, Some(reef), Some(port)) => format!("{reef}:{port}"),
            (true, Some(reef), None) => reef.to_string(),
            _ => endpoint.to_string(),
        }
    }
}

/// An `ssh://` endpoint to Reef's own machine (`ssh://localhost`, say), as
/// this desktop reaches it: `ssh://` to `reef`. `None` for any other host.
fn ssh_on(target: &paseo_client::ssh::Target, reef: &str) -> Option<String> {
    let (user, machine) = match target.host.rsplit_once('@') {
        Some((user, machine)) => (Some(user), machine),
        None => (None, target.host.as_str()),
    };
    if !matches!(machine, "127.0.0.1" | "localhost" | "::1") {
        return None;
    }
    let mut out = String::from("ssh://");
    if let Some(user) = user {
        out.push_str(user);
        out.push('@');
    }
    out.push_str(reef);
    if let Some(port) = target.ssh_port {
        out.push_str(&format!(":{port}"));
    }
    if target.daemon_port != paseo_client::ssh::DEFAULT_DAEMON_PORT {
        out.push_str(&format!("?daemonPort={}", target.daemon_port));
    }
    Some(out)
}

impl ReefLink {
    /// Asks Reef for its hosts and its label catalog. Blocks; call off the
    /// UI thread.
    pub fn fetch(&self) -> Result<(Vec<HostConfig>, Catalog), String> {
        let hosts = self.hosts(&self.call("hosts", None)?)?;
        let labels = catalog(&self.call("labels", None)?)?;
        Ok((hosts, labels))
    }

    /// Makes a label at Reef (`{"op": "create", "name"}`, Reef picking its
    /// colour) or changes one, and answers the catalog after it. Blocks.
    pub fn edit_labels(&self, edit: &Value) -> Result<(Value, Catalog), String> {
        let answer = self.call("labels", Some(edit))?;
        if answer["ok"] != true {
            let error = answer["error"]
                .as_str()
                .unwrap_or("Reef refused the label change");
            return Err(error.to_string());
        }
        let labels = catalog(&answer)?;
        Ok((answer, labels))
    }

    /// Reef's own settings (`/api/settings`), after `edit` if one is given,
    /// such as `{"agent_routing": true}`. Blocks.
    pub fn settings(&self, edit: Option<&Value>) -> Result<Value, String> {
        self.call("settings", edit)
    }

    /// `GET /api/<path>`, or a POST of `body`, as JSON. The token goes in an
    /// `Authorization: Bearer` header, where a log of requests does not keep
    /// it. A Reef from before that, which answers 401, is sent it in the query
    /// as that Reef reads it, so a desktop upgraded first still works.
    fn call(&self, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let url = format!("{}/api/{path}", self.url.trim_end_matches('/'));
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            // A 401 is an answer to look at, not an error.
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = self.send(&agent, &url, body, self.token.as_deref(), false);
        let unauthorized = matches!(&response, Ok(answer) if answer.status().as_u16() == 401);
        if unauthorized && let Some(token) = &self.token {
            response = self.send(&agent, &url, body, Some(token), true);
        }
        let mut response = response.map_err(|error| format!("Reef at {}: {error}", self.url))?;
        let status = response.status().as_u16();
        let text = (response.body_mut().read_to_string())
            .map_err(|error| format!("Reef at {}: {error}", self.url))?;
        if !(200..300).contains(&status) {
            return Err(format!("Reef at {} answered {status}", self.url));
        }
        serde_json::from_str(&text).map_err(|error| format!("Reef's {path}: {error}"))
    }

    /// One request, with `token` (if any) in the header, or in the query when
    /// `in_query`.
    fn send(
        &self,
        agent: &ureq::Agent,
        url: &str,
        body: Option<&Value>,
        token: Option<&str>,
        in_query: bool,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let (url, bearer) = match token {
            Some(token) if in_query => (format!("{url}?t={}", encode(token)), None),
            Some(token) => (url.to_string(), Some(format!("Bearer {token}"))),
            None => (url.to_string(), None),
        };
        match body {
            Some(body) => {
                let mut request = (agent.post(&url)).header("content-type", "application/json");
                if let Some(bearer) = &bearer {
                    request = request.header("authorization", bearer);
                }
                request.send(body.to_string())
            }
            None => {
                let mut request = agent.get(&url);
                if let Some(bearer) = &bearer {
                    request = request.header("authorization", bearer);
                }
                request.call()
            }
        }
    }
}

/// The catalog in an `/api/labels` answer.
fn catalog(answer: &Value) -> Result<Catalog, String> {
    serde_json::from_value(answer["catalog"].clone())
        .map_err(|error| format!("Reef's labels: {error}"))
}

/// A token for a query string: `+`, `/` and `=` would not survive as they are.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn link(url: &str) -> ReefLink {
        ReefLink {
            url: url.into(),
            ..Default::default()
        }
    }

    #[test]
    fn reefs_loopback_hosts_are_on_reefs_machine_and_overrides_win() {
        let answer = json!({"reef": "0.1.0", "hosts": [
            {"id": "devbox", "name": "devbox", "endpoint": "192.0.2.20:6767", "password": false},
            {"id": "reefbox", "name": "reefbox", "endpoint": "127.0.0.1:6767", "password": false},
            {"id": "laptop", "name": "laptop", "endpoint": "localhost:6797", "password": true},
        ]});
        let mut reef = link("http://192.0.2.10:6798/");
        reef.overrides.insert(
            "laptop".into(),
            HostOverride {
                endpoint: Some("127.0.0.1:6797".into()),
                password: Some("secret".into()),
            },
        );
        let hosts = reef.hosts(&answer).unwrap();
        let shown: Vec<(&str, &str, Option<&str>)> = (hosts.iter())
            .map(|h| {
                (
                    h.id.as_str(),
                    h.endpoint.as_deref().unwrap(),
                    h.password.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("devbox", "192.0.2.20:6767", None),
                // Reef's own machine, as this desktop reaches Reef.
                ("reefbox", "192.0.2.10:6767", None),
                // This desktop's own address and password for it.
                ("laptop", "127.0.0.1:6797", Some("secret")),
            ]
        );
        assert!(link("http://192.0.2.10:6798").hosts(&json!({})).is_err());
    }

    #[test]
    fn an_ssh_host_is_reached_through_this_desktops_ssh() {
        let reef = link("http://192.0.2.10:6798/");
        assert_eq!(reef.reachable("ssh://dev@devbox"), "ssh://dev@devbox");
        assert_eq!(
            reef.reachable("ssh://dev@localhost:2222?daemonPort=6797"),
            "ssh://dev@192.0.2.10:2222?daemonPort=6797",
            "ssh to Reef's own machine is ssh to it as this desktop reaches it"
        );
        assert_eq!(reef.reachable("ssh://127.0.0.1"), "ssh://192.0.2.10");
        assert_eq!(reef.reachable("ssh://bad:pw@x"), "ssh://bad:pw@x");
    }

    #[test]
    fn reefs_machine_is_read_from_its_url() {
        assert_eq!(link("http://192.0.2.10:6798").machine(), Some("192.0.2.10"));
        assert_eq!(link("http://reef.example/").machine(), Some("reef.example"));
        assert_eq!(link("192.0.2.10:6798").machine(), Some("192.0.2.10"));
        assert_eq!(
            link("http://[2001:db8::1]:6798").machine(),
            Some("[2001:db8::1]")
        );
        assert_eq!(link("http://").machine(), None);
        assert_eq!(encode("a+b/c="), "a%2Bb%2Fc%3D");
    }

    /// What a test Reef was sent: each request's line and headers (lower
    /// case), and body.
    type Seen = std::sync::Arc<std::sync::Mutex<Vec<(Vec<String>, String)>>>;

    /// A Reef for a test, on its own thread: each request it is sent is
    /// kept, and answered by `answer` from what the line and headers said.
    fn reef_that(
        answer: impl Fn(&[String]) -> (u16, &'static str) + Send + 'static,
    ) -> (String, Seen) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body_at) = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                        let length = (head.lines())
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break (head, end + 4);
                        }
                    }
                    if n == 0 {
                        break (String::new(), request.len());
                    }
                };
                let lines: Vec<String> = head.lines().map(str::to_string).collect();
                let (status, body) = answer(&lines);
                let text = String::from_utf8_lossy(&request[body_at..]).to_string();
                kept.lock().unwrap().push((lines, text));
                let reply = format!(
                    "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        (url, seen)
    }

    const HOSTS: &str = r#"{"reef": "0.2.0", "hosts": [{"id": "box", "name": "box", "endpoint": "192.0.2.20:6767"}]}"#;
    const LABELS: &str = r#"{"ok": true, "revision": 1, "catalog": {"revision": 1, "labels": []}}"#;

    #[test]
    fn the_token_goes_in_a_header_and_never_in_the_address() {
        let (url, seen) = reef_that(|lines| {
            let right = lines.contains(&"authorization: bearer tok+/=".to_string());
            match (lines[0].as_str(), right) {
                ("get /api/hosts http/1.1", true) => (200, HOSTS),
                ("get /api/labels http/1.1", true) => (200, LABELS),
                ("post /api/labels http/1.1", true) => (200, LABELS),
                _ => (401, "Unauthorized"),
            }
        });
        let reef = ReefLink {
            url: format!("{url}/"),
            token: Some("tok+/=".into()),
            ..Default::default()
        };
        let (hosts, labels) = reef.fetch().expect("both asked for, with the header");
        assert_eq!(hosts[0].endpoint.as_deref(), Some("192.0.2.20:6767"));
        assert_eq!(labels.revision, 1);
        let edited = reef.edit_labels(&json!({"op": "create", "name": "urgent"}));
        assert!(edited.is_ok(), "{edited:?}");

        let seen = seen.lock().unwrap();
        let lines: Vec<&str> = seen.iter().map(|(head, _)| head[0].as_str()).collect();
        // One request each, none in the query: the first answer was taken.
        assert_eq!(
            lines,
            [
                "get /api/hosts http/1.1",
                "get /api/labels http/1.1",
                "post /api/labels http/1.1"
            ]
        );
        assert!(seen.iter().all(|(head, _)| !head[0].contains("t=")));
        assert!(seen[2].1.contains(r#""op":"create""#), "{}", seen[2].1);
    }

    #[test]
    fn a_reef_that_reads_only_the_query_is_sent_it_there_after_the_header_is_refused() {
        // Reef 0.1: no header, `?t=` only; answers 401 to anything else.
        let (url, seen) = reef_that(|lines| {
            if lines[0] == "get /api/hosts?t=tok%2b%2f%3d http/1.1" {
                (200, HOSTS)
            } else {
                (401, "Unauthorized")
            }
        });
        let reef = ReefLink {
            url: url.clone(),
            token: Some("tok+/=".into()),
            ..Default::default()
        };
        let answer = reef.call("hosts", None).expect("the older Reef is read");
        assert_eq!(answer["reef"], "0.2.0");
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "the header first, then the query");
            assert!(
                seen[0]
                    .0
                    .contains(&"authorization: bearer tok+/=".to_string())
            );
            assert!(
                !seen[1]
                    .0
                    .iter()
                    .any(|line| line.starts_with("authorization"))
            );
        }
        // A wrong token is told so after both, not looped on.
        let wrong = ReefLink {
            token: Some("nope".into()),
            ..reef.clone()
        };
        let refused = wrong.call("hosts", None).unwrap_err();
        assert!(refused.contains("401"), "{refused}");
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn a_reef_that_is_open_is_asked_with_no_token_at_all() {
        let (url, seen) = reef_that(|_| (200, HOSTS));
        let reef = ReefLink {
            url,
            ..Default::default()
        };
        assert!(reef.call("hosts", None).is_ok());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0[0], "get /api/hosts http/1.1");
        assert!(
            !seen[0]
                .0
                .iter()
                .any(|line| line.starts_with("authorization"))
        );
        // Another status is an error that says which.
        let (url, _) = reef_that(|_| (500, "oops"));
        let broken = ReefLink {
            url,
            ..Default::default()
        };
        assert!(broken.call("hosts", None).unwrap_err().contains("500"));
    }
}
