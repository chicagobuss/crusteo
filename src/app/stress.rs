//! A synthetic fleet far bigger and busier than any real one, for
//! performance work. If the UI is smooth here, it is smooth everywhere.
//!
//! `CRUSTEO_STRESS=1` uses the defaults. Knobs override them, e.g.
//! `CRUSTEO_STRESS=hosts:5,workspaces:20,agents:3,churn:50`:
//!
//! - `hosts`: synthetic daemons (default 20)
//! - `workspaces`: workspaces per host (default 50)
//! - `agents`: agents per workspace (default 5)
//! - `labels`: size of the label pool; each workspace gets 1-3 (default 60)
//! - `running`: percent of agents running, i.e. spinners (default 20)
//! - `churn`: agent updates per second across the fleet (default 200)
//! - `attention`: seconds between "agent finished" notifications for a
//!   random agent (default 0, none)
//! - `empty`: percent of workspaces without agents (default 0)
//! - `mixed`: 1 gives each workspace 1 to `agents` agents rather than
//!   exactly `agents` (default 0)
//! - `subagents`: 1 makes a workspace's later agents subagents of its
//!   first, and every fourth a subagent of the one before (default 0)
//! - `background`: percent of workspaces that are Paseo 0.12 background
//!   workspaces (default 0)
//! - `turns`: gives every conversation this many more turns, each a prompt
//!   and a long Markdown reply, for work on long transcripts (default 0)
//!
//! The defaults give 5,000 agents and roughly 10,000 Lanes cards. Data goes
//! through the same `Directory` reducers and push path as a real daemon's,
//! so views cannot tell the difference. Real hosts are not contacted.

use std::time::Duration;

use fleet::{Connection, Fleet, HostConfig, HostId};
use serde_json::{Value, json};

use crate::app::store::iso_from_millis;

#[derive(Debug, Clone)]
pub struct Stress {
    pub hosts: usize,
    pub workspaces: usize,
    pub agents: usize,
    pub labels: usize,
    pub running: u64,
    pub churn: u64,
    /// Seconds between attention notifications; 0 for none.
    pub attention: u64,
    /// Percent of workspaces with no agents.
    pub empty: u64,
    /// Workspaces hold 1 to `agents` agents rather than exactly `agents`.
    pub mixed: bool,
    /// A workspace's later agents are subagents of earlier ones.
    pub subagents: bool,
    /// Percent of workspaces that are background ones.
    pub background: u64,
}

impl Default for Stress {
    fn default() -> Self {
        Self {
            hosts: 20,
            workspaces: 50,
            agents: 5,
            labels: 60,
            running: 20,
            churn: 200,
            attention: 0,
            empty: 0,
            mixed: false,
            subagents: false,
            background: 0,
        }
    }
}

pub fn from_env() -> Option<Stress> {
    let raw = std::env::var("CRUSTEO_STRESS").ok()?;
    let mut stress = Stress::default();
    for part in raw.split(',') {
        let Some((key, value)) = part.split_once(':') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            continue;
        };
        let count = (value as usize).max(1);
        match key.trim() {
            "hosts" => stress.hosts = count,
            "workspaces" => stress.workspaces = count,
            "agents" => stress.agents = count,
            "labels" => stress.labels = count,
            "running" => stress.running = value.min(100),
            "churn" => stress.churn = value,
            "attention" => stress.attention = value,
            "empty" => stress.empty = value.min(100),
            "mixed" => stress.mixed = value > 0,
            "subagents" => stress.subagents = value > 0,
            "background" => stress.background = value.min(100),
            _ => {}
        }
    }
    Some(stress)
}

const WORDS: &[&str] = &[
    "fix",
    "flaky",
    "billing",
    "retry",
    "migration",
    "for",
    "the",
    "search",
    "index",
    "and",
    "rewrite",
    "onboarding",
    "flow",
    "investigate",
    "latency",
    "spike",
    "in",
    "payments",
    "api",
    "add",
    "tests",
    "around",
    "auth",
    "token",
    "refresh",
    "why",
    "is",
    "ci",
    "red",
    "again",
    "bump",
    "deps",
    "cleanup",
    "dead",
    "code",
    "telemetry",
    "dashboard",
    "port",
    "parser",
    "to",
    "rust",
    "design",
    "review",
    "notes",
    "überarbeiten",
    "日本語",
    "レビュー",
    "naïve",
    "café",
];
const LABELS: &[&str] = &[
    "infra",
    "billing",
    "search",
    "onboarding",
    "perf",
    "mobile",
    "web",
    "api",
    "auth",
    "data",
    "ml",
    "docs",
    "release",
    "security",
    "growth",
    "payments",
    "ci",
    "observability",
    "design",
    "support",
];
const COLORS: &[&str] = &[
    "violet", "sky", "emerald", "orange", "pink", "indigo", "teal", "red", "amber", "blue",
];
const PROJECTS: &[&str] = &[
    "monorepo",
    "website",
    "mobile-app",
    "data-pipeline",
    "infra-live",
    "design-system",
    "billing-svc",
    "search-svc",
];
/// What a pending permission asks to run.
const COMMANDS: &[&str] = &[
    "cargo test --workspace --all-targets -- --test-threads=1",
    "git push --force-with-lease origin HEAD",
    "rm -rf target/debug/incremental",
    "make deploy-preview\necho done",
];
/// What a failed agent's daemon says went wrong.
const ERRORS: &[&str] = &[
    "The provider stopped",
    "Rate limited: try again in 20 minutes",
    "Context window exceeded",
    "Provider process exited with code 137",
];
const PROVIDERS: &[(&str, &str)] = &[
    ("claude", "claude-opus-5-5"),
    ("codex", "gpt-6.1-sol"),
    ("gemini", "gemini-3.8-flash-high"),
    ("claude", "claude-sonnet-5-5"),
];

/// SplitMix64: tiny, deterministic, good enough for fake data.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x2545_f491_4f6c_dd1d)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn percent(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

