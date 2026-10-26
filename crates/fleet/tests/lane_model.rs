//! The incremental lane model must always equal a full rebuild. Random
//! churn goes through the real directory reducers, and after every step the
//! patched lanes are compared with `LaneView::build` on the same fleet.

use fleet::lanes::{LabelLaneMode, Lane, LaneGrouping, LaneModel, LaneState, LaneView};
use fleet::{DirChange, Fleet, HostConfig, HostId, labels};
use serde_json::{Value, json};

/// xorshift64*: deterministic, so a failure reproduces from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

const HOSTS: [&str; 3] = ["alpha", "bravo", "charlie"];
const LABELS: [&str; 5] = ["infra", "Billing", "search", "ux", "Ops"];
/// Labels no host defines, in two spellings, so a lane's title depends on
/// which workspace a full build meets first.
const ADHOC: [&str; 2] = ["adhoc", "AdHoc"];
const STATUSES: [&str; 3] = ["running", "idle", "error"];

fn iso(tick: u64) -> String {
    format!(
        "2026-10-05T{:02}:{:02}:{:02}.000Z",
        tick / 3600 % 24,
        tick / 60 % 60,
        tick % 60
    )
}

fn workspace(rng: &mut Rng, host: usize, w: usize) -> Value {
    let mut labels: Vec<&str> = Vec::new();
    for _ in 0..rng.below(4) {
        let label = if rng.chance(15) {
            ADHOC[rng.below(ADHOC.len())]
        } else {
            LABELS[rng.below(LABELS.len())]
        };
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    let project = ["web", "api", "data"][w % 3];
    // Every sixth workspace is a background one, as Paseo 0.12 makes them,
    // and another moves in and out of the background (and its lane).
    let background = w == 5 || (w == 4 && rng.chance(25));
    // Renames reorder a lane's workspaces and retitle workspace lanes.
    let name = if rng.chance(20) {
        format!("renamed {w}")
    } else {
        format!("ws {w}")
    };
    json!({
        "id": format!("w{host}-{w}"),
        "projectId": format!("p-{project}"),
        "projectDisplayName": project,
        "projectRootPath": format!("/code/{project}"),
        "name": name,
        "status": "done",
        "labels": labels,
        "background": background,
    })
}

/// An agent; with `labels` (reef mode's catalog ids), its own label
/// assignment, often none (so it inherits), sometimes explicitly empty, one in
/// a format from the future, or a deleted label's id.
fn agent(
    rng: &mut Rng,
    id: &str,
    workspace: Option<String>,
    tick: u64,
    labels: &[String],
) -> Value {
    let mut agent = json!({
        "id": id,
        "title": format!("task {}", rng.below(5)),
        "provider": "claude",
        "model": "opus",
        "status": STATUSES[rng.below(STATUSES.len())],
        "updatedAt": iso(tick),
    });
    if let Some(workspace) = workspace {
        agent["workspaceId"] = json!(workspace);
    }
    if rng.chance(30) {
        agent["requiresAttention"] = json!(true);
        agent["attentionReason"] = json!("finished");
    }
    if !labels.is_empty() {
        let roll = rng.below(100);
        let assignment = if roll < 35 {
            None
        } else if roll < 45 {
            Some("v1:".to_string())
        } else if roll < 50 {
            Some("v2:anything".to_string())
        } else {
            let picked: Vec<&str> = (0..1 + rng.below(3))
                .map(|_| labels[rng.below(labels.len())].as_str())
                .collect();
            Some(format!("v1:{}", picked.join(",")))
        };
        if let Some(assignment) = assignment {
            agent["labels"] = json!({ labels::KEY: assignment });
        }
    }
    agent
}

struct World {
    rng: Rng,
    fleet: Fleet,
    tick: u64,
    next_agent: usize,
    /// In reef mode, the catalog's ids agents may name, a deleted one among
    /// them; empty otherwise.
    label_ids: Vec<String>,
}

impl World {
    fn new(seed: u64) -> Self {
        Self::with(seed, false)
    }

    /// With `reef`, the fleet has a catalog imported from the hosts' labels,
    /// one label deleted, and agents carry their own assignments.
    fn with(seed: u64, reef: bool) -> Self {
        let mut rng = Rng(seed);
        let mut fleet = Fleet::new(HOSTS.iter().map(|name| HostConfig {
            name: name.to_string(),
            ..Default::default()
        }));
        let labels: Vec<Value> = LABELS
            .iter()
            .map(|name| json!({"name": name, "color": "red"}))
            .collect();
        let mut next_agent = 0;
        for (h, host) in fleet.hosts.iter_mut().enumerate() {
            host.directory.projects_from_workspaces = true;
            host.directory
                .apply_labels_reply(&json!({ "labels": labels }));
            let workspaces: Vec<Value> = (0..6).map(|w| workspace(&mut rng, h, w)).collect();
            host.directory
                .apply_workspaces_reply(&json!({ "entries": workspaces }), true);
        }
        let mut label_ids = Vec::new();
        if reef {
            let mut catalog = labels::Catalog::default();
            let hosts: Vec<labels::ImportHost> = (fleet.hosts.iter())
                .map(|host| labels::ImportHost {
                    labels: &host.directory.labels,
                    workspaces: host.directory.workspaces.values().collect(),
                })
                .collect();
            catalog.import(&hosts, |_| "violet".into());
            label_ids = catalog.labels.iter().map(|l| l.id.to_string()).collect();
            let ops = catalog.by_name("Ops").unwrap().id.clone();
            catalog.delete(&ops).unwrap();
            fleet.labels = Some(catalog);
        }
        for (h, host) in fleet.hosts.iter_mut().enumerate() {
            let mut agents = Vec::new();
            for _ in 0..12 {
                let ws = (!rng.chance(10)).then(|| format!("w{h}-{}", rng.below(6)));
                let made = agent(&mut rng, &format!("a{next_agent}"), ws, 1, &label_ids);
                agents.push(json!({ "agent": made }));
                next_agent += 1;
            }
            host.directory
                .apply_agents_reply(&json!({ "entries": agents }), true);
            host.directory.agents_hydrated = true;
            host.directory.journal.clear();
        }
        Self {
            rng,
            fleet,
            tick: 10,
            next_agent,
            label_ids,
        }
    }

    /// One random change through the reducers; returns its journal entries.
    fn step(&mut self) -> Vec<(String, DirChange)> {
        self.tick += 1;
        let h = self.rng.below(HOSTS.len());
        let ids: Vec<String> = self.fleet.hosts[h]
            .directory
            .agents
            .keys()
            .cloned()
            .collect();
        let pick = |rng: &mut Rng| ids.get(rng.below(ids.len().max(1))).cloned();
        let roll = self.rng.below(100);
        let message = if roll < 55 && !ids.is_empty() {
            // Status, title or attention changes in place.
            let id = pick(&mut self.rng).unwrap();
            let ws = self.fleet.hosts[h].directory.agents[&id]
                .workspace_id
                .clone();
            let tick = self.tick;
            json!({"type": "agent_update", "payload": {"kind": "upsert", "agent": agent(&mut self.rng, &id, ws, tick, &self.label_ids)}})
        } else if roll < 62 && !ids.is_empty() {
            // A stale snapshot, which the directory ignores.
            let id = pick(&mut self.rng).unwrap();
            json!({"type": "agent_update", "payload": {"kind": "upsert", "agent": agent(&mut self.rng, &id, None, 0, &self.label_ids)}})
        } else if roll < 74 {
            let id = format!("a{}", self.next_agent);
            self.next_agent += 1;
            let ws = (!self.rng.chance(15)).then(|| format!("w{h}-{}", self.rng.below(6)));
            let tick = self.tick;
            json!({"type": "agent_update", "payload": {"kind": "upsert", "agent": agent(&mut self.rng, &id, ws, tick, &self.label_ids)}})
        } else if roll < 84 && !ids.is_empty() {
            let id = pick(&mut self.rng).unwrap();
            if self.rng.chance(50) {
                json!({"type": "agent_update", "payload": {"kind": "remove", "agentId": id}})
            } else {
                let mut archived = agent(&mut self.rng, &id, None, self.tick, &self.label_ids);
                archived["archivedAt"] = json!(iso(self.tick));
                json!({"type": "agent_update", "payload": {"kind": "upsert", "agent": archived}})
            }
        } else if roll < 94 && !ids.is_empty() {
            // Moves to another workspace, or out of any.
            let id = pick(&mut self.rng).unwrap();
            let ws = (!self.rng.chance(20)).then(|| format!("w{h}-{}", self.rng.below(6)));
            let tick = self.tick;
            json!({"type": "agent_update", "payload": {"kind": "upsert", "agent": agent(&mut self.rng, &id, ws, tick, &self.label_ids)}})
        } else if roll < 97 {
            let w = self.rng.below(6);
            let ws = workspace(&mut self.rng, h, w);
            json!({"type": "workspace_update", "payload": {"kind": "upsert", "workspace": ws}})
        } else if roll < 99 {
            // Only its status changes.
            let w = self.rng.below(6);
            let mut ws = workspace(&mut self.rng, h, w);
            if let Some(old) = self.fleet.hosts[h]
                .directory
                .workspaces
                .get(&format!("w{h}-{w}"))
            {
                ws["name"] = json!(old.name);
                ws["labels"] = json!(old.labels);
            }
            ws["status"] = json!("running");
            json!({"type": "workspace_update", "payload": {"kind": "upsert", "workspace": ws}})
        } else {
            // Gone; a later upsert brings it back.
            let id = format!("w{h}-{}", self.rng.below(6));
            json!({"type": "workspace_update", "payload": {"kind": "remove", "id": id}})
        };
        let host = &mut self.fleet.hosts[h];
        host.directory.apply_push(&message);
        let name = host.config.name.clone();
        host.directory
            .journal
            .drain(..)
            .map(|change| (name.clone(), change))
            .collect()
    }
}

fn unwrap(lanes: &[std::rc::Rc<Lane>]) -> Vec<Lane> {
    lanes.iter().map(|lane| (**lane).clone()).collect()
}

fn check(grouping: LaneGrouping, mode: LabelLaneMode, seed: u64, hide_background: bool) {
    check_in(World::new(seed), grouping, mode, seed, hide_background);
}

fn check_in(
    mut world: World,
    grouping: LaneGrouping,
    mode: LabelLaneMode,
    seed: u64,
    hide_background: bool,
) {
    let state = LaneState {
        label_mode: mode,
        // A user arrangement for some lanes, and one collapsed.
        order: vec!["search".into(), "web".into()],
        collapsed: ["infra".to_string()].into_iter().collect(),
        hide_background,
    };
    let mut model = LaneModel::build(&world.fleet, grouping, &state);
    for step in 0..600 {
        let changes = world.step();
        // The world's hosts were saved by name, so each name is its id.
        let changes: Vec<(HostId, &DirChange)> = (changes.iter())
            .map(|(host, change)| (HostId::new(host.clone()), change))
            .collect();
        model.apply(
            &world.fleet,
            changes.iter().map(|(host, change)| (host, *change)),
        );
        let expected = LaneView::build(&world.fleet, grouping, &state);
        assert!(
            unwrap(model.lanes()) == expected,
            "{grouping:?}/{mode:?} seed {seed}: diverged at step {step} after {changes:?}"
        );
    }
    // Most changes patch. What rebuilds is a lane titled by whichever of its
    // workspaces a full build meets first: undefined labels spelled two ways,
    // and renamed workspaces in workspace lanes.
    assert!(
        model.rebuilds() < 30,
        "{grouping:?}/{mode:?} seed {seed}: {} rebuilds in 600 steps",
        model.rebuilds()
    );
}

#[test]
fn incremental_lanes_equal_a_full_build_in_reef_mode() {
    // Agents carry their own labels, inherit their workspace's, or neither;
    // a workspace follows its agents. The patch must agree with a build.
    let world = World::with(1, true);
    let lanes = LaneView::build(&world.fleet, LaneGrouping::Label, &LaneState::default());
    assert!(
        lanes.iter().any(|lane| lane.key.starts_with("lbl_")),
        "lanes keyed by label id"
    );
    for seed in [1, 7, 42, 1234, 99_991] {
        for hide in [false, true] {
            for mode in LabelLaneMode::ALL {
                check_in(
                    World::with(seed, true),
                    LaneGrouping::Label,
                    mode,
                    seed,
                    hide,
                );
            }
            // Other groupings ignore the catalog.
            check_in(
                World::with(seed, true),
                LaneGrouping::Project,
                LabelLaneMode::Labels,
                seed,
                hide,
            );
        }
    }
}

#[test]
fn incremental_lanes_equal_a_full_build_under_random_churn() {
    for seed in [1, 7, 42, 1234, 99_991] {
        for hide in [false, true] {
            for mode in LabelLaneMode::ALL {
                check(LaneGrouping::Label, mode, seed, hide);
            }
            check(LaneGrouping::Project, LabelLaneMode::Labels, seed, hide);
            check(LaneGrouping::Host, LabelLaneMode::Labels, seed, hide);
            check(LaneGrouping::Workspace, LabelLaneMode::Labels, seed, hide);
        }
    }
}

#[test]
fn unchanged_lanes_keep_their_identity() {
    let mut world = World::new(5);
    let state = LaneState::default();
    let mut model = LaneModel::build(&world.fleet, LaneGrouping::Host, &state);
    let before: Vec<_> = model.lanes().to_vec();
    // An in-place update on the first host touches only that host's lane.
    let id = world.fleet.hosts[0]
        .directory
        .agents
        .keys()
        .next()
        .unwrap()
        .clone();
    let message = json!({"type": "agent_update", "payload": {"kind": "upsert",
        "agent": {"id": id, "status": "running", "updatedAt": iso(99_999)}}});
    world.fleet.hosts[0].directory.apply_push(&message);
    let changes = std::mem::take(&mut world.fleet.hosts[0].directory.journal);
    let alpha = HostId::new("alpha");
    model.apply(&world.fleet, changes.iter().map(|change| (&alpha, change)));
    for lane in model.lanes() {
        let old = before.iter().find(|old| old.key == lane.key).unwrap();
        assert_eq!(
            std::rc::Rc::ptr_eq(old, lane),
            lane.key != "alpha",
            "lane {}",
            lane.key
        );
    }
}
