//! Paseo 0.15's project suggestions (`project.suggestions.request`): the
//! folders the user already ran coding agents in, from each provider's own
//! session records ([`burrow_provider::Provider::recent_sessions`]), as
//! projects they could add. As Paseo's `listProjectSuggestions`: sessions
//! count toward their repository's main checkout (a worktree or a
//! subdirectory toward the checkout), only those in the window (30 days
//! unless `since` says), not projects already added, the home directory or
//! the filesystem root, and not the sessions that only made an agent's
//! title. The most recently used come first.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::agent_requests::Late;
use crate::daemon::Daemon;

const REQUEST: &str = "project.suggestions.request";
const RESPONSE: &str = "project.suggestions.response";
const DEFAULT_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const DEFAULT_LIMIT: usize = 20;
/// Sessions read per provider: enough to find every folder in the window.
const SESSION_SCAN_LIMIT: usize = 500;
/// The prompt Paseo's daemon starts a title-making session with.
pub(crate) const METADATA_PROMPT: &str =
    "Generate metadata for a coding agent based on the user prompt.";

/// Answers [`REQUEST`] once every provider has been read, off the async
/// threads (the reads are files and short-lived CLIs).
pub fn handle(daemon: &Arc<Daemon>, message: &Value, late: &Late) {
    let request_id = message["requestId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let now = crate::clock::now_ms();
    let since = match message["since"].as_str() {
        None => now - DEFAULT_WINDOW_MS,
        Some(text) => match crate::clock::parse_iso_ms(text) {
            Some(ms) => ms,
            None => {
                let _ = late.send(rpc_error(
                    &request_id,
                    "invalid_since",
                    &format!("Invalid since timestamp: {text}"),
                ));
                return;
            }
        },
    };
    let limit = (message["limit"].as_u64())
        .map(|l| (l as usize).clamp(1, 100))
        .unwrap_or(DEFAULT_LIMIT);
    let wanted: Option<BTreeSet<String>> = message["providers"].as_array().map(|list| {
        list.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    });
    let providers: Vec<_> = (daemon.agents.by_id().into_iter())
        .filter(|(id, _)| wanted.as_ref().is_none_or(|w| w.contains(id)))
        .collect();
    // Burrow keeps only the projects still added.
    let added: Vec<String> = daemon.read(|store| {
        (store.projects.iter())
            .map(|(p, _)| p.root_path.clone())
            .collect()
    });
    let late = late.clone();
    tokio::task::spawn_blocking(move || {
        let mut sessions = Vec::new();
        let mut errors = Vec::new();
        for (id, provider) in providers {
            match provider.recent_sessions(SESSION_SCAN_LIMIT) {
                Ok(found) => sessions.extend(found.into_iter().map(|s| (id.clone(), s))),
                Err(message) => errors.push(json!({"provider": id, "message": message})),
            }
        }
        let suggestions = suggest(sessions, since, limit, &added, &|cwd| root_of(cwd));
        let payload = json!({"requestId": request_id, "suggestions": suggestions,
                             "providerErrors": errors});
        let _ = late.send(burrow_protocol::envelope::session(RESPONSE, payload));
    });
}

fn rpc_error(request_id: &str, code: &str, error: &str) -> Value {
    burrow_protocol::envelope::session(
        "rpc_error",
        json!({"requestId": request_id, "requestType": REQUEST, "error": error, "code": code}),
    )
}

/// Where a project for `cwd` would be added: its repository's main
/// checkout, else its work tree, else the folder itself; none when it is
/// gone.
fn root_of(cwd: &str) -> Option<String> {
    let path = Path::new(cwd);
    if !path.is_dir() {
        return None;
    }
    let root = match crate::git::probe(path) {
        Some(facts) => facts.main_repo_root.unwrap_or(facts.worktree_root),
        None => cwd.to_string(),
    };
    Path::new(&root).is_dir().then_some(root)
}

/// The key two paths that name one folder share.
fn identity(path: &str) -> String {
    let canonical = crate::git::canonical(Path::new(path))
        .to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_string();
    if cfg!(windows) {
        canonical.to_lowercase()
    } else {
        canonical
    }
}

struct Group {
    root: String,
    sessions: u64,
    providers: BTreeSet<String>,
    last: i64,
}

/// The suggestions for `sessions` (provider id and session), given the
/// window's start, the most to return, the projects already added and how
/// a folder maps to its project root.
fn suggest(
    sessions: Vec<(String, burrow_provider::RecentSession)>,
    since: i64,
    limit: usize,
    added: &[String],
    root_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<Value> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let mut excluded: BTreeSet<String> = added.iter().map(|p| identity(p)).collect();
    if !home.is_empty() {
        excluded.insert(identity(&home));
        if let Some(fs_root) = Path::new(&home).ancestors().last() {
            excluded.insert(identity(&fs_root.to_string_lossy()));
        }
    }
    let mut roots: HashMap<String, Option<String>> = HashMap::new();
    let mut groups: HashMap<String, Group> = HashMap::new();
    for (provider, session) in sessions {
        let title_only = (session.first_prompt.as_deref())
            .is_some_and(|p| p.trim_start().starts_with(METADATA_PROMPT));
        if session.last_activity_ms < since || title_only {
            continue;
        }
        let root = roots
            .entry(session.cwd.clone())
            .or_insert_with(|| root_of(&session.cwd))
            .clone();
        let Some(root) = root else {
            continue;
        };
        let key = identity(&root);
        if excluded.contains(&key) {
            continue;
        }
        let group = groups.entry(key).or_insert_with(|| Group {
            root,
            sessions: 0,
            providers: BTreeSet::new(),
            last: 0,
        });
        group.sessions += 1;
        group.providers.insert(provider);
        group.last = group.last.max(session.last_activity_ms);
    }
    let mut groups: Vec<Group> = groups.into_values().collect();
    groups.sort_by(|a, b| b.last.cmp(&a.last).then(a.root.cmp(&b.root)));
    (groups.into_iter().take(limit))
        .map(|g| {
            let name = (Path::new(&g.root).file_name())
                .map(|n| n.to_string_lossy().to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| g.root.clone());
            json!({
                "rootPath": g.root, "displayName": name, "sessionCount": g.sessions,
                "providerIds": g.providers, "lastActivityAt": crate::clock::iso_from_ms(g.last),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use burrow_provider::RecentSession;

    fn session(cwd: &str, at: i64, prompt: &str) -> RecentSession {
        RecentSession {
            id: format!("s-{at}"),
            cwd: cwd.into(),
            first_prompt: Some(prompt.into()),
            last_activity_ms: at,
            ..RecentSession::default()
        }
    }

    #[test]
    fn sessions_group_by_project_root_newest_first_without_added_or_title_ones() {
        // A worktree and a subfolder count toward the checkout they belong to.
        let root_of = |cwd: &str| -> Option<String> {
            match cwd {
                "/work/app/.worktrees/fix" | "/work/app/src" | "/work/app" => {
                    Some("/work/app".into())
                }
                "/gone" => None,
                other => Some(other.into()),
            }
        };
        let sessions = vec![
            (
                "claude".to_string(),
                session("/work/app/src", 5_000, "fix it"),
            ),
            (
                "codex".to_string(),
                session("/work/app/.worktrees/fix", 9_000, "and this"),
            ),
            ("claude".to_string(), session("/work/notes", 7_000, "notes")),
            (
                "claude".to_string(),
                session("/work/added", 8_000, "already a project"),
            ),
            (
                "claude".to_string(),
                session("/gone", 8_500, "deleted folder"),
            ),
            (
                "claude".to_string(),
                session("/work/old", 500, "before the window"),
            ),
            (
                "claude".to_string(),
                session("/work/titles", 8_800, METADATA_PROMPT),
            ),
        ];
        let added = vec!["/work/added".to_string()];
        let found = suggest(sessions, 1_000, 20, &added, &root_of);
        let roots: Vec<&str> = found
            .iter()
            .map(|s| s["rootPath"].as_str().unwrap())
            .collect();
        assert_eq!(roots, ["/work/app", "/work/notes"], "{found:?}");
        assert_eq!(found[0]["displayName"], "app");
        assert_eq!(found[0]["sessionCount"], 2);
        assert_eq!(found[0]["providerIds"], json!(["claude", "codex"]));
        // The limit keeps the most recent.
        let one = suggest(
            vec![
                ("claude".to_string(), session("/work/a", 2_000, "a")),
                ("claude".to_string(), session("/work/b", 3_000, "b")),
            ],
            1_000,
            1,
            &[],
            &root_of,
        );
        assert_eq!(one.len(), 1);
        assert_eq!(one[0]["rootPath"], "/work/b");
    }
}
