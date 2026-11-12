//! Importing a provider session, through `paseo-client`: the sessions a
//! provider lists for Paseo's import sheet (`fetch_recent_provider_sessions_request`)
//! and the agent made of one (`import_agent_request`). The providers are
//! fakes that list canned sessions and run the echo provider; no CLI is
//! started, and nothing reads `~/.claude` or `~/.codex`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_daemon::git::canonical;
use burrow_daemon::{Config, Daemon, Running, clock, start};
use burrow_provider::echo::EchoProvider;
use burrow_provider::{HistoryItem, Provider, ProviderInfo, RecentSession, SessionSpec};
use paseo_client::{Client, ConnState, Error, Event, EventReceiver, ServerInfo};
use serde_json::{Value, json};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);
const DAY: i64 = 24 * 60 * 60 * 1000;
const METADATA_PROMPT: &str = "Generate metadata for a coding agent based on the user prompt.";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "burrow-import-{name}-{}",
        burrow_daemon::ids::agent_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    canonical(&dir)
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The folder and the limit of each listing a provider was asked for.
type Asked = Vec<(Option<String>, usize)>;

/// What a fake provider was asked, and did.
#[derive(Clone, Default)]
struct Seen {
    asked: Arc<Mutex<Asked>>,
    /// The sessions whose conversation was read.
    read: Arc<Mutex<Vec<String>>>,
    /// The sessions that were started.
    started: Arc<Mutex<Vec<SessionSpec>>>,
}

/// A provider with sessions of its own to import. It lists them as a CLI
/// that cannot narrow by folder does (every one, newest first, up to the
/// limit), and runs agents as the echo provider does.
struct Importer {
    id: String,
    echo: EchoProvider,
    sessions: Vec<RecentSession>,
    history: HashMap<String, Vec<HistoryItem>>,
    titles: HashMap<String, String>,
    listing_error: Option<String>,
    seen: Seen,
}

impl Importer {
    fn new(id: &str, sessions: Vec<RecentSession>) -> Self {
        Self {
            id: id.into(),
            echo: EchoProvider::default(),
            sessions,
            history: HashMap::new(),
            titles: HashMap::new(),
            listing_error: None,
            seen: Seen::default(),
        }
    }

    fn with_history(mut self, session: &str, turns: &[(&str, &str)]) -> Self {
        let items = turns.iter().map(|(kind, text)| HistoryItem {
            timestamp: Some("2026-01-02T03:04:05.000Z".into()),
            item: json!({"type": kind, "text": text, "messageId": format!("m-{text}")}),
        });
        self.history.insert(session.into(), items.collect());
        self
    }

    fn with_title(mut self, session: &str, title: &str) -> Self {
        self.titles.insert(session.into(), title.into());
        self
    }

    fn failing(mut self, error: &str) -> Self {
        self.listing_error = Some(error.into());
        self
    }
}

impl Provider for Importer {
    fn info(&self) -> ProviderInfo {
        let mut info = self.echo.info();
        info.id = self.id.clone();
        info.label = format!("Label of {}", self.id);
        info
    }

    fn start(
        &self,
        spec: SessionSpec,
    ) -> Result<burrow_provider::Session, burrow_provider::ProviderError> {
        self.seen.started.lock().unwrap().push(spec.clone());
        self.echo.start(spec)
    }

    fn history(&self, _cwd: &str, session_id: &str) -> Vec<HistoryItem> {
        self.seen.read.lock().unwrap().push(session_id.into());
        self.history.get(session_id).cloned().unwrap_or_default()
    }

    fn session_title(&self, _cwd: &str, session_id: &str) -> Option<String> {
        self.titles.get(session_id).cloned()
    }

    fn imports_sessions(&self) -> bool {
        true
    }

