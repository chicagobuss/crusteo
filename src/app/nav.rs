//! Where the user is, shared by every layout: the selected workspace, the
//! tabs open in each workspace, and sidebar preferences. Client-local only,
//! like the Paseo app's layout store.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use fleet::Fleet;
use fleet::lanes::{LabelLaneMode, LaneGrouping};
use fleet::sidebar::{Filters, Grouping};
use gpui_kit::gpui::{Context, EventEmitter};

use crate::app::config::{self, UiState};
use crate::app::notifications;

pub const LANE_WIDTH_DEFAULT: f32 = 360.;
pub const LANE_WIDTH_MIN: f32 = 200.;
pub const LANE_WIDTH_MAX: f32 = 600.;
/// Seconds a card opened from a notification blinks, until changed.
pub const NOTIFICATION_FLASH_DEFAULT: u8 = 4;
/// The choices Settings offers for it, 0 being off.
pub const NOTIFICATION_FLASH_CHOICES: [u8; 4] = [0, 2, 4, 8];

/// The widest the window's border can be set.
pub const WINDOW_BORDER_MAX: u8 = 8;
/// The window border's color until one is chosen: a bright blue that
/// stands out against the dark theme and against most apps beside it.
pub const WINDOW_BORDER_COLOR: &str = "#3b9eff";

