//! Requests about the daemon itself and its providers, as Paseo's CLI makes
//! them: `paseo status` and `paseo daemon status` (`daemon.get_status`,
//! behind `daemonStatusRpc`), `paseo daemon pair` (`daemon.get_pairing_offer`,
//! asked of any host that advertises status), `paseo provider models` and
//! `paseo provider diagnostic`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use burrow_protocol::envelope::session;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent_requests::Late;
use crate::daemon::Daemon;

/// The kinds this module answers.
pub const KINDS: &[&str] = &[
    "daemon.get_status.request",
    "daemon.get_pairing_offer.request",
    "list_provider_models_request",
    "provider_diagnostic_request",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Plain {
    request_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderRequest {
    request_id: String,
    provider: String,
}

/// `None`: the message did not validate.
pub fn handle(
    daemon: &Arc<Daemon>,
    kind: &str,
    message: &Value,
    late: &Late,
) -> Option<Vec<Value>> {
    fn parse<T: for<'de> Deserialize<'de>>(message: &Value) -> Option<T> {
        T::deserialize(message).ok()
    }
    let daemon = daemon.clone();
    let late = late.clone();
    let reply_later =
        move |future: std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>| {
            tokio::spawn(async move {
                let _ = late.send(future.await);
            });
            Some(Vec::new())
        };
    match kind {
        "daemon.get_status.request" => {
            let request: Plain = parse(message)?;
            reply_later(Box::pin(async move {
                status(&daemon, &request.request_id).await
            }))
        }
        // Burrow has no relay: Paseo's CLI turns this into "Relay pairing is
        // disabled", and the app's direct connection is the way to add it.
        "daemon.get_pairing_offer.request" => {
            let request: Plain = parse(message)?;
            Some(vec![session(
                "daemon.get_pairing_offer.response",
                json!({"requestId": request.request_id, "url": "", "qr": null,
                       "relayEnabled": false}),
            )])
        }
        "list_provider_models_request" => {
            let request: ProviderRequest = parse(message)?;
            Some(vec![models(&daemon, &request)])
        }
        "provider_diagnostic_request" => {
            let request: ProviderRequest = parse(message)?;
            reply_later(Box::pin(async move {
                let text = diagnostic(&daemon, &request.provider).await;
                session(
                    "provider_diagnostic_response",
                    json!({"requestId": request.request_id, "provider": request.provider,
                           "diagnostic": text}),
                )
            }))
        }
        _ => None,
    }
}

/// `daemon.get_status.response`: who and where the daemon is, and which
/// providers it offers. `nodePath` is the program running, Burrow itself.
async fn status(daemon: &Daemon, request_id: &str) -> Value {
    let offered: Vec<String> = daemon
        .agents
        .infos()
        .into_iter()
        .map(|info| info.id)
        .collect();
    let mut providers: Vec<Value> = offered
        .iter()
        .map(|id| json!({"provider": id, "available": true, "error": null}))
        .collect();
    for (id, _, stamp) in crate::discovery::watched(daemon).await {
        if !offered.contains(&id) {
            let error = match stamp {
                None => "its CLI was not found",
                Some(_) => "its CLI runs but could not be set up",
            };
            providers.push(json!({"provider": id, "available": false, "error": error}));
        }
    }
    let program = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    session(
        "daemon.get_status.response",
        json!({
            "requestId": request_id,
            "serverId": daemon.config.server_id,
            "version": daemon.config.version,
            "pid": std::process::id(),
            "nodePath": program,
            "startedAt": daemon.started_at,
            "listen": daemon.local_endpoint.get(),
            "relay": null,
            "providers": providers,
        }),
    )
}

/// `list_provider_models_response`: the provider's models as the providers
/// snapshot has them.
fn models(daemon: &Daemon, request: &ProviderRequest) -> Value {
    let snapshot = crate::agents::providers_snapshot(daemon, false);
    let entry = (snapshot["entries"].as_array().into_iter().flatten())
        .find(|entry| entry["provider"] == request.provider.as_str());
    let (models, error) = match entry {
        Some(entry) if entry["status"] == "loading" => (
            Value::Null,
            json!(format!("{} is still loading its models", request.provider)),
        ),
        Some(entry) => (
            entry.get("models").cloned().unwrap_or_else(|| json!([])),
            Value::Null,
        ),
        None => (
            Value::Null,
            json!(format!("Provider {} is not available", request.provider)),
        ),
    };
    let mut payload = json!({"requestId": request.request_id, "provider": request.provider,
                             "error": error, "fetchedAt": crate::clock::now_iso()});
    if !models.is_null() {
        payload["models"] = models;
    }
    session("list_provider_models_response", payload)
}

/// `provider_diagnostic_response`: what Burrow knows about finding the
/// provider, as lines of text.
async fn diagnostic(daemon: &Daemon, provider: &str) -> String {
    let watched = crate::discovery::watched(daemon).await;
    let found = watched.iter().find(|(id, ..)| id == provider);
    let info = daemon
        .agents
        .provider(provider)
        .map(|p| (p.info(), p.loading()));
    let mut lines = vec![format!("Provider: {provider}")];
    match found {
        Some((_, program, stamp)) => {
            if let Some(program) = program {
                lines.push(format!("Command: {}", program.display()));
                let resolved = resolve(program, std::env::var_os("PATH"));
                lines.push(format!(
                    "Resolved: {}",
                    resolved.map_or("not found on PATH".to_string(), |p| p.display().to_string())
                ));
            }
            lines.push(format!(
                "Version: {}",
                stamp
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("did not run")
            ));
        }
        None => lines.push("Not looked for: turned off on this daemon".into()),
    }
    match info {
        Some((info, loading)) => {
            lines.push(format!(
                "Available: yes{}",
                if loading { " (loading)" } else { "" }
            ));
            lines.push(format!("Models: {}", info.models.len()));
            let modes: Vec<&str> = info.modes.iter().map(|m| m.id.as_str()).collect();
            lines.push(format!(
                "Modes: {}",
                if modes.is_empty() {
                    "none".to_string()
                } else {
                    modes.join(", ")
                }
            ));
        }
        None => lines.push("Available: no".into()),
    }
    lines.push(format!(
        "PATH: {}",
        std::env::var("PATH").unwrap_or_default()
    ));
    lines.join("\n")
}

/// Where `program` runs from: itself when it has a directory, else the
/// first match on `path`.
fn resolve(program: &Path, path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    std::env::split_paths(&path?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}