    fn importable_sessions(
        &self,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RecentSession>, String> {
        let asked = (cwd.map(str::to_string), limit);
        self.seen.asked.lock().unwrap().push(asked);
        if let Some(error) = &self.listing_error {
            return Err(error.clone());
        }
        Ok(self.sessions.iter().take(limit).cloned().collect())
    }
}

/// A session that ran in `cwd` and was last used at `at` (milliseconds).
fn session(id: &str, cwd: &Path, at: i64) -> RecentSession {
    RecentSession {
        id: id.into(),
        cwd: text(cwd),
        last_activity_ms: at,
        ..RecentSession::default()
    }
}

/// [`session`], with what a CLI knows of it.
fn known(id: &str, cwd: &Path, at: i64, title: &str, first: &str, last: &str) -> RecentSession {
    RecentSession {
        title: Some(title.into()),
        first_prompt: Some(first.into()),
        last_prompt: Some(last.into()),
        ..session(id, cwd, at)
    }
}

struct Harness {
    running: Running,
    daemon: Arc<Daemon>,
    client: Client,
    events: EventReceiver,
    info: ServerInfo,
}

async fn harness(home: &Path, server_id: &str, providers: Vec<Importer>) -> Harness {
    let mut config = Config::ephemeral();
    config.home = Some(home.to_path_buf());
    config.server_id = server_id.into();
    let daemon = Daemon::open(config).unwrap();
    // The echo provider lists nothing and cannot import.
    daemon.agents.register(Arc::new(EchoProvider::default()));
    for provider in providers {
        daemon.agents.register(Arc::new(provider));
    }
    let running = start(daemon.clone()).await.unwrap();
    let client = Client::connect(paseo_client::Config::new(
        running.local_addr.to_string(),
        paseo_client::generate_client_id(),
    ));
    let events = client.events();
    let info = timeout(WAIT, async {
        loop {
            if let Event::State(ConnState::Connected(info)) = events.recv().await.unwrap() {
                return info;
            }
        }
    })
    .await
    .expect("connected");
    let harness = Harness {
        running,
        daemon,
        client,
        events,
        info,
    };
    for subscription in [
        json!({"type": "fetch_workspaces_request", "includeBackground": true, "subscribe": {}}),
        json!({"type": "fetch_agents_request", "scope": "active", "subscribe": {}}),
    ] {
        harness.request(subscription).await;
    }
    harness
}

fn typed(kind: &str, mut message: Value) -> Value {
    message["type"] = kind.into();
    message
}

impl Harness {
    async fn request(&self, message: Value) -> Value {
        self.client.request(message).await.expect("request")
    }

    /// The code and message of the `rpc_error` a request is answered with.
    async fn refused(&self, message: Value) -> (Option<String>, String) {
        match self.client.request(message).await {
            Err(Error::Rpc { code, message }) => (code, message),
            other => panic!("expected an rpc_error, got {other:?}"),
        }
    }

