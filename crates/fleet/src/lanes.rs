//! Lanes: every agent as a card in a workstream column.
//!
//! Grouping, column order and collapse live here so the view can stay a
//! projection. Column keys are stable across rebuilds.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use paseo_client::proto::{Agent, Bucket, Workspace};

use crate::directory::{DirChange, LabelDef, label_key};
use crate::sidebar::natural;
use crate::{Fleet, Host};

/// How a column decides which agents it owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaneGrouping {
    /// The workspace's labels. An agent with several labels is in each column.
    #[default]
    Label,
    /// The project, merged across hosts by project key (same rule as the sidebar).
    Project,
    /// The host the agent runs on.
    Host,
    /// The agent's workspace.
    Workspace,
}

impl LaneGrouping {
    pub fn as_str(self) -> &'static str {
        match self {
            LaneGrouping::Label => "label",
            LaneGrouping::Project => "project",
            LaneGrouping::Host => "host",
            LaneGrouping::Workspace => "workspace",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "project" => LaneGrouping::Project,
            "host" => LaneGrouping::Host,
            "workspace" => LaneGrouping::Workspace,
            _ => LaneGrouping::Label,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LaneGrouping::Label => "Label",
            LaneGrouping::Project => "Project",
            LaneGrouping::Host => "Host",
            LaneGrouping::Workspace => "Workspace",
        }
    }
}

/// Which label-derived columns the client displays. Other groupings ignore this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LabelLaneMode {
    #[default]
    Labels,
    Combinations,
    Both,
}
impl LabelLaneMode {
    pub const ALL: [Self; 3] = [Self::Labels, Self::Combinations, Self::Both];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Labels => "labels",
            Self::Combinations => "combinations",
            Self::Both => "both",
        }
    }
    pub fn parse(value: &str) -> Self {
        match value {
            "combinations" => Self::Combinations,
            "both" => Self::Both,
            _ => Self::Labels,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Labels => "Labels",
            Self::Combinations => "Combinations",
            Self::Both => "Both",
        }
    }
}

/// Label-derived membership mode, column order and collapse remembered by the client.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LaneState {
    pub label_mode: LabelLaneMode,
    /// Keys the user has arranged, front to back. Columns not listed sort
    /// after these.
    pub order: Vec<String>,
    pub collapsed: BTreeSet<String>,
}

/// A workspace the column's draft can create an agent in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneWorkspace {
    pub host: String,
    pub workspace_id: String,
    pub name: String,
}

/// One agent card. The same agent can appear in more than one lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneCard {
    pub host: String,
    pub agent_id: String,
    pub workspace_id: Option<String>,
    pub workspace_name: String,
    pub title: String,
    pub provider: String,
    pub model: Option<String>,
    pub updated_at: Option<String>,
    pub bucket: Bucket,
    pub attention_reason: Option<String>,
    /// The agent that started this one, for a subagent.
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaneKind {
    Label,
    Combination,
    Unlabelled,
    #[default]
    Other,
}

/// One workstream column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub key: String,
    pub kind: LaneKind,
    pub title: String,
    /// Label colour name (`red`, `violet`, …) when this column is a label.
    pub color: Option<String>,
    /// Normalized, sorted label members. Empty for unlabelled/non-label lanes.
    pub label_keys: Vec<String>,
    pub hosts: Vec<String>,
    pub cards: Vec<LaneCard>,
    pub workspaces: Vec<LaneWorkspace>,
    pub collapsed: bool,
}

impl Lane {
    pub fn is_combination(&self) -> bool {
        self.kind == LaneKind::Combination
    }
    pub fn bucket_counts(&self) -> [(Bucket, usize); 5] {
        Bucket::GROUP_ORDER.map(|bucket| {
            (
                bucket,
                self.cards
                    .iter()
                    .filter(|card| card.bucket == bucket)
                    .count(),
            )
        })
    }
}

/// Agents with no workspace label land here, and the column sorts last
/// unless the user has placed it.
pub const UNLABELLED: &str = "\u{0}unlabelled";

pub struct LaneView;

impl LaneView {
    pub fn build(fleet: &Fleet, grouping: LaneGrouping, state: &LaneState) -> Vec<Lane> {
        let mut cols: BTreeMap<String, Acc> = BTreeMap::new();
        match grouping {
            LaneGrouping::Label => collect_labels(fleet, &mut cols, state.label_mode),
            LaneGrouping::Project => collect_projects(fleet, &mut cols),
            LaneGrouping::Host => collect_hosts(fleet, &mut cols),
            LaneGrouping::Workspace => collect_workspaces(fleet, &mut cols),
        }
        let mut lanes: Vec<Lane> = cols
            .into_iter()
            .map(|(key, acc)| acc.finish(key, state))
            .collect();
        sort_lanes(&mut lanes, state, grouping);
        lanes
    }
}

struct Acc {
    kind: LaneKind,
    title: String,
    color: Option<String>,
    label_keys: Vec<String>,
    hosts: Vec<String>,
    cards: Vec<LaneCard>,
    workspaces: Vec<LaneWorkspace>,
    seen_workspaces: BTreeSet<(String, String)>,
}

impl Acc {
    fn new(title: String, color: Option<String>, kind: LaneKind, label_keys: Vec<String>) -> Self {
        Self {
            kind,
            title,
            color,
            label_keys,
            hosts: Vec::new(),
            cards: Vec::new(),
            workspaces: Vec::new(),
            seen_workspaces: BTreeSet::new(),
        }
    }

    fn host(&mut self, name: &str) {
        if !self.hosts.iter().any(|host| host == name) {
            self.hosts.push(name.to_string());
        }
    }

    fn workspace(&mut self, host: &str, workspace: &Workspace) {
        self.host(host);
        if !self
            .seen_workspaces
            .insert((host.to_string(), workspace.id.clone()))
        {
            return;
        }
        let name = workspace_name(Some(workspace));
        self.workspaces.push(LaneWorkspace {
            host: host.to_string(),
            workspace_id: workspace.id.clone(),
            name,
        });
    }

    /// Contract: every collector adds each (host, agent.id) at most once
    /// per column. Labels deduplicate slots; other collectors iterate each
    /// host's unique live agents once.
    fn card(&mut self, host: &str, agent: &Agent, workspace_name: String) {
        self.host(host);
        self.cards.push(lane_card(host, agent, workspace_name));
    }