/// `#rrggbb` (the `#` optional) as `0xrrggbb`.
pub fn parse_hex(hex: &str) -> Option<u32> {
    let digits = hex.trim().trim_start_matches('#');
    (digits.len() == 6)
        .then(|| u32::from_str_radix(digits, 16).ok())
        .flatten()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tab {
    Agent(String),
    /// A not-yet-created agent; the id is local.
    Draft(String),
    /// A daemon PTY, keyed by terminal id.
    Terminal(String),
}

impl Tab {
    pub fn key(&self) -> String {
        match self {
            Tab::Agent(id) => format!("agent:{id}"),
            Tab::Draft(id) => format!("draft:{id}"),
            Tab::Terminal(id) => format!("terminal:{id}"),
        }
    }

    pub fn parse(key: &str) -> Option<Self> {
        let (kind, id) = key.rsplit_once(':')?;
        match kind {
            "agent" => Some(Tab::Agent(id.to_string())),
            "draft" => Some(Tab::Draft(id.to_string())),
            "terminal" => Some(Tab::Terminal(id.to_string())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Workspace,
    NewWorkspace {
        host: Option<String>,
        project: Option<String>,
    },
    Hosts,
    Settings,
    History,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub host: String,
    pub workspace: String,
}

impl Selection {
    pub fn key(&self) -> String {
        format!("{}:{}", self.host, self.workspace)
    }
}

/// Moves past the neighbouring displayed column while preserving every
/// hidden lane in the saved order. The other keys retain their relative order.
fn reorder_visible_lanes(
    keys: &[String],
    key: &str,
    delta: isize,
    collapsed: &BTreeSet<String>,
) -> Option<Vec<String>> {
    let visible: Vec<usize> = keys
        .iter()
        .enumerate()
        .filter(|(_, key)| !collapsed.contains(*key))
        .map(|(index, _)| index)
        .collect();
    let source = visible.iter().position(|&index| keys[index] == key)?;
    let target = source.checked_add_signed(delta)?;
    let from = visible[source];
    let to = *visible.get(target)?;
    if from == to {
        return None;
    }
    let mut order = keys.to_vec();
    let lane = order.remove(from);
    order.insert(to, lane);
    Some(order)
}

/// `keys` with `key` moved to where `target` is: after it when it was to
/// the left, before it when it was to the right. `None` when nothing moves.
pub(crate) fn move_lane_to(keys: &[String], key: &str, target: &str) -> Option<Vec<String>> {
    let from = keys.iter().position(|item| item == key)?;
    let to = keys.iter().position(|item| item == target)?;
    if from == to {
        return None;
    }
    let mut order = keys.to_vec();
    let lane = order.remove(from);
    order.insert(to, lane);
    Some(order)
}

/// `keys`, every lane in order, with `key` placed where `shown` has it:
/// `shown` is the order on screen, which leaves out collapsed lanes. It
/// goes before the lane shown after it, or after the one shown before it;
/// collapsed lanes keep their places. `None` when nothing moves.
fn place_as_shown(keys: &[String], shown: &[String], key: &str) -> Option<Vec<String>> {
    let at = shown.iter().position(|item| item == key)?;
    let mut order = keys.to_vec();
    let from = order.iter().position(|item| item == key)?;
    let lane = order.remove(from);
    let to = if let Some(next) = shown.get(at + 1) {
        order.iter().position(|item| item == next)?
    } else if at > 0 {
        order.iter().position(|item| item == &shown[at - 1])? + 1
    } else {
        from
    };
    order.insert(to, lane);
    (order != keys).then_some(order)
}

/// A workspace as pins and card order name it.
pub fn workspace_key(host: &str, workspace: &str) -> String {
    format!("{host}:{workspace}")
}

/// Which agents Lanes shows. Within a kind, any chosen value matches;
/// across kinds, all must. Empty kinds match everything.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LaneFilter {
    pub hosts: BTreeSet<String>,
    /// Provider ids: `claude`, `codex`, …
    pub tools: BTreeSet<String>,
    /// Label keys, as [`fleet::directory::label_key`] makes them.
    pub labels: BTreeSet<String>,
    /// Agent states, as [`fleet::Bucket::as_str`] names them.
    pub states: BTreeSet<String>,
}

impl LaneFilter {
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
            && self.tools.is_empty()
            && self.labels.is_empty()
            && self.states.is_empty()
    }
}

pub enum NavEvent {
    Selected,
}

pub struct Nav {
    pub selection: Option<Selection>,
    pub screen: Screen,
    pub sidebar_open: bool,
    pub sidebar_width: f32,
    pub grouping: Grouping,
    pub collapsed: BTreeSet<String>,
    pub collapsed_pinned: bool,
    pub filters: Filters,
    pub tabs: BTreeMap<String, Vec<Tab>>,
    pub focused: BTreeMap<String, Tab>,
    pub hidden_agents: BTreeMap<String, BTreeSet<String>>,
    pub layout: String,
    pub command_open: bool,
    /// Provider to preselect on the next draft tab. Not saved.
    pub pending_provider: Option<String>,
    /// The extension panel open beside the layout, by id. Not saved.
    pub panel: Option<String>,
    /// Text the panel should adopt, such as a question, bumped with
    /// [`Self::panel_gen`] so the same text can be handed over again.
    pub panel_input: Option<String>,
    pub panel_gen: u64,
    /// Groups showing all rows instead of the first 20.
    pub show_all: BTreeSet<String>,
    pub lane_grouping: LaneGrouping,
    pub lane_label_mode: LabelLaneMode,
    /// Column keys in the user's order. Empty means the default sort.
    pub lane_order: Vec<String>,
    pub lane_collapsed: BTreeSet<String>,
    /// Lanes column width, before fitting to the view.
    pub lane_width: f32,
    pub lane_fit: bool,
    pub follow_relabel: bool,
    pub colored_lanes: bool,
    pub recency_headers: bool,
    /// Lanes boxes a workspace holding one agent, as it does those holding
    /// several or none.
    pub box_single_workspaces: bool,
    /// Window zoom, 1 for 100%.
    pub zoom: f32,
    /// Where each toolbar item sits.
    pub toolbars: crate::toolbars::Arrangement,
    pub lane_filter: LaneFilter,
    /// Workspaces pinned to the top of their lane, as [`workspace_key`]s.
    pub pinned_workspaces: BTreeSet<String>,
    /// The card order the user gave inside a workspace's box.
    pub card_order: BTreeMap<String, Vec<String>>,
    /// Bumped when pins or card order change, so arranged lanes are redone.
    pub ordering_revision: u64,
    /// Agents of one state singled out on the Lanes board, after a click on
    /// the title bar's counts; not saved.
    pub state_highlight: Option<fleet::Bucket>,
    /// Local display overrides by canonical label-derived lane key. Retained when a label
    /// disappears so returning hosts keep their choices; renames use a new key.
    pub lane_color_choices: BTreeMap<String, String>,
    /// Conversations Lanes is showing, kept streaming alongside open tabs.
    pub pinned_sessions: BTreeSet<(String, String)>,
    /// Terminals Lanes is showing, kept attached alongside open tabs.
    pub pinned_terminals: BTreeSet<(String, String)>,
    pub notification_corner: notifications::Corner,
    pub notification_route: notifications::Route,
    /// Seconds a card opened from a notification blinks its outline; 0 is
    /// not at all.
    pub notification_flash: u8,
    /// Seconds an agent's notification stays up; 0 is until closed.
    pub notification_seconds: f32,
    pub notification_layout: notifications::Layout,
    /// The most notifications kept at once.
    pub notification_keep: u8,
    /// A solid border around the window this many pixels wide, 0 to
    /// [`WINDOW_BORDER_MAX`]; 0 is none.
    pub window_border: u8,
    /// Its color, `#rrggbb`.
    pub window_border_color: String,
    /// An agent Lanes should focus and expand, e.g. after a notification
    /// click. Lanes takes it. Not saved.
    pub reveal_agent: Option<(String, String)>,
    next_draft: u64,
}

impl EventEmitter<NavEvent> for Nav {}

impl Nav {
    pub fn load() -> Self {
        let ui = config::load_ui();
        let tabs = ui
            .tabs
            .iter()
            .map(|(key, tabs)| {
                (
                    key.clone(),
                    tabs.iter().filter_map(|t| Tab::parse(t)).collect(),
                )
            })
            .collect();
        let focused = ui
            .focused_tab
            .iter()
            .filter_map(|(key, tab)| Some((key.clone(), Tab::parse(tab)?)))
            .collect();
        Self {
            selection: match (ui.host, ui.workspace) {
                (Some(host), Some(workspace)) => Some(Selection { host, workspace }),
                _ => None,
            },
            screen: Screen::Workspace,
            sidebar_open: ui.sidebar_open.unwrap_or(true),
            sidebar_width: ui
                .sidebar_width
                .unwrap_or(crate::theme::size::SIDEBAR_WIDTH),
            grouping: if ui.group_by_status {
                Grouping::Status
            } else {
                Grouping::Project
            },
            collapsed: ui.collapsed.into_iter().collect(),
            collapsed_pinned: ui.collapsed_pinned,
            filters: Filters {
                hosts: ui.host_filter,
                labels: Vec::new(),
            },
            tabs,
            focused,
            hidden_agents: ui
                .hidden_agents
                .into_iter()
                .map(|(k, v)| (k, v.into_iter().collect()))
                .collect(),
            layout: ui.layout.unwrap_or_else(|| "paseo".into()),
            command_open: false,
            pending_provider: None,
            panel: None,
            panel_input: None,
            panel_gen: 0,
            show_all: BTreeSet::new(),
            lane_grouping: LaneGrouping::parse(&ui.lane_grouping),
            lane_label_mode: LabelLaneMode::parse(&ui.lane_label_mode),
            lane_order: ui.lane_order,
            lane_collapsed: ui.lane_collapsed.into_iter().collect(),
            lane_width: ui
                .lane_width
                .unwrap_or(LANE_WIDTH_DEFAULT)
                .clamp(LANE_WIDTH_MIN, LANE_WIDTH_MAX),
            lane_fit: ui.lane_fit,
            follow_relabel: ui.follow_relabel.unwrap_or(true),
            colored_lanes: ui.colored_lanes,
            recency_headers: ui.recency_headers.unwrap_or(true),
            box_single_workspaces: ui.box_single_workspaces.unwrap_or(true),
            zoom: ui.zoom.unwrap_or(1.).clamp(0.25, 5.),
            toolbars: ui.toolbars,
            lane_filter: ui.lane_filter,
            pinned_workspaces: ui.pinned_workspaces.into_iter().collect(),
            card_order: ui.card_order,
            ordering_revision: 0,
            state_highlight: None,
            lane_color_choices: ui.lane_color_choices,
            pinned_sessions: BTreeSet::new(),
            pinned_terminals: BTreeSet::new(),
            notification_corner: notifications::Corner::parse(&ui.notification_corner),
            notification_route: notifications::Route::parse(&ui.notification_route),
            notification_flash: ui.notification_flash.unwrap_or(NOTIFICATION_FLASH_DEFAULT),
            notification_seconds: (ui.notification_seconds)
                .unwrap_or(notifications::SECONDS_DEFAULT)
                .clamp(0., notifications::SECONDS_MAX),
            notification_layout: notifications::Layout::parse(&ui.notification_layout),
            notification_keep: ui.notification_keep.unwrap_or(notifications::KEEP_DEFAULT),
            window_border: ui.window_border.min(WINDOW_BORDER_MAX),
            window_border_color: parse_hex(&ui.window_border_color)
                .map(|_| ui.window_border_color.clone())
                .unwrap_or_else(|| WINDOW_BORDER_COLOR.to_string()),
            reveal_agent: None,
            next_draft: 0,
        }
    }

    pub fn save(&self) {
        let ui = UiState {
            layout: Some(self.layout.clone()),
            host: self.selection.as_ref().map(|s| s.host.clone()),
            workspace: self.selection.as_ref().map(|s| s.workspace.clone()),
            sidebar_width: Some(self.sidebar_width),
            sidebar_open: Some(self.sidebar_open),
            group_by_status: self.grouping == Grouping::Status,
            collapsed: self.collapsed.iter().cloned().collect(),
            collapsed_pinned: self.collapsed_pinned,
            host_filter: self.filters.hosts.clone(),
            tabs: self
                .tabs
                .iter()
                .map(|(k, tabs)| {
                    (
                        k.clone(),
                        tabs.iter()
                            .filter(|t| matches!(t, Tab::Agent(_) | Tab::Terminal(_)))
                            .map(Tab::key)
                            .collect(),
                    )
                })
                .collect(),
            focused_tab: self
                .focused
                .iter()
                .filter(|(_, t)| matches!(t, Tab::Agent(_) | Tab::Terminal(_)))
                .map(|(k, t)| (k.clone(), t.key()))
                .collect(),
            hidden_agents: self
                .hidden_agents
                .iter()
                .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
                .collect(),
            lane_grouping: self.lane_grouping.as_str().to_string(),
            lane_label_mode: self.lane_label_mode.as_str().to_string(),
            lane_order: self.lane_order.clone(),
            lane_collapsed: self.lane_collapsed.iter().cloned().collect(),
            lane_width: Some(self.lane_width),
            lane_fit: self.lane_fit,
            follow_relabel: Some(self.follow_relabel),
            colored_lanes: self.colored_lanes,
            recency_headers: Some(self.recency_headers),
            box_single_workspaces: Some(self.box_single_workspaces),
            zoom: (self.zoom != 1.).then_some(self.zoom),
            toolbars: self.toolbars.clone(),
            lane_filter: self.lane_filter.clone(),
            pinned_workspaces: self.pinned_workspaces.iter().cloned().collect(),
            card_order: self.card_order.clone(),
            lane_color_choices: self.lane_color_choices.clone(),
            notification_corner: self.notification_corner.as_str().to_string(),
            notification_route: self.notification_route.as_str().to_string(),
            notification_flash: Some(self.notification_flash),
            notification_seconds: Some(self.notification_seconds),
            notification_layout: self.notification_layout.as_str().to_string(),
            notification_keep: Some(self.notification_keep),
            window_border: self.window_border,
            window_border_color: self.window_border_color.clone(),
        };
        config::save_ui(&ui);
    }

    pub fn set_layout(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.layout == id
            || !crate::layouts::registry()
                .iter()
                .any(|layout| layout.id == id)
        {
            return;
        }
        self.layout = id.to_string();
        self.save();
        cx.notify();
    }

    pub fn select(&mut self, host: &str, workspace: &str, cx: &mut Context<Self>) {
        self.selection = Some(Selection {
            host: host.to_string(),
            workspace: workspace.to_string(),
        });
        self.screen = Screen::Workspace;
        self.save();
        cx.emit(NavEvent::Selected);
        cx.notify();
    }

    pub fn show(&mut self, screen: Screen, cx: &mut Context<Self>) {
        self.screen = screen;
        cx.notify();
    }

    pub fn panel_open(&self, id: &str) -> bool {
        self.panel.as_deref() == Some(id)
    }

    /// Opens panel `id`, or closes it if it is open.
    pub fn toggle_panel(&mut self, id: &str, cx: &mut Context<Self>) {
        self.panel = if self.panel_open(id) {
            None
        } else {
            Some(id.to_string())
        };
        cx.notify();
    }

    /// Opens panel `id`, handing it `input` if any.
    pub fn open_panel(&mut self, id: &str, input: Option<String>, cx: &mut Context<Self>) {
        self.panel = Some(id.to_string());
        if let Some(input) = input {
            self.panel_input = Some(input);
            self.panel_gen = self.panel_gen.wrapping_add(1);
        }
        cx.notify();
    }

    pub fn close_panel(&mut self, cx: &mut Context<Self>) {
        self.panel = None;
        cx.notify();
    }

    pub fn is_collapsed(&self, key: &str) -> bool {
        self.collapsed.contains(key)
    }

    pub fn toggle_collapsed(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.to_string());
        }
        self.save();
        cx.notify();
    }

    pub fn tabs_for(&self, key: &str) -> &[Tab] {
        self.tabs.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn focused_tab(&self, key: &str) -> Option<&Tab> {
        self.focused
            .get(key)
            .filter(|tab| self.tabs_for(key).contains(tab))
            .or_else(|| self.tabs_for(key).first())
    }

    pub fn focus_tab(&mut self, key: &str, tab: Tab, cx: &mut Context<Self>) {
        let tabs = self.tabs.entry(key.to_string()).or_default();
        if !tabs.contains(&tab) {
            tabs.push(tab.clone());
        }
        if let Tab::Agent(id) = &tab
            && let Some(hidden) = self.hidden_agents.get_mut(key)
        {
            hidden.remove(id);
        }
        self.focused.insert(key.to_string(), tab);
        self.save();
        cx.notify();
    }

    pub fn new_draft(&mut self, key: &str, cx: &mut Context<Self>) -> Tab {
        self.next_draft += 1;
        let tab = Tab::Draft(format!("d{}", self.next_draft));
        self.focus_tab(key, tab.clone(), cx);
        tab
    }

    /// Replaces a draft tab with the agent it became, keeping its position.
    pub fn promote_draft(
        &mut self,
        key: &str,
        draft: &Tab,
        agent_id: &str,
        cx: &mut Context<Self>,
    ) {
        let agent = Tab::Agent(agent_id.to_string());
        if let Some(tabs) = self.tabs.get_mut(key) {
            tabs.retain(|t| t != &agent);
            if let Some(slot) = tabs.iter_mut().find(|t| *t == draft) {
                *slot = agent.clone();
            } else {
                tabs.push(agent.clone());
            }
        }
        self.focused.insert(key.to_string(), agent);
        self.save();
        cx.notify();
    }

    /// Closes a tab and picks the neighbour to focus. The caller archives an
    /// agent or kills a terminal.
    pub fn close_tab(&mut self, key: &str, tab: &Tab, cx: &mut Context<Self>) -> Option<Closed> {
        let tabs = self.tabs.get_mut(key)?;
        let index = tabs.iter().position(|t| t == tab)?;
        tabs.remove(index);
        if self.focused.get(key) == Some(tab) {
            let next = tabs
                .get(index)
                .or_else(|| index.checked_sub(1).and_then(|i| tabs.get(i)))
                .cloned();
            match next {
                Some(next) => {
                    self.focused.insert(key.to_string(), next);
                }
                None => {
                    self.focused.remove(key);
                }
            }
        }
        let closed = match tab {
            Tab::Agent(id) => {
                self.hidden_agents
                    .entry(key.to_string())
                    .or_default()
                    .insert(id.clone());
                Some(Closed::Agent(id.clone()))
            }
            Tab::Terminal(id) => Some(Closed::Terminal(id.clone())),
            Tab::Draft(_) => None,
        };
        self.save();
        cx.notify();
        closed
    }

    /// Directory disappearance clears selection regardless of which client
    /// archived or removed the workspace; archive actions do not own navigation.
    pub fn reconcile_selection(&mut self, fleet: &Fleet) -> bool {
        let gone = self
            .selection
            .as_ref()
            .is_some_and(|selection| selection_is_gone(fleet, selection));
        if gone {
            self.selection = None;
        }
        gone
    }

    /// Paseo's reconciliation: every live root agent of a workspace gets a
    /// tab unless the user closed it; tabs of agents that are gone close.
    pub fn reconcile(&mut self, inputs: Vec<ReconcileInput>) -> bool {
        let mut changed = false;
        for input in inputs {
            let hidden = self
                .hidden_agents
                .get(&input.key)
                .cloned()
                .unwrap_or_default();
            let tabs = self.tabs.entry(input.key.clone()).or_default();
            let before = tabs.len();
            tabs.retain(|tab| match tab {
                Tab::Agent(id) => input.live.contains(id),
                Tab::Draft(_) => true,
                // A missing id means the PTY died. Keep the tab so the view can
                // say so; the user closes it. New ids are added below.
                Tab::Terminal(_) => true,
            });
            if let Some(ids) = &input.terminals {
                for id in ids {
                    let tab = Tab::Terminal(id.clone());
                    if !tabs.contains(&tab) {
                        tabs.push(tab);
                        changed = true;
                    }
                }
            }
            for id in &input.roots {
                let tab = Tab::Agent(id.clone());
                if !hidden.contains(id) && !tabs.contains(&tab) {
                    tabs.push(tab);
                    changed = true;
                }
            }
            changed |= tabs.len() != before;
        }
        changed
    }

    /// Every (host, agent) a tab or an expanded Lanes card currently shows.
    pub fn open_agents(&self) -> BTreeSet<(String, String)> {
        let mut out = self.open_agent_tabs();
        out.extend(self.pinned_sessions.iter().cloned());
        out
    }

    /// Every (host, agent) with a tab open.
    pub fn open_agent_tabs(&self) -> BTreeSet<(String, String)> {
        let mut out = BTreeSet::new();
        for (key, tabs) in &self.tabs {
            let Some((host, _)) = key.rsplit_once(':') else {
                continue;
            };
            for tab in tabs {
                if let Tab::Agent(id) = tab {
                    out.insert((host.to_string(), id.clone()));
                }
            }
        }
        out
    }

    pub fn set_lane_grouping(&mut self, grouping: LaneGrouping, cx: &mut Context<Self>) {
        if self.lane_grouping == grouping {
            return;
        }
        self.lane_grouping = grouping;
        self.save();
        cx.notify();
    }

    pub fn set_lane_label_mode(&mut self, mode: LabelLaneMode, cx: &mut Context<Self>) {
        if self.lane_label_mode == mode {
            return;
        }
        self.lane_label_mode = mode;
        self.save();
        cx.notify();
    }

    /// `persist: false` while a slider is still being dragged.
    pub fn set_lane_width(&mut self, width: f32, persist: bool, cx: &mut Context<Self>) {
        let width = width.clamp(LANE_WIDTH_MIN, LANE_WIDTH_MAX);
        if self.lane_width != width {
            self.lane_width = width;
            cx.notify();
        }
        if persist {
            self.save();
        }
    }

    pub fn set_lane_fit(&mut self, fit: bool, cx: &mut Context<Self>) {
        if self.lane_fit != fit {
            self.lane_fit = fit;
            self.save();
            cx.notify();
        }
    }

    pub fn set_follow_relabel(&mut self, follow: bool, cx: &mut Context<Self>) {
        self.follow_relabel = follow;
        self.save();
        cx.notify();
    }

    pub fn set_colored_lanes(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.colored_lanes = enabled;
        self.save();
        cx.notify();
    }

    pub fn set_notification_flash(&mut self, seconds: u8, cx: &mut Context<Self>) {
        self.notification_flash = seconds;
        self.save();
        cx.notify();
    }

    /// How long an agent's notification stays up. A slider calls this as it
    /// moves; `persist` saves, as its release does.
    pub fn set_notification_seconds(
        &mut self,
        seconds: f32,
        persist: bool,
        cx: &mut Context<Self>,
    ) {
        self.notification_seconds = seconds.clamp(0., notifications::SECONDS_MAX);
        if persist {
            self.save();
        }
        cx.notify();
    }

    pub fn set_notification_layout(
        &mut self,
        layout: notifications::Layout,
        cx: &mut Context<Self>,
    ) {
        self.notification_layout = layout;
        notifications::apply_layout(layout, self.notification_keep);
        self.save();
        cx.notify();
    }

    pub fn set_notification_keep(&mut self, keep: u8, cx: &mut Context<Self>) {
        self.notification_keep = keep;
        notifications::apply_layout(self.notification_layout, keep);
        self.save();
        cx.notify();
    }

    pub fn set_window_border(&mut self, width: u8, cx: &mut Context<Self>) {
        self.window_border = width.min(WINDOW_BORDER_MAX);
        self.save();
        cx.notify();
    }

    /// Sets the window border's color, given as `#rrggbb`; anything else
    /// is ignored.
    pub fn set_window_border_color(&mut self, hex: &str, cx: &mut Context<Self>) {
        if parse_hex(hex).is_none() || self.window_border_color == hex {
            return;
        }
        self.window_border_color = hex.to_string();
        self.save();
        cx.notify();
    }

    pub fn set_notification_corner(
        &mut self,
        corner: notifications::Corner,
        cx: &mut Context<Self>,
    ) {
        self.notification_corner = corner;
        self.save();
        cx.notify();
    }

    pub fn set_notification_route(&mut self, route: notifications::Route, cx: &mut Context<Self>) {
        self.notification_route = route;
        self.save();
        cx.notify();
    }

    /// Asks Lanes to focus and expand an agent's card.
    pub fn reveal_in_lanes(&mut self, host: &str, agent: &str, cx: &mut Context<Self>) {
        self.reveal_agent = Some((host.to_string(), agent.to_string()));
        cx.notify();
    }

    pub fn set_recency_headers(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.recency_headers = enabled;
        self.save();
        cx.notify();
    }

    /// Pins a workspace to the top of its lane, or unpins it.
    pub fn toggle_workspace_pin(&mut self, host: &str, workspace: &str, cx: &mut Context<Self>) {
        let key = workspace_key(host, workspace);
        if !self.pinned_workspaces.remove(&key) {
            self.pinned_workspaces.insert(key);
        }
        self.ordering_revision += 1;
        self.save();
        cx.notify();
    }

    /// The order of a workspace's cards in its box, by agent id; empty goes
    /// back to most urgent, then most recent, first.
    pub fn set_card_order(
        &mut self,
        host: &str,
        workspace: &str,
        order: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let key = workspace_key(host, workspace);
        if order.is_empty() {
            self.card_order.remove(&key);
        } else {
            self.card_order.insert(key, order);
        }
        self.ordering_revision += 1;
        self.save();
        cx.notify();
    }

    pub fn set_state_highlight(&mut self, state: Option<fleet::Bucket>, cx: &mut Context<Self>) {
        if self.state_highlight != state {
            self.state_highlight = state;
            cx.notify();
        }
    }

    pub fn set_lane_filter(&mut self, filter: LaneFilter, cx: &mut Context<Self>) {
        if self.lane_filter == filter {
            return;
        }
        self.lane_filter = filter;
        self.save();
        cx.notify();
    }

    pub fn set_toolbars(&mut self, toolbars: crate::toolbars::Arrangement, cx: &mut Context<Self>) {
        if self.toolbars == toolbars {
            return;
        }
        self.toolbars = toolbars;
        self.save();
        cx.notify();
    }

    pub fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        self.zoom = zoom;
        self.save();
        cx.notify();
    }

    pub fn set_box_single_workspaces(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.box_single_workspaces = enabled;
        self.save();
        cx.notify();
    }

    pub fn set_lane_color(&mut self, key: &str, index: usize, cx: &mut Context<Self>) {
        let Some((name, _)) = crate::theme::LANE_COLORS.get(index) else {
            return;
        };
        if key.is_empty() {
            return;
        }
        self.lane_color_choices.insert(key.into(), (*name).into());
        self.save();
        cx.notify();
    }

    pub fn set_lane_collapsed(&mut self, key: &str, collapsed: bool, cx: &mut Context<Self>) {
        let changed = if collapsed {
            self.lane_collapsed.insert(key.to_string())
        } else {
            self.lane_collapsed.remove(key)
        };
        if changed {
            self.save();
            cx.notify();
        }
    }

    /// `keys` is the current visual order. The moved column, and every
    /// column, then stick in that order.
    pub fn reorder_lane(
        &mut self,
        keys: &[String],
        key: &str,
        delta: isize,
        cx: &mut Context<Self>,
    ) {
        let Some(order) = reorder_visible_lanes(keys, key, delta, &self.lane_collapsed) else {
            return;
        };
        self.lane_order = order;
        self.save();
        cx.notify();
    }

    /// Moves a lane to the front (`first`) or the back of `keys`, the lanes
    /// in their current order.
    pub fn move_lane_to_end(
        &mut self,
        keys: &[String],
        key: &str,
        first: bool,
        cx: &mut Context<Self>,
    ) {
        let mut order = keys.to_vec();
        let Some(index) = order.iter().position(|item| item == key) else {
            return;
        };
        let lane = order.remove(index);
        if first {
            order.insert(0, lane);
        } else {
            order.push(lane);
        }
        if order == keys {
            return;
        }
        self.lane_order = order;
        self.save();
        cx.notify();
    }

    /// Moves a dragged lane to where the board showed it as it was dropped:
    /// `keys` is every lane in its current order, `shown` the order on
    /// screen.
    pub fn place_lane_as_shown(
        &mut self,
        keys: &[String],
        shown: &[String],
        key: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(order) = place_as_shown(keys, shown, key) else {
            return;
        };
        self.lane_order = order;
        self.save();
        cx.notify();
    }

    pub fn set_pinned_sessions(
        &mut self,
        pins: BTreeSet<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        if self.pinned_sessions == pins {
            return;
        }
        self.pinned_sessions = pins;
        cx.notify();
    }

    pub fn set_pinned_terminals(
        &mut self,
        pins: BTreeSet<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        if self.pinned_terminals == pins {
            return;
        }
        self.pinned_terminals = pins;
        cx.notify();
    }

    /// Every (host, terminal) a tab currently shows.
    pub fn open_terminals(&self) -> BTreeSet<(String, String)> {
        let mut out = BTreeSet::new();
        for (key, tabs) in &self.tabs {
            let Some((host, _)) = key.rsplit_once(':') else {
                continue;
            };
            for tab in tabs {
                if let Tab::Terminal(id) = tab {
                    out.insert((host.to_string(), id.clone()));
                }
            }
        }
        out.extend(self.pinned_terminals.iter().cloned());
        out
    }
}