fn host_name(h: usize) -> String {
    format!("stress-{h:02}")
}

/// Unlike a host saved before ids existed, a stress host's id is not its
/// name, so a view that confuses the two shows it under load.
fn host_id(h: usize) -> HostId {
    HostId::new(format!("hst_stress_{h:02}"))
}

fn label_name(i: usize) -> String {
    let word = LABELS[i % LABELS.len()];
    match i / LABELS.len() {
        0 => word.to_string(),
        n => format!("{word} {}", n + 1),
    }
}

impl Stress {
    /// How many agents workspace `w` holds on every host. Spread by a fixed
    /// hash, so churn agrees with the initial fleet.
    fn agents_in(&self, w: usize) -> usize {
        let spread = w.wrapping_mul(2_654_435_761) >> 7;
        if self.agents == 0 || (spread % 100) < self.empty as usize {
            0
        } else if self.mixed {
            1 + (spread / 100) % self.agents
        } else {
            self.agents
        }
    }

    /// Whether workspace `w` of host `h` is a background one. A fixed hash,
    /// so a workspace churn pushes again keeps its flag.
    fn background_at(&self, h: usize, w: usize) -> bool {
        let mut rng = Rng::new(((h as u64) << 32 | w as u64) ^ 0xb6);
        rng.percent(self.background)
    }

    /// The whole fleet, already "connected" and hydrated.
    pub fn fleet(&self, now: i64) -> Fleet {
        let configs = (0..self.hosts).map(|h| HostConfig {
            id: host_id(h),
            name: host_name(h),
            endpoint: None,
            password: None,
        });
        let mut fleet = Fleet::new(configs);
        let labels: Vec<Value> = (0..self.labels)
            .map(|i| json!({"name": label_name(i), "color": COLORS[i % COLORS.len()]}))
            .collect();
        let labels = json!({ "labels": labels });
        let mut rng = Rng::new(1);
        for (h, host) in fleet.hosts.iter_mut().enumerate() {
            host.connection = Connection::Online;
            host.server = Some(server_info(h));
            host.directory.projects_from_workspaces = true;
            host.directory.apply_labels_reply(&labels);
            let workspaces: Vec<Value> = (0..self.workspaces)
                .map(|w| self.workspace(&mut rng, h, w))
                .collect();
            host.directory
                .apply_workspaces_reply(&json!({ "entries": workspaces }), true);
            let mut agents = Vec::new();
            for w in 0..self.workspaces {
                for a in 0..self.agents_in(w) {
                    let age = rng.below(7 * 24 * 3600) as i64 * 1000;
                    agents.push(json!({ "agent": self.agent(&mut rng, h, w, a, now - age) }));
                }
            }
            host.directory
                .apply_agents_reply(&json!({ "entries": agents }), true);
            host.directory.agents_hydrated = true;
        }
        fleet
    }

    pub fn churn(&self) -> Churn {
        Churn {
            stress: self.clone(),
            rng: Rng::new(0xc0ffee),
            carry: 0.0,
            since_attention: Duration::ZERO,
        }
    }

    /// Stable fields come from the ids; `rng` picks the labels.
    fn workspace(&self, rng: &mut Rng, h: usize, w: usize) -> Value {
        let project = PROJECTS[(h + w) % PROJECTS.len()];
        let count = 1 + rng.below(3);
        let mut labels: Vec<String> = Vec::new();
        for _ in 0..count {
            let name = label_name(rng.below(self.labels));
            if !labels.contains(&name) {
                labels.push(name);
            }
        }
        json!({
            "id": format!("wks_{h}_{w}"),
            "projectId": format!("prj_{h}_{project}"),
            "projectDisplayName": project,
            "projectRootPath": format!("/home/dev/code/{project}"),
            "workspaceDirectory": format!("/home/dev/.paseo/worktrees/{project}-{w}"),
            "projectKind": "git",
            "workspaceKind": "worktree",
            "name": format!("{project}-{}-{w}", WORDS[w % WORDS.len()]),
            "status": "done",
            "labels": labels,
            "background": self.background_at(h, w),
        })
    }

    /// Title, provider and workspace come from the ids so updates keep
    /// them; status and attention are rolled from `rng`.
    fn agent(&self, rng: &mut Rng, h: usize, w: usize, a: usize, updated_at: i64) -> Value {
        let mut stable = Rng::new(((h * 1_000 + w) * 1_000 + a) as u64);
        let words = 3 + stable.below(14);
        let title: Vec<&str> = (0..words)
            .map(|_| WORDS[stable.below(WORDS.len())])
            .collect();
        let (provider, model) = PROVIDERS[stable.below(PROVIDERS.len())];
        let mut agent = json!({
            "id": format!("agt_{h}_{w}_{a}"),
            "provider": provider,
            "model": model,
            "cwd": format!("/home/dev/.paseo/worktrees/{}-{w}", PROJECTS[(h + w) % PROJECTS.len()]),
            "title": title.join(" "),
            "workspaceId": format!("wks_{h}_{w}"),
            "createdAt": iso_from_millis(updated_at - 3_600_000),
            "updatedAt": iso_from_millis(updated_at),
            "status": "idle",
        });
        if self.subagents && a > 0 {
            let parent = if a % 4 == 3 { a - 1 } else { 0 };
            agent["parentAgentId"] = json!(format!("agt_{h}_{w}_{parent}"));
        }
        if rng.percent(self.running) {
            agent["status"] = json!("running");
        } else {
            match rng.below(100) {
                0..=2 => {
                    agent["requiresAttention"] = json!(true);
                    agent["attentionReason"] = json!("permission");
                    // Every other one asks a question rather than leave.
                    agent["pendingPermissions"] = if (h + w + a).is_multiple_of(2) {
                        json!([question(&format!("perm_{h}_{w}_{a}"))])
                    } else {
                        let command = COMMANDS[stable.below(COMMANDS.len())];
                        json!([{
                            "id": format!("perm_{h}_{w}_{a}"),
                            "name": "Bash",
                            "kind": "tool",
                            "input": { "command": command },
                            "detail": { "type": "shell", "command": command },
                        }])
                    };
                }
                3..=4 => {
                    agent["status"] = json!("error");
                    agent["requiresAttention"] = json!(true);
                    agent["attentionReason"] = json!("error");
                    agent["lastError"] = json!(ERRORS[stable.below(ERRORS.len())]);
                }
                5..=34 => {
                    agent["requiresAttention"] = json!(true);
                    agent["attentionReason"] = json!("finished");
                }
                _ => {}
            }
        }
        agent
    }
}