    fn finish(mut self, key: String, state: &LaneState) -> Lane {
        self.cards.sort_by(card_order);
        self.workspaces.sort_by(workspace_order);
        Lane {
            collapsed: state.collapsed.contains(&key),
            key,
            kind: self.kind,
            title: self.title,
            color: self.color,
            label_keys: self.label_keys,
            hosts: self.hosts,
            cards: self.cards,
            workspaces: self.workspaces,
        }
    }
}

fn lane_card(host: &str, agent: &Agent, workspace_name: String) -> LaneCard {
    LaneCard {
        host: host.to_string(),
        agent_id: agent.id.clone(),
        workspace_id: agent.workspace_id.clone(),
        workspace_name,
        title: agent.display_title(),
        provider: agent.provider_id().to_string(),
        model: agent.model_label().map(str::to_string),
        updated_at: agent.updated_at.clone(),
        bucket: agent.bucket(),
        attention_reason: agent.attention_reason.clone(),
        parent: agent.parent_agent_id.clone(),
    }
}

fn workspace_order(a: &LaneWorkspace, b: &LaneWorkspace) -> std::cmp::Ordering {
    natural(&a.name, &b.name)
        .then(a.host.cmp(&b.host))
        .then(a.workspace_id.cmp(&b.workspace_id))
}

/// Most urgent first, then most recently updated. Total, so a lane's card
/// order never depends on the order cards were added.
fn card_order(a: &LaneCard, b: &LaneCard) -> std::cmp::Ordering {
    urgency(a.bucket)
        .cmp(&urgency(b.bucket))
        .then(b.updated_at.cmp(&a.updated_at))
        .then(a.host.cmp(&b.host))
        .then(a.agent_id.cmp(&b.agent_id))
}

fn column<'a>(
    cols: &'a mut BTreeMap<String, Acc>,
    key: &str,
    title: String,
    color: Option<String>,
) -> &'a mut Acc {
    cols.entry(key.to_string())
        .or_insert_with(|| Acc::new(title, color, LaneKind::Other, vec![]))
}

/// One label-derived column that a workspace belongs to.
struct Slot {
    kind: LaneKind,
    key: String,
    title: String,
    color: Option<String>,
    label_keys: Vec<String>,
}
impl Slot {
    fn unlabelled() -> Self {
        Self {
            kind: LaneKind::Unlabelled,
            key: UNLABELLED.into(),
            title: "Unlabelled".into(),
            color: None,
            label_keys: vec![],
        }
    }
    fn column<'a>(&self, cols: &'a mut BTreeMap<String, Acc>) -> &'a mut Acc {
        column_lazy(cols, &self.key, || {
            Acc::new(
                self.title.clone(),
                self.color.clone(),
                self.kind,
                self.label_keys.clone(),
            )
        })
    }
}

fn collect_labels(fleet: &Fleet, cols: &mut BTreeMap<String, Acc>, mode: LabelLaneMode) {
    let labels = LabelIndex::new(fleet);
    let unlabelled = vec![Slot::unlabelled()];
    for host in &fleet.hosts {
        // Resolve each workspace only once, then share its slots with agents.
        let mut slots: HashMap<&str, Vec<Slot>> = HashMap::new();
        for workspace in host.directory.workspaces.values() {
            let resolved = label_slots(workspace, &labels, mode);
            for slot in &resolved {
                slot.column(cols).workspace(host.name(), workspace);
            }
            slots.insert(workspace.id.as_str(), resolved);
        }
        for agent in live_agents(host) {
            let workspace = agent
                .workspace_id
                .as_ref()
                .and_then(|id| host.directory.workspaces.get(id));
            let agent_slots = workspace
                .and_then(|workspace| slots.get(workspace.id.as_str()))
                .unwrap_or(&unlabelled);
            let workspace_name = workspace_name(workspace);
            for slot in agent_slots {
                slot.column(cols)
                    .card(host.name(), agent, workspace_name.clone());
            }
        }
    }
}

/// Like [`column`], without allocating when the column already exists.
fn column_lazy<'a>(
    cols: &'a mut BTreeMap<String, Acc>,
    key: &str,
    init: impl FnOnce() -> Acc,
) -> &'a mut Acc {
    if !cols.contains_key(key) {
        cols.insert(key.to_string(), init());
    }
    cols.get_mut(key).expect("just inserted")
}

/// Every host's label definitions by key. The first host to define a label
/// wins, the same answer as asking each host's directory in order.
struct LabelIndex<'a>(HashMap<String, &'a LabelDef>);

impl<'a> LabelIndex<'a> {
    fn new(fleet: &'a Fleet) -> Self {
        let mut index = HashMap::new();
        for host in &fleet.hosts {
            for def in &host.directory.labels {
                index.entry(label_key(&def.name)).or_insert(def);
            }
        }
        Self(index)
    }

    fn title(&self, key: &str, raw: &str) -> String {
        self.0
            .get(key)
            .map_or_else(|| raw.to_string(), |def| def.name.clone())
    }

    fn color(&self, key: &str) -> Option<String> {
        self.0.get(key).map(|def| def.color.clone())
    }
}

fn collect_projects(fleet: &Fleet, cols: &mut BTreeMap<String, Acc>) {
    for host in &fleet.hosts {
        for workspace in host.directory.workspaces.values() {
            let key = project_key(host, workspace);
            let title = project_title(host, workspace, &key);
            column(cols, &key, title, None).workspace(host.name(), workspace);
        }
        for agent in live_agents(host) {
            let workspace = agent
                .workspace_id
                .as_ref()
                .and_then(|id| host.directory.workspaces.get(id));
            let (key, title) = match workspace {
                Some(workspace) => {
                    let key = project_key(host, workspace);
                    (key.clone(), project_title(host, workspace, &key))
                }
                None => ("\u{0}none".into(), "No project".into()),
            };
            let name = workspace_name(workspace);
            column(cols, &key, title, None).card(host.name(), agent, name);
        }
    }
}