/// What closing a tab should do on the daemon.
pub enum Closed {
    Agent(String),
    Terminal(String),
}

/// What reconciliation needs to know about one workspace.
pub struct ReconcileInput {
    pub key: String,
    pub roots: Vec<String>,
    /// Every agent on the host; shared by all of the host's workspaces.
    pub live: Rc<BTreeSet<String>>,
    /// Standalone terminals. `None` means the list has not loaded yet.
    pub terminals: Option<Vec<String>>,
}

pub fn reconcile_inputs(
    fleet: &Fleet,
    terminals: &BTreeMap<String, Option<Vec<String>>>,
) -> Vec<ReconcileInput> {
    let mut out = Vec::new();
    for host in &fleet.hosts {
        // Only trust a host's agent list once it has loaded.
        if !host.directory.hydrated
            || !host.directory.agents_hydrated
            || !host.connection.is_online()
        {
            continue;
        }
        let live: Rc<BTreeSet<String>> = Rc::new(host.directory.agents.keys().cloned().collect());
        let mut by_workspace = host.directory.roots_by_workspace();
        for workspace in host.directory.workspaces.values() {
            let mut roots = by_workspace
                .remove(workspace.id.as_str())
                .unwrap_or_default();
            roots.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
            let key = format!("{}:{}", host.config.name, workspace.id);
            out.push(ReconcileInput {
                roots: roots.into_iter().map(|a| a.id.clone()).collect(),
                live: live.clone(),
                terminals: terminals.get(&key).cloned().unwrap_or(None),
                key,
            });
        }
    }
    out
}

