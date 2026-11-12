//! Paseo's import of a provider session: the sessions Claude Code's
//! transcripts and Codex's threads hold that Burrow has no agent for
//! (`fetch_recent_provider_sessions_request`, answered as Paseo's
//! `listImportableProviderSessions`), and the agent made of one
//! (`import_agent_request`, by [`crate::agents::import_session`], answered
//! with a `status` frame as Paseo does, never `rpc_error`).
//!
//! Paseo's import sheet asks for one provider at a time, and for the most
//! recent sessions of a folder when it is opened from a workspace.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use burrow_protocol::envelope::{rpc_error, session};
use burrow_provider::RecentSession;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent_requests::Late;
use crate::agents::{self, ImportSession};
use crate::clock;
use crate::daemon::Daemon;
use crate::git;
use crate::store::Store;
use crate::suggestions::METADATA_PROMPT;

const LIST: &str = "fetch_recent_provider_sessions_request";
const LISTED: &str = "fetch_recent_provider_sessions_response";
const IMPORT: &str = "import_agent_request";

/// The kinds this module answers.
pub const KINDS: &[&str] = &[LIST, IMPORT];

/// Paseo's default and greatest `limit`.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;
/// Sessions read from each provider to search them.
const SEARCH_SCAN_LIMIT: usize = 500;
/// How long a provider may take to list its sessions.
const LIST_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListRequest {
    request_id: String,
    /// Only the sessions that ran in this folder.
    cwd: Option<String>,
    providers: Option<Vec<String>>,
    /// Only the sessions used since this time.
    since: Option<String>,
    limit: Option<usize>,
    /// Only the sessions it appears in (a name, a prompt or the folder's
    /// name), in any case.
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Import {
    request_id: String,
    provider_id: Option<String>,
    provider_handle_id: Option<String>,
    /// The older shape, as Paseo's CLI still sends it: the provider and
    /// the session's id.
    provider: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    workspace_id: Option<String>,
    labels: Option<BTreeMap<String, String>>,
}

/// A session of one provider.
struct Listed {
    provider: String,
    label: String,
    session: RecentSession,
}

/// Answers a request of [`KINDS`] when the work is done, on `late`. `None`:
/// the message did not validate.
pub fn handle(
    daemon: &Arc<Daemon>,
    kind: &str,
    message: &Value,
    late: &Late,
) -> Option<Vec<Value>> {
    let (daemon, late) = (daemon.clone(), late.clone());
    match kind {
        LIST => {
            let request = ListRequest::deserialize(message).ok()?;
            if (request.limit).is_some_and(|limit| !(1..=MAX_LIMIT).contains(&limit)) {
                return None;
            }
            tokio::spawn(async move {
                let _ = late.send(list(&daemon, request).await);
            });
        }
        IMPORT => {
            let request = Import::deserialize(message).ok()?;
            tokio::spawn(async move {
                let _ = late.send(import(&daemon, request).await);
            });
        }
        _ => return None,
    }
    Some(Vec::new())
}

async fn list(daemon: &Arc<Daemon>, request: ListRequest) -> Value {
    let since = match request.since.as_deref().filter(|since| !since.is_empty()) {
        None => None,
        Some(text) => match clock::parse_iso_ms(text) {
            Some(ms) => Some(ms),
            None => {
                return rpc_error(
                    &request.request_id,
                    Some(LIST),
                    "Invalid recent provider sessions since",
                    "invalid_since",
                );
            }
        },
    };
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let query = (request.query.as_deref())
        .map(|query| query.trim().to_lowercase())
        .filter(|query| !query.is_empty());
    // As an import reads its folder: `~` expanded, symlinks resolved.
    let cwd = (request.cwd.as_deref().map(str::trim))
        .filter(|cwd| !cwd.is_empty())
        .map(|cwd| {
            (git::resolve(Path::new(&git::expand_tilde(cwd))))
                .to_string_lossy()
                .into_owned()
        });
    let cwd = cwd.as_deref();

    // Providers are still being found while Burrow starts.
    match &request.providers {
        Some(ids) => {
            for id in ids {
                daemon.agents.until_found(id).await;
            }
        }
        None => daemon.agents.until_discovered().await,
    }
    let wanted = |id: &str| {
        (request.providers.as_ref()).is_none_or(|ids| ids.iter().any(|wanted| wanted == id))
    };
    let providers: Vec<_> = (daemon.agents.by_id().into_iter())
        .filter(|(id, provider)| wanted(id) && provider.imports_sessions())
        .collect();
    let (imported, imported_count) = daemon.read(|store| imported_sessions(store, &wanted));

    // Sessions agents already have are read past, so the page still fills.
    let list_limit = if query.is_some() {
        SEARCH_SCAN_LIMIT
    } else {
        limit + imported_count
    };
    let results = futures_util::future::join_all(providers.iter().map(|(id, provider)| {
        let (provider, cwd) = (provider.clone(), cwd.map(str::to_string));
        async move {
            let work = tokio::task::spawn_blocking(move || {
                provider.importable_sessions(cwd.as_deref(), list_limit)
            });
            match tokio::time::timeout(LIST_TIMEOUT, work).await {
                Ok(Ok(found)) => found,
                Ok(Err(panicked)) => Err(panicked.to_string()),
                Err(_) => Err(format!(
                    "Timed out listing importable sessions for provider '{id}' after {}ms",
                    LIST_TIMEOUT.as_millis()
                )),
            }
        }
    }))
    .await;

    let mut listed = Vec::new();
    let mut errors = Vec::new();
    for ((id, provider), result) in providers.iter().zip(results) {
        match result {
            Ok(found) => {
                let label = provider.info().label;
                listed.extend(
                    (found.into_iter())
                        .filter(|session| matches_query(session, query.as_deref()))
                        .map(|session| Listed {
                            provider: id.clone(),
                            label: label.clone(),
                            session,
                        }),
                );
            }
            Err(message) => errors.push(json!({"provider": id, "message": message})),
        }
    }
    newest_first(&mut listed);
    listed.truncate(list_limit);
    let (shown, already_imported) = select(listed, cwd.map(Path::new), since, &imported, limit);

    let entries: Vec<Value> = shown.iter().map(descriptor).collect();
    let mut payload = json!({"requestId": request.request_id, "entries": entries});
    if already_imported > 0 {
        payload["filteredAlreadyImportedCount"] = already_imported.into();
    }
    if !errors.is_empty() {
        payload["providerErrors"] = errors.into();
    }
    session(LISTED, payload)
}

/// The sessions the active agents of `wanted` providers have, as (provider,
/// session) by the session's id and by its native handle, and how many
/// sessions that is. An archived agent's does not count: it is listed, and
/// importing it brings the agent back.
fn imported_sessions(
    store: &Store,
    wanted: &dyn Fn(&str) -> bool,
) -> (HashSet<(String, String)>, usize) {
    let mut handles = HashSet::new();
    let mut sessions = HashSet::new();
    for (agent, _) in store
        .agents
        .iter()
        .filter(|(agent, _)| wanted(&agent.provider))
    {
        let Some(persistence) = &agent.persistence else {
            continue;
        };
        if let Some(id) = persistence["sessionId"].as_str() {
            sessions.insert((agent.provider.clone(), id.to_string()));
            handles.insert((agent.provider.clone(), id.to_string()));
        }
        if let Some(native) = persistence["nativeHandle"].as_str() {
            handles.insert((agent.provider.clone(), native.to_string()));
        }
    }
    (handles, sessions.len())
}

/// Whether `query` (trimmed and in lower case) is in the session's name, its
/// first or latest prompt, or the name of its folder.
fn matches_query(session: &RecentSession, query: Option<&str>) -> bool {
    let Some(query) = query else {
        return true;
    };
    let folder = session.cwd.replace('\\', "/");
    let folder = folder
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let texts = [
        session.title.as_deref(),
        session.first_prompt.as_deref(),
        session.last_prompt.as_deref(),
        Some(folder),
    ];
    texts
        .into_iter()
        .flatten()
        .any(|text| text.to_lowercase().contains(query))
}

fn newest_first(sessions: &mut [Listed]) {
    sessions.sort_by_key(|listed| std::cmp::Reverse(listed.session.last_activity_ms));
}

/// The sessions to show, newest first, at most `limit`: those that ran in
/// `cwd` (the same folder, symlinks resolved), at or after `since`, that
/// were not made to name an agent, and that no agent has, as `imported`
/// lists them. How many were left out for being imported goes with them.
fn select(
    listed: Vec<Listed>,
    cwd: Option<&Path>,
    since: Option<i64>,
    imported: &HashSet<(String, String)>,
    limit: usize,
) -> (Vec<Listed>, usize) {
    let folder = cwd.map(git::canonical);
    let mut already_imported = 0;
    let mut kept = Vec::new();
    for entry in listed {
        let session = &entry.session;
        if (folder.as_ref())
            .is_some_and(|folder| git::canonical(Path::new(&session.cwd)) != *folder)
            || since.is_some_and(|since| session.last_activity_ms < since)
            || (session.first_prompt.as_deref())
                .is_some_and(|prompt| prompt.trim_start().starts_with(METADATA_PROMPT))
        {
            continue;
        }
        if imported.contains(&(entry.provider.clone(), session.id.clone())) {
            already_imported += 1;
            continue;
        }
        kept.push(entry);
    }
    newest_first(&mut kept);
    kept.truncate(limit);
    (kept, already_imported)
}

/// `RecentProviderSessionDescriptorPayload`: all eight keys, `null` for what
/// the provider does not know.
fn descriptor(entry: &Listed) -> Value {
    let session = &entry.session;
    json!({
        "providerId": entry.provider,
        "providerLabel": entry.label,
        "providerHandleId": session.id,
        "cwd": session.cwd,
        "title": session.title,
        "firstPromptPreview": session.first_prompt,
        "lastPromptPreview": session.last_prompt,
        "lastActivityAt": clock::iso_from_ms(session.last_activity_ms),
    })
}

/// `import_agent_request`: `agent_resumed` with the agent, or
/// `agent_create_failed` with why not.
async fn import(daemon: &Arc<Daemon>, request: Import) -> Value {
    let failed = |error: &str| {
        session(
            "status",
            json!({"status": "agent_create_failed", "requestId": request.request_id,
                   "error": error}),
        )
    };
    let provider = request.provider_id.or(request.provider).unwrap_or_default();
    let handle = (request.provider_handle_id.or(request.session_id)).unwrap_or_default();
    if provider.is_empty() || handle.is_empty() {
        return failed("Import requires providerId and providerHandleId");
    }
    let Some(cwd) = request.cwd.filter(|cwd| !cwd.is_empty()) else {
        return failed("Import requires cwd from the selected provider session");
    };
    let session_to_import = ImportSession {
        provider,
        handle,
        cwd,
        workspace_id: request.workspace_id.filter(|id| !id.is_empty()),
        labels: request.labels.unwrap_or_default(),
    };
    match agents::import_session(daemon, session_to_import).await {
        Ok((agent, timeline_size)) => session(
            "status",
            json!({"status": "agent_resumed", "agentId": agent.id,
                   "requestId": request.request_id, "timelineSize": timeline_size,
                   "agent": agent}),
        ),
        Err(error) => failed(&error),
    }
}