fn collect_hosts(fleet: &Fleet, cols: &mut BTreeMap<String, Acc>) {
    for host in &fleet.hosts {
        let acc = column(cols, host.name(), host.name().to_string(), None);
        // The grouping host is known even before its directory has arrived.
        acc.host(host.name());
        for workspace in host.directory.workspaces.values() {
            acc.workspace(host.name(), workspace);
        }
        for agent in live_agents(host) {
            let workspace = agent
                .workspace_id
                .as_ref()
                .and_then(|id| host.directory.workspaces.get(id));
            acc.card(host.name(), agent, workspace_name(workspace));
        }
    }
}

fn collect_workspaces(fleet: &Fleet, cols: &mut BTreeMap<String, Acc>) {
    for host in &fleet.hosts {
        for workspace in host.directory.workspaces.values() {
            let key = format!("{}\u{0}{}", host.name(), workspace.id);
            let title = workspace_name(Some(workspace));
            column(cols, &key, title, None).workspace(host.name(), workspace);
        }
        for agent in live_agents(host) {
            let workspace = agent
                .workspace_id
                .as_ref()
                .and_then(|id| host.directory.workspaces.get(id));
            let name = workspace_name(workspace);
            match workspace {
                Some(workspace) => {
                    let key = format!("{}\u{0}{}", host.name(), workspace.id);
                    let title = workspace_name(Some(workspace));
                    column(cols, &key, title, None).card(host.name(), agent, name);
                }
                None => {
                    column(cols, "\u{0}noworkspace", "No workspace".into(), None).card(
                        host.name(),
                        agent,
                        name,
                    );
                }
            }
        }
    }
}

fn live_agents(host: &Host) -> impl Iterator<Item = &Agent> {
    host.directory
        .agents
        .values()
        .filter(|agent| agent.archived_at.is_none())
}

const COMBINATION_PREFIX: &str = "\u{0}label-set:";

/// Reserved namespace and JSON tuple encoding keep set identities independent
/// of display titles, separators inside names, and label order.
/// Classify a saved source key even after its lane disappears on relabeling;
/// consumers need not reconstruct or interpret the set's serialized members.
pub fn is_combination_key(key: &str) -> bool {
    key.starts_with(COMBINATION_PREFIX)
}

/// Normal label keys retain their existing saved state. Escape only labels
/// that enter the reserved control-character namespace used by synthetic lanes.
fn label_lane_key(name: &str) -> String {
    let key = label_key(name);
    if key.starts_with('\0') {
        format!(
            "\u{0}label:{}",
            serde_json::to_string(&key).expect("string serializes")
        )
    } else {
        key
    }
}

/// Canonical sorted label members, retaining the first raw spelling for display.
pub fn label_set(workspace: &Workspace) -> BTreeMap<String, &str> {
    let mut members = BTreeMap::new();
    for raw in &workspace.labels {
        let key = label_key(raw);
        if !key.is_empty() {
            members.entry(key).or_insert(raw.as_str());
        }
    }
    members
}

fn label_slots(workspace: &Workspace, labels: &LabelIndex<'_>, mode: LabelLaneMode) -> Vec<Slot> {
    let members = label_set(workspace);
    if members.is_empty() {
        return vec![Slot::unlabelled()];
    }
    let singles: Vec<Slot> = members
        .iter()
        .map(|(key, raw)| Slot {
            kind: LaneKind::Label,
            key: label_lane_key(key),
            title: labels.title(key, raw),
            color: labels.color(key),
            label_keys: vec![key.clone()],
        })
        .collect();
    // A singleton has one identity in every mode, so Both never duplicates it.
    match (mode, singles.len() > 1) {
        (LabelLaneMode::Labels, _) | (_, false) => singles,
        (LabelLaneMode::Combinations | LabelLaneMode::Both, true) => {
            let keys: Vec<String> = members.keys().cloned().collect();
            let encoded = serde_json::to_string(&keys).expect("strings serialize");
            let combo = Slot {
                kind: LaneKind::Combination,
                key: format!("{COMBINATION_PREFIX}{encoded}"),
                title: singles
                    .iter()
                    .map(|slot| slot.title.as_str())
                    .collect::<Vec<_>>()
                    .join(" + "),
                color: None,
                label_keys: keys,
            };
            match mode {
                LabelLaneMode::Both => {
                    let mut slots = singles;
                    slots.push(combo);
                    slots
                }
                _ => vec![combo],
            }
        }
    }
}

/// Same fallback as the sidebar: project key, else `host\0projectId`.
fn project_key(host: &Host, workspace: &Workspace) -> String {
    let project = host.directory.projects.get(&workspace.project_id);
    project
        .and_then(|project| project.project_key.clone())
        .filter(|key| !key.is_empty())
        .or_else(|| {
            workspace
                .project_key()
                .map(str::to_string)
                .filter(|key| !key.is_empty())
        })
        .unwrap_or_else(|| format!("{}\u{0}{}", host.name(), workspace.project_id))
}

fn project_title(host: &Host, workspace: &Workspace, key: &str) -> String {
    if let Some(project) = host.directory.projects.get(&workspace.project_id) {
        let name = project.name();
        if !name.is_empty() {
            return name;
        }
    }
    if !workspace.project_display_name.is_empty() {
        return workspace.project_display_name.clone();
    }
    if let Some(name) = workspace
        .project_custom_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        return name.to_string();
    }
    key.to_string()
}

/// What a workspace is called: the title the user gave it, else its name,
/// else its id.
fn workspace_name(workspace: Option<&Workspace>) -> String {
    match workspace {
        Some(workspace)
            if workspace
                .title
                .as_deref()
                .is_some_and(|title| !title.trim().is_empty()) =>
        {
            workspace.title.clone().unwrap_or_default()
        }
        Some(workspace) if !workspace.name.is_empty() => workspace.name.clone(),
        Some(workspace) => workspace.id.clone(),
        None => String::new(),
    }
}

fn urgency(bucket: Bucket) -> u8 {
    Bucket::GROUP_ORDER
        .iter()
        .position(|item| *item == bucket)
        .unwrap_or(4) as u8
}

/// Most urgent card, or past Done when the column has no cards.
fn column_urgency(lane: &Lane) -> u8 {
    lane.cards
        .iter()
        .map(|card| urgency(card.bucket))
        .min()
        .unwrap_or(u8::MAX)
}