/// Clear selection only after an online host has supplied an authoritative directory.
/// A missing host or an in-progress reconnect must not erase the user's saved place.
pub fn selection_is_gone(fleet: &Fleet, selection: &Selection) -> bool {
    fleet.host(&selection.host).is_some_and(|host| {
        host.connection.is_online()
            && host.directory.hydrated
            && !host.directory.workspaces.contains_key(&selection.workspace)
    })
}

#[cfg(test)]
mod selection_reconcile_tests {
    use super::*;
    use fleet::{Connection, Host, HostConfig};

    #[test]
    fn removes_only_authoritatively_missing_selection_on_the_exact_host() {
        let selection = Selection {
            host: "mac".into(),
            workspace: "w".into(),
        };
        let mut fleet = Fleet {
            hosts: vec![Host {
                config: HostConfig {
                    name: "mac".into(),
                    ..Default::default()
                },
                connection: Connection::Online,
                ..Default::default()
            }],
        };
        assert!(!selection_is_gone(&fleet, &selection));
        fleet.hosts[0].directory.hydrated = true;
        assert!(selection_is_gone(&fleet, &selection));
        fleet.hosts[0].connection = Connection::Offline("offline".into());
        assert!(!selection_is_gone(&fleet, &selection));
        fleet.hosts[0].connection = Connection::Online;
        fleet.hosts[0].directory.workspaces.insert(
            "w".into(),
            serde_json::from_value(serde_json::json!({"id":"w","name":"w"})).unwrap(),
        );
        assert!(!selection_is_gone(&fleet, &selection));
        assert!(!selection_is_gone(
            &fleet,
            &Selection {
                host: "other".into(),
                ..selection
            }
        ));
    }
}