/// Produces daemon-shaped pushes at the configured rate.
pub struct Churn {
    stress: Stress,
    rng: Rng,
    carry: f64,
    since_attention: Duration,
}

impl Churn {
    /// Pushes due after `elapsed`, as `(host, message)`. Mostly agent
    /// status flips (cards re-sort); one in fifty relabels a workspace
    /// (cards move between lanes).
    pub fn tick(&mut self, elapsed: Duration, now: i64) -> Vec<(HostId, Value)> {
        self.carry += self.stress.churn as f64 * elapsed.as_secs_f64();
        let due = self.carry.floor() as usize;
        self.carry -= due as f64;
        let s = &self.stress;
        let mut pushes = Vec::new();
        self.since_attention += elapsed;
        if s.attention > 0 && self.since_attention >= Duration::from_secs(s.attention) {
            self.since_attention = Duration::ZERO;
            let (h, w) = (self.rng.below(s.hosts), self.rng.below(s.workspaces));
            let a = self.rng.below(s.agents);
            pushes.push((
                host_id(h),
                json!({"type": "agent_attention_required", "payload": {
                    "agentId": format!("agt_{h}_{w}_{a}"),
                    "reason": "finished",
                    "notification": {
                        "title": "Agent finished",
                        "body": "Ready for review",
                        "data": {"workspaceId": format!("wks_{h}_{w}")},
                    },
                }}),
            ));
        }
        pushes.extend((0..due)
            .map(|_| {
                let h = self.rng.below(s.hosts);
                let w = self.rng.below(s.workspaces);
                // An empty workspace only ever changes itself.
                let message = if self.rng.below(50) == 0 || s.agents_in(w) == 0 {
                    let workspace = s.workspace(&mut self.rng, h, w);
                    json!({"type": "workspace_update", "payload": {"kind": "upsert", "workspace": workspace}})
                } else {
                    let a = self.rng.below(s.agents_in(w));
                    let agent = s.agent(&mut self.rng, h, w, a, now);
                    json!({"type": "agent_update", "payload": {"kind": "upsert", "agent": agent}})
                };
                (host_id(h), message)
            }));
        pushes
    }
}

/// What a synthetic daemon says it is. The second host is like Burrow
/// today, without Paseo's project and worktree requests, so the views'
/// plainer paths are exercised too.
fn server_info(h: usize) -> paseo_client::ServerInfo {
    let features = if h == 1 {
        json!({})
    } else {
        json!({
            "projectAdd": true, "stableProjectIdentity": true,
            "workspaceMultiplicity": true, "creationLifecycle": true, "forgeSearch": true,
            "projectRemove": true,
        })
    };
    let version = if h == 1 { "0.1.0" } else { "0.12.0" };
    paseo_client::ServerInfo::from_status(&json!({"type": "status", "payload": {
        "status": "server_info", "serverId": format!("srv_stress_{h}"),
        "hostname": host_name(h), "version": version, "features": features}}))
    .unwrap_or_default()
}

/// Folders a synthetic daemon has: its projects' checkouts, and a few more,
/// one of them not a git repository.
fn stress_folders() -> Vec<String> {
    let mut folders: Vec<String> = ["/home/dev", "/home/dev/code", "/home/dev/src"]
        .iter()
        .map(|dir| dir.to_string())
        .collect();
    folders.extend(PROJECTS.iter().map(|p| format!("/home/dev/code/{p}")));
    folders.extend(
        [
            "/home/dev/code/scratch-notes",
            "/home/dev/code/api-gateway",
            "/home/dev/code/api-gateway/docs",
            "/home/dev/src/tools",
            "/home/dev/src/website-legacy",
        ]
        .iter()
        .map(|dir| dir.to_string()),
    );
    folders
}

/// Ids for what synthetic daemons make, unique across the run.
static MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A synthetic daemon's reply, and the pushes, as host and message, that
/// follow it.
pub type Answer = (Result<Value, String>, Vec<(HostId, Value)>);

