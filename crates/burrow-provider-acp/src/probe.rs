//! What an ACP agent offers before any agent runs: Paseo's `fetchCatalog`.
//! A throwaway process is started, initialized and asked for a new session,
//! whose modes and models are the catalog; then it is closed.

use burrow_provider::{Mode, Model};
use serde_json::{Value, json};

use crate::catalog;
use crate::process::{Output, Transport};
use crate::session::{initialize_params, summarize_error};

/// How an agent lists its models.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Models {
    /// From the session's own answer (`models`, else the `model` option).
    #[default]
    Session,
    /// Cursor's `cursor/list_available_models` extension, which lists each
    /// model's own options without selecting it (selecting one in a probe
    /// would change the user's Cursor CLI preference).
    CursorExtension,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub modes: Vec<Mode>,
    pub models: Vec<Model>,
}

struct Probe {
    transport: Transport,
    next_id: u64,
}

impl Probe {
    /// Sends a request and waits for its answer, refusing what the agent
    /// asks meanwhile. `Err` is the agent's error, or why there is none.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let _ = self
            .transport
            .input
            .send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let message = match self.transport.output.recv().await {
                Some(Output::Message(message)) => message,
                Some(Output::Garbage(_)) => continue,
                Some(Output::Exited { stderr, .. }) => {
                    let detail = stderr.trim().lines().last().unwrap_or_default().to_string();
                    return Err(format!("the agent exited during {method}: {detail}"));
                }
                None => return Err(format!("the agent closed during {method}")),
            };
            match (message["method"].as_str(), message.get("id")) {
                (Some("session/request_permission"), Some(request)) => {
                    let _ = self
                        .transport
                        .input
                        .send(json!({"jsonrpc": "2.0", "id": request,
                        "result": {"outcome": {"outcome": "cancelled"}}}));
                }
                (Some(other), Some(request)) if !request.is_null() => {
                    let _ = self.transport.input.send(json!({"jsonrpc": "2.0", "id": request,
                        "error": {"code": -32601, "message": format!("\"Method not found\": {other}"),
                                  "data": {"method": other}}}));
                }
                (None, Some(answer))
                    if answer.as_u64() == Some(id) || answer.as_str() == Some(&id.to_string()) =>
                {
                    return match message.get("error").filter(|e| !e.is_null()) {
                        Some(error) => Err(summarize_error(error).0),
                        None => Ok(message["result"].clone()),
                    };
                }
                _ => {} // notifications and stray answers
            }
        }
    }
}

/// Probes the agent behind `transport`, with a session in `cwd`.
pub async fn run(
    transport: Transport,
    cwd: &str,
    meta: Option<&Value>,
    models: Models,
) -> Result<Catalog, String> {
    let mut probe = Probe {
        transport,
        next_id: 0,
    };
    let initialized = probe.call("initialize", initialize_params(meta)).await?;
    let session = probe
        .call("session/new", json!({"cwd": cwd, "mcpServers": []}))
        .await?;
    let options = session["configOptions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let session_models = catalog::models(&session["models"], &options);
    let models = match models {
        Models::Session => session_models,
        Models::CursorExtension => {
            let current = session_models
                .iter()
                .find(|m| m.is_default)
                .map(|m| m.id.clone());
            let listed = probe
                .call("cursor/list_available_models", json!({}))
                .await
                .map_err(|err| {
                    format!(
                        "Update Cursor CLI: this version does not support cursor/list_available_models ({err})"
                    )
                })?;
            catalog::cursor_models(&listed, current.as_deref())
        }
    };
    let (modes, _) = catalog::modes(&session["modes"], &options);
    if !initialized["agentCapabilities"]["sessionCapabilities"]["close"].is_null()
        && let Some(id) = session["sessionId"].as_str()
    {
        let close = probe.call("session/close", json!({"sessionId": id}));
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), close).await;
    }
    Ok(Catalog { modes, models })
}