#[cfg(test)]
mod visible_order_tests {
    use super::*;

    #[test]
    fn a_dropped_lane_takes_the_place_of_the_one_it_lands_on() {
        let keys = ["a", "b", "c", "d"].map(str::to_string);
        assert_eq!(move_lane_to(&keys, "a", "c").unwrap(), ["b", "c", "a", "d"]);
        assert_eq!(move_lane_to(&keys, "d", "b").unwrap(), ["a", "d", "b", "c"]);
        assert_eq!(move_lane_to(&keys, "b", "b"), None);
        assert_eq!(move_lane_to(&keys, "b", "gone"), None);
    }

    #[test]
    fn a_dropped_lane_lands_as_shown_and_collapsed_lanes_stay_put() {
        let keys = ["a", "hidden", "b", "c"].map(str::to_string);
        let shown = |order: [&str; 3]| order.map(str::to_string);
        // "c" dragged to the front, past the collapsed lane.
        assert_eq!(
            place_as_shown(&keys, &shown(["c", "a", "b"]), "c").unwrap(),
            ["c", "a", "hidden", "b"]
        );
        // "a" dragged to the end.
        assert_eq!(
            place_as_shown(&keys, &shown(["b", "c", "a"]), "a").unwrap(),
            ["hidden", "b", "c", "a"]
        );
        assert_eq!(place_as_shown(&keys, &shown(["a", "b", "c"]), "b"), None);
    }

    #[test]
    fn lane_movers_pass_hidden_neighbours_without_losing_their_keys() {
        let keys = ["a", "hidden", "b", "c"].map(str::to_string);
        let collapsed = BTreeSet::from(["hidden".to_string()]);
        assert_eq!(
            reorder_visible_lanes(&keys, "a", 1, &collapsed).unwrap(),
            ["hidden", "b", "a", "c"]
        );
        assert_eq!(
            reorder_visible_lanes(&keys, "b", -1, &collapsed).unwrap(),
            ["b", "a", "hidden", "c"]
        );
    }

    #[test]
    fn lane_movers_ignore_hidden_sources_and_visible_edges() {
        let keys = ["hidden", "a", "b"].map(str::to_string);
        let collapsed = BTreeSet::from(["hidden".to_string()]);
        assert!(reorder_visible_lanes(&keys, "a", -1, &collapsed).is_none());
        assert!(reorder_visible_lanes(&keys, "b", 1, &collapsed).is_none());
        assert!(reorder_visible_lanes(&keys, "hidden", 1, &collapsed).is_none());
        assert!(reorder_visible_lanes(&keys, "missing", 1, &collapsed).is_none());
    }
}