fn sort_lanes(lanes: &mut [Lane], state: &LaneState, grouping: LaneGrouping) {
    lanes.sort_by(|a, b| lane_order(a, b, state, grouping));
}

/// The user's arrangement first, then most urgent, then by title. Total,
/// because ties fall back to the unique key.
fn lane_order(a: &Lane, b: &Lane, state: &LaneState, grouping: LaneGrouping) -> std::cmp::Ordering {
    let ao = state.order.iter().position(|key| key == &a.key);
    let bo = state.order.iter().position(|key| key == &b.key);
    match (ao, bo) {
        (Some(i), Some(j)) => i.cmp(&j),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => {
            let a_last = grouping == LaneGrouping::Label && a.key == UNLABELLED;
            let b_last = grouping == LaneGrouping::Label && b.key == UNLABELLED;
            match (a_last, b_last) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => column_urgency(a)
                    .cmp(&column_urgency(b))
                    .then(natural(&a.title, &b.title))
                    .then(a.key.cmp(&b.key)),
            }
        }
    }
}

/// Lanes kept current from directory changes rather than rebuilt from the
/// whole fleet. A change moves the cards and workspace entries it touched,
/// creates and drops lanes, and relists their hosts, just as a full build
/// would. Only what a patch cannot know falls back to a full
/// [`LaneView::build`]: snapshots and definition changes, and a lane whose
/// title came from whichever workspace a full build met first. Either way
/// `lanes` equals what a full build returns.
///
/// Lanes are reference counted: a patch replaces only the lanes it touched,
/// so a consumer holding an earlier snapshot can tell what changed by
/// pointer.
pub struct LaneModel {
    grouping: LaneGrouping,
    state: LaneState,
    lanes: Vec<Rc<Lane>>,
    /// Lane key to its position in `lanes`.
    index: HashMap<String, usize>,
    /// (host, agent id) to the sorted keys of the lanes its card is in.
    homes: HashMap<(String, String), Vec<String>>,
    rebuilds: u64,
}

impl LaneModel {
    pub fn build(fleet: &Fleet, grouping: LaneGrouping, state: &LaneState) -> Self {
        let lanes = LaneView::build(fleet, grouping, state)
            .into_iter()
            .map(Rc::new)
            .collect();
        let mut model = Self {
            grouping,
            state: state.clone(),
            lanes,
            index: HashMap::new(),
            homes: HashMap::new(),
            rebuilds: 0,
        };
        model.reindex();
        for lane in &model.lanes {
            for card in &lane.cards {
                model
                    .homes
                    .entry((card.host.clone(), card.agent_id.clone()))
                    .or_default()
                    .push(lane.key.clone());
            }
        }
        for keys in model.homes.values_mut() {
            keys.sort();
        }
        model
    }

    pub fn lanes(&self) -> &[Rc<Lane>] {
        &self.lanes
    }

    pub fn grouping(&self) -> LaneGrouping {
        self.grouping
    }

    pub fn state(&self) -> &LaneState {
        &self.state
    }

