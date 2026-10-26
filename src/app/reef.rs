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
    /// loopback one, else as it is.
    fn reachable(&self, endpoint: &str) -> String {
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

    /// `GET /api/<path>`, or a POST of `body`, as JSON.
    fn call(&self, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let mut url = format!("{}/api/{path}", self.url.trim_end_matches('/'));
        if let Some(token) = &self.token {
            url.push_str("?t=");
            url.push_str(&encode(token));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .build()
            .into();
        let response = match body {
            Some(body) => (agent.post(&url))
                .header("content-type", "application/json")
                .send(body.to_string()),
            None => agent.get(&url).call(),
        };
        let text = response
            .and_then(|mut response| response.body_mut().read_to_string())
            .map_err(|error| format!("Reef at {}: {error}", self.url))?;
        serde_json::from_str(&text).map_err(|error| format!("Reef's {path}: {error}"))
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
}