    /// Pushed messages up to and including the first that matches.
    async fn until(&self, what: &str, matches: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        timeout(WAIT, async {
            loop {
                if let Event::Message(message) = self.events.recv().await.unwrap() {
                    let hit = matches(&message);
                    seen.push(message);
                    if hit {
                        return;
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {what} within {WAIT:?}; saw {seen:#?}"));
        seen
    }

    async fn list(&self, extra: Value) -> Value {
        self.request(typed("fetch_recent_provider_sessions_request", extra))
            .await
    }

    /// The ids of the sessions a listing offers, in order.
    async fn listed(&self, extra: Value) -> Vec<String> {
        ids(&self.list(extra).await)
    }

    async fn import(&self, extra: Value) -> Value {
        self.request(typed("import_agent_request", extra)).await
    }

    /// Imports `session` of the importer, that ran in `cwd`.
    async fn import_session(&self, session: &str, cwd: &Path) -> Value {
        self.import(
            json!({"providerId": "importer", "providerHandleId": session,
                           "cwd": text(cwd)}),
        )
        .await
    }

    async fn workspaces(&self) -> Vec<Value> {
        let reply = self
            .request(json!({"type": "fetch_workspaces_request", "includeBackground": true}))
            .await;
        reply["entries"].as_array().unwrap().clone()
    }

    async fn agent_ids(&self) -> Vec<String> {
        let reply = self
            .request(json!({"type": "fetch_agents_request", "scope": "active"}))
            .await;
        (reply["entries"].as_array().unwrap().iter())
            .map(|entry| entry["agent"]["id"].as_str().unwrap().to_string())
            .collect()
    }

    async fn agent(&self, id: &str) -> Value {
        let reply = self
            .request(json!({"type": "fetch_agent_request", "agentId": id}))
            .await;
        reply["agent"].clone()
    }

    /// An agent's whole conversation, as the items a client shows.
    async fn conversation(&self, id: &str) -> Vec<Value> {
        let page = self
            .request(
                json!({"type": "fetch_agent_timeline_request", "agentId": id,
                       "direction": "tail", "limit": 40, "projection": "projected"}),
            )
            .await;
        (page["entries"].as_array().unwrap().iter())
            .map(|entry| entry["item"].clone())
            .collect()
    }

    async fn stop(self) {
        drop(self.client);
        self.running.shutdown().await;
        self.daemon.close().unwrap();
    }
}

fn ids(reply: &Value) -> Vec<String> {
    (reply["entries"].as_array().unwrap().iter())
        .map(|entry| entry["providerHandleId"].as_str().unwrap().to_string())
        .collect()
}

/// What each item says: its kind and its text.
fn texts(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            let (kind, text) = (&item["type"], &item["text"]);
            format!("{}:{}", kind.as_str().unwrap(), text.as_str().unwrap_or(""))
        })
        .collect()
}

fn failed(reply: &Value) -> &str {
    assert_eq!(reply["status"], "agent_create_failed", "{reply}");
    reply["error"].as_str().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_import_sheets_features_are_advertised() {
    let home = scratch("features-home");
    let h = harness(&home, "srv_import_features", Vec::new()).await;
    // The sheet opened from a workspace and its search box need the first
    // two; every sheet needs the snapshot, which lists the providers.
    for feature in [
        "importSessionWorkspaceTarget",
        "importSessionSearch",
        "providersSnapshot",
    ] {
        assert!(h.info.has(feature), "{feature}");
    }
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_are_listed_newest_first_as_the_import_sheet_reads_them() {
    let home = scratch("list-home");
    let app = scratch("list-app");
    let now = clock::now_ms();
    let importer = Importer::new(
        "importer",
        vec![
            session("s-new", &app, now - 1_000),
            known(
                "s-mid",
                &app,
                now - 2_000,
                "Parser work",
                "fix the parser",
                "and the lexer",
            ),
            session("s-old", &app, now - 3_000),
        ],
    );
    let asked = importer.seen.asked.clone();
    let h = harness(&home, "srv_import_list", vec![importer]).await;

    let reply = h.list(json!({"providers": ["importer"], "limit": 2})).await;
    assert_eq!(ids(&reply), ["s-new", "s-mid"]);
    let entries = reply["entries"].as_array().unwrap();
    // All eight keys, `null` for what the CLI does not know, and none of the
    // rest (Paseo's tests pin the absence of the session's own handle).
    assert_eq!(
        entries[1],
        json!({
            "providerId": "importer", "providerLabel": "Label of importer",
            "providerHandleId": "s-mid", "cwd": text(&app), "title": "Parser work",
            "firstPromptPreview": "fix the parser", "lastPromptPreview": "and the lexer",
            "lastActivityAt": clock::iso_from_ms(now - 2_000),
        })
    );
    let bare = entries[0].as_object().unwrap();
    assert_eq!(bare.len(), 8, "{bare:?}");
    assert_eq!(bare["title"], Value::Null);
    assert_eq!(bare["firstPromptPreview"], Value::Null);
    assert_eq!(bare["lastPromptPreview"], Value::Null);
    // Nothing left out and nothing failed: neither key is sent.
    assert!(
        reply.get("filteredAlreadyImportedCount").is_none(),
        "{reply}"
    );
    assert!(reply.get("providerErrors").is_none(), "{reply}");

    // Twenty by default, and the provider is asked for as many.
    assert_eq!(
        h.listed(json!({"providers": ["importer"]})).await,
        ["s-new", "s-mid", "s-old"]
    );
    assert_eq!(
        *asked.lock().unwrap(),
        [(None, 2), (None, 20)],
        "no folder asked for"
    );

    // The limit is 1 to 200, a whole number.
    assert_eq!(h.listed(json!({"limit": 200})).await.len(), 3);
    for limit in [json!(0), json!(201), json!(-1), json!(1.5), json!("ten")] {
        let (code, _) = h
            .refused(typed(
                "fetch_recent_provider_sessions_request",
                json!({"limit": limit}),
            ))
            .await;
        assert_eq!(code.as_deref(), Some("unknown_schema"), "{limit}");
    }
    h.stop().await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_keeps_the_folder_the_time_and_leaves_out_sessions_that_name_agents() {
    let home = scratch("filter-home");
    let app = scratch("filter-app");
    let other = scratch("filter-other");
    let now = clock::now_ms();
    let mut titling = session("s-titling", &app, now - 500);
    titling.first_prompt = Some(format!("  {METADATA_PROMPT} Prompt: fix the build"));
    let importer = Importer::new(
        "importer",
        vec![
            session("s-app", &app, now - 1_000),
            session("s-other", &other, now - 2_000),
            titling,
            session("s-ancient", &app, now - 10 * DAY),
        ],
    );
    let asked = importer.seen.asked.clone();
    let h = harness(&home, "srv_import_filter", vec![importer]).await;

    // The provider is told the folder; the daemon keeps to it whatever the
    // provider sent. A session that only named an agent is never listed.
    let in_app = h.listed(json!({"cwd": text(&app)})).await;
    assert_eq!(in_app, ["s-app", "s-ancient"]);
    assert_eq!(
        asked.lock().unwrap().last().unwrap(),
        &(Some(text(&app)), 20)
    );
    assert_eq!(
        h.listed(json!({})).await,
        ["s-app", "s-other", "s-ancient"],
        "every folder's, when none is asked for"
    );

    let since = clock::iso_from_ms(now - DAY);
    assert_eq!(
        h.listed(json!({"cwd": text(&app), "since": since})).await,
        ["s-app"]
    );
    assert_eq!(
        h.listed(json!({"since": ""})).await.len(),
        3,
        "no time at all"
    );
    let (code, message) = h
        .refused(typed(
            "fetch_recent_provider_sessions_request",
            json!({"since": "last tuesday"}),
        ))
        .await;
    assert_eq!(code.as_deref(), Some("invalid_since"));
    assert_eq!(message, "Invalid recent provider sessions since");
    h.stop().await;
    for dir in [home, app, other] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A folder reached through a symlink is the same folder, whichever of the
/// two the request or the session names.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_folder_is_the_same_folder() {
    let home = scratch("link-home");
    let app = scratch("link-app");
    let other = scratch("link-other");
    let link = std::env::temp_dir().join(format!(
        "burrow-import-link-{}",
        burrow_daemon::ids::agent_id()
    ));
    std::os::unix::fs::symlink(&app, &link).unwrap();
    let now = clock::now_ms();
    let importer = Importer::new(
        "importer",
        vec![
            session("s-real", &app, now - 1_000),
            session("s-link", &link, now - 2_000),
            session("s-other", &other, now - 3_000),
        ],
    );
    let h = harness(&home, "srv_import_link", vec![importer]).await;

    for asked in [&app, &link] {
        assert_eq!(
            h.listed(json!({"cwd": text(asked)})).await,
            ["s-real", "s-link"],
            "asked for {}",
            asked.display()
        );
    }
    h.stop().await;
    let _ = std::fs::remove_file(&link);
    for dir in [home, app, other] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_matches_names_prompts_and_the_folders_name_in_any_case() {
    let home = scratch("query-home");
    let ledger = scratch("ledger");
    let elsewhere = scratch("elsewhere");
    let now = clock::now_ms();
    let importer = Importer::new(
        "importer",
        vec![
            known(
                "by-title",
                &elsewhere,
                now - 1_000,
                "Invoice parser",
                "x",
                "y",
            ),
            known(
                "by-first",
                &elsewhere,
                now - 2_000,
                "Misc",
                "Fix the BILLING bug",
                "y",
            ),
            known(
                "by-last",
                &elsewhere,
                now - 3_000,
                "Misc",
                "x",
                "Say Hello World",
            ),
            known("by-folder", &ledger, now - 4_000, "Misc", "x", "y"),
        ],
    );
    let asked = importer.seen.asked.clone();
    let h = harness(&home, "srv_import_query", vec![importer]).await;

    for (query, expected) in [
        ("INVOICE", vec!["by-title"]),
        ("  billing  ", vec!["by-first"]),
        ("hello world", vec!["by-last"]),
        ("LEDGER", vec!["by-folder"]),
        ("zzz", vec![]),
        ("misc", vec!["by-first", "by-last", "by-folder"]),
        ("  ", vec!["by-title", "by-first", "by-last", "by-folder"]),
    ] {
        assert_eq!(
            h.listed(json!({"providers": ["importer"], "query": query}))
                .await,
            expected,
            "searching {query:?}"
        );
    }
    // A search reads 500 sessions, to find the page's worth in.
    let asked = asked.lock().unwrap().clone();
    assert!(
        asked[..6].iter().all(|(_, limit)| *limit == 500),
        "{asked:?}"
    );
    assert_eq!(asked[6], (None, 20), "a blank search is none");
    h.stop().await;
    for dir in [home, ledger, elsewhere] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_provider_is_listed_alone_and_one_that_fails_is_reported() {
    let home = scratch("providers-home");
    let app = scratch("providers-app");
    let now = clock::now_ms();
    let h = harness(
        &home,
        "srv_import_providers",
        vec![
            Importer::new("importer", vec![session("s-1", &app, now - 1_000)]),
            Importer::new("second", vec![session("s-2", &app, now - 2_000)]),
            Importer::new("broken", Vec::new()).failing("cannot read the sessions"),
        ],
    )
    .await;

    let all = h.list(json!({})).await;
    assert_eq!(ids(&all), ["s-1", "s-2"]);
    assert_eq!(
        all["providerErrors"],
        json!([{"provider": "broken", "message": "cannot read the sessions"}])
    );
    let only = h.list(json!({"providers": ["second"]})).await;
    assert_eq!(ids(&only), ["s-2"]);
    assert_eq!(only["entries"][0]["providerId"], "second");
    assert!(only.get("providerErrors").is_none(), "{only}");
    let broken = h.list(json!({"providers": ["broken"]})).await;
    assert_eq!(ids(&broken), Vec::<String>::new());
    assert_eq!(broken["providerErrors"][0]["provider"], "broken");
    // A provider that does not list, or that Burrow does not have: nothing.
    for provider in ["echo", "nobody"] {
        let reply = h.list(json!({"providers": [provider]})).await;
        assert_eq!(reply["entries"], json!([]), "{provider}");
        assert!(reply.get("providerErrors").is_none(), "{provider}: {reply}");
    }
    assert_eq!(h.listed(json!({"providers": []})).await.len(), 0);
    h.stop().await;
    for dir in [home, app] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_an_agent_has_is_counted_not_listed_until_the_agent_is_archived() {
    let home = scratch("imported-home");
    let app = scratch("imported-app");
    let now = clock::now_ms();
    let importer = Importer::new(
        "importer",
        vec![
            session("s-a", &app, now - 1_000),
            session("s-b", &app, now - 2_000),
        ],
    );
    let asked = importer.seen.asked.clone();
    let h = harness(&home, "srv_import_counted", vec![importer]).await;

    let imported = h.import_session("s-a", &app).await;
    assert_eq!(imported["status"], "agent_resumed", "{imported}");
    let reply = h.list(json!({"providers": ["importer"], "limit": 5})).await;
    assert_eq!(ids(&reply), ["s-b"]);
    assert_eq!(reply["filteredAlreadyImportedCount"], 1);
    // The provider is asked for the page and the sessions that will go.
    assert_eq!(asked.lock().unwrap().last().unwrap(), &(None, 6));

    // An archived agent's session is listed again, and not counted.
    let agent = imported["agentId"].as_str().unwrap();
    h.request(json!({"type": "archive_agent_request", "agentId": agent}))
        .await;
    let reply = h.list(json!({"providers": ["importer"], "limit": 5})).await;
    assert_eq!(ids(&reply), ["s-a", "s-b"]);
    assert!(
        reply.get("filteredAlreadyImportedCount").is_none(),
        "{reply}"
    );
    assert_eq!(asked.lock().unwrap().last().unwrap(), &(None, 5));
    h.stop().await;
    for dir in [home, app] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn importing_makes_a_closed_agent_with_the_sessions_conversation_that_resumes_it() {
    let home = scratch("import-home");
    let app = scratch("import-app");
    let now = clock::now_ms();
    let make = |app: &Path| {
        Importer::new(
            "importer",
            vec![
                session("s-a", app, now - 1_000),
                session("s-b", app, now - 2_000),
                session("s-c", app, now - 3_000),
            ],
        )
        .with_history(
            "s-a",
            &[
                ("user_message", "  Fix the parser\nplease"),
                ("assistant_message", "Done."),
                ("user_message", "Thanks"),
            ],
        )
        .with_title("s-c", "Renamed in the CLI")
    };
    let importer = make(&app);
    let seen = importer.seen.clone();
    let server_id = "srv_import_fresh";
    let h = harness(&home, server_id, vec![importer]).await;
    assert!(h.workspaces().await.is_empty());

    let reply = h
        .import(
            json!({"providerId": "importer", "providerHandleId": "s-a", "cwd": text(&app),
                       "labels": {"team": "x"}}),
        )
        .await;
    assert_eq!(reply["status"], "agent_resumed", "{reply}");
    assert_eq!(reply["timelineSize"], 3);
    let agent = &reply["agent"];
    let id = reply["agentId"].as_str().unwrap().to_string();
    assert_eq!(agent["id"], id.as_str());
    assert_eq!(agent["provider"], "importer");
    assert_eq!(agent["cwd"], text(&app));
    // Nothing runs until its first prompt, as after a restart.
    assert_eq!(agent["status"], "closed");
    assert!(agent["archivedAt"].is_null());
    assert_eq!(agent["title"], "Fix the parser");
    assert_eq!(agent["labels"], json!({"team": "x"}));
    assert_eq!(
        agent["persistence"],
        json!({"provider": "importer", "sessionId": "s-a", "nativeHandle": "s-a"})
    );
    assert!(
        seen.started.lock().unwrap().is_empty(),
        "no session started"
    );

    // A workspace was made for the folder, and the clients were told.
    let workspaces = h.workspaces().await;
    assert_eq!(workspaces.len(), 1, "{workspaces:#?}");
    assert_eq!(workspaces[0]["workspaceDirectory"], text(&app));
    assert_eq!(agent["workspaceId"], workspaces[0]["id"]);
    let pushed = h
        .until("the agent", |m| {
            m["type"] == "agent_update"
                && m["payload"]["kind"] == "upsert"
                && m["payload"]["agent"]["id"] == id.as_str()
        })
        .await;
    assert!(
        pushed.iter().any(|m| m["type"] == "workspace_update"
            && m["payload"]["kind"] == "upsert"
            && m["payload"]["workspace"]["workspaceDirectory"] == text(&app)),
        "the new workspace comes first: {pushed:#?}"
    );

    // Its conversation is there already.
    let expected = [
        "user_message:  Fix the parser\nplease",
        "assistant_message:Done.",
        "user_message:Thanks",
    ];
    assert_eq!(texts(&h.conversation(&id).await), expected);
    assert_eq!(*seen.read.lock().unwrap(), ["s-a"]);

    // The first prompt resumes the session, and does not read it again.
    let sent = h
        .request(json!({"type": "send_agent_message_request", "agentId": id,
                        "text": "hello", "messageId": "m-1"}))
        .await;
    assert_eq!(sent["accepted"], true, "{sent}");
    h.request(json!({"type": "wait_for_finish_request", "agentId": id}))
        .await;
    {
        let started = seen.started.lock().unwrap();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].resume.as_deref(), Some("s-a"));
        assert_eq!(started[0].cwd, text(&app));
    }
    assert_eq!(*seen.read.lock().unwrap(), ["s-a"], "read once");
    let items = texts(&h.conversation(&id).await);
    assert_eq!(items[..3], expected);
    assert!(items.last().unwrap().contains("echo: hello"), "{items:?}");
    assert_eq!(h.agent(&id).await["persistence"]["sessionId"], "s-a");

    // The session is imported now; the others reuse the folder's workspace.
    let again = h.import_session("s-a", &app).await;
    assert_eq!(failed(&again), "Provider session is already imported: s-a");
    // Paseo's CLI still sends `provider` and `sessionId`.
    let legacy = h
        .import(json!({"provider": "importer", "sessionId": "s-b", "cwd": text(&app)}))
        .await;
    assert_eq!(legacy["status"], "agent_resumed", "{legacy}");
    assert_eq!(legacy["timelineSize"], 0, "a session with no conversation");
    assert!(legacy["agent"]["title"].is_null());
    assert_eq!(legacy["agent"]["workspaceId"], agent["workspaceId"]);
    // A rename the user gave the session is its agent's name.
    let renamed = h.import_session("s-c", &app).await;
    assert_eq!(renamed["agent"]["title"], "Renamed in the CLI", "{renamed}");
    assert_eq!(h.workspaces().await.len(), 1);
    assert_eq!(h.agent_ids().await.len(), 3);
    h.stop().await;

    // A restart keeps it: closed, its conversation in place.
    let h = harness(&home, server_id, vec![make(&app)]).await;
    assert_eq!(h.agent_ids().await.len(), 3);
    let restored = h.agent(&id).await;
    assert_eq!(restored["status"], "closed");
    assert_eq!(restored["persistence"]["sessionId"], "s-a");
    assert_eq!(h.conversation(&id).await.len(), 5);
    h.stop().await;
    for dir in [home, app] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_import_that_cannot_go_ahead_is_refused_in_paseos_words_and_changes_nothing() {
    let home = scratch("refused-home");
    let app = scratch("refused-app");
    let gone = app.join("gone");
    let h = harness(
        &home,
        "srv_import_refused",
        vec![Importer::new("importer", vec![session("s-a", &app, 1_000)])],
    )
    .await;
    let cwd = text(&app);
    let missing = format!(
        "Working directory does not exist or is not a directory: {}",
        gone.display()
    );

    for (message, why) in [
        (
            json!({"providerId": "importer", "cwd": cwd}),
            "Import requires providerId and providerHandleId",
        ),
        (
            json!({"providerHandleId": "s-a", "cwd": cwd}),
            "Import requires providerId and providerHandleId",
        ),
        (
            json!({"provider": "importer", "cwd": cwd}),
            "Import requires providerId and providerHandleId",
        ),
        (
            json!({"providerId": "importer", "providerHandleId": "s-a"}),
            "Import requires cwd from the selected provider session",
        ),
        (
            json!({"providerId": "importer", "providerHandleId": "s-a", "cwd": ""}),
            "Import requires cwd from the selected provider session",
        ),
        (
            json!({"providerId": "importer", "providerHandleId": "../../elsewhere", "cwd": cwd}),
            "Invalid provider session id: ../../elsewhere",
        ),
        (
            json!({"providerId": "nobody", "providerHandleId": "x", "cwd": cwd}),
            "Provider 'nobody' is not available",
        ),
        (
            json!({"providerId": "echo", "providerHandleId": "x", "cwd": cwd}),
            "Provider 'echo' does not support importing sessions",
        ),
        (
            json!({"providerId": "importer", "providerHandleId": "s-a", "cwd": text(&gone)}),
            missing.as_str(),
        ),
    ] {
        let reply = h.import(message.clone()).await;
        assert_eq!(failed(&reply), why, "{message}");
        assert!(reply["requestId"].is_string());
    }
    // Not a request at all.
    let (code, _) = h
        .refused(typed("import_agent_request", json!({"providerId": 5})))
        .await;
    assert_eq!(code.as_deref(), Some("unknown_schema"));
    assert!(h.agent_ids().await.is_empty());
    assert!(h.workspaces().await.is_empty());
    h.stop().await;
    for dir in [home, app] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_import_goes_into_the_workspace_asked_for_when_it_holds_the_folder() {
    let home = scratch("target-home");
    let app = scratch("target-app");
    let other = scratch("target-other");
    let h = harness(
        &home,
        "srv_import_target",
        vec![Importer::new(
            "importer",
            vec![session("s-a", &app, 1_000), session("s-b", &app, 2_000)],
        )],
    )
    .await;
    // Two workspaces for one folder: the one asked for is the one used.
    let mut made = Vec::new();
    for _ in 0..2 {
        let reply = h
            .request(json!({"type": "workspace.create.request",
                            "source": {"kind": "directory", "path": text(&app)}}))
            .await;
        made.push(reply["workspace"]["id"].as_str().unwrap().to_string());
    }
    let (first, second) = (&made[0], &made[1]);

    let reply = h
        .import(
            json!({"providerId": "importer", "providerHandleId": "s-a", "cwd": text(&app),
                       "workspaceId": second}),
        )
        .await;
    assert_eq!(reply["status"], "agent_resumed", "{reply}");
    assert_eq!(reply["agent"]["workspaceId"], second.as_str());
    assert_eq!(h.workspaces().await.len(), 2, "none made for it");

    let refusals = [
        (
            "wks_0000000000000000",
            text(&app),
            "Workspace not found: wks_0000000000000000".to_string(),
        ),
        (
            first.as_str(),
            text(&other),
            format!("Import cwd does not match workspace: {first}"),
        ),
    ];
    for (workspace, cwd, why) in refusals {
        let reply = h
            .import(
                json!({"providerId": "importer", "providerHandleId": "s-b", "cwd": cwd,
                           "workspaceId": workspace}),
            )
            .await;
        assert_eq!(failed(&reply), why);
    }
    // An archived workspace is not one to import into.
    h.request(json!({"type": "archive_workspace_request", "workspaceId": first}))
        .await;
    let reply = h
        .import(
            json!({"providerId": "importer", "providerHandleId": "s-b", "cwd": text(&app),
                       "workspaceId": first}),
        )
        .await;
    assert_eq!(failed(&reply), format!("Workspace not found: {first}"));
    assert_eq!(h.workspaces().await.len(), 1, "and none was made");
    h.stop().await;
    for dir in [home, app, other] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archived_agent_of_the_session_is_brought_back_not_made_again() {
    let home = scratch("restore-home");
    let app = scratch("restore-app");
    let other = scratch("restore-other");
    let importer = Importer::new("importer", vec![session("s-a", &app, 1_000)]).with_history(
        "s-a",
        &[("user_message", "hello"), ("assistant_message", "hi")],
    );
    let seen = importer.seen.clone();
    let h = harness(&home, "srv_import_restore", vec![importer]).await;

    let first = h
        .import(
            json!({"providerId": "importer", "providerHandleId": "s-a", "cwd": text(&app),
                       "labels": {"paseo.parent-agent-id": "parent-1", "keep": "1"}}),
        )
        .await;
    assert_eq!(first["status"], "agent_resumed", "{first}");
    let id = first["agentId"].as_str().unwrap().to_string();
    h.request(json!({"type": "archive_agent_request", "agentId": id}))
        .await;
    assert!(h.agent(&id).await["archivedAt"].is_string());
    let reply = h
        .request(json!({"type": "workspace.create.request",
                        "source": {"kind": "directory", "path": text(&app)}}))
        .await;
    let workspace = reply["workspace"]["id"].as_str().unwrap().to_string();
    let workspaces = h.workspaces().await.len();

    // Its folder must be the one it ran in; nothing changes when it is not.
    let wrong = h.import_session("s-a", &other).await;
    assert_eq!(
        failed(&wrong),
        "Provider session cwd does not match import cwd: s-a"
    );
    assert!(h.agent(&id).await["archivedAt"].is_string());
    assert_eq!(h.workspaces().await.len(), workspaces);

    let back = h
        .import(
            json!({"providerId": "importer", "providerHandleId": "s-a", "cwd": text(&app),
                       "workspaceId": workspace, "labels": {"new": "2"}}),
        )
        .await;
    assert_eq!(back["status"], "agent_resumed", "{back}");
    assert_eq!(back["agentId"], id.as_str(), "the same agent");
    assert_eq!(back["timelineSize"], 2, "its timeline as it was");
    let agent = &back["agent"];
    assert!(agent["archivedAt"].is_null());
    assert_eq!(agent["status"], "closed");
    assert_eq!(agent["workspaceId"], workspace.as_str());
    // Labels are merged; the parent that made the session is not its parent now.
    assert_eq!(agent["labels"], json!({"keep": "1", "new": "2"}));
    assert_eq!(h.agent_ids().await, std::slice::from_ref(&id));
    assert!(h.agent(&id).await["archivedAt"].is_null());
    assert_eq!(
        *seen.read.lock().unwrap(),
        ["s-a"],
        "its conversation was not read again"
    );
    assert_eq!(h.conversation(&id).await.len(), 2);
    h.stop().await;
    for dir in [home, app, other] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_imports_of_one_session_make_one_agent() {
    let home = scratch("race-home");
    let app = scratch("race-app");
    let importer = Importer::new("importer", vec![session("s-a", &app, 1_000)])
        .with_history("s-a", &[("user_message", "hello")]);
    let h = harness(&home, "srv_import_race", vec![importer]).await;

    let (one, two) = tokio::join!(h.import_session("s-a", &app), h.import_session("s-a", &app));
    let (made, refused) = if one["status"] == "agent_resumed" {
        (one, two)
    } else {
        (two, one)
    };
    assert_eq!(made["status"], "agent_resumed", "{made}");
    assert_eq!(
        failed(&refused),
        "Provider session is already imported: s-a"
    );
    assert_eq!(h.agent_ids().await, [made["agentId"].as_str().unwrap()]);
    assert_eq!(h.workspaces().await.len(), 1);
    assert_eq!(
        h.conversation(made["agentId"].as_str().unwrap())
            .await
            .len(),
        1
    );
    h.stop().await;
    for dir in [home, app] {
        let _ = std::fs::remove_dir_all(dir);
    }
}
