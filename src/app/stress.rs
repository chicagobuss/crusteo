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
//!
//! The defaults give 5,000 agents and roughly 10,000 Lanes cards. Data goes
//! through the same `Directory` reducers and push path as a real daemon's,
//! so views cannot tell the difference. Real hosts are not contacted.

use std::time::Duration;

use fleet::{Connection, Fleet, HostConfig};
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

    /// The whole fleet, already "connected" and hydrated.
    pub fn fleet(&self, now: i64) -> Fleet {
        let configs = (0..self.hosts).map(|h| HostConfig {
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
                    agent["pendingPermissions"] = json!([{ "id": format!("perm_{h}_{w}_{a}") }]);
                }
                3..=4 => {
                    agent["status"] = json!("error");
                    agent["requiresAttention"] = json!(true);
                    agent["attentionReason"] = json!("error");
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
    pub fn tick(&mut self, elapsed: Duration, now: i64) -> Vec<(String, Value)> {
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
                host_name(h),
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
                (host_name(h), message)
            }));
        pushes
    }
}

/// A short exchange for a synthetic agent's conversation, as a daemon's
/// timeline page: a prompt, and a reply in Markdown with the kinds of
/// markup a copy as Markdown has to keep.
pub fn exchange(agent_id: &str) -> Value {
    let reply = "## What changed\n\n\
        The parser now **streams** pages instead of buffering them, so \
        memory stays flat:\n\n\
        - `parse_page` yields rows as it reads\n\
        - [the benchmark](https://example.com/bench) dropped from 1.4 s to 0.3 s\n\
        - nothing else *moved*\n\n\
        ```rust\nfn parse_page(input: &str) -> impl Iterator<Item = Row> {\n    input.lines().map(Row::from)\n}\n```\n\n\
        Want me to open a pull request?";
    let entry = |seq: u64, item: Value| {
        json!({
            "provider": "claude",
            "item": item,
            "timestamp": "2026-10-13T09:00:00.000Z",
            "seqStart": seq,
            "seqEnd": seq,
            "turnId": "stress-turn-1",
        })
    };
    json!({
        "agentId": agent_id,
        "direction": "tail",
        "reset": true,
        "epoch": "stress",
        "window": { "maxSeq": 2 },
        "startCursor": { "seq": 1 },
        "endCursor": { "seq": 2 },
        "hasOlder": false,
        "hasNewer": false,
        "entries": [
            entry(1, json!({
                "type": "user_message",
                "text": "Make the parser stream its pages, then tell me what changed.",
                "messageId": "stress-user-1",
            })),
            entry(2, json!({
                "type": "assistant_message",
                "text": reply,
                "messageId": "stress-reply-1",
            })),
        ],
    })
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
}