/// A synthetic daemon's answer to the requests of the new-workspace flow,
/// as Paseo 0.12 answers them, with the pushes that follow; None for any
/// other request. Prompts or branches with "fail" fail, for the error paths.
pub fn answer(host: &HostId, message: &Value, fleet: &Fleet) -> Option<Answer> {
    let kind = message["type"].as_str()?;
    let record = fleet.host(host);
    let full = record.is_some_and(|h| h.has("projectAdd"));
    let push = |message: Value| (host.clone(), message);
    let next = || MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let reply = match kind {
        "get_providers_snapshot_request" => json!({"entries": [
            {"provider": "claude", "label": "Claude", "status": "ready", "enabled": true,
             "defaultModeId": "default", "modes": [{"id": "default", "label": "Default"}],
             "models": [
                {"id": "claude-haiku-5", "label": "Haiku", "isDefault": true},
                {"id": "claude-opus-5-5", "label": "Opus"}]},
            {"provider": "codex", "label": "Codex", "status": "ready", "enabled": true,
             "models": [{"id": "gpt-6.1-sol", "label": "GPT-6.1", "isDefault": true}]},
        ]}),
        "directory_suggestions_request" if full => {
            let query = message["query"].as_str().unwrap_or_default().trim();
            let expanded = query.replacen('~', "/home/dev", 1);
            let rooted = expanded.starts_with('/');
            let needle = expanded.to_lowercase();
            let entries: Vec<Value> = if query.is_empty() {
                Vec::new()
            } else {
                stress_folders()
                    .into_iter()
                    .filter(|dir| {
                        if rooted {
                            dir.to_lowercase().starts_with(&needle) && *dir != expanded
                        } else {
                            dir.rsplit('/').next().unwrap_or(dir).contains(&needle)
                        }
                    })
                    .map(|path| json!({"path": path, "kind": "directory"}))
                    .collect()
            };
            json!({"directories": [], "entries": entries, "error": null})
        }
        "project.add.request" if full => {
            let cwd = message["cwd"]
                .as_str()
                .unwrap_or_default()
                .trim_end_matches('/');
            let cwd = cwd.replacen('~', "/home/dev", 1);
            if !stress_folders().contains(&cwd) {
                return Some((
                    Ok(
                        json!({"project": null, "error": format!("Directory not found: {cwd}"),
                              "errorCode": "directory_not_found"}),
                    ),
                    Vec::new(),
                ));
            }
            let name = cwd.rsplit('/').next().unwrap_or(&cwd).to_string();
            let existing = record.and_then(|h| {
                h.directory
                    .projects
                    .values()
                    .find(|p| p.project_root_path == cwd)
            });
            let id = existing
                .map(|p| p.project_id.clone())
                .unwrap_or_else(|| format!("prj_added_{}", fleet::creation::slugify(&name, 40)));
            let kind = if name.contains("notes") || cwd.matches('/').count() <= 2 {
                "non_git"
            } else {
                "git"
            };
            let project = json!({"projectId": id, "projectDisplayName": name,
                "projectRootPath": cwd, "projectKind": kind});
            return Some((
                Ok(json!({"project": project, "error": null})),
                vec![push(json!({"type": "project.update",
                    "payload": {"kind": "upsert", "project": project}}))],
            ));
        }
        "project.remove.request" if record.is_some_and(fleet::project_remove::supported) => {
            let id = message["projectId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            // The third host cannot write its registry, for the error path.
            if *host == host_id(2) {
                return Some((
                    Ok(
                        json!({"projectId": id, "accepted": false, "removedWorkspaceIds": [],
                        "error": "EACCES: permission denied, open '/home/dev/.paseo/projects.json'"}),
                    ),
                    Vec::new(),
                ));
            }
            // As Paseo: the open workspaces are archived with their agents,
            // each removal naming the project, then the project goes.
            let mut pushes = Vec::new();
            let mut removed = Vec::new();
            if let Some(record) = record {
                let directory = &record.directory;
                for workspace in directory.workspaces.values().filter(|w| w.project_id == id) {
                    for agent in directory.workspace_agents(&workspace.id) {
                        pushes.push(push(json!({"type": "agent_update",
                            "payload": {"kind": "remove", "agentId": agent.id}})));
                    }
                    pushes.push(push(json!({"type": "workspace_update", "payload": {
                        "kind": "remove", "id": workspace.id, "removedProjectId": id}})));
                    removed.push(workspace.id.clone());
                }
            }
            if removed.is_empty() {
                pushes.push(push(json!({"type": "workspace_update", "payload": {
                    "kind": "remove", "id": id, "removedProjectId": id}})));
            }
            pushes.push(push(json!({"type": "project.update",
                "payload": {"kind": "remove", "projectId": id}})));
            return Some((
                Ok(
                    json!({"projectId": id, "accepted": true, "removedWorkspaceIds": removed,
                          "error": null}),
                ),
                pushes,
            ));
        }
        "branch_suggestions_request" if full => {
            let query = message["query"].as_str().unwrap_or_default().to_lowercase();
            let details: Vec<Value> = [
                ("main", true, true, 2),
                ("develop", false, true, 0),
                ("release/1.2", false, true, 0),
                ("fix-login", true, true, 0),
                ("feat/old-ui", true, false, 0),
            ]
            .iter()
            .filter(|(name, ..)| name.contains(&query))
            .map(|(name, local, remote, behind)| {
                json!({"name": name, "hasLocal": local, "hasRemote": remote,
                       "localAhead": 0, "localBehind": behind})
            })
            .collect();
            json!({"branches": [], "branchDetails": details, "error": null})
        }
        "forge.search.request" if full => {
            let query = message["query"].as_str().unwrap_or_default().to_lowercase();
            let items: Vec<Value> = [
                (128, "Stream parser pages", "feat/stream-pages"),
                (131, "Retry flaky billing webhooks", "fix/billing-retry"),
                (140, "Bump deps for the release", "chore/deps"),
            ]
            .iter()
            .filter(|(_, title, head)| {
                title.to_lowercase().contains(&query) || head.contains(&query)
            })
            .map(|(number, title, head)| {
                json!({"kind": "change_request", "forge": "github", "number": number,
                       "title": title, "url": format!("https://example.com/pull/{number}"),
                       "state": "open", "body": null, "labels": [],
                       "headRefName": head, "baseRefName": "main"})
            })
            .collect();
            json!({"items": items, "authState": "authenticated", "error": null})
        }
        "workspace.create.request" => {
            let source = &message["source"];
            if source["kind"] == "worktree" && !full {
                return Some((
                    Ok(json!({"workspace": null, "setupTerminalId": null,
                              "error": "Worktree workspaces are not supported yet."})),
                    Vec::new(),
                ));
            }
            if message.get("agent").is_some() && !full {
                return Some((
                    Ok(json!({"workspace": null, "setupTerminalId": null,
                              "error": "An initial agent is not supported yet; create it with agent.create.request."})),
                    Vec::new(),
                ));
            }
            let branch = source["branchName"].as_str().unwrap_or_default();
            if branch.contains("fail") {
                return Some((
                    Ok(json!({"workspace": null, "setupTerminalId": null,
                              "error": format!("A branch named '{branch}' already exists")})),
                    Vec::new(),
                ));
            }
            let project_id = source["projectId"].as_str().unwrap_or_default();
            let project = record.and_then(|h| h.directory.projects.get(project_id));
            let root = project
                .map(|p| p.project_root_path.clone())
                .or_else(|| source["path"].as_str().map(str::to_string))
                .unwrap_or_default();
            let project_name = project
                .map(|p| p.name())
                .unwrap_or_else(|| root.rsplit('/').next().unwrap_or("project").to_string());
            let n = next();
            let worktree = source["kind"] == "worktree";
            let name = [
                &source["branchName"],
                &source["worktreeSlug"],
                &source["refName"],
            ]
            .iter()
            .find_map(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| project_name.clone());
            let directory = if worktree {
                format!(
                    "/home/dev/.paseo/worktrees/{project_name}/{}",
                    fleet::creation::slugify(&name, 50)
                )
            } else {
                root.clone()
            };
            let prompt = message["agent"]["initialPrompt"]
                .as_str()
                .unwrap_or_default();
            let workspace = json!({
                "id": format!("wks_made_{n}"),
                "projectId": if project_id.is_empty() { format!("prj_made_{n}") } else { project_id.to_string() },
                "projectDisplayName": project_name,
                "projectRootPath": root,
                "workspaceDirectory": directory,
                "projectKind": project.map(|p| p.project_kind.clone()).unwrap_or_else(|| "git".into()),
                "workspaceKind": if worktree { "worktree" } else { "local_checkout" },
                "name": name,
                "title": message["title"].as_str().map(str::to_string)
                    .or_else(|| fleet::creation::prompt_title(prompt)),
                "status": if prompt.is_empty() { "done" } else { "running" },
                "labels": [],
                "gitRuntime": {"currentBranch": name},
            });
            let mut pushes = vec![push(json!({"type": "workspace_update",
                "payload": {"kind": "upsert", "workspace": workspace}}))];
            let mut reply = json!({"workspace": workspace, "setupTerminalId": null, "error": null});
            if let Some(agent) = message.get("agent") {
                if prompt.contains("fail") {
                    reply["error"] = json!("The provider stopped before the first turn");
                } else {
                    let made = made_agent(host, &workspace, agent, n);
                    pushes.push(push(json!({"type": "agent_update",
                        "payload": {"kind": "upsert", "agent": made}})));
                    reply["agent"] = made;
                }
            }
            return Some((Ok(reply), pushes));
        }
        "agent.create.request" => {
            let id = message["workspaceId"].as_str().unwrap_or_default();
            let prompt = message["initialPrompt"].as_str().unwrap_or_default();
            if prompt.contains("fail") {
                return Some((
                    Ok(
                        json!({"agent": null, "error": "The provider stopped before the first turn"}),
                    ),
                    Vec::new(),
                ));
            }
            let workspace = json!({"id": id, "workspaceDirectory": message["config"]["cwd"]});
            let made = made_agent(host, &workspace, message, next());
            return Some((
                Ok(json!({"agent": made, "error": null})),
                vec![push(json!({"type": "agent_update",
                    "payload": {"kind": "upsert", "agent": made}}))],
            ));
        }
        // As a daemon without them refuses them.
        "directory_suggestions_request"
        | "project.add.request"
        | "branch_suggestions_request"
        | "forge.search.request" => {
            let error = format!("Unknown message type {kind} (unknown_schema)");
            return Some((Err(error), Vec::new()));
        }
        _ => return None,
    };
    Some((Ok(reply), Vec::new()))
}

/// The agent a synthetic daemon starts in `workspace`.
fn made_agent(host: &HostId, workspace: &Value, request: &Value, n: u64) -> Value {
    let now = crate::app::store::now_millis();
    let prompt = request["initialPrompt"].as_str().unwrap_or_default();
    json!({
        "id": format!("agt_made_{host}_{n}"),
        "provider": request["config"]["provider"],
        "model": request["config"]["model"],
        "cwd": workspace["workspaceDirectory"],
        "title": fleet::creation::prompt_title(prompt).unwrap_or_else(|| "New agent".into()),
        "workspaceId": workspace["id"],
        "createdAt": iso_from_millis(now),
        "updatedAt": iso_from_millis(now),
        "status": if prompt.is_empty() { "idle" } else { "running" },
    })
}

/// A pending question, as Claude Code's AskUserQuestion asks one: a single
/// choice that also takes a typed answer, and a multiple choice.
fn question(id: &str) -> Value {
    json!({
        "id": id,
        "kind": "question",
        "name": "AskUserQuestion",
        "title": "Question",
        "input": {
            "questions": [
                {
                    "id": "0",
                    "header": "Scope",
                    "question": "Do you want the full campaign resumed, or just a bounded compatibility pass?",
                    "options": [
                        { "label": "Bounded compatibility pass only" },
                        { "label": "Resume the full campaign too", "description": "Every lane picks up where it stopped" }
                    ],
                    "isOther": true
                },
                {
                    "id": "1",
                    "header": "Platforms",
                    "question": "Which platforms should it cover?",
                    "multiSelect": true,
                    "options": [{ "label": "macOS" }, { "label": "Linux" }, { "label": "Windows" }]
                }
            ]
        }
    })
}

/// Where a synthetic daemon keeps an agent's prompt images, as Burrow
/// lists them, and the file its reply's chart was saved to, as Paseo names
/// it. A name with "missing" is one the daemon cannot find.
fn stress_image_paths(agent_id: &str) -> [String; 3] {
    [
        format!("/home/dev/.burrow/attachments/{agent_id}-screen.png"),
        format!("/home/dev/.burrow/attachments/{agent_id}-missing.png"),
        format!("/tmp/paseo-attachments-stress/{agent_id}-chart.png"),
    ]
}

/// Reads a synthetic daemon's image: a picture made up from its path,
/// after a moment, as if over the network.
pub fn read_image(
    file: &fleet::images::ImageFile,
    cx: &gpui_kit::gpui::App,
) -> gpui_kit::gpui::Task<Result<fleet::prompt::ImageAttachment, crate::app::images::Failure>> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use gpui_kit::gpui::AppContext as _;
    let path = file.path.clone();
    let timer = cx.background_executor().timer(Duration::from_millis(400));
    cx.background_spawn(async move {
        timer.await;
        if path.contains("missing") {
            return Err(crate::app::images::Failure::Refused(
                "File not found".into(),
            ));
        }
        let mut rng = Rng::new(path.bytes().map(u64::from).sum());
        let (r, g, b) = (
            rng.below(200) as u8,
            rng.below(200) as u8,
            rng.below(200) as u8,
        );
        let (width, height) = if path.contains("chart") {
            (720, 360)
        } else {
            (640, 400)
        };
        let picture = image::RgbImage::from_fn(width, height, |x, y| {
            let shade = |c: u8, t: u32, span: u32| c.saturating_add((t * 55 / span) as u8);
            image::Rgb([shade(r, x, width), shade(g, y, height), b])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        picture
            .write_to(&mut png, image::ImageFormat::Png)
            .map_err(|e| crate::app::images::Failure::Refused(e.to_string()))?;
        Ok(fleet::prompt::ImageAttachment::new(
            STANDARD.encode(png.into_inner()),
            "image/png",
        ))
    })
}

/// A short exchange for a synthetic agent's conversation, as a daemon's
/// timeline page: a prompt with two images another client sent, one of
/// them gone, and a reply in Markdown with the kinds of markup a copy as
/// Markdown has to keep, and an image saved by the daemon. Agents in odd
/// workspaces talk to other agents instead; see [`agent_talk`].
pub fn exchange(agent_id: &str) -> Value {
    if let Some(page) = agent_talk(agent_id) {
        return page;
    }
    let [screen, missing, chart] = stress_image_paths(agent_id);
    let reply = format!(
        "## What changed\n\n\
        The parser now **streams** pages instead of buffering them, so \
        memory stays flat:\n\n\
        - `parse_page` yields rows as it reads\n\
        - [the benchmark](https://example.com/bench) dropped from 1.4 s to 0.3 s\n\
        - nothing else *moved*\n\n\
        ```rust\nfn parse_page(input: &str) -> impl Iterator<Item = Row> {{\n    input.lines().map(Row::from)\n}}\n```\n\n\
        ![Image](file://{chart})\n\n\
        Want me to open a pull request?"
    );
    let entry = |seq: u64, turn: &str, at: &str, item: Value| {
        json!({
            "provider": "claude",
            "item": item,
            "timestamp": format!("2026-10-13T09:{at}.000Z"),
            "seqStart": seq,
            "seqEnd": seq,
            "turnId": turn,
        })
    };
    // A long transcript first, when asked for: prompts and long replies.
    let long: Vec<Value> = (0..extra_turns())
        .flat_map(|n| {
            let turn = format!("stress-long-{n}");
            let (seq, at) = (
                2 * n as u64 + 1,
                format!("{:02}:{:02}", n / 60 % 60, n % 60),
            );
            [
                entry(
                    seq,
                    &turn,
                    &at,
                    json!({
                        "type": "user_message",
                        "text": format!("Step {n}: look at the next module and say what it does."),
                        "messageId": format!("stress-long-user-{n}"),
                    }),
                ),
                entry(
                    seq + 1,
                    &turn,
                    &at,
                    json!({
                        "type": "assistant_message",
                        "text": format!("### Module {n}\n\n{}", LONG_REPLY),
                        "messageId": format!("stress-long-reply-{n}"),
                    }),
                ),
            ]
        })
        .collect();
    let base = long.len() as u64;
    let shift = |mut entry: Value| {
        let seq = entry["seqStart"].as_u64().unwrap_or(0) + base;
        entry["seqStart"] = json!(seq);
        entry["seqEnd"] = json!(seq);
        entry
    };
    // After answering, the agent resumes on its own twice, as an
    // orchestrator does: turns that no message starts.
    let page = json!({
        "agentId": agent_id,
        "direction": "tail",
        "reset": true,
        "epoch": "stress",
        "window": { "maxSeq": base + 5 },
        "startCursor": { "seq": 1 },
        "endCursor": { "seq": base + 5 },
        "hasOlder": false,
        "hasNewer": false,
        "entries": [
            entry(1, "stress-turn-1", "00:00", json!({
                "type": "user_message",
                "text": "Make the parser stream its pages, then tell me what changed.",
                "messageId": "stress-user-1",
                "images": [
                    { "path": screen, "mimeType": "image/png" },
                    { "path": missing, "mimeType": "image/png" },
                ],
            })),
            entry(2, "stress-turn-1", "01:12", json!({
                "type": "assistant_message",
                "text": reply,
                "messageId": "stress-reply-1",
            })),
            entry(3, "stress-turn-2", "09:30", json!({
                "type": "tool_call",
                "callId": "stress-call-1",
                "name": "Bash",
                "detail": { "type": "shell", "command": "cargo bench -p parser" },
                "status": "completed",
                "error": null,
            })),
            entry(4, "stress-turn-2", "10:05", json!({
                "type": "assistant_message",
                "text": "The nightly benchmark finished: still 0.3 s, so the change holds.",
                "messageId": "stress-reply-2",
            })),
            entry(5, "stress-turn-3", "20:00", json!({
                "type": "assistant_message",
                "text": "Quota at 09:20: plenty left for the pull request.",
                "messageId": "stress-reply-3",
            })),
        ],
    });
    if long.is_empty() {
        return page;
    }
    let mut page = page;
    let tail: Vec<Value> = page["entries"].as_array().cloned().unwrap_or_default();
    page["entries"] = json!(
        long.into_iter()
            .chain(tail.into_iter().map(shift))
            .collect::<Vec<_>>()
    );
    page
}

/// A long reply, as agents write them: headings, lists, code and prose.
const LONG_REPLY: &str = "It reads the pages a request names, checks each against the \
index, and hands the rows on in order, so nothing waits for the whole file. Two things \
are worth knowing before changing it:\n\n\
- the index is read once per request and kept for its pages\n\
- a page that fails its checksum is skipped and logged, not retried\n\
- rows keep the order the pages had, even when pages arrive out of order\n\n\
```rust\npub fn rows(&self, pages: &[PageId]) -> impl Iterator<Item = Row> + '_ {\n    \
pages.iter().filter_map(|id| self.page(*id)).flat_map(|page| page.rows())\n}\n```\n\n\
Its tests cover the happy path and a corrupt page, but not a missing index; that is the \
first thing I would add. Everything else in it is small and reads the way it runs, so it \
should be safe to change once that test is there.";

/// `turns` from `CRUSTEO_STRESS`, read once.
fn extra_turns() -> usize {
    static TURNS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *TURNS.get_or_init(|| {
        let raw = std::env::var("CRUSTEO_STRESS").unwrap_or_default();
        (raw.split(','))
            .find_map(|part| part.strip_prefix("turns:")?.trim().parse().ok())
            .unwrap_or(0)
    })
}

/// The conversation of an agent in an odd workspace, which works with the
/// first agent of the workspace before it, in every shape agents' messages
/// take: one received and one sent, the other agent's notices that it
/// finished or failed, one from an agent no host knows any more with a
/// long title, and the same as an older daemon or a replayed history
/// leaves them, as tagged prompts.
fn agent_talk(agent_id: &str) -> Option<Value> {
    let mut ids = agent_id.strip_prefix("agt_")?.split('_');
    let (h, w) = (ids.next()?, ids.next()?.parse::<usize>().ok()?);
    if w % 2 == 0 {
        return None;
    }
    let peer = format!("agt_{h}_{}_0", w - 1);
    let gone = "agt_gone_7f3a";
    let gone_title = "Coordinate the quarterly ledger migration across checkout, refunds, invoicing and the reporting warehouse";
    let entry = |seq: u64, turn: &str, at: &str, item: Value| {
        json!({
            "provider": "claude",
            "item": item,
            "timestamp": format!("2026-10-13T09:{at}.000Z"),
            "seqStart": seq,
            "seqEnd": seq,
            "turnId": turn,
        })
    };
    // As Paseo 0.12 projects a message from another agent.
    let received = |id: &str, event: &str, sender: Value, text: &str| {
        json!({
            "type": "tool_call",
            "callId": format!("paseo-agent-message:{id}"),
            "name": "agent_message",
            "status": "completed",
            "error": null,
            "detail": { "type": "plain_text", "text": text, "icon": "bot" },
            "agentMessage": { "event": event, "sender": sender, "text": text },
        })
    };
    let reply = |id: &str, text: &str| json!({ "type": "assistant_message", "text": text, "messageId": id });
    let finished = format!(
        "Agent {peer} (parser review) finished.\n\n<agent-response>\nFixed the second unwrap in parse_page and added a test for empty pages.\n</agent-response>"
    );
    let errored = format!(
        "Agent {gone} ({gone_title}) errored.\n\n<agent-response>\nThe refunds service would not start: port 7020 is taken.\n</agent-response>"
    );
    let entries = vec![
        entry(
            1,
            "talk-1",
            "00:00",
            received(
                "m1",
                "message",
                json!({ "id": peer }),
                "Review the parser change in crates/parser before I merge it.\nFocus on the error paths; the happy path is covered.",
            ),
        ),
        entry(
            2,
            "talk-1",
            "00:40",
            json!({
                "type": "tool_call",
                "callId": "talk-read-1",
                "name": "Read",
                "detail": { "type": "read", "filePath": "/home/dev/code/parser/src/page.rs", "content": "fn parse_page() {}" },
                "status": "completed",
                "error": null,
            }),
        ),
        entry(
            3,
            "talk-1",
            "01:10",
            json!({
                "type": "tool_call",
                "callId": "talk-send-1",
                "name": "mcp__paseo__send_agent_prompt",
                "detail": { "type": "unknown",
                    "input": { "agentId": peer, "prompt": "Found two unchecked unwraps in parse_page; can you take the second one?\nThe first is mine." },
                    "output": { "status": "accepted" } },
                "status": "completed",
                "error": null,
            }),
        ),
        entry(
            4,
            "talk-1",
            "01:12",
            reply(
                "talk-reply-1",
                "The error paths look right except two unwraps; I asked for help with the second.",
            ),
        ),
        entry(
            5,
            "talk-2",
            "06:00",
            received("m2", "finished", json!({ "id": peer }), &finished),
        ),
        entry(
            6,
            "talk-2",
            "06:20",
            reply(
                "talk-reply-2",
                "Both unwraps are fixed and tested. The change is ready to merge.",
            ),
        ),
        entry(
            7,
            "talk-3",
            "08:00",
            received(
                "m3",
                "message",
                json!({ "id": gone, "title": gone_title }),
                "Pause any parser work until the release branch is cut.",
            ),
        ),
        entry(8, "talk-3", "08:05", reply("talk-reply-3", "Paused.")),
        entry(
            9,
            "talk-4",
            "09:00",
            received(
                "m4",
                "errored",
                json!({ "id": gone, "title": gone_title }),
                &errored,
            ),
        ),
        entry(
            10,
            "talk-4",
            "09:10",
            reply("talk-reply-4", "Noted; nothing here depends on refunds."),
        ),
        // An older daemon, or a provider's history replayed, keeps the tag.
        entry(
            11,
            "talk-5",
            "10:00",
            json!({
                "type": "user_message",
                "text": format!("<paseo-system version=\"1\" kind=\"agent-message\" source-agent-id=\"{peer}\" message-id=\"m5\">\nThe release branch is cut; carry on.\n</paseo-system>"),
                "messageId": "talk-user-5",
            }),
        ),
        entry(
            12,
            "talk-5",
            "10:30",
            reply("talk-reply-5", "Carrying on with the parser."),
        ),
        entry(
            13,
            "talk-6",
            "12:00",
            json!({
                "type": "user_message",
                "text": format!("<paseo-system>\nAgent {peer} (parser review) finished.\n\n<agent-response>\nRebased onto the release branch.\n</agent-response>\n</paseo-system>"),
                "messageId": "talk-user-6",
            }),
        ),
        entry(
            14,
            "talk-6",
            "12:15",
            reply(
                "talk-reply-6",
                "Good, everything is on the release branch now.",
            ),
        ),
        entry(
            15,
            "talk-7",
            "14:00",
            json!({
                "type": "user_message",
                "text": "Thanks both. Open the pull request when it's green.",
                "messageId": "talk-user-7",
            }),
        ),
        entry(16, "talk-7", "14:30", reply("talk-reply-7", "Will do.")),
    ];
    let last = entries.len();
    Some(json!({
        "agentId": agent_id,
        "direction": "tail",
        "reset": true,
        "epoch": "stress",
        "window": { "maxSeq": last },
        "startCursor": { "seq": 1 },
        "endCursor": { "seq": last },
        "hasOlder": false,
        "hasNewer": false,
        "entries": entries,
    }))
}

#[cfg(test)]
mod edge_tests {
    use super::*;
    #[test]
    fn mixed_empty_fleet_does_not_divide_by_zero() {
        let stress = Stress {
            agents: 0,
            mixed: true,
            hosts: 1,
            workspaces: 2,
            ..Default::default()
        };
        assert_eq!(stress.agents_in(0), 0);
        assert_eq!(stress.agents_in(1), 0);
        assert!(stress.fleet(0).hosts[0].directory.agents.is_empty());
    }

    #[test]
    fn synthetic_daemons_answer_the_new_workspace_flow() {
        let stress = Stress {
            hosts: 3,
            workspaces: 2,
            agents: 1,
            ..Default::default()
        };
        let fleet = stress.fleet(0);
        let full = host_id(0);
        let plain = host_id(1);
        // The second host is like Burrow: no project or worktree requests.
        assert!(fleet.host(&full).unwrap().has("projectAdd"));
        assert!(!fleet.host(&plain).unwrap().has("projectAdd"));
        let search = fleet::requests::directory_suggestions("notes", 30);
        let (reply, _) = answer(&full, &search, &fleet).unwrap();
        assert_eq!(
            reply.unwrap()["entries"][0]["path"],
            "/home/dev/code/scratch-notes"
        );
        let (reply, _) = answer(&plain, &search, &fleet).unwrap();
        assert!(fleet::creation::unsupported(&reply.unwrap_err()));

        let add = fleet::requests::add_project("/home/dev/code/scratch-notes");
        let (reply, pushes) = answer(&full, &add, &fleet).unwrap();
        assert_eq!(reply.unwrap()["project"]["projectKind"], "non_git");
        assert_eq!(pushes[0].1["type"], "project.update");

        let worktree = json!({"type": "workspace.create.request",
            "source": {"kind": "worktree", "cwd": "/home/dev/code/monorepo",
                       "projectId": "prj_0_monorepo", "worktreeSlug": "fix-login"},
            "agent": {"config": {"provider": "claude"}, "initialPrompt": "Fix login"}});
        let (reply, pushes) = answer(&full, &worktree, &fleet).unwrap();
        let reply = reply.unwrap();
        assert!(
            reply["workspace"]["workspaceDirectory"]
                .as_str()
                .unwrap()
                .ends_with("/fix-login")
        );
        assert!(reply["agent"]["id"].is_string());
        assert_eq!(pushes.len(), 2);
        // Refused where worktrees are not made.
        let (reply, _) = answer(&plain, &worktree, &fleet).unwrap();
        assert!(reply.unwrap()["error"].is_string());
    }

    #[test]
    fn synthetic_daemons_remove_projects_as_paseo_does() {
        let stress = Stress {
            hosts: 3,
            workspaces: 14,
            agents: 2,
            ..Default::default()
        };
        let mut fleet = stress.fleet(0);
        let full = host_id(0);
        // The first host's "monorepo" has workspaces 0 and 8.
        let remove = fleet::requests::remove_project("prj_0_monorepo");
        let (reply, pushes) = answer(&full, &remove, &fleet).unwrap();
        let removed = fleet::project_remove::parse_reply(&reply.unwrap()).unwrap();
        assert_eq!(removed, ["wks_0_0", "wks_0_8"]);
        assert_eq!(pushes.last().unwrap().1["type"], "project.update");
        let directory = &mut fleet.host_mut(&full).unwrap().directory;
        for (_, push) in &pushes {
            directory.apply_push(push);
        }
        assert!(!directory.projects.contains_key("prj_0_monorepo"));
        assert!(!directory.workspaces.contains_key("wks_0_0"));
        assert!(!directory.agents.contains_key("agt_0_0_0"));
        // Not offered where the daemon lacks it; refused on the third host.
        assert!(answer(&host_id(1), &remove, &fleet).is_none());
        let (reply, pushes) = answer(&host_id(2), &remove, &fleet).unwrap();
        assert!(fleet::project_remove::parse_reply(&reply.unwrap()).is_err());
        assert!(pushes.is_empty());
    }

    #[test]
    fn background_knob_marks_about_that_share_of_workspaces() {
        let none = Stress {
            hosts: 2,
            workspaces: 100,
            agents: 1,
            ..Default::default()
        };
        let some = Stress {
            background: 20,
            ..none.clone()
        };
        let count = |stress: &Stress| {
            let fleet = stress.fleet(0);
            (fleet.hosts.iter())
                .flat_map(|host| host.directory.workspaces.values())
                .filter(|workspace| workspace.background)
                .count()
        };
        assert_eq!(count(&none), 0);
        let marked = count(&some);
        assert!((20..=60).contains(&marked), "{marked} of 200");
        // A churned workspace keeps its flag.
        assert_eq!(
            some.workspace(&mut Rng::new(9), 1, 3)["background"],
            some.background_at(1, 3)
        );
    }
}