    /// How many times `apply` fell back to a full build.
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }

    /// Brings the lanes up to date with `changes`, each tagged with the host
    /// it happened on.
    pub fn apply<'a>(
        &mut self,
        fleet: &Fleet,
        changes: impl IntoIterator<Item = (&'a str, &'a DirChange)>,
    ) {
        let mut agents: Vec<(&str, String)> = Vec::new();
        let mut workspaces: Vec<(&str, &str)> = Vec::new();
        for (host, change) in changes {
            match change {
                DirChange::AgentAdded(id)
                | DirChange::AgentUpdated(id)
                | DirChange::AgentMoved(id)
                | DirChange::AgentRemoved(id) => agents.push((host, id.clone())),
                DirChange::Workspace(id) => workspaces.push((host, id)),
                DirChange::WorkspaceDetail(_) => {}
                DirChange::Bulk => return self.rebuild(fleet),
            }
        }
        workspaces.sort_unstable();
        workspaces.dedup();
        let labels = LabelIndex::new(fleet);
        let mut touched = BTreeSet::new();
        // A workspace's agents follow it, so their cards are patched below
        // along with the agents that changed themselves.
        for &(host, id) in &workspaces {
            let Some(moved) = self.patch_workspace(fleet, &labels, host, id, &mut touched) else {
                return self.rebuild(fleet);
            };
            agents.extend(moved.into_iter().map(|agent| (host, agent)));
        }
        agents.sort_unstable();
        agents.dedup();
        for (host, id) in agents {
            self.patch(fleet, &labels, host, &id, &mut touched);
        }
        if touched.is_empty() {
            return;
        }
        self.settle(fleet, &touched);
        let (state, grouping) = (&self.state, self.grouping);
        self.lanes.sort_by(|a, b| lane_order(a, b, state, grouping));
        self.reindex();
    }

    fn rebuild(&mut self, fleet: &Fleet) {
        let rebuilds = self.rebuilds + 1;
        *self = Self::build(fleet, self.grouping, &self.state);
        self.rebuilds = rebuilds;
    }

    fn reindex(&mut self) {
        self.index = self
            .lanes
            .iter()
            .enumerate()
            .map(|(ix, lane)| (lane.key.clone(), ix))
            .collect();
    }

    /// Moves one agent's cards to the lanes it now belongs in.
    fn patch(
        &mut self,
        fleet: &Fleet,
        labels: &LabelIndex<'_>,
        host_name: &str,
        id: &str,
        touched: &mut BTreeSet<String>,
    ) {
        let home = (host_name.to_string(), id.to_string());
        let old = self.homes.remove(&home).unwrap_or_default();
        let host = fleet.host(host_name);
        let agent = host
            .and_then(|host| host.directory.agents.get(id))
            .filter(|agent| agent.archived_at.is_none());
        let (slots, card) = match (host, agent) {
            (Some(host), Some(agent)) => {
                let workspace = agent
                    .workspace_id
                    .as_ref()
                    .and_then(|id| host.directory.workspaces.get(id));
                (
                    self.slots(host, workspace, labels),
                    Some(lane_card(host.name(), agent, workspace_name(workspace))),
                )
            }
            _ => (Vec::new(), None),
        };
        for key in &old {
            let Some(&ix) = self.index.get(key) else {
                continue;
            };
            let lane = Rc::make_mut(&mut self.lanes[ix]);
            if let Some(pos) = lane
                .cards
                .iter()
                .position(|card| card.host == host_name && card.agent_id == id)
            {
                lane.cards.remove(pos);
            }
            touched.insert(key.clone());
        }
        let Some(card) = card else {
            return;
        };
        for slot in &slots {
            let lane = self.lane_mut(slot);
            let pos = lane
                .cards
                .binary_search_by(|other| card_order(other, &card))
                .unwrap_or_else(|pos| pos);
            lane.cards.insert(pos, card.clone());
            touched.insert(slot.key.clone());
        }
        if !slots.is_empty() {
            self.homes
                .insert(home, slots.into_iter().map(|slot| slot.key).collect());
        }
    }

    /// Moves one workspace's entry to the lanes it now belongs in and
    /// returns the ids of its agents, whose cards must follow. None when the
    /// model must be rebuilt instead: a lane's title could have come from
    /// this workspace, and a patch cannot tell which workspace a full build
    /// would title it from.
    fn patch_workspace(
        &mut self,
        fleet: &Fleet,
        labels: &LabelIndex<'_>,
        host_name: &str,
        id: &str,
        touched: &mut BTreeSet<String>,
    ) -> Option<Vec<String>> {
        let host = fleet.host(host_name)?;
        let workspace = host.directory.workspaces.get(id);
        let slots = match workspace {
            Some(workspace) => self.slots(host, Some(workspace), labels),
            None => Vec::new(),
        };
        for slot in &slots {
            if let Some(&ix) = self.index.get(&slot.key) {
                let lane = &self.lanes[ix];
                if lane.title != slot.title || lane.color != slot.color {
                    return None;
                }
            }
        }
        let held: Vec<usize> = (0..self.lanes.len())
            .filter(|&ix| {
                self.lanes[ix]
                    .workspaces
                    .iter()
                    .any(|w| w.host == host_name && w.workspace_id == id)
            })
            .collect();
        for &ix in &held {
            let stays = slots.iter().any(|slot| slot.key == self.lanes[ix].key);
            if !stays && !self.title_is_fixed(&self.lanes[ix], labels) {
                return None;
            }
        }
        let entry = workspace.map(|workspace| LaneWorkspace {
            host: host_name.to_string(),
            workspace_id: workspace.id.clone(),
            name: workspace_name(Some(workspace)),
        });
        for ix in held {
            let lane = &self.lanes[ix];
            let unchanged = entry.as_ref().is_some_and(|entry| {
                slots.iter().any(|slot| slot.key == lane.key) && lane.workspaces.contains(entry)
            });
            if unchanged {
                continue;
            }
            let key = lane.key.clone();
            let lane = Rc::make_mut(&mut self.lanes[ix]);
            lane.workspaces
                .retain(|w| !(w.host == host_name && w.workspace_id == id));
            touched.insert(key);
        }
        if let Some(entry) = &entry {
            for slot in &slots {
                let lane = self.lane_mut(slot);
                if lane.workspaces.contains(entry) {
                    continue;
                }
                let at = lane
                    .workspaces
                    .binary_search_by(|other| workspace_order(other, entry))
                    .unwrap_or_else(|at| at);
                lane.workspaces.insert(at, entry.clone());
                touched.insert(slot.key.clone());
            }
        }
        let agents = host
            .directory
            .agents
            .values()
            .filter(|agent| agent.workspace_id.as_deref() == Some(id))
            .map(|agent| agent.id.clone())
            .collect();
        Some(agents)
    }

    /// The lane for `slot`, created empty if this is its first member.
    fn lane_mut(&mut self, slot: &Slot) -> &mut Lane {
        let ix = match self.index.get(&slot.key) {
            Some(&ix) => ix,
            None => {
                self.lanes.push(Rc::new(Lane {
                    key: slot.key.clone(),
                    kind: slot.kind,
                    title: slot.title.clone(),
                    color: slot.color.clone(),
                    label_keys: slot.label_keys.clone(),
                    hosts: Vec::new(),
                    cards: Vec::new(),
                    workspaces: Vec::new(),
                    collapsed: self.state.collapsed.contains(&slot.key),
                }));
                let ix = self.lanes.len() - 1;
                self.index.insert(slot.key.clone(), ix);
                ix
            }
        };
        Rc::make_mut(&mut self.lanes[ix])
    }

    /// Drops touched lanes left with nothing in them, and relists the hosts
    /// of the rest in fleet order, as a full build would.
    fn settle(&mut self, fleet: &Fleet, touched: &BTreeSet<String>) {
        // A host's lane lists that host, and stays even when empty.
        if self.grouping == LaneGrouping::Host {
            return;
        }
        let mut emptied = false;
        for key in touched {
            let ix = self.index[key];
            let lane = &self.lanes[ix];
            if lane.cards.is_empty() && lane.workspaces.is_empty() {
                emptied = true;
                continue;
            }
            let present: HashSet<&str> = lane
                .cards
                .iter()
                .map(|card| card.host.as_str())
                .chain(lane.workspaces.iter().map(|w| w.host.as_str()))
                .collect();
            let hosts: Vec<String> = fleet
                .hosts
                .iter()
                .map(Host::name)
                .filter(|name| present.contains(name))
                .map(str::to_string)
                .collect();
            if hosts != lane.hosts {
                Rc::make_mut(&mut self.lanes[ix]).hosts = hosts;
            }
        }
        if emptied {
            self.lanes
                .retain(|lane| !lane.cards.is_empty() || !lane.workspaces.is_empty());
        }
    }

    /// The lanes an agent or workspace of `host` in `workspace` belongs in,
    /// keyed and titled by the same rules the collectors use, sorted by key.
    fn slots(
        &self,
        host: &Host,
        workspace: Option<&Workspace>,
        labels: &LabelIndex<'_>,
    ) -> Vec<Slot> {
        let other = |key: String, title: String| Slot {
            kind: LaneKind::Other,
            key,
            title,
            color: None,
            label_keys: Vec::new(),
        };
        let mut slots = match (self.grouping, workspace) {
            (LaneGrouping::Label, Some(workspace)) => {
                label_slots(workspace, labels, self.state.label_mode)
            }
            (LaneGrouping::Label, None) => vec![Slot::unlabelled()],
            (LaneGrouping::Project, Some(workspace)) => {
                let key = project_key(host, workspace);
                let title = project_title(host, workspace, &key);
                vec![other(key, title)]
            }
            (LaneGrouping::Project, None) => vec![other("\u{0}none".into(), "No project".into())],
            (LaneGrouping::Host, _) => vec![other(host.name().into(), host.name().into())],
            (LaneGrouping::Workspace, Some(workspace)) => vec![other(
                format!("{}\u{0}{}", host.name(), workspace.id),
                workspace_name(Some(workspace)),
            )],
            (LaneGrouping::Workspace, None) => {
                vec![other("\u{0}noworkspace".into(), "No workspace".into())]
            }
        };
        slots.sort_by(|a, b| a.key.cmp(&b.key));
        slots.dedup_by(|a, b| a.key == b.key);
        slots
    }

    /// True when a lane's title cannot depend on which of its workspaces a
    /// full build met first, so one leaving cannot change it.
    fn title_is_fixed(&self, lane: &Lane, labels: &LabelIndex<'_>) -> bool {
        match self.grouping {
            LaneGrouping::Label => lane.label_keys.iter().all(|key| labels.0.contains_key(key)),
            LaneGrouping::Host => true,
            LaneGrouping::Project | LaneGrouping::Workspace => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::{Directory, LabelDef};
    use crate::{Host, HostConfig};
    use paseo_client::proto::Project;
    use serde_json::json;

    fn host(name: &str, directory: Directory) -> Host {
        Host {
            config: HostConfig {
                name: name.into(),
                ..Default::default()
            },
            directory,
            ..Default::default()
        }
    }

    fn workspace(id: &str, project: &str, name: &str, labels: &[&str]) -> Workspace {
        serde_json::from_value(json!({
            "id": id,
            "projectId": project,
            "projectDisplayName": name,
            "projectRootPath": format!("/p/{project}"),
            "name": name,
            "labels": labels,
            "status": "done",
        }))
        .unwrap()
    }

    fn agent(
        id: &str,
        workspace: &str,
        status: &str,
        updated: &str,
        attention: Option<&str>,
    ) -> Agent {
        let mut value = json!({
            "id": id,
            "workspaceId": workspace,
            "status": status,
            "updatedAt": updated,
            "provider": "claude",
            "model": "haiku",
            "title": id,
        });
        if let Some(reason) = attention {
            value["requiresAttention"] = json!(true);
            value["attentionReason"] = json!(reason);
        }
        if status == "running" {
            value["activeTurn"] = json!({"phase": "open"});
        }
        Agent::from_value(value).unwrap()
    }

    fn put_workspace(dir: &mut Directory, workspace: Workspace) {
        dir.workspaces.insert(workspace.id.clone(), workspace);
    }

    fn put_agent(dir: &mut Directory, agent: Agent) {
        dir.agents.insert(agent.id.clone(), agent);
    }

    fn label_mode_fixture() -> Fleet {
        let mut left = Directory {
            labels: vec![
                LabelDef {
                    name: "A".into(),
                    color: "red".into(),
                },
                LabelDef {
                    name: "B".into(),
                    color: "teal".into(),
                },
            ],
            ..Default::default()
        };
        for (id, labels) in [
            ("ab", vec![" B ", "a", "A", ""]),
            ("abc", vec!["c", "a", "b"]),
            ("single", vec!["a"]),
            ("unlabelled", vec![" "]),
            ("empty", vec!["c", "b", "a"]),
        ] {
            put_workspace(&mut left, workspace(id, "p", id, &labels));
            if id != "empty" {
                put_agent(
                    &mut left,
                    agent(id, id, "idle", "2026-10-02T00:00:00Z", None),
                );
            }
        }
        put_agent(
            &mut left,
            agent("orphan", "missing", "idle", "2026-10-02T00:00:00Z", None),
        );
        let mut archived = agent("archived", "ab", "idle", "2026-10-02T00:00:00Z", None);
        archived.archived_at = Some("2026-10-03T00:00:00Z".into());
        put_agent(&mut left, archived);
        let mut right = Directory::default();
        put_workspace(&mut right, workspace("ba", "p", "ba", &["a", "B"]));
        put_agent(
            &mut right,
            agent("ab", "ba", "idle", "2026-10-02T00:00:00Z", None),
        );
        Fleet {
            hosts: vec![host("left", left), host("right", right)],
        }
    }

    #[test]
    fn three_label_modes_partition_exact_sets_and_both_keeps_each_lane_unique() {
        let fleet = label_mode_fixture();
        let build = |label_mode| {
            LaneView::build(
                &fleet,
                LaneGrouping::Label,
                &LaneState {
                    label_mode,
                    ..Default::default()
                },
            )
        };
        let labels = build(LabelLaneMode::Labels);
        assert_eq!(labels.len(), 4);
        assert!(labels.iter().all(|lane| !lane.is_combination()));
        let a = labels.iter().find(|lane| lane.key == "a").unwrap();
        assert_eq!(a.cards.len(), 4); // ab on two hosts, abc, and single.
        assert_eq!(a.color.as_deref(), Some("red"));
        let exact = build(LabelLaneMode::Combinations);
        assert_eq!(exact.len(), 4); // a, a+b, a+b+c, unlabelled; no b+c subset.
        let ab = exact
            .iter()
            .find(|lane| lane.label_keys == ["a", "b"])
            .unwrap();
        assert_eq!(ab.title, "A + B");
        assert_eq!(ab.cards.len(), 2);
        assert_eq!(ab.hosts, ["left", "right"]);
        assert!(ab.cards.iter().all(|card| card.agent_id == "ab"));
        let abc = exact
            .iter()
            .find(|lane| lane.label_keys == ["a", "b", "c"])
            .unwrap();
        assert_eq!(abc.cards.len(), 1);
        assert_eq!(abc.workspaces.len(), 2); // Includes a workspace with no agents, usable for a draft.
        let single = exact.iter().find(|lane| lane.key == "a").unwrap();
        assert_eq!(single.cards.len(), 1);
        assert_eq!(single.cards[0].agent_id, "single");
        let both = build(LabelLaneMode::Both);
        assert_eq!(both.len(), 6);
        assert_eq!(
            both.iter()
                .map(|lane| &lane.key)
                .collect::<BTreeSet<_>>()
                .len(),
            both.len()
        );
        assert_eq!(both.iter().filter(|lane| lane.key == "a").count(), 1);
        assert_eq!(both.iter().filter(|lane| lane.key == UNLABELLED).count(), 1);
        let copies = |cards: &[Lane], host: &str, agent: &str| {
            cards
                .iter()
                .flat_map(|lane| &lane.cards)
                .filter(|card| card.host == host && card.agent_id == agent)
                .count()
        };
        assert_eq!(copies(&labels, "left", "ab"), 2);
        assert_eq!(copies(&exact, "left", "ab"), 1);
        assert_eq!(copies(&both, "left", "ab"), 3);
        assert_eq!(copies(&both, "left", "abc"), 4);
        assert_eq!(copies(&both, "left", "single"), 1);
        assert_eq!(copies(&both, "left", "unlabelled"), 1);
        assert_eq!(copies(&both, "left", "orphan"), 1);
        assert_eq!(copies(&both, "left", "archived"), 0);
        assert_eq!(
            both.iter()
                .find(|lane| lane.key == ab.key)
                .unwrap()
                .label_keys,
            ab.label_keys
        );
    }

    #[test]
    fn combination_identity_is_canonical_collision_free_and_preserves_state() {
        let fleet = label_mode_fixture();
        let labels = LabelIndex::new(&fleet);
        let resolve = |names: &[&str]| {
            label_slots(
                &workspace("w", "p", "w", names),
                &labels,
                LabelLaneMode::Combinations,
            )
            .remove(0)
        };
        let ab = resolve(&["A", "b"]);
        let reordered = resolve(&[" b ", "a", "A", ""]);
        assert_eq!(ab.key, reordered.key);
        assert_ne!(resolve(&["a + b", "c"]).key, resolve(&["a", "b + c"]).key);
        assert_ne!(resolve(&["a,b", "c"]).key, resolve(&["a", "b,c"]).key);
        assert_ne!(resolve(&[&ab.key]).key, ab.key); // Raw names cannot enter synthetic namespace.
        assert_ne!(resolve(&[UNLABELLED]).key, UNLABELLED);
        assert!(!is_combination_key(&resolve(&[&ab.key]).key));
        let state = LaneState {
            label_mode: LabelLaneMode::Both,
            order: vec![ab.key.clone()],
            collapsed: BTreeSet::from([ab.key.clone()]),
        };
        let both = LaneView::build(&fleet, LaneGrouping::Label, &state);
        assert_eq!(both[0].key, ab.key);
        assert!(both[0].collapsed);
        let exact = LaneView::build(
            &fleet,
            LaneGrouping::Label,
            &LaneState {
                label_mode: LabelLaneMode::Combinations,
                ..state
            },
        );
        assert_eq!(exact[0].key, ab.key);
        assert!(exact[0].collapsed);
    }

    #[test]
    fn label_mode_does_not_change_project_host_or_workspace_grouping() {
        let fleet = label_mode_fixture();
        for grouping in [
            LaneGrouping::Host,
            LaneGrouping::Project,
            LaneGrouping::Workspace,
        ] {
            let original = LaneView::build(&fleet, grouping, &LaneState::default());
            for label_mode in [LabelLaneMode::Combinations, LabelLaneMode::Both] {
                assert_eq!(
                    LaneView::build(
                        &fleet,
                        grouping,
                        &LaneState {
                            label_mode,
                            ..Default::default()
                        }
                    ),
                    original
                );
            }
        }
    }

    #[test]
    fn empty_host_lanes_keep_their_known_host_without_directory_data() {
        // Hosts appear before they connect and can remain empty or offline.
        let fleet = Fleet {
            hosts: vec![host("mac-mini", Directory::default())],
        };
        let lanes = LaneView::build(&fleet, LaneGrouping::Host, &LaneState::default());
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].hosts, ["mac-mini"]);
        assert!(lanes[0].cards.is_empty());
        assert!(lanes[0].workspaces.is_empty());
    }

    #[test]
    fn label_grouping_splits_multi_label_and_keeps_unlabelled_last() {
        let mut dir = Directory::default();
        put_workspace(
            &mut dir,
            workspace(
                "w-both",
                "p",
                "both",
                &["Atlas", "atlas", " ATLAS ", "devtools"],
            ),
        );
        put_workspace(&mut dir, workspace("w-atl", "p", "atl", &["atlas"]));
        put_workspace(&mut dir, workspace("w-none", "p", "loose", &[]));
        dir.labels = vec![
            LabelDef {
                name: "Atlas".into(),
                color: "violet".into(),
            },
            LabelDef {
                name: "devtools".into(),
                color: "teal".into(),
            },
        ];
        put_agent(
            &mut dir,
            agent("both", "w-both", "idle", "2026-10-02T00:00:00Z", None),
        );
        put_agent(
            &mut dir,
            agent(
                "needs",
                "w-atl",
                "idle",
                "2026-10-02T01:00:00Z",
                Some("permission"),
            ),
        );
        put_agent(
            &mut dir,
            agent(
                "loose",
                "w-none",
                "idle",
                "2026-10-02T02:00:00Z",
                Some("error"),
            ),
        );
        let fleet = Fleet {
            hosts: vec![host("alpha", dir)],
        };
        let lanes = LaneView::build(&fleet, LaneGrouping::Label, &LaneState::default());
        let titles: Vec<_> = lanes.iter().map(|lane| lane.title.as_str()).collect();
        // "needs" makes Atlas more urgent than devtools. Unlabelled is last
        // even though its card failed.
        assert_eq!(titles, ["Atlas", "devtools", "Unlabelled"]);
        assert_eq!(lanes[0].color.as_deref(), Some("violet"));
        assert!(!lanes[0].collapsed);
        assert!(!lanes[2].collapsed);

        let both_in = |title: &str| {
            lanes
                .iter()
                .find(|lane| lane.title == title)
                .unwrap()
                .cards
                .iter()
                .any(|card| card.agent_id == "both")
        };
        assert!(both_in("Atlas") && both_in("devtools"));
        // Repeated/case/whitespace aliases create one card per agent per lane.
        assert_eq!(lanes[0].cards.len(), 2);
        assert_eq!(lanes[1].cards.len(), 1);
        assert!(!both_in("Unlabelled"));
        let unlabelled = lanes
            .iter()
            .find(|lane| lane.title == "Unlabelled")
            .unwrap();
        assert_eq!(unlabelled.cards.len(), 1);
        assert_eq!(unlabelled.cards[0].agent_id, "loose");
        // Needs input sorts ahead of the idle card in the Atlas column.
        assert_eq!(lanes[0].cards[0].agent_id, "needs");
        assert_eq!(lanes[0].cards[1].agent_id, "both");
    }

    #[test]
    fn project_grouping_merges_hosts_by_project_key() {
        let mut left = Directory::default();
        left.projects.insert(
            "p1".into(),
            Project {
                project_id: "p1".into(),
                project_key: Some("shared".into()),
                project_display_name: "Shared".into(),
                ..Default::default()
            },
        );
        put_workspace(&mut left, workspace("w1", "p1", "left-ws", &[]));
        put_agent(
            &mut left,
            agent("a-left", "w1", "idle", "2026-10-02T00:00:00Z", None),
        );

        let mut right = Directory::default();
        right.projects.insert(
            "p9".into(),
            Project {
                project_id: "p9".into(),
                project_key: Some("shared".into()),
                project_display_name: "Shared".into(),
                ..Default::default()
            },
        );
        right.projects.insert(
            "p2".into(),
            Project {
                project_id: "p2".into(),
                project_key: Some("other".into()),
                project_display_name: "Other".into(),
                ..Default::default()
            },
        );
        put_workspace(&mut right, workspace("w9", "p9", "right-ws", &[]));
        put_workspace(&mut right, workspace("w2", "p2", "other-ws", &[]));
        put_agent(
            &mut right,
            agent("a-right", "w9", "running", "2026-10-02T03:00:00Z", None),
        );
        put_agent(
            &mut right,
            agent("a-other", "w2", "idle", "2026-10-02T03:00:00Z", None),
        );

        let fleet = Fleet {
            hosts: vec![host("left", left), host("right", right)],
        };
        let lanes = LaneView::build(&fleet, LaneGrouping::Project, &LaneState::default());
        let shared = lanes.iter().find(|lane| lane.title == "Shared").unwrap();
        assert_eq!(shared.hosts, ["left", "right"]);
        let ids: Vec<_> = shared
            .cards
            .iter()
            .map(|card| card.agent_id.as_str())
            .collect();
        assert_eq!(ids, ["a-right", "a-left"]);
        assert_eq!(shared.workspaces.len(), 2);
        assert!(lanes.iter().any(|lane| lane.title == "Other"));
        assert_eq!(lanes.len(), 2);
    }

    #[test]
    fn ordering_is_stable_user_then_urgency_then_name() {
        let mut dir = Directory::default();
        put_workspace(&mut dir, workspace("w-a", "pa", "alpha", &["alpha"]));
        put_workspace(&mut dir, workspace("w-b", "pb", "beta", &["beta"]));
        put_workspace(&mut dir, workspace("w-u", "pu", "urgent", &["urgent"]));
        dir.labels = vec![
            LabelDef {
                name: "alpha".into(),
                color: "blue".into(),
            },
            LabelDef {
                name: "beta".into(),
                color: "red".into(),
            },
            LabelDef {
                name: "urgent".into(),
                color: "amber".into(),
            },
        ];
        put_agent(
            &mut dir,
            agent("a", "w-a", "idle", "2026-10-02T00:00:00Z", None),
        );
        put_agent(
            &mut dir,
            agent("b", "w-b", "idle", "2026-10-02T00:00:00Z", None),
        );
        put_agent(
            &mut dir,
            agent("u", "w-u", "error", "2026-10-02T00:00:00Z", Some("error")),
        );
        let fleet = Fleet {
            hosts: vec![host("h", dir)],
        };
        let once = LaneView::build(&fleet, LaneGrouping::Label, &LaneState::default());
        let twice = LaneView::build(&fleet, LaneGrouping::Label, &LaneState::default());
        let names = |lanes: &[Lane]| {
            lanes
                .iter()
                .map(|lane| lane.title.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&once), names(&twice));
        // Failed outranks done; the two done columns fall back to name.
        assert_eq!(names(&once), ["urgent", "alpha", "beta"]);

        let state = LaneState {
            order: vec![label_key("beta")],
            ..Default::default()
        };
        let ordered = LaneView::build(&fleet, LaneGrouping::Label, &state);
        assert_eq!(names(&ordered), ["beta", "urgent", "alpha"]);
    }

    #[test]
    fn collapsed_and_reordered_state_round_trips() {
        let mut dir = Directory::default();
        put_workspace(&mut dir, workspace("w-a", "pa", "alpha", &["alpha"]));
        put_workspace(&mut dir, workspace("w-b", "pb", "beta", &["beta"]));
        put_agent(
            &mut dir,
            agent("a", "w-a", "idle", "2026-10-01T00:00:00Z", None),
        );
        put_agent(
            &mut dir,
            agent("b", "w-b", "idle", "2026-10-01T00:00:00Z", None),
        );
        let fleet = Fleet {
            hosts: vec![host("h", dir)],
        };
        let beta = label_key("beta");
        let alpha = label_key("alpha");
        let state = LaneState {
            order: vec![beta.clone(), alpha.clone()],
            collapsed: BTreeSet::from([beta.clone()]),
            ..Default::default()
        };
        let lanes = LaneView::build(&fleet, LaneGrouping::Label, &state);
        assert_eq!(lanes[0].key, beta);
        assert!(lanes[0].collapsed);
        assert_eq!(lanes[1].key, alpha);
        assert!(!lanes[1].collapsed);

        let again = LaneView::build(&fleet, LaneGrouping::Label, &state);
        assert_eq!(
            lanes.iter().map(|lane| &lane.key).collect::<Vec<_>>(),
            again.iter().map(|lane| &lane.key).collect::<Vec<_>>()
        );
    }
}
