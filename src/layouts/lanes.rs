//! Lanes: one column per workstream, each agent a card. Expanding a card
//! embeds the Paseo conversation in the column.

mod activity;
mod colors;
mod edges;
mod find;
mod placement;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use fleet::lanes::{
    LabelLaneMode, Lane, LaneCard, LaneGrouping, LaneKind, LaneModel, LaneState, UNLABELLED,
};
use fleet::providers::{Model, Provider};
use fleet::timeline::ItemKind;
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::Icon;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::gpui::{
    Anchor, AnyElement, AnyView, App, AppContext, ClickEvent, Context, CursorStyle, DismissEvent,
    Div, DragMoveEvent, Entity, FocusHandle, Focusable, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, Rgba, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, SharedString, Stateful, StatefulInteractiveElement, StyleRefinement, Styled,
    Subscription, WeakEntity, Window, anchored, canvas, deferred, div, list, point, px, rgb,
};

use gpui_kit::prelude::FluentBuilder;
use serde_json::json;

use crate::app::Core;
use crate::app::nav::{LANE_WIDTH_MAX, LANE_WIDTH_MIN, LaneFilter, workspace_key};
use crate::app::store::{FleetStore, LabelBatch, LabelChange, TerminalWatch, now_millis};
use crate::layouts::paseo::{Conversation, ConversationEvent, TerminalView};
use crate::theme::{self, palette, size, with_alpha};
use crate::ui;
use crate::widgets::agent_rename::{AgentRename, RenameDone};
use crate::widgets::label_picker::{ApplyLabels, LabelPicker};
use crate::widgets::lane_color_picker;

const EXPANDED_MIN: f32 = 360.;
const EXPANDED_MAX: f32 = 720.;
/// Narrowest an expanded column can be dragged to.
const EXPANDED_RESIZE_MIN: f32 = 300.;
const COLUMN_GAP: f32 = 12.;
const COLUMNS_PADDING: f32 = 16.;
const CARD_GAP: f32 = 6.;
const CARD_HEIGHT: f32 = 90.;
const HEADER_PADDING: f32 = 10.;
const HEADER_TITLE_HEIGHT: f32 = 24.;
const HEADER_STATUS_HEIGHT: f32 = 16.;
const HEADER_HOST_HEIGHT: f32 = 20.;
const HEADER_ROW_GAP: f32 = 6.;
const HEADER_HEIGHT: f32 = HEADER_PADDING * 2.
    + HEADER_TITLE_HEIGHT
    + HEADER_STATUS_HEIGHT
    + HEADER_HOST_HEIGHT
    + HEADER_ROW_GAP * 2.
    + 1.;

struct RelabelPending {
    source: String,
    host: String,
    /// The card relabeled from, followed to its new lane; None for a
    /// workspace relabeled from its box.
    agent: Option<String>,
    workspace: String,
    progress: placement::Progress,
    follow: bool,
    token: u64,
    /// Every lane key, in board order, when the relabel began: a lane not
    /// among them is new, and is placed right after `source`.
    known: Vec<String>,
}

/// Opens the find bar on a Lanes board, for the shell to call when Ctrl+F
/// arrives with focus outside the board.
pub fn open_find(view: &AnyView, window: &mut Window, cx: &mut App) {
    if let Ok(lanes) = view.clone().downcast::<Lanes>() {
        lanes.update(cx, |lanes, cx| lanes.open_find(window, cx));
    }
}

pub fn build(core: &Core, window: &mut Window, cx: &mut App) -> AnyView {
    let core = core.clone();
    cx.new(|cx| Lanes::new(core, window, cx)).into()
}

struct DraftState {
    lane: String,
    input: Entity<TextareaState>,
    workspace: Option<(String, String)>,
    /// A workspace to make for the agent, in place of `workspace`.
    new_workspace: Option<NewWorkspace>,
    provider: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    creating: bool,
    error: Option<String>,
    /// Where the card was drawn: a click outside it closes the draft.
    bounds: Rc<Cell<Option<gpui_kit::gpui::Bounds<Pixels>>>>,
}

/// A workspace the draft will make, in a project on a host.
#[derive(Clone)]
struct NewWorkspace {
    host: String,
    project: String,
    /// In a new git worktree rather than the project's own checkout.
    worktree: bool,
}

/// Lanes' controls in the toolbars.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LanesControl {
    Grouping,
    LabelMode,
    Filters,
    CollapsedLanes,
}

/// A Lanes control as a toolbar item. It shows only while Lanes does.
struct LanesItem {
    kind: LanesControl,
    board: WeakEntity<Lanes>,
}

impl crate::toolbars::ToolbarItem for LanesItem {
    fn id(&self) -> &str {
        match self.kind {
            LanesControl::Grouping => "lanes.grouping",
            LanesControl::LabelMode => "lanes.label-mode",
            LanesControl::Filters => "lanes.filters",
            LanesControl::CollapsedLanes => "lanes.collapsed",
        }
    }
    fn title(&self) -> SharedString {
        match self.kind {
            LanesControl::Grouping => "Lanes: group by",
            LanesControl::LabelMode => "Lanes: label lanes",
            LanesControl::Filters => "Lanes: filters",
            LanesControl::CollapsedLanes => "Lanes: collapsed lanes",
        }
        .into()
    }
    fn home(&self) -> (&'static str, crate::toolbars::Side) {
        (crate::toolbars::LAYOUT, crate::toolbars::Side::Left)
    }
    fn shows_in(&self, layout: &str, _: &App) -> bool {
        layout == "lanes"
    }
    fn grows(&self) -> bool {
        self.kind == LanesControl::CollapsedLanes
    }
    // The filters go before the collapsed lanes, which take the rest.
    fn order(&self) -> i32 {
        match self.kind {
            LanesControl::Filters => 5,
            LanesControl::CollapsedLanes => 10,
            _ => 0,
        }
    }
    fn render(&self, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let board = self.board.upgrade()?;
        board.update(cx, |board, cx| board.toolbar_item(self.kind, cx))
    }
}

/// A lane as grouped for display: the lane it came from, and the grouped
/// lane.
type ArrangedLane = (Rc<Lane>, Rc<Lane>);

/// The workspace a card belongs to, if it has one.
fn card_workspace(card: &LaneCard) -> Option<(&str, &str)> {
    Some((card.host.as_str(), card.workspace_id.as_deref()?))
}

/// Who started whom among a lane's cards. A card's parent is the card of
/// the agent that started it, when that card is in the lane in the same
/// workspace; a card without one is a root.
struct Family {
    parent: Vec<Option<usize>>,
    root: Vec<bool>,
}

impl Family {
    fn of(lane: &Lane) -> Self {
        let cards = &lane.cards;
        let index: HashMap<(&str, &str), usize> = cards
            .iter()
            .enumerate()
            .map(|(ix, card)| ((card.host.as_str(), card.agent_id.as_str()), ix))
            .collect();
        let parent: Vec<Option<usize>> = cards
            .iter()
            .enumerate()
            .map(|(ix, card)| {
                let workspace = card_workspace(card)?;
                let parent = *index.get(&(card.host.as_str(), card.parent.as_deref()?))?;
                (parent != ix && card_workspace(&cards[parent]) == Some(workspace))
                    .then_some(parent)
            })
            .collect();
        // A card is a root unless following its parents ends at one, so a
        // cycle cannot hide cards.
        let root = (0..cards.len())
            .map(|ix| {
                let mut at = ix;
                for _ in 0..cards.len() {
                    match parent[at] {
                        Some(next) if next == ix => return true,
                        Some(next) => at = next,
                        None => return at == ix,
                    }
                }
                true
            })
            .collect();
        Self { parent, root }
    }

    /// The root a card descends from.
    fn root_of(&self, mut ix: usize) -> usize {
        while !self.root[ix] {
            match self.parent[ix] {
                Some(parent) => ix = parent,
                None => break,
            }
        }
        ix
    }

    /// How many parents up a card's root is.
    fn depth(&self, mut ix: usize) -> usize {
        let mut depth = 0;
        while !self.root[ix]
            && let Some(parent) = self.parent[ix]
        {
            ix = parent;
            depth += 1;
        }
        depth
    }
}

/// `lane` with each workspace's cards together, since labels belong to the
/// workspace and relabeling moves them all, and each subagent right under
/// the agent that started it. A workspace sits where its most urgent, then
/// most recent, card sorts, and an agent where the most urgent of it and its
/// subagents sorts. None when the cards already are in that order.
fn group_workspaces(lane: &Lane, placement: &Placement) -> Option<Lane> {
    let cards = &lane.cards;
    let family = Family::of(lane);
    let mut first: HashMap<(&str, &str), usize> = HashMap::new();
    let workspace_rank: Vec<usize> = cards
        .iter()
        .enumerate()
        .map(|(ix, card)| card_workspace(card).map_or(ix, |key| *first.entry(key).or_insert(ix)))
        .collect();
    let mut rank: Vec<usize> = (0..cards.len()).collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); cards.len()];
    for ix in 0..cards.len() {
        let root = family.root_of(ix);
        rank[root] = rank[root].min(ix);
        if !family.root[ix]
            && let Some(parent) = family.parent[ix]
        {
            children[parent].push(ix);
        }
    }
    let mut roots: Vec<usize> = (0..cards.len()).filter(|&ix| family.root[ix]).collect();
    // Pinned workspaces first; inside a workspace, the order the user gave,
    // then the rest as they sort.
    let key = |ix: usize| card_workspace(&cards[ix]).map(|(host, ws)| workspace_key(host, ws));
    let pinned: Vec<bool> = (0..cards.len())
        .map(|ix| key(ix).is_some_and(|key| placement.pinned.contains(&key)))
        .collect();
    let given: Vec<usize> = (0..cards.len())
        .map(|ix| {
            key(ix)
                .and_then(|key| placement.order.get(&key))
                .and_then(|order| order.iter().position(|id| *id == cards[ix].agent_id))
                .unwrap_or(usize::MAX)
        })
        .collect();
    roots.sort_by_key(|&ix| (!pinned[ix], workspace_rank[ix], given[ix], rank[ix], ix));
    let mut order = Vec::with_capacity(cards.len());
    let mut stack = Vec::new();
    for root in roots {
        stack.push(root);
        while let Some(ix) = stack.pop() {
            order.push(ix);
            // Children were pushed in card order; pop them in it.
            stack.extend(children[ix].iter().rev());
        }
    }
    if order.iter().enumerate().all(|(at, &ix)| at == ix) {
        return None;
    }
    Some(Lane {
        cards: order.into_iter().map(|ix| cards[ix].clone()).collect(),
        ..lane.clone()
    })
}

/// What the user said about where cards go: workspaces pinned to the top
/// of their lane, and the card order given inside a workspace's box.
struct Placement<'a> {
    pinned: &'a BTreeSet<String>,
    order: &'a BTreeMap<String, Vec<String>>,
}

/// Lanes show more than this many empty workspaces folded away.
const EMPTY_UNFOLDED: usize = 3;
/// An agent shows more than this many subagents folded away, all but those
/// that need the user.
const SUBAGENTS_UNFOLDED: usize = 2;
const BOX_HEADER_HEIGHT: f32 = 24.;
const EMPTY_HEIGHT: f32 = 26.;
const FOLD_HEIGHT: f32 = 24.;
const COMPACT_HEIGHT: f32 = 28.;
const SUBAGENT_HEIGHT: f32 = 52.;
/// Indent per level of subagent.
const SUBAGENT_INDENT: f32 = 14.;
const BOX_RADIUS: f32 = 6.;
/// The least height an expanded card is squeezed to.
const EXPANDED_MIN_HEIGHT: f32 = 240.;

/// One row of a lane's card list. Workspace boxes span several rows: a
/// header, then its cards, the last of which closes the box.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Row {
    /// A workspace box's header: the index of its first card, and how many
    /// cards it holds.
    Header { card: usize, count: usize },
    /// A card, inside its workspace's box when `boxed`; `last` closes it.
    /// `depth` is how many parents up its root agent is.
    Card {
        card: usize,
        boxed: bool,
        last: bool,
        depth: usize,
    },
    /// Folds away the subagents of `card`, `count` of them.
    Subagents {
        card: usize,
        count: usize,
        open: bool,
        boxed: bool,
        last: bool,
    },
    /// A workspace's `index`th terminal, in its box.
    Terminal {
        owner: Owner,
        index: usize,
        boxed: bool,
        last: bool,
    },
    /// The toggle that folds away a lane's empty workspaces.
    Fold { count: usize, open: bool },
    /// A workspace without agents: an index into `lane.workspaces`. With
    /// terminals, it heads a box of them (`boxed`); else it is a dashed
    /// line of its own.
    Empty { workspace: usize, boxed: bool },
}

/// The workspace a terminal row belongs to: that of a lane's card, or one
/// of its workspaces without agents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Owner {
    Card(usize),
    Workspace(usize),
}

impl Owner {
    /// Host and workspace id.
    fn workspace(self, lane: &Lane) -> (&str, &str) {
        match self {
            Owner::Card(card) => card_workspace(&lane.cards[card]).unwrap_or_default(),
            Owner::Workspace(ix) => {
                let ws = &lane.workspaces[ix];
                (ws.host.as_str(), ws.workspace_id.as_str())
            }
        }
    }
}

impl Row {
    /// Whether the gap between boxes follows this row.
    fn ends_box(self) -> bool {
        match self {
            Row::Header { .. } => false,
            Row::Card { boxed, last, .. }
            | Row::Subagents { boxed, last, .. }
            | Row::Terminal { boxed, last, .. } => !boxed || last,
            Row::Empty { boxed, .. } => !boxed,
            Row::Fold { .. } => true,
        }
    }

    fn set_last(&mut self) {
        if let Row::Card { last, .. } | Row::Subagents { last, .. } | Row::Terminal { last, .. } =
            self
        {
            *last = true;
        }
    }

    /// Whether `self` in `lane` shows the same as `other` in `other_lane`.
    fn same_as(self, lane: &Lane, other: Row, other_lane: &Lane) -> bool {
        let same_card = |a: usize, b: usize| lane.cards[a] == other_lane.cards[b];
        match (self, other) {
            (Row::Header { card: a, count: m }, Row::Header { card: b, count: n }) => {
                m == n
                    && card_workspace(&lane.cards[a]) == card_workspace(&other_lane.cards[b])
                    && lane.cards[a].workspace_name == other_lane.cards[b].workspace_name
            }
            (
                Row::Card {
                    card: a,
                    boxed,
                    last,
                    depth,
                },
                Row::Card {
                    card: b,
                    boxed: boxed_b,
                    last: last_b,
                    depth: depth_b,
                },
            ) => boxed == boxed_b && last == last_b && depth == depth_b && same_card(a, b),
            (
                Row::Subagents {
                    card: a,
                    count,
                    open,
                    boxed,
                    last,
                },
                Row::Subagents {
                    card: b,
                    count: count_b,
                    open: open_b,
                    boxed: boxed_b,
                    last: last_b,
                },
            ) => {
                (count, open, boxed, last) == (count_b, open_b, boxed_b, last_b) && same_card(a, b)
            }
            (
                Row::Empty {
                    workspace: a,
                    boxed,
                },
                Row::Empty {
                    workspace: b,
                    boxed: boxed_b,
                },
            ) => boxed == boxed_b && lane.workspaces[a] == other_lane.workspaces[b],
            (
                Row::Terminal {
                    owner,
                    index,
                    boxed,
                    last,
                },
                Row::Terminal {
                    owner: owner_b,
                    index: index_b,
                    boxed: boxed_b,
                    last: last_b,
                },
            ) => {
                (index, boxed, last) == (index_b, boxed_b, last_b)
                    && owner.workspace(lane) == owner_b.workspace(other_lane)
            }
            (a, b) => a == b,
        }
    }
}

/// Whether a card needs the user, so folding never hides it.
fn needs_user(card: &LaneCard) -> bool {
    matches!(
        card.bucket,
        fleet::Bucket::NeedsInput | fleet::Bucket::Failed
    )
}

/// The rows of a lane in [`group_workspaces`] order. A workspace with one
/// card is boxed only when `box_single`. An agent's subagents follow it,
/// folded away past [`SUBAGENTS_UNFOLDED`] unless `subagents_open` says it
/// is open, and its workspace's `terminals` close its box. Workspaces
/// without agents follow the cards: those with terminals boxed with them,
/// the empty ones folded away past [`EMPTY_UNFOLDED`] unless `empty_open`.
fn lane_rows(
    lane: &Lane,
    box_single: bool,
    empty_open: bool,
    subagents_open: &dyn Fn(&LaneCard) -> bool,
    terminals: &dyn Fn(&str, &str) -> usize,
) -> Vec<Row> {
    let family = Family::of(lane);
    let cards = &lane.cards;
    let mut rows = Vec::with_capacity(cards.len() + 4);
    let mut start = 0;
    while start < cards.len() {
        let key = card_workspace(&cards[start]);
        let end = match key {
            Some(key) => {
                start
                    + cards[start..]
                        .iter()
                        .take_while(|card| card_workspace(card) == Some(key))
                        .count()
            }
            None => start + 1,
        };
        let count = end - start;
        let shells = key.map_or(0, |(host, workspace)| terminals(host, workspace));
        let boxed = key.is_some() && (count + shells > 1 || box_single);
        if boxed {
            rows.push(Row::Header { card: start, count });
        }
        let first_row = rows.len();
        let mut at = start;
        while at < end {
            rows.push(Row::Card {
                card: at,
                boxed,
                last: false,
                depth: 0,
            });
            // Its subagents follow it, up to the next root.
            let below = cards[at + 1..end]
                .iter()
                .enumerate()
                .take_while(|(offset, _)| !family.root[at + 1 + offset])
                .count();
            let folds = below > SUBAGENTS_UNFOLDED;
            let open = !folds || subagents_open(&cards[at]);
            if folds {
                rows.push(Row::Subagents {
                    card: at,
                    count: below,
                    open,
                    boxed,
                    last: false,
                });
            }
            for (sub, card) in cards.iter().enumerate().skip(at + 1).take(below) {
                if open || needs_user(card) {
                    rows.push(Row::Card {
                        card: sub,
                        boxed,
                        last: false,
                        depth: family.depth(sub),
                    });
                }
            }
            at += 1 + below;
        }
        rows.extend((0..shells).map(|index| Row::Terminal {
            owner: Owner::Card(start),
            index,
            boxed,
            last: false,
        }));
        if rows.len() > first_row {
            rows.last_mut().expect("just pushed").set_last();
        }
        start = end;
    }
    let occupied: HashSet<(&str, &str)> = cards.iter().filter_map(card_workspace).collect();
    let mut empty: Vec<usize> = Vec::new();
    for (ix, ws) in lane.workspaces.iter().enumerate() {
        if occupied.contains(&(ws.host.as_str(), ws.workspace_id.as_str())) {
            continue;
        }
        let shells = terminals(&ws.host, &ws.workspace_id);
        if shells == 0 {
            empty.push(ix);
            continue;
        }
        rows.push(Row::Empty {
            workspace: ix,
            boxed: true,
        });
        rows.extend((0..shells).map(|index| Row::Terminal {
            owner: Owner::Workspace(ix),
            index,
            boxed: true,
            last: index + 1 == shells,
        }));
    }
    let folds = empty.len() > EMPTY_UNFOLDED;
    if folds {
        rows.push(Row::Fold {
            count: empty.len(),
            open: empty_open,
        });
    }
    if !folds || empty_open {
        rows.extend(empty.into_iter().map(|workspace| Row::Empty {
            workspace,
            boxed: false,
        }));
    }
    rows
}

/// How a card sits in its lane.
#[derive(Clone, Copy)]
enum Frame {
    /// A card of its own, outlined and rounded.
    Alone,
    /// A slice of its workspace's box; `last` closes the box. Unless
    /// `framed`, the box draws its own outline.
    Boxed { last: bool, framed: bool },
}

/// The colours of a workspace's box in one lane.
#[derive(Clone, Copy)]
struct BoxLook {
    border: Hsla,
    bg: Rgba,
    hover: Rgba,
    /// Selected in this lane: outlined in the accent.
    outlined: bool,
    /// Selected in another lane: tinted with the accent.
    tinted: bool,
}

/// Wraps a card's `content` in its frame. Alone, it is its own rounded card;
/// boxed, a slice of its workspace's box, divided from the row above by a
/// hairline. Focus outlines it in the accent either way.
fn card_frame(
    id: SharedString,
    height: f32,
    focused: bool,
    frame: Frame,
    look: BoxLook,
    content: Div,
) -> Stateful<Div> {
    let p = palette();
    let accent: Hsla = p.accent.into();
    let hover = look.hover;
    let shell = div()
        .id(id)
        .h(px(height))
        .flex_shrink_0()
        .min_w(px(0.))
        .overflow_hidden();
    match frame {
        Frame::Alone => shell
            .rounded(px(BOX_RADIUS))
            .border_1()
            .border_color(if focused || look.outlined {
                accent
            } else {
                look.border
            })
            .bg(look.bg)
            .hover(move |style| style.bg(hover))
            .child(content.size_full()),
        Frame::Boxed { last, framed } => {
            let closes = framed && last;
            shell
                .when(framed, |shell| shell.border_l_1().border_r_1())
                .when(closes, |shell| shell.border_b_1().rounded_b(px(BOX_RADIUS)))
                .border_color(if focused { accent } else { look.border })
                .child(
                    // GPUI tracks hover only on elements with an id; without
                    // one, hovering repaints only when something else does.
                    content
                        .id("card-inner")
                        .size_full()
                        .border_t_1()
                        .when(focused && !closes, |content| content.border_b_1())
                        .border_color(if focused {
                            accent
                        } else {
                            with_alpha(p.border, 0.7)
                        })
                        .when(closes, |content| content.rounded_b(px(BOX_RADIUS - 1.)))
                        .bg(if focused { look.hover } else { look.bg })
                        .hover(move |style| style.bg(hover)),
                )
        }
    }
}

/// A keyboard or click target in a lane other than a card.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum RowFocus {
    /// An empty workspace, by host and workspace id.
    Workspace(String, String),
    /// The empty workspaces' fold toggle.
    Fold,
    /// An agent's subagents' fold toggle, by host and agent id.
    Subagents(String, String),
    /// A terminal, by host and terminal id.
    Terminal(String, String),
}

/// A workspace box's label picker, opened from its header.
struct BoxPicker {
    lane: String,
    host: String,
    workspace: String,
    picker: Entity<LabelPicker>,
}

/// The board order after a relabel: `known` (the order before it) less
/// lanes that are gone, with lanes `current` has that `known` lacks placed
/// right after `source`, or where it stood if it is gone. None when no lane
/// is new.
fn order_with_new_lanes(known: &[String], source: &str, current: &[String]) -> Option<Vec<String>> {
    let new: Vec<&String> = current.iter().filter(|key| !known.contains(key)).collect();
    if new.is_empty() {
        return None;
    }
    let at = known
        .iter()
        .position(|key| key == source)
        .map_or(known.len(), |ix| ix + 1);
    Some(
        known[..at]
            .iter()
            .chain(new)
            .chain(&known[at..])
            .filter(|key| current.contains(key))
            .cloned()
            .collect(),
    )
}

/// A drag of an expanded column's right edge.
struct ColumnResize {
    lane: String,
    /// Pointer x and column width when the drag began.
    start_x: Pixels,
    start_width: Pixels,
}

/// An open card context menu. Cards only carry a right-click listener; the
/// board owns the single menu, as Zed's panels do. A `context_menu()` per
/// card laid out and painted its own menu layer on every frame.
struct CardMenu {
    position: Point<Pixels>,
    menu: Entity<PopupMenu>,
    _dismiss: Subscription,
}

pub struct Lanes {
    core: Core,
    focus: FocusHandle,
    h_scroll: ScrollHandle,
    collapsed_scroll: ScrollHandle,
    /// Lane key → (host, agent) expanded in that column.
    expanded: BTreeMap<String, (String, String)>,
    /// Lane key → (host, terminal) open in that column, in place of an
    /// expanded agent.
    expanded_terminals: BTreeMap<String, (String, String)>,
    terminal_views: HashMap<(String, String), Entity<TerminalView>>,
    /// The workspaces whose terminal lists Lanes last asked to watch.
    terminal_watch_key: u64,
    conversations: HashMap<(String, String), Entity<Conversation>>,
    /// Agent just created, kept expanded until the directory lists it.
    pending: Option<(String, String, String)>,
    focused_lane: Option<String>,
    focused_agent: Option<(String, String)>,
    draft_focused: bool,
    /// The lane and text of a draft closed without creating, restored by
    /// the next draft opened in that lane.
    draft_stash: Option<(String, String)>,
    draft: Option<DraftState>,
    /// Focus this composer's input once it exists.
    focus_composer: Option<(String, String)>,
    /// Per lane key: the virtual list of its cards.
    lists: HashMap<String, LaneList>,
    /// Per lane key: the cached view the column renders in.
    columns: HashMap<String, Entity<ColumnView>>,
    /// The lane model, patched from the store's change log and rebuilt only
    /// when the nav's grouping, order or colours change.
    lanes_cache: RefCell<Option<LaneCache>>,
    middle: Option<MiddleScroll>,
    /// Last pointer position over the board, in window coordinates.
    pointer: Point<Pixels>,
    width_slider: Entity<SliderState>,
    /// Last observed nav transition for focus reconciliation and stopping pan.
    /// Model validity itself belongs to LaneCache::nav_key.
    observed_nav_key: Option<LaneModelKey>,
    /// Width of an ordinary column in the last render, after fitting.
    column_width: Pixels,
    width_save_generation: u64,
    /// The one right-click menu for every card, open over the card it was
    /// opened on.
    card_menu: Option<CardMenu>,
    /// Widths the user dragged expanded columns to, by lane. A lane drops
    /// its width when it no longer has an expanded card, so it returns to
    /// the width the other lanes share.
    expanded_widths: HashMap<String, Pixels>,
    /// An expanded column's right edge being dragged.
    resizing: Option<ColumnResize>,
    /// Since a resize, the board keeps its length while the view is past
    /// its end, so narrowing a lane does not move the board under the
    /// pointer.
    hold_board_length: bool,
    /// A lane to scroll fully into view on the next render, once its width
    /// is known: an expanded card should be seen whole.
    reveal_column: Option<String>,
    /// An expanded card's title being edited in place.
    renaming: Option<(Entity<AgentRename>, Subscription)>,
    /// Per lane key: the lane it was grouped from and the grouped lane, so
    /// an unchanged lane keeps its identity.
    arranged_cache: RefCell<HashMap<String, ArrangedLane>>,
    /// The nav ordering revision the arranged lanes were grouped with.
    arranged_revision: Cell<u64>,
    /// The focused row when it is not a card.
    focused_row: Option<RowFocus>,
    /// Lanes showing their empty workspaces unfolded.
    empty_open: HashSet<String>,
    box_picker: Option<BoxPicker>,
    /// The card shown large and centered over the dimmed board, by lane,
    /// host and agent. It stays expanded in its lane underneath.
    focus_mode: Option<(String, String, String)>,
    /// The lane whose box a workspace rename was started from; the same
    /// workspace's boxes in other lanes keep showing its name.
    rename_lane: Option<String>,
    /// Agents showing their subagents unfolded, by host and agent id.
    subagents_open: HashMap<String, HashSet<String>>,
    label_pickers: HashMap<(String, String, String, String), Entity<LabelPicker>>,
    relabel_pending: Option<RelabelPending>,
    next_relabel: u64,
    window_active: bool,
    pointer_inside: bool,
    view_membership: Option<placement::ViewMembership>,
    focus_check_needed: Cell<bool>,
    /// A lane being dragged, and where the board shows it.
    lane_drag: Option<LaneDragPreview>,
    /// The find bar, while open: see [`find`].
    find: Option<find::Find>,
    /// Where a card dragged inside its box would land: before or after
    /// this card, as host, agent and whether before.
    card_drop: Option<(String, String, bool)>,
    /// Loaded conversations lowercased for find, kept between searches.
    find_index: RefCell<HashMap<(String, String), find::Indexed>>,
}

/// One lane column as its own cached view. It tracks the live summaries it
/// depends on; its render asks the board to build the column. GPUI
/// reuses a cached view's subtree until that entity is notified, so hovering
/// a card or scrolling a lane re-renders one column, and the board notifies
/// a column only when what it shows changed (see [`ColumnInputs`]).
struct ColumnView {
    board: WeakEntity<Lanes>,
    inputs: Option<ColumnInputs>,
    /// Stream changes need to invalidate this cached view independently of
    /// directory changes, including another copy of an expanded agent.
    session_watches: Vec<Subscription>,
    watched_lane: Option<(Rc<Lane>, u64)>,
}

/// Everything a column shows, as last handed to it by the board.
#[derive(Clone)]
struct ColumnInputs {
    lanes: LaneSet,
    ix: usize,
    width: Pixels,
    style: LaneStyle,
    /// A hash of the board state the column reads: focus, expansion, draft,
    /// relabeling, live-session membership, store revision where it matters,
    /// and the minute for "3m ago" labels. See [`Lanes::column_fingerprint`].
    fingerprint: u64,
}

impl ColumnInputs {
    fn shows_the_same_as(&self, other: &ColumnInputs) -> bool {
        self.width == other.width
            && self.fingerprint == other.fingerprint
            && (Rc::ptr_eq(&self.lanes[self.ix], &other.lanes[other.ix])
                || self.lanes[self.ix] == other.lanes[other.ix])
    }
}

impl ColumnView {
    fn watch_sessions(
        &mut self,
        lane: &Rc<Lane>,
        store: &Entity<FleetStore>,
        cx: &mut Context<Self>,
    ) {
        let store = store.read(cx);
        let revision = store.sessions_revision();
        if self
            .watched_lane
            .as_ref()
            .is_none_or(|(old, at)| *at != revision || !Rc::ptr_eq(old, lane))
        {
            let sessions: Vec<_> = if lane.collapsed {
                Vec::new()
            } else {
                lane.cards
                    .iter()
                    .filter_map(|card| store.session(&card.host, &card.agent_id))
                    .collect()
            };
            self.session_watches = sessions
                .iter()
                .map(|session| {
                    let mut previous = assistant_blurb(&session.read(cx).timeline);
                    cx.observe(session, move |_, session, cx| {
                        let latest = assistant_blurb(&session.read(cx).timeline);
                        if latest != previous {
                            previous = latest;
                            cx.notify();
                        }
                    })
                })
                .collect();
            self.watched_lane = Some((lane.clone(), revision));
        }
    }
}

impl Render for ColumnView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(inputs) = self.inputs.clone() else {
            return div().into_any_element();
        };
        self.board
            .update(cx, |board, cx| {
                board.column(
                    &inputs.lanes,
                    inputs.ix,
                    inputs.width,
                    inputs.style,
                    window,
                    cx,
                )
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

/// The board's lanes. Each lane is shared, so a snapshot is cheap and an
/// unchanged lane keeps its identity across updates.
type LaneSet = Rc<Vec<Rc<Lane>>>;

/// Aggregated once per lane change, rather than per scroll or hover frame.
fn lane_decor(lane: &Lane, colors: &BTreeMap<String, String>) -> LaneDecor {
    LaneDecor {
        activity: activity::Summary::from_cards(&lane.cards),
        swatch: matches!(lane.kind, LaneKind::Label | LaneKind::Combination)
            .then(|| colors::index(&lane.key, lane.color.as_deref(), colors)),
    }
}

/// The incrementally maintained lane model, its latest snapshot, and display
/// data derived per lane.
struct LaneCache {
    model: LaneModel,
    /// Store change-log position the model reflects.
    cursor: u64,
    nav_key: LaneModelKey,
    lanes: LaneSet,
    visible: LaneSet,
    /// The filter `visible` was made with; `None` when it must be remade.
    visible_filter: Option<LaneFilter>,
    decor: HashMap<String, LaneDecor>,
}
#[derive(Clone, Copy, Default)]
struct LaneDecor {
    activity: activity::Summary,
    swatch: Option<usize>,
}

/// Which of a lane header's controls fit beside its title. As the lane
/// narrows they go in this order: move to the front and back, move one
/// left and right, collapse, new agent, the card count. The grip stays: the
/// narrowest lane leaves room for it.
#[derive(Debug, PartialEq, Eq)]
struct HeaderFit {
    count: bool,
    add: bool,
    collapse: bool,
    steps: bool,
    ends: bool,
}

impl HeaderFit {
    /// Room the title keeps before any control is shown.
    const TITLE_MIN: f32 = 64.;
    /// A small icon button.
    const BUTTON: f32 = 20.;
    const GRIP: f32 = 14.;
    /// The colour dot or its picker.
    const DOT: f32 = 20.;
    const COMBINATION_ICON: f32 = 12.;
    const DIGIT: f32 = 7.;

    fn for_width(width: f32, combination: bool, digits: usize) -> Self {
        // The column's border and padding, then what is always there.
        let mut room = width - 2. - 2. * HEADER_PADDING;
        // The gap either side of the spacer after the title and new agent.
        room -= Self::DOT + HEADER_ROW_GAP + Self::TITLE_MIN + 2. * HEADER_ROW_GAP + Self::GRIP;
        if combination {
            room -= Self::COMBINATION_ICON + HEADER_ROW_GAP;
        }
        // Each in turn, while there is room; once one does not fit, neither
        // do those after it.
        let mut take = |cost: f32| {
            let fits = room >= cost;
            room = if fits { room - cost } else { -1. };
            fits
        };
        Self {
            count: take(digits as f32 * Self::DIGIT + HEADER_ROW_GAP),
            add: take(Self::BUTTON + HEADER_ROW_GAP),
            collapse: take(Self::BUTTON),
            steps: take(2. * Self::BUTTON),
            ends: take(2. * Self::BUTTON),
        }
    }
}

gpui_kit::actions!(lanes, [FindOnBoard]);

/// A lane being dragged by its grip, onto another lane.
#[derive(Clone)]
struct LaneDrag {
    key: String,
    title: SharedString,
}

/// A card being dragged to another place in its workspace's box.
#[derive(Clone)]
struct CardDrag {
    lane: String,
    host: String,
    agent: String,
    workspace: String,
    title: SharedString,
}

/// Where a dragged lane would land: the board shows its lanes in `order`,
/// the keys of the lanes on screen, until it is dropped or let go.
struct LaneDragPreview {
    key: String,
    order: Vec<String>,
}

/// Resolved once per rendered lane, then copied into virtual card builders.
#[derive(Clone, Copy)]
struct LaneStyle {
    column: Rgba,
    column_hover: Rgba,
    header: Rgba,
    card: Rgba,
    card_hover: Rgba,
    swatch: Option<usize>,
    editable: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct LaneModelKey {
    membership: (LaneGrouping, LabelLaneMode),
    order: Vec<String>,
    collapsed: BTreeSet<String>,
    colors: BTreeMap<String, String>,
}

#[derive(PartialEq, Eq)]
struct LaneModelKeyRef<'a> {
    membership: (LaneGrouping, LabelLaneMode),
    order: &'a [String],
    collapsed: &'a BTreeSet<String>,
    colors: &'a BTreeMap<String, String>,
}

impl LaneModelKey {
    fn as_ref(&self) -> LaneModelKeyRef<'_> {
        LaneModelKeyRef {
            membership: self.membership,
            order: &self.order,
            collapsed: &self.collapsed,
            colors: &self.colors,
        }
    }
    fn matches_nav(&self, nav: &crate::app::Nav) -> bool {
        self.as_ref() == lane_model_key_ref(nav)
    }
}

fn lane_model_key_ref(nav: &crate::app::Nav) -> LaneModelKeyRef<'_> {
    LaneModelKeyRef {
        membership: (nav.lane_grouping, nav.lane_label_mode),
        order: &nav.lane_order,
        collapsed: &nav.lane_collapsed,
        colors: &nav.lane_color_choices,
    }
}

fn lane_model_key(nav: &crate::app::Nav) -> LaneModelKey {
    let key = lane_model_key_ref(nav);
    LaneModelKey {
        membership: key.membership,
        order: key.order.to_vec(),
        collapsed: key.collapsed.clone(),
        colors: key.colors.clone(),
    }
}

const FIT_MIN_WIDTH: f32 = 160.;
/// Below this width a column header shows its buttons only on hover, and
/// its host chips collapse to a count.
const NARROW_WIDTH: f32 = 290.;
/// Ctrl+wheel width change per wheel line.
const WHEEL_WIDTH_STEP: f32 = 20.;

/// Scrolling across columns with the middle mouse button, the way browsers
/// do it on Windows: hold and drag to pan, or click to autoscroll.
#[derive(Debug, Clone, Copy)]
enum MiddleScroll {
    /// Button held. Turns into a pan once the pointer moves a few pixels.
    Held {
        origin: Point<Pixels>,
        offset: Pixels,
        panning: bool,
    },
    /// Clicked without moving: the board scrolls toward the pointer, faster
    /// the further it is from `origin`. Any click or Escape ends it.
    Auto { origin: Point<Pixels> },
}

const PAN_THRESHOLD: f32 = 4.;
const AUTOSCROLL_DEAD_ZONE: f32 = 12.;
/// Scroll speed in pixels per second, per pixel the pointer is past the dead zone.
const AUTOSCROLL_GAIN: f32 = 8.;
const AUTOSCROLL_TICK: Duration = Duration::from_millis(7);

/// A lane's card list only builds the rows in view. `source` is the lane
/// and row settings the list state was last told about, to see what
/// changed; `rows` are that lane's rows.
/// What a lane's rows were made from: the lane, whether single-agent
/// workspaces are boxed, whether its empty workspaces are unfolded, and the
/// subagent-fold and terminal-list generations.
type ListSource = (Rc<Lane>, bool, bool, (u64, u64));

struct LaneList {
    state: ListState,
    source: Option<ListSource>,
    rows: Rc<Vec<Row>>,
}

impl LaneList {
    fn new() -> Self {
        Self {
            state: ListState::new(0, ListAlignment::Top, px(240.)),
            source: None,
            rows: Rc::new(Vec::new()),
        }
    }

    /// Tells the list which rows changed, keeping the scroll position.
    fn sync(&mut self, lane: &Rc<Lane>, board: &Lanes, box_single: bool, cx: &App) {
        let empty_open = board.empty_open.contains(&lane.key);
        let generation = board.row_generation(lane, cx);
        if self
            .source
            .as_ref()
            .is_some_and(|(old, single, open, seen)| {
                Rc::ptr_eq(old, lane)
                    && *single == box_single
                    && *open == empty_open
                    && *seen == generation
            })
        {
            return;
        }
        let rows = board.rows(lane, box_single, cx);
        let same = |a: &Row, b: &Row| match &self.source {
            Some((old, ..)) => a.same_as(old, *b, lane),
            None => false,
        };
        let old = &self.rows;
        let prefix = old
            .iter()
            .zip(&rows)
            .take_while(|(a, b)| same(a, b))
            .count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(rows[prefix..].iter().rev())
            .take_while(|(a, b)| same(a, b))
            .count();
        let (old_end, new_end) = (old.len() - suffix, rows.len() - suffix);
        if prefix == old.len() && old.len() == rows.len() {
            // Nothing moved.
        } else if old_end - prefix == new_end - prefix {
            self.state.remeasure_items(prefix..old_end);
        } else {
            self.state.splice(prefix..old_end, new_end - prefix);
        }
        self.rows = Rc::new(rows);
        self.source = Some((lane.clone(), box_single, empty_open, generation));
    }
}

impl Lanes {
    fn new(core: Core, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |this, _, cx| {
            this.finish_relabel(false, cx);
            this.prune_expanded(cx);
            let membership = this.view_membership(cx);
            if this.view_membership.as_ref() != Some(&membership) {
                let follow = this.core.nav.read(cx).follow_relabel;
                this.reconcile_view(follow, cx);
            }
            cx.notify();
        })
        .detach();
        cx.observe_in(&core.nav, window, |this, nav, window, cx| {
            if nav.read(cx).layout != "lanes" {
                this.terminal_watch_key = 0;
                this.stop_middle(cx);
            }
            // A notification click asked for an agent's card.
            if let Some((host, agent)) = nav.read(cx).reveal_agent.clone() {
                nav.update(cx, |nav, _| nav.reveal_agent = None);
                this.reveal_agent(&host, &agent, window, cx);
            }
            // Width and other view settings don't change the lane model.
            if !this
                .observed_nav_key
                .as_ref()
                .is_some_and(|old| old.matches_nav(nav.read(cx)))
            {
                let key = lane_model_key(nav.read(cx));
                let collapsed_changed = this
                    .observed_nav_key
                    .as_ref()
                    .is_some_and(|old| old.collapsed != key.collapsed);
                let membership_changed = this
                    .observed_nav_key
                    .as_ref()
                    .is_none_or(|old| old.membership != key.membership);
                this.observed_nav_key = Some(key);
                if collapsed_changed || membership_changed {
                    this.stop_middle(cx);
                }
                if membership_changed {
                    this.prune_expanded(cx);
                    this.reconcile_view(true, cx);
                }
            }
            // Keep the slider in step when the width changes another way.
            let width = nav.read(cx).lane_width;
            this.width_slider.update(cx, |slider, cx| {
                if slider.value().start() != width {
                    slider.set_value(width, window, cx);
                }
            });
            cx.notify();
        })
        .detach();
        cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            if !window.is_window_active() {
                this.stop_middle(cx);
            } else {
                cx.notify();
            }
        })
        .detach();
        let width = core.nav.read(cx).lane_width;
        let width_slider = cx.new(|_| {
            // Max first: `min` clamps against the current max, which starts at 100.
            SliderState::new()
                .max(LANE_WIDTH_MAX)
                .min(LANE_WIDTH_MIN)
                .step(10.)
                .default_value(width)
        });
        cx.subscribe(&width_slider, |this, _, event: &SliderEvent, cx| {
            let (value, persist) = match event {
                SliderEvent::Change(value) => (value.start(), false),
                SliderEvent::Release(value) => (value.start(), true),
            };
            this.core
                .nav
                .update(cx, |nav, cx| nav.set_lane_width(value, persist, cx));
        })
        .detach();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        // Relative ages need one redraw per minute while this view is visible,
        // not a frame loop. Cached parents no longer provide incidental ticks.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(60))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.window_active && this.core.nav.read(cx).layout == "lanes" {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let initial_model_key = lane_model_key(core.nav.read(cx));
        // Lanes' controls are toolbar items; the shell draws them where the
        // user put them, by default in the bar over the board.
        let board = cx.entity().downgrade();
        core.toolbars.update(cx, |registry, cx| {
            for kind in [
                LanesControl::Grouping,
                LanesControl::LabelMode,
                LanesControl::Filters,
                LanesControl::CollapsedLanes,
            ] {
                registry.register(Rc::new(LanesItem {
                    kind,
                    board: board.clone(),
                }));
            }
            cx.notify();
        });
        Self {
            core,
            focus,
            h_scroll: ScrollHandle::new(),
            collapsed_scroll: ScrollHandle::new(),
            expanded: BTreeMap::new(),
            expanded_terminals: BTreeMap::new(),
            terminal_views: HashMap::new(),
            terminal_watch_key: 0,
            conversations: HashMap::new(),
            pending: None,
            focused_lane: None,
            focused_agent: None,
            draft_focused: false,
            draft_stash: None,
            draft: None,
            focus_composer: None,
            lists: HashMap::new(),
            columns: HashMap::new(),
            lanes_cache: RefCell::new(None),
            middle: None,
            pointer: Point::default(),
            width_slider,
            observed_nav_key: Some(initial_model_key),
            column_width: px(width),
            width_save_generation: 0,
            card_menu: None,
            expanded_widths: HashMap::new(),
            resizing: None,
            hold_board_length: false,
            reveal_column: None,
            renaming: None,
            arranged_cache: RefCell::new(HashMap::new()),
            arranged_revision: Cell::new(0),
            focused_row: None,
            empty_open: HashSet::new(),
            box_picker: None,
            focus_mode: None,
            rename_lane: None,
            subagents_open: HashMap::new(),
            label_pickers: HashMap::new(),
            relabel_pending: None,
            next_relabel: 0,
            window_active: window.is_window_active(),
            pointer_inside: false,
            view_membership: None,
            focus_check_needed: Cell::new(false),
            lane_drag: None,
            find: None,
            card_drop: None,
            find_index: RefCell::new(HashMap::new()),
        }
    }

    /// The board's lanes, current with the store. The model is patched from
    /// the store's change log since it was last read, so an agent update
    /// costs a card move rather than a rebuild of every lane.
    fn lanes(&self, cx: &App) -> LaneSet {
        let store = self.core.store.read(cx);
        let cursor = store.change_cursor();
        let nav = self.core.nav.read(cx);
        let mut slot = self.lanes_cache.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|cache| !cache.nav_key.matches_nav(nav))
        {
            *slot = None;
        }
        if let Some(cache) = slot.as_mut() {
            if cache.cursor == cursor {
                return cache.lanes.clone();
            }
            match store.changes_since(cache.cursor) {
                Some(changes) => cache.model.apply(
                    &store.fleet,
                    changes.map(|change| (change.host.as_str(), &change.change)),
                ),
                None => {
                    cache.model =
                        LaneModel::build(&store.fleet, cache.model.grouping(), cache.model.state())
                }
            }
            cache.cursor = cursor;
        } else {
            let nav = self.core.nav.read(cx);
            let state = LaneState {
                label_mode: nav.lane_label_mode,
                order: nav.lane_order.clone(),
                collapsed: nav.lane_collapsed.clone(),
            };
            *slot = Some(LaneCache {
                model: LaneModel::build(&store.fleet, nav.lane_grouping, &state),
                cursor,
                nav_key: lane_model_key(nav),
                lanes: Rc::new(Vec::new()),
                visible: Rc::new(Vec::new()),
                visible_filter: None,
                decor: HashMap::new(),
            });
        }
        let cache = slot.as_mut().expect("just filled");
        let lanes: LaneSet = Rc::new(cache.model.lanes().to_vec());
        if lanes.len() == cache.lanes.len()
            && lanes
                .iter()
                .zip(cache.lanes.iter())
                .all(|(a, b)| Rc::ptr_eq(a, b))
        {
            return cache.lanes.clone();
        }
        // Display data only for lanes that changed; the rest keep theirs.
        let colors = &self.core.nav.read(cx).lane_color_choices;
        let old: HashMap<&str, &Rc<Lane>> = cache
            .lanes
            .iter()
            .map(|lane| (lane.key.as_str(), lane))
            .collect();
        let mut decor = HashMap::with_capacity(lanes.len());
        for lane in lanes.iter() {
            let kept = old
                .get(lane.key.as_str())
                .filter(|previous| Rc::ptr_eq(previous, lane))
                .and_then(|_| cache.decor.get(&lane.key));
            decor.insert(
                lane.key.clone(),
                kept.copied().unwrap_or_else(|| lane_decor(lane, colors)),
            );
        }
        cache.decor = decor;
        cache.visible_filter = None;
        cache.lanes = lanes.clone();
        self.focus_check_needed.set(true);
        lanes
    }

    /// The lanes user actions act on. The same as [`Self::lanes`] now that
    /// nothing throttles it.
    fn current_lanes(&self, cx: &App) -> LaneSet {
        self.lanes(cx)
    }

    /// Displayed columns are cached separately; hidden lanes keep their model,
    /// saved order, draft and list state without occupying board scroll space.
    fn visible_lanes(&self, cx: &App) -> LaneSet {
        self.lanes(cx);
        let filter = &self.core.nav.read(cx).lane_filter;
        let mut slot = self.lanes_cache.borrow_mut();
        let cache = slot.as_mut().expect("lanes initialized");
        // Made again only when the lanes or the filter change.
        if cache.visible_filter.as_ref() != Some(filter) {
            let fleet = &self.core.store.read(cx).fleet;
            let labels = |host: &str, workspace: &str| -> Vec<String> {
                fleet
                    .workspace(host, workspace)
                    .map(|ws| {
                        ws.labels
                            .iter()
                            .map(|l| fleet::directory::label_key(l))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let shown = filter_lanes(&cache.lanes, filter, labels);
            cache.visible = displayed_lanes(&shown);
            cache.visible_filter = Some(filter.clone());
        }
        cache.visible.clone()
    }

    /// The selected workspace, as host and workspace id: the focused empty
    /// workspace, or the focused card's. Its box is outlined in the focused
    /// lane and tinted in the others.
    fn selected_workspace(&self, cx: &App) -> Option<(String, String)> {
        match &self.focused_row {
            Some(RowFocus::Workspace(host, workspace)) => {
                return Some((host.clone(), workspace.clone()));
            }
            Some(RowFocus::Fold) => return None,
            Some(RowFocus::Terminal(host, id)) => {
                let workspace = self.core.store.read(cx).terminal_workspace(host, id)?;
                return Some((host.clone(), workspace));
            }
            Some(RowFocus::Subagents(..)) | None => {}
        }
        if self.draft_focused {
            return None;
        }
        let (host, agent) = match &self.focused_row {
            Some(RowFocus::Subagents(host, agent)) => (host, agent),
            _ => self
                .focused_agent
                .as_ref()
                .map(|(host, agent)| (host, agent))?,
        };
        let workspace = self
            .core
            .store
            .read(cx)
            .fleet
            .agent(host, agent)?
            .workspace_id
            .clone()?;
        Some((host.clone(), workspace))
    }

    fn row_generation(&self, lane: &Lane, cx: &App) -> (u64, u64) {
        use std::hash::{Hash, Hasher};
        let mut folds = std::collections::hash_map::DefaultHasher::new();
        for card in &lane.cards {
            self.subagents_open
                .get(&card.host)
                .is_some_and(|ids| ids.contains(&card.agent_id))
                .hash(&mut folds);
        }
        let mut terminals = std::collections::hash_map::DefaultHasher::new();
        let store = self.core.store.read(cx);
        for workspace in &lane.workspaces {
            store
                .workspace_terminals_revision(&workspace.host, &workspace.workspace_id)
                .hash(&mut terminals);
        }
        (folds.finish(), terminals.finish())
    }

    /// A grouped lane's rows, with this board's folds. See [`lane_rows`].
    fn rows(&self, lane: &Lane, box_single: bool, cx: &App) -> Vec<Row> {
        let store = self.core.store.read(cx);
        lane_rows(
            lane,
            box_single,
            self.empty_open.contains(&lane.key),
            &|card: &LaneCard| {
                self.subagents_open
                    .get(&card.host)
                    .is_some_and(|ids| ids.contains(&card.agent_id))
            },
            &|host, workspace| store.workspace_terminal_ids(host, workspace).count(),
        )
    }

    fn toggle_subagents(&mut self, lane: &str, host: &str, agent: &str, cx: &mut Context<Self>) {
        let ids = self.subagents_open.entry(host.to_string()).or_default();
        if !ids.remove(agent) {
            ids.insert(agent.to_string());
        }
        self.focus_row(
            lane,
            RowFocus::Subagents(host.to_string(), agent.to_string()),
            cx,
        );
    }

    /// Lanes in the order they are shown and navigated, each workspace's
    /// cards together. See [`group_workspaces`].
    fn arranged(&self, lanes: &LaneSet, cx: &App) -> LaneSet {
        let nav = self.core.nav.read(cx);
        let mut cache = self.arranged_cache.borrow_mut();
        // Pins and card order changed: every lane is grouped again.
        if self.arranged_revision.replace(nav.ordering_revision) != nav.ordering_revision {
            cache.clear();
        }
        let placement = Placement {
            pinned: &nav.pinned_workspaces,
            order: &nav.card_order,
        };
        let current = self.lanes_cache.borrow();
        if let Some(current) = current.as_ref() {
            let keys: HashSet<_> = current.lanes.iter().map(|lane| &lane.key).collect();
            cache.retain(|key, _| keys.contains(key));
        }
        let mut rearranged = false;
        let out: Vec<Rc<Lane>> = lanes
            .iter()
            .map(|lane| {
                if let Some((source, grouped)) = cache.get(&lane.key)
                    && Rc::ptr_eq(source, lane)
                {
                    rearranged |= !Rc::ptr_eq(grouped, lane);
                    return grouped.clone();
                }
                let grouped =
                    group_workspaces(lane, &placement).map_or_else(|| lane.clone(), Rc::new);
                rearranged |= !Rc::ptr_eq(&grouped, lane);
                cache.insert(lane.key.clone(), (lane.clone(), grouped.clone()));
                grouped
            })
            .collect();
        if rearranged {
            Rc::new(out)
        } else {
            lanes.clone()
        }
    }

    fn reveal_lane(&mut self, key: &str, cx: &mut Context<Self>) {
        self.core
            .nav
            .update(cx, |nav, cx| nav.set_lane_collapsed(key, false, cx));
        let lanes = self.visible_lanes(cx);
        if let Some(index) = lanes.iter().position(|lane| lane.key == key) {
            self.h_scroll.scroll_to_item(index);
        }
        cx.notify();
    }

    fn restore_lane(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.reveal_lane(key, cx);
        let lanes = self.visible_lanes(cx);
        if let Some(lane) = lanes.iter().find(|lane| lane.key == key) {
            self.focused_lane = Some(key.to_string());
            self.focused_agent = self.expanded.get(key).cloned().or_else(|| {
                lane.cards
                    .first()
                    .map(|card| (card.host.clone(), card.agent_id.clone()))
            });
            self.draft_focused = self.draft.as_ref().is_some_and(|draft| draft.lane == key);
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn hide_lane(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let lanes = self.current_lanes(cx);
        if let Some(index) = lanes
            .iter()
            .position(|lane| lane.key == key && !lane.collapsed)
        {
            let next = next_visible_lane(&lanes, index);
            self.core
                .nav
                .update(cx, |nav, cx| nav.set_lane_collapsed(key, true, cx));
            if self.focused_lane.as_deref() == Some(key) {
                self.focused_lane = next;
                self.focused_agent = None;
                self.draft_focused = false;
                self.move_column(0, cx);
            }
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn prune_expanded(&mut self, cx: &mut Context<Self>) {
        self.reconcile_expansions(None, cx);
    }

    fn reconcile_expansions(
        &mut self,
        requested: Option<(&(String, String), &str)>,
        cx: &mut Context<Self>,
    ) {
        if self.expanded.is_empty() && self.pending.is_none() {
            self.prune_label_pickers(cx);
            self.prune_conversations(cx);
            self.sync_pins(cx);
            return;
        }
        let lanes = self.current_lanes(cx);
        if let Some((lane, host, agent)) = self.pending.clone()
            && let Some(index) = placement::destination(&lanes, &lane, &host, &agent, &[], false)
        {
            let found = &lanes[index];
            if found.key != lane {
                self.expanded.remove(&lane);
                self.expanded
                    .insert(found.key.clone(), (host.clone(), agent.clone()));
            }
            self.pending = None;
        }
        let pending = self.pending.clone();
        let job = self.relabel_pending.as_ref();
        let protected = self.protected_agents();
        // Compute the whole new map before replacing it: sequential moves lose
        // an expansion when two workspaces exchange lanes.
        let relocated = placement::relocate(
            &self.expanded,
            &lanes,
            &protected,
            self.focused_agent.as_ref(),
            requested,
        );
        self.expanded = relocated;
        self.expanded.retain(|lane, (host, agent)| {
            lanes.iter().any(|item| {
                &item.key == lane
                    && item
                        .cards
                        .iter()
                        .any(|card| &card.host == host && &card.agent_id == agent)
            }) || pending
                .as_ref()
                .is_some_and(|(key, pending_host, pending_agent)| {
                    key == lane && pending_host == host && pending_agent == agent
                })
                || job.is_some_and(|job| &job.host == host && job.agent.as_ref() == Some(agent))
        });
        self.prune_conversations(cx);
        self.prune_label_pickers(cx);
        self.sync_pins(cx);
    }

    fn view_membership(&self, cx: &App) -> placement::ViewMembership {
        placement::view_membership(
            &self.core.store.read(cx).fleet,
            self.focused_agent.as_ref(),
            self.draft
                .as_ref()
                .and_then(|draft| draft.workspace.as_ref()),
        )
    }

    fn protected_agents(&self) -> Vec<(String, String)> {
        self.relabel_pending
            .as_ref()
            .and_then(|job| Some((job.host.clone(), job.agent.clone()?)))
            .into_iter()
            .chain(
                self.pending
                    .as_ref()
                    .map(|(_, host, agent)| (host.clone(), agent.clone())),
            )
            .collect()
    }

    fn reconcile_view(&mut self, follow: bool, cx: &mut Context<Self>) {
        self.reconcile_view_at(follow, None, cx);
    }

    fn reconcile_view_at(&mut self, follow: bool, preferred: Option<&str>, cx: &mut Context<Self>) {
        // Batch relabeling owns its final relocation; do not follow intermediate
        // label pushes for that focused conversation.
        let protected_focus = self
            .focused_agent
            .as_ref()
            .is_some_and(|agent| self.protected_agents().contains(agent));
        let has_active_view = self.draft.is_some() || self.focused_lane.is_some();
        if !has_active_view || protected_focus {
            self.ensure_visible_focus(cx);
            self.view_membership = Some(self.view_membership(cx));
            return;
        }
        let lanes = self.current_lanes(cx);
        let outcome = placement::reconcile(
            &lanes,
            placement::ViewInputs {
                source: self.focused_lane.as_deref(),
                preferred,
                focused: self.focused_agent.as_ref(),
                draft: self
                    .draft
                    .as_ref()
                    .map(|draft| (draft.lane.as_str(), draft.workspace.as_ref())),
                draft_focused: self.draft_focused,
                follow,
            },
        );
        if let Some(draft) = &mut self.draft
            && let Some(lane) = outcome.draft_lane
        {
            draft.lane = lane;
        }
        self.draft_focused = outcome.draft_focused;
        self.focused_lane = outcome.focused_lane;
        self.focused_agent = outcome.focused_agent;
        self.ensure_visible_focus(cx);
        if let Some(index) = outcome.scroll_to {
            // Reactive reconciliation must respect explicitly hidden lanes.
            // Only restore actions and explicit relabel follow reveal them.
            let visible = self.visible_lanes(cx);
            if self.focused_lane.as_deref() == Some(lanes[index].key.as_str())
                && let Some(index) = visible.iter().position(|lane| lane.key == lanes[index].key)
            {
                self.h_scroll.scroll_to_item(index);
            }
        }
        self.view_membership = Some(self.view_membership(cx));
    }

    /// Reconciliation may move a conversation to a hidden lane without opening
    /// it. Keep its view state there, but keyboard focus stays on the board.
    fn ensure_visible_focus(&mut self, cx: &mut Context<Self>) {
        let lanes = self.current_lanes(cx);
        let resolved = visible_focus_lane(&lanes, self.focused_lane.as_deref());
        if resolved != self.focused_lane {
            self.focused_lane = resolved;
            self.focused_agent = None;
            self.draft_focused = false;
            self.move_column(0, cx);
        }
    }

    fn prune_label_pickers(&mut self, cx: &App) {
        self.label_pickers
            .retain(|(lane, host, agent, workspace), _| {
                self.expanded
                    .get(lane)
                    .is_some_and(|pair| pair == &(host.clone(), agent.clone()))
                    && self
                        .core
                        .store
                        .read(cx)
                        .fleet
                        .agent(host, agent)
                        .is_some_and(|agent| agent.workspace_id.as_ref() == Some(workspace))
                    || self.relabel_pending.as_ref().is_some_and(|job| {
                        &job.source == lane
                            && &job.host == host
                            && job.agent.as_ref() == Some(agent)
                            && &job.workspace == workspace
                    })
            });
    }

    fn prune_conversations(&mut self, cx: &App) {
        let keep: HashSet<(String, String)> = self.expanded.values().cloned().collect();
        let released: Vec<_> = self
            .conversations
            .keys()
            .filter(|key| !keep.contains(*key))
            .cloned()
            .collect();
        for key in released {
            self.release_conversation(&key, cx);
        }
    }

    /// Unsent text and attachments outlive their expansion. Every close,
    /// replacement, and directory-driven relocation uses the same rule.
    fn release_conversation(&mut self, key: &(String, String), cx: &App) {
        if self.expanded.values().any(|expanded| expanded == key) {
            return;
        }
        if self
            .conversations
            .get(key)
            .is_some_and(|view| view.read(cx).composer_is_blank(cx))
        {
            self.conversations.remove(key);
        }
    }

    fn sync_pins(&self, cx: &mut Context<Self>) {
        let pins: BTreeSet<_> = self
            .expanded
            .values()
            .cloned()
            .chain(
                self.conversations
                    .iter()
                    .filter(|(_, view)| !view.read(cx).composer_is_blank(cx))
                    .map(|(key, _)| key.clone()),
            )
            // Keep drafts for a temporarily missing host without repeatedly
            // subscribing to a deleted or archived agent.
            .filter(|(host, agent)| {
                self.core
                    .store
                    .read(cx)
                    .fleet
                    .agent(host, agent)
                    .is_some_and(|agent| agent.archived_at.is_none())
            })
            .collect();
        self.core
            .nav
            .update(cx, |nav, cx| nav.set_pinned_sessions(pins, cx));
        let terminals: BTreeSet<_> = self.expanded_terminals.values().cloned().collect();
        self.core
            .nav
            .update(cx, |nav, cx| nav.set_pinned_terminals(terminals, cx));
    }

    fn label_picker(
        &mut self,
        lane: &str,
        host: &str,
        agent: &str,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<LabelPicker> {
        let key = (
            lane.to_string(),
            host.to_string(),
            agent.to_string(),
            workspace.to_string(),
        );
        if let Some(picker) = self.label_pickers.get(&key) {
            return picker.clone();
        }
        let picker = self.new_label_picker(lane, host, Some(agent), workspace, window, cx);
        self.label_pickers.insert(key, picker.clone());
        picker
    }

    /// A picker for `workspace`'s labels that relabels it from `lane`,
    /// following `agent`'s card when there is one.
    fn new_label_picker(
        &mut self,
        lane: &str,
        host: &str,
        agent: Option<&str>,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<LabelPicker> {
        let core = self.core.clone();
        let picker = cx.new(|cx| LabelPicker::new(core, host, workspace, window, cx));
        cx.observe(&picker, |_, _, cx| cx.notify()).detach();
        let source = lane.to_string();
        let host = host.to_string();
        let agent = agent.map(str::to_string);
        let workspace = workspace.to_string();
        cx.subscribe(&picker, move |this, _, event: &ApplyLabels, cx| {
            this.start_relabel(
                &source,
                &host,
                agent.as_deref(),
                &workspace,
                event.0.clone(),
                cx,
            );
        })
        .detach();
        picker
    }

    /// Opens the label picker for a workspace box's header.
    fn open_box_picker(
        &mut self,
        lane: &str,
        host: &str,
        agent: Option<&str>,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picker = self.new_label_picker(lane, host, agent, workspace, window, cx);
        picker.update(cx, |picker, cx| picker.set_open(true, window, cx));
        // The header shows the picker while it is open.
        cx.observe(&picker, |this, picker, cx| {
            if !picker.read(cx).is_open()
                && this
                    .box_picker
                    .as_ref()
                    .is_some_and(|open| open.picker == picker)
            {
                this.box_picker = None;
                cx.notify();
            }
        })
        .detach();
        self.box_picker = Some(BoxPicker {
            lane: lane.to_string(),
            host: host.to_string(),
            workspace: workspace.to_string(),
            picker,
        });
        cx.notify();
    }

    fn start_relabel(
        &mut self,
        source: &str,
        host: &str,
        agent: Option<&str>,
        workspace: &str,
        changes: Vec<LabelChange>,
        cx: &mut Context<Self>,
    ) {
        if self.relabel_pending.is_some() {
            self.core.store.update(cx, |store, cx| {
                store.toast(
                    "Another label update is still in progress. Please try again.",
                    cx,
                )
            });
            return;
        }
        if let Some(agent) = agent
            && !self
                .core
                .store
                .read(cx)
                .fleet
                .agent(host, agent)
                .is_some_and(|agent| agent.workspace_id.as_deref() == Some(workspace))
        {
            self.core.store.update(cx, |store, cx| store.toast("This card moved to a different workspace. Reopen Relabel to update its labels.", cx));
            return;
        }
        self.next_relabel += 1;
        let token = self.next_relabel;
        let known = self
            .current_lanes(cx)
            .iter()
            .map(|lane| lane.key.clone())
            .collect();
        self.relabel_pending = Some(RelabelPending {
            known,
            source: source.into(),
            host: host.into(),
            agent: agent.map(str::to_string),
            workspace: workspace.into(),
            progress: placement::Progress::Writing,
            follow: self.core.nav.read(cx).follow_relabel,
            token,
        });
        let task = self
            .core
            .store
            .read(cx)
            .set_workspace_labels(host, workspace, changes, cx);
        cx.spawn(async move |this, cx| {
            let batch = task.await;
            let _ = this.update(cx, |this, cx| this.label_batch_done(token, batch, cx));
        })
        .detach();
        cx.notify();
    }

    fn label_batch_done(&mut self, token: u64, batch: LabelBatch, cx: &mut Context<Self>) {
        let Some(job) = &mut self.relabel_pending else {
            return;
        };
        if job.token != token {
            return;
        }
        job.progress = placement::Progress::Acknowledged(batch.applied);
        if let Some(error) = batch.error {
            self.core
                .store
                .update(cx, |store, cx| store.toast(error, cx));
        }
        self.finish_relabel(false, cx);
        if self.relabel_pending.is_some() {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(10)).await;
                let _ = this.update(cx, |this, cx| {
                    if this.relabel_pending.as_ref().is_some_and(|job| job.token == token) {
                        this.core.store.update(cx, |store, cx| store.toast(
                            "Labels were acknowledged, but the updated workspace has not arrived yet.", cx
                        ));
                        this.finish_relabel(true, cx);
                    }
                });
            }).detach();
        }
        cx.notify();
    }

    fn finish_relabel(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(job) = &self.relabel_pending else {
            return;
        };
        let labels = self
            .core
            .store
            .read(cx)
            .fleet
            .workspace(&job.host, &job.workspace)
            .map(|workspace| workspace.labels.as_slice());
        if !job.progress.can_finish(labels, force) {
            return;
        }
        let job = self.relabel_pending.take().unwrap();
        self.place_new_lanes(&job.known, &job.source, cx);
        let Some(job_agent) = job.agent.clone() else {
            // A workspace relabeled from its box has no card to follow.
            self.reconcile_expansions(None, cx);
            self.reconcile_view(false, cx);
            self.sync_pins(cx);
            cx.notify();
            return;
        };
        let lanes = self.current_lanes(cx);
        let label_grouping = self.core.nav.read(cx).lane_grouping == LaneGrouping::Label;
        let destination = placement::destination(
            &lanes,
            &job.source,
            &job.host,
            &job_agent,
            job.progress.changes(),
            label_grouping,
        );
        let agent = (job.host.clone(), job_agent);
        let was_expanded = self.expanded.values().any(|pair| pair == &agent);
        let requested = destination
            .filter(|_| was_expanded)
            .map(|index| (&agent, lanes[index].key.as_str()));
        self.reconcile_expansions(requested, cx);
        if let Some(index) = destination.filter(|_| was_expanded) {
            let lane = &lanes[index];
            if job.follow {
                self.focused_lane = Some(lane.key.clone());
                self.focused_agent = Some(agent.clone());
                self.focused_row = None;
                self.reveal_lane(&lane.key, cx);
            }
        }
        let preferred = destination
            .filter(|_| was_expanded && self.focused_agent.as_ref() == Some(&agent))
            .map(|index| lanes[index].key.as_str());
        self.reconcile_view_at(job.follow, preferred, cx);
        self.sync_pins(cx);
        cx.notify();
    }

    /// Lanes a relabel created appear right after the lane it was made from,
    /// rather than wherever a new lane sorts. This fixes the board's order
    /// as it stands, as moving a lane by hand does.
    fn place_new_lanes(&mut self, known: &[String], source: &str, cx: &mut Context<Self>) {
        let current: Vec<String> = self
            .current_lanes(cx)
            .iter()
            .map(|lane| lane.key.clone())
            .collect();
        let Some(order) = order_with_new_lanes(known, source, &current) else {
            return;
        };
        self.core.nav.update(cx, |nav, cx| {
            nav.lane_order = order;
            nav.save();
            cx.notify();
        });
    }

    fn conversation(
        &mut self,
        host: &str,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Conversation> {
        let key = (host.to_string(), agent.to_string());
        if let Some(view) = self.conversations.get(&key) {
            return view.clone();
        }
        let core = self.core.clone();
        let view = cx.new(|cx| {
            let mut view = Conversation::new(core, host, agent, window, cx);
            // As wide as its lane: the lane's width is the user's to set.
            view.set_wide(true, cx);
            view
        });
        // A Paseo subagent opened from the track expands its own card.
        cx.subscribe_in(
            &view,
            window,
            |this, _, event: &ConversationEvent, window, cx| match event {
                ConversationEvent::OpenAgent { host, agent } => {
                    this.reveal_agent(host, agent, window, cx)
                }
            },
        )
        .detach();
        self.conversations.insert(key, view.clone());
        view
    }

    fn expand(
        &mut self,
        lane: &str,
        host: &str,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An agent opens in place of a terminal open in the lane.
        self.expanded_terminals.remove(lane);
        let previous = self
            .expanded
            .insert(lane.to_string(), (host.to_string(), agent.to_string()));
        if let Some(previous) = previous {
            self.release_conversation(&previous, cx);
        }
        self.prune_label_pickers(cx);
        self.focused_lane = Some(lane.to_string());
        self.focused_agent = Some((host.to_string(), agent.to_string()));
        self.focused_row = None;
        self.draft_focused = false;
        self.reveal_column = Some(lane.to_string());
        self.sync_pins(cx);
        let view = self.conversation(host, agent, window, cx);
        view.update(cx, |conversation, cx| {
            conversation.focus_composer(window, cx)
        });
        cx.notify();
    }

    fn collapse_lane(&mut self, lane: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((host, agent)) = self.expanded.remove(lane) {
            self.release_conversation(&(host, agent), cx);
        }
        if self.pending.as_ref().is_some_and(|(key, _, _)| key == lane) {
            self.pending = None;
        }
        self.prune_label_pickers(cx);
        self.sync_pins(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Edits an expanded card's title in place.
    fn start_rename(
        &mut self,
        lane: &str,
        host: &str,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let editor = cx.new(|cx| AgentRename::new(&core, host, agent, window, cx));
        self.rename_lane = Some(lane.to_string());
        let done = cx.subscribe(&editor, |this, _, _: &RenameDone, cx| {
            this.renaming = None;
            cx.notify();
        });
        self.renaming = Some((editor, done));
        cx.notify();
    }

    /// The editor for `host`/`agent`'s title in `lane`, if it is being
    /// renamed there.
    fn rename_editor(
        &self,
        lane: &str,
        host: &str,
        agent: &str,
        cx: &App,
    ) -> Option<Entity<AgentRename>> {
        self.renaming
            .as_ref()
            .map(|(editor, _)| editor)
            .filter(|editor| editor.read(cx).edits(host, agent))
            .filter(|_| self.rename_lane.as_deref() == Some(lane))
            .cloned()
    }

    /// Shows `agent`'s card large and centered over the dimmed board,
    /// expanding it first, or brings it back to its lane.
    fn toggle_focus_mode(
        &mut self,
        lane: &str,
        host: &str,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = (lane.to_string(), host.to_string(), agent.to_string());
        if self.focus_mode.as_ref() == Some(&target) {
            self.focus_mode = None;
            self.focus.focus(window, cx);
            cx.notify();
            return;
        }
        if self.expanded.get(lane) != Some(&(host.to_string(), agent.to_string())) {
            self.expand(lane, host, agent, window, cx);
        }
        self.focus_mode = Some(target);
        if let Some(view) = self
            .conversations
            .get(&(host.to_string(), agent.to_string()))
        {
            view.update(cx, |conversation, cx| {
                conversation.focus_composer(window, cx)
            });
        }
        cx.notify();
    }

    /// Whether `agent`'s card in `lane` is shown in focus mode, so its lane
    /// shows where it went instead.
    fn in_focus_mode(&self, lane: &str, host: &str, agent: &str) -> bool {
        self.focus_mode
            .as_ref()
            .is_some_and(|(l, h, a)| l == lane && h == host && a == agent)
    }

    /// The focused card over a dimmed board. A click on the dimmed board or
    /// Escape brings it back. GPUI cannot blur what is behind it, so the
    /// board is only darkened.
    fn focus_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (lane_key, host, agent) = self.focus_mode.clone()?;
        if self.expanded.get(&lane_key) != Some(&(host.clone(), agent.clone())) {
            // Its card was collapsed or moved away.
            self.focus_mode = None;
            return None;
        }
        let lanes = self.arranged(&self.current_lanes(cx), cx);
        let lane = lanes.iter().find(|lane| lane.key == lane_key)?.clone();
        let style = self.lane_style(&lane, cx);
        let card = self.expanded_card(&lane, &host, &agent, style, true, window, cx);
        let viewport = window.viewport_size();
        let width = (viewport.width * 0.72)
            .max(px(520.))
            .min(px(1100.))
            .min(viewport.width - px(32.));
        let height = (viewport.height * 0.86).min(viewport.height - px(32.));
        Some(
            div()
                .id("lanes-focus")
                .absolute()
                .inset_0()
                .occlude()
                .bg(gpui_kit::gpui::hsla(0., 0., 0., 0.62))
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.focus_mode = None;
                        this.focus.focus(window, cx);
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .id("lanes-focus-card")
                        .w(width)
                        .h(height)
                        .flex()
                        .flex_col()
                        .rounded(px(BOX_RADIUS))
                        .shadow_lg()
                        // Clicks on the card stay on the card.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(card),
                )
                .into_any_element(),
        )
    }

    /// Where a card in focus mode sits in its lane while it is away.
    fn focus_placeholder(
        &self,
        lane: &str,
        host: &str,
        agent: &str,
        framed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let (lane_key, host, agent) = (lane.to_string(), host.to_string(), agent.to_string());
        div()
            .id(eid_parts(
                "lane-focus-placeholder",
                &[&lane_key, &host, &agent],
            ))
            .h(px(56.))
            .flex_shrink_0()
            .when(framed, |d| {
                d.rounded(px(BOX_RADIUS))
                    .border_1()
                    .border_color(p.border_accent)
            })
            .when(!framed, |d| d.border_t_1().border_color(p.border))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .text_size(size::sm())
            .text_color(p.muted)
            .cursor_pointer()
            .hover(move |d| d.bg(p.surface2))
            .child(ui::icon("maximize-2", 12., p.muted))
            .child("Open in focus · Esc to bring it back")
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.toggle_focus_mode(&lane_key, &host, &agent, window, cx);
            }))
            .into_any_element()
    }

    /// Opens a terminal in its lane, in place of an expanded agent.
    fn expand_terminal(
        &mut self,
        lane: &str,
        host: &str,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = self.expanded.remove(lane) {
            self.release_conversation(&previous, cx);
        }
        let pair = (host.to_string(), id.to_string());
        self.expanded_terminals
            .insert(lane.to_string(), pair.clone());
        self.focused_lane = Some(lane.to_string());
        self.focused_row = Some(RowFocus::Terminal(host.to_string(), id.to_string()));
        self.draft_focused = false;
        self.reveal_column = Some(lane.to_string());
        self.sync_pins(cx);
        let view = self.terminal_view(host, id, window, cx);
        view.update(cx, |view, cx| view.focus(window, cx));
        cx.notify();
    }

    fn collapse_terminal(&mut self, lane: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pair) = self.expanded_terminals.remove(lane)
            && !self.expanded_terminals.values().any(|other| other == &pair)
        {
            self.terminal_views.remove(&pair);
        }
        self.sync_pins(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn terminal_view(
        &mut self,
        host: &str,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        let key = (host.to_string(), id.to_string());
        if let Some(view) = self.terminal_views.get(&key) {
            return view.clone();
        }
        let core = self.core.clone();
        let view = cx.new(|cx| TerminalView::new(core, host, id, window, cx));
        self.terminal_views.insert(key, view.clone());
        view
    }

    /// Asks the store to list the terminals of the workspaces in lanes near
    /// the view, once per set; workspaces outside that set release their terminal-list subscription.
    fn watch_terminals(&mut self, workspaces: Vec<(String, String)>, cx: &mut Context<Self>) {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        workspaces.hash(&mut hash);
        let key = hash.finish();
        if key == self.terminal_watch_key {
            return;
        }
        self.terminal_watch_key = key;
        let store = self.core.store.read(cx);
        let wanted: Vec<TerminalWatch> = workspaces
            .into_iter()
            .filter_map(|(host, workspace)| {
                let cwd = store
                    .fleet
                    .workspace(&host, &workspace)?
                    .directory()
                    .to_string();
                (!cwd.is_empty()).then_some(TerminalWatch {
                    host,
                    workspace,
                    cwd,
                })
            })
            .collect();
        let store = self.core.store.clone();
        let nav = self.core.nav.clone();
        cx.defer(move |cx| {
            if nav.read(cx).layout == "lanes" {
                store.update(cx, |store, cx| {
                    store.ensure_terminal_watches("lanes", wanted, cx)
                });
            }
        });
    }

    /// Makes a terminal in a workspace and opens it in `lane`.
    fn new_terminal(
        &mut self,
        lane: &str,
        host: &str,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cwd) = self
            .core
            .store
            .read(cx)
            .fleet
            .workspace(host, workspace)
            .map(|record| record.directory().to_string())
            .filter(|cwd| !cwd.is_empty())
        else {
            return;
        };
        let task = self.core.store.update(cx, |store, cx| {
            store.create_terminal(host, workspace, &cwd, cx)
        });
        let (lane, host) = (lane.to_string(), host.to_string());
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| match result {
                    Ok(id) => this.expand_terminal(&lane, &host, &id, window, cx),
                    Err(error) => this
                        .core
                        .store
                        .update(cx, |store, cx| store.toast(error, cx)),
                });
            });
        })
        .detach();
    }

    /// Edits a workspace's title in place, from its box.
    fn start_workspace_rename(
        &mut self,
        lane: &str,
        host: &str,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let editor = cx.new(|cx| AgentRename::workspace(&core, host, workspace, window, cx));
        self.rename_lane = Some(lane.to_string());
        let done = cx.subscribe(&editor, |this, _, _: &RenameDone, cx| {
            this.renaming = None;
            cx.notify();
        });
        self.renaming = Some((editor, done));
        cx.notify();
    }

    /// A workspace's name in its box: its rename editor while it is being
    /// renamed, else the name, which a click starts renaming.
    fn workspace_name(
        &self,
        lane: &str,
        host: &str,
        workspace: &str,
        name: String,
        color: Rgba,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        if let Some(editor) = self
            .renaming
            .as_ref()
            .map(|(editor, _)| editor)
            .filter(|editor| editor.read(cx).edits_workspace(host, workspace))
            .filter(|_| self.rename_lane.as_deref() == Some(lane))
        {
            return editor.clone().into_any_element();
        }
        let (lane_key, host, workspace) =
            (lane.to_string(), host.to_string(), workspace.to_string());
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .child(
                div()
                    .id(eid_parts("lane-box-name", &[lane, &host, &workspace]))
                    .min_w(px(0.))
                    .truncate()
                    .px(px(3.))
                    .mx(px(-3.))
                    .rounded(px(4.))
                    .text_color(color)
                    .cursor(CursorStyle::IBeam)
                    .hover(move |style| style.bg(p.surface2).text_color(p.foreground))
                    .child(name)
                    .tooltip(|window, cx| Tooltip::new("Rename workspace").build(window, cx))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.start_workspace_rename(&lane_key, &host, &workspace, window, cx);
                    })),
            )
            .into_any_element()
    }

    /// Collapses every expanded card.
    fn collapse_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.expanded_terminals.clear();
        self.terminal_views.clear();
        for (_, pair) in std::mem::take(&mut self.expanded) {
            self.release_conversation(&pair, cx);
        }
        self.pending = None;
        self.prune_label_pickers(cx);
        self.sync_pins(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Focuses an agent's card and expands it, showing its lane if it was
    /// collapsed to the tabs and scrolling it into view. Prefers the lane
    /// where it is already expanded, then the focused lane, then the first
    /// lane holding its card.
    fn reveal_agent(
        &mut self,
        host: &str,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pair = (host.to_string(), agent.to_string());
        let lanes = self.current_lanes(cx);
        let holds = |lane: &Rc<Lane>| {
            lane.cards
                .iter()
                .any(|card| card.host == host && card.agent_id == agent)
        };
        let lane = lanes
            .iter()
            .find(|lane| self.expanded.get(&lane.key) == Some(&pair))
            .or_else(|| {
                lanes.iter().find(|lane| {
                    Some(lane.key.as_str()) == self.focused_lane.as_deref() && holds(lane)
                })
            })
            .or_else(|| lanes.iter().find(|lane| holds(lane)));
        let Some(lane) = lane else {
            return;
        };
        let (key, collapsed) = (lane.key.clone(), lane.collapsed);
        if collapsed {
            self.reveal_lane(&key, cx);
        }
        if self.expanded.get(&key) == Some(&pair) {
            self.focused_lane = Some(key.clone());
            self.focused_agent = Some(pair);
            self.focused_row = None;
            self.draft_focused = false;
            self.reveal_column = Some(key);
            cx.notify();
        } else {
            self.expand(&key, host, agent, window, cx);
        }
    }

    fn open_draft(&mut self, lane: &Lane, window: &mut Window, cx: &mut Context<Self>) {
        self.open_draft_in(lane, None, window, cx);
    }

    /// Opens a new agent draft in `lane`, in `workspace` when given, else
    /// in the lane's first workspace. Without a workspace, a draft already
    /// open in the lane closes instead.
    fn open_draft_in(
        &mut self,
        lane: &Lane,
        workspace: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = self.draft.as_mut().filter(|draft| draft.lane == lane.key) {
            if let Some((host, id)) = workspace {
                self.core
                    .store
                    .update(cx, |store, cx| store.ensure_providers(&host, cx));
                draft.workspace = Some((host, id));
                draft.new_workspace = None;
                draft.input.update(cx, |input, cx| input.focus(window, cx));
                self.draft_focused = true;
                self.focused_row = None;
            } else {
                self.draft = None;
                self.draft_focused = false;
            }
            cx.notify();
            return;
        }
        if lane.collapsed {
            self.reveal_lane(&lane.key, cx);
        }
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
                .submit_on_enter(true)
                .placeholder("What should the agent do?")
        });
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.create_draft(window, cx);
            }
        })
        .detach();
        let workspace = workspace.or_else(|| {
            lane.workspaces
                .first()
                .map(|workspace| (workspace.host.clone(), workspace.workspace_id.clone()))
        });
        if let Some((host, _)) = &workspace {
            let host = host.clone();
            self.core
                .store
                .update(cx, |store, cx| store.ensure_providers(&host, cx));
        }
        // What was typed in a draft closed without creating comes back.
        if let Some((_, text)) = self
            .draft_stash
            .take_if(|(stashed, _)| *stashed == lane.key)
        {
            input.update(cx, |input, cx| input.set_value(text, window, cx));
        }
        input.update(cx, |input, cx| input.focus(window, cx));
        self.focused_lane = Some(lane.key.clone());
        self.draft_focused = true;
        self.focused_row = None;
        // A lane with no workspace can only make one.
        let new_workspace = workspace
            .is_none()
            .then(|| self.default_new_workspace(lane, None, cx))
            .flatten();
        self.draft = Some(DraftState {
            lane: lane.key.clone(),
            input,
            workspace,
            new_workspace,
            provider: None,
            model: None,
            thinking: None,
            creating: false,
            error: None,
            bounds: Rc::new(Cell::new(None)),
        });
        cx.notify();
    }

    /// Closes the draft without creating, keeping what was typed for the
    /// next draft in its lane. Not while it is being created.
    fn dismiss_draft(&mut self, refocus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.draft else {
            return;
        };
        if draft.creating {
            return;
        }
        let text = draft.input.read(cx).value().to_string();
        self.draft_stash = (!text.trim().is_empty()).then(|| (draft.lane.clone(), text));
        self.draft = None;
        self.draft_focused = false;
        if refocus {
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    /// The host the draft's agent will run on.
    fn draft_host(&self) -> Option<String> {
        let draft = self.draft.as_ref()?;
        match &draft.new_workspace {
            Some(new) => Some(new.host.clone()),
            None => draft.workspace.as_ref().map(|(host, _)| host.clone()),
        }
    }

    /// Projects a new workspace can go in, as host, project id and name:
    /// those this lane's workspaces are in first, then every other project
    /// on an online host.
    fn project_choices(&self, lane: &Lane, cx: &App) -> Vec<(String, String, String)> {
        let fleet = &self.core.store.read(cx).fleet;
        let name = |host: &str, id: &str| {
            let project = fleet.host(host)?.directory.projects.get(id)?;
            let name = project
                .project_custom_name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| project.project_display_name.clone());
            Some(format!("{name} · {host}"))
        };
        let mut choices: Vec<(String, String, String)> = Vec::new();
        for workspace in &lane.workspaces {
            let Some(id) = fleet
                .workspace(&workspace.host, &workspace.workspace_id)
                .map(|ws| ws.project_id.clone())
            else {
                continue;
            };
            if !choices
                .iter()
                .any(|(host, project, _)| *host == workspace.host && *project == id)
                && let Some(label) = name(&workspace.host, &id)
            {
                choices.push((workspace.host.clone(), id, label));
            }
        }
        let mut others: Vec<(String, String, String)> = fleet
            .hosts
            .iter()
            .filter(|host| host.connection.is_online())
            .flat_map(|host| {
                host.directory
                    .projects
                    .keys()
                    .map(move |id| (host.name().to_string(), id.clone()))
            })
            .filter(|(host, id)| !choices.iter().any(|(h, p, _)| h == host && p == id))
            .filter_map(|(host, id)| {
                let label = name(&host, &id)?;
                Some((host, id, label))
            })
            .collect();
        others.sort_by_key(|a| a.2.to_lowercase());
        choices.extend(others);
        choices
    }

    /// Where a new workspace goes unless the user picks: the project of the
    /// workspace the draft was in, else the lane's first project, else any.
    fn default_new_workspace(
        &self,
        lane: &Lane,
        from: Option<&(String, String)>,
        cx: &App,
    ) -> Option<NewWorkspace> {
        let fleet = &self.core.store.read(cx).fleet;
        let from = from.and_then(|(host, id)| {
            let project = fleet.workspace(host, id)?.project_id.clone();
            Some((host.clone(), project))
        });
        let (host, project) = from.or_else(|| {
            self.project_choices(lane, cx)
                .into_iter()
                .next()
                .map(|(host, project, _)| (host, project))
        })?;
        Some(NewWorkspace {
            host,
            project,
            worktree: true,
        })
    }

    fn create_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.draft else {
            return;
        };
        if draft.creating {
            return;
        }
        let prompt = draft.input.read(cx).value().trim().to_string();
        if let Some(new) = draft.new_workspace.clone() {
            self.create_in_new_workspace(new, prompt, cx);
            return;
        }
        let Some((host, workspace)) = draft.workspace.clone() else {
            if let Some(draft) = &mut self.draft {
                draft.error = Some("This column has no workspace".into());
            }
            cx.notify();
            return;
        };
        let Some((provider, model, thinking)) = self.resolved(cx) else {
            if let Some(draft) = &mut self.draft {
                draft.error = Some("No ready providers on this host".into());
            }
            cx.notify();
            return;
        };
        let Some(directory) = self
            .core
            .store
            .read(cx)
            .fleet
            .workspace(&host, &workspace)
            .map(|workspace| workspace.directory().to_string())
        else {
            return;
        };
        let mut config = json!({ "provider": provider.id, "cwd": directory });
        if let Some(model) = model {
            config["model"] = json!(model);
        }
        if let Some(thinking) = thinking {
            config["thinkingOptionId"] = json!(thinking);
        }
        if let Some(mode) = provider.default_mode.clone() {
            config["modeId"] = json!(mode);
        }
        if let Some(draft) = &mut self.draft {
            draft.creating = true;
            draft.error = None;
        }
        cx.notify();
        let lane = self
            .draft
            .as_ref()
            .map(|draft| draft.lane.clone())
            .unwrap_or_default();
        let task = self.core.store.update(cx, |store, cx| {
            store.create_agent(
                &host,
                &workspace,
                config,
                Some(fleet::prompt::Prompt {
                    text: prompt,
                    ..Default::default()
                }),
                cx,
            )
        });
        let _ = window;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.draft_created(lane, host, result, cx);
            });
        })
        .detach();
    }

    /// Opens the agent a draft created, or shows why it was not.
    fn draft_created(
        &mut self,
        lane: String,
        host: String,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(agent_id) => {
                self.draft = None;
                self.draft_focused = false;
                self.pending = Some((lane.clone(), host.clone(), agent_id.clone()));
                self.expanded
                    .insert(lane.clone(), (host.clone(), agent_id.clone()));
                self.focused_lane = Some(lane);
                self.focused_agent = Some((host.clone(), agent_id.clone()));
                self.focus_composer = Some((host, agent_id));
                // The directory push can precede the response, and the
                // selected mode may have changed while creation ran.
                self.prune_expanded(cx);
                self.reconcile_view(true, cx);
            }
            Err(err) => {
                if let Some(draft) = &mut self.draft {
                    draft.creating = false;
                    draft.error = Some(err);
                }
            }
        }
        cx.notify();
    }

    /// Makes a workspace with the draft's agent in it, as Paseo's own new
    /// workspace screen does, then gives it the lane's labels so it lands in
    /// the lane it was started from.
    fn create_in_new_workspace(
        &mut self,
        new: NewWorkspace,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        let fail = |this: &mut Self, error: &str, cx: &mut Context<Self>| {
            if let Some(draft) = &mut this.draft {
                draft.error = Some(error.to_string());
            }
            cx.notify();
        };
        if prompt.is_empty() {
            return fail(self, "Describe the work for the first agent", cx);
        }
        let Some(project) = self
            .core
            .store
            .read(cx)
            .fleet
            .host(&new.host)
            .and_then(|host| host.directory.projects.get(&new.project).cloned())
        else {
            return fail(self, "Pick a project", cx);
        };
        let Some((provider, model, thinking)) = self.resolved(cx) else {
            return fail(self, "No ready providers on this host", cx);
        };
        let Some(lane) = self
            .lanes(cx)
            .iter()
            .find(|lane| {
                self.draft
                    .as_ref()
                    .is_some_and(|draft| draft.lane == lane.key)
            })
            .cloned()
        else {
            return;
        };
        let mut config = json!({ "provider": provider.id, "cwd": project.project_root_path });
        if let Some(model) = model {
            config["model"] = json!(model);
        }
        if let Some(thinking) = thinking {
            config["thinkingOptionId"] = json!(thinking);
        }
        if let Some(mode) = provider.default_mode.clone() {
            config["modeId"] = json!(mode);
        }
        let worktree = new.worktree && project.project_kind == "git";
        let source = if worktree {
            json!({ "kind": "worktree", "cwd": project.project_root_path, "projectId": project.project_id })
        } else {
            json!({ "kind": "directory", "path": project.project_root_path, "projectId": project.project_id })
        };
        let message = json!({
            "type": "workspace.create.request",
            "idempotencyKey": crate::app::store::uuid_v4(),
            "source": source,
            "agent": {
                "config": config,
                "initialPrompt": prompt,
                "clientMessageId": crate::app::store::uuid_v4(),
                "labels": {},
            },
            "firstAgentContext": { "prompt": prompt },
        });
        let labels = lane_labels(&lane, &new.host, &self.core.store.read(cx).fleet);
        if let Some(draft) = &mut self.draft {
            draft.creating = true;
            draft.error = None;
        }
        cx.notify();
        let host = new.host.clone();
        let task = self.core.store.read(cx).write(&host, message, cx);
        cx.spawn(async move |this, cx| {
            let created = match task.await {
                Ok(payload) if payload["error"].is_string() => {
                    Err(payload["error"].as_str().unwrap_or_default().to_string())
                }
                Ok(payload) => match (
                    payload["workspace"]["id"].as_str(),
                    payload["agent"]["id"].as_str(),
                ) {
                    (Some(workspace), Some(agent)) => {
                        Ok((workspace.to_string(), agent.to_string()))
                    }
                    _ => Err("The host did not return the new workspace".to_string()),
                },
                Err(error) => Err(error),
            };
            let result = match created {
                Ok((workspace, agent)) => {
                    // In a label lane it takes the lane's labels, or it would
                    // land under Unlabelled.
                    if !labels.is_empty() {
                        let batch = this.update(cx, |this, cx| {
                            this.core
                                .store
                                .read(cx)
                                .set_workspace_labels(&host, &workspace, labels, cx)
                        });
                        if let Ok(batch) = batch
                            && let Some(error) = batch.await.error
                        {
                            let _ = this.update(cx, |this, cx| {
                                this.core.store.update(cx, |store, cx| {
                                    store.toast(format!("Labelling the new workspace: {error}"), cx)
                                });
                            });
                        }
                    }
                    Ok(agent)
                }
                Err(error) => Err(error),
            };
            let _ = this.update(cx, |this, cx| {
                this.draft_created(lane.key.clone(), host, result, cx);
            });
        })
        .detach();
    }

    fn providers(&self, cx: &App) -> Vec<Provider> {
        let Some(host) = self.draft_host() else {
            return Vec::new();
        };
        self.core
            .store
            .read(cx)
            .providers
            .get(&host)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(Provider::ready)
            .collect()
    }

    /// Picked provider and model, defaulting to Claude Haiku when the host has it.
    fn resolved(&self, cx: &App) -> Option<(Provider, Option<String>, Option<String>)> {
        let draft = self.draft.as_ref()?;
        let providers = self.providers(cx);
        let provider = draft
            .provider
            .as_ref()
            .and_then(|id| providers.iter().find(|provider| &provider.id == id))
            .or_else(|| providers.iter().find(|provider| provider.id == "claude"))
            .or_else(|| providers.first())?
            .clone();
        let model = prefer_model(&provider, draft.model.as_ref());
        let thinking = model.and_then(|model| {
            draft
                .thinking
                .clone()
                .filter(|id| model.thinking.iter().any(|choice| &choice.id == id))
                .or_else(|| model.default_thinking.clone())
        });
        let model_id = model.map(|model| model.id.clone());
        Some((provider, model_id, thinking))
    }

    fn move_column(&mut self, delta: isize, cx: &mut Context<Self>) {
        let lanes = self.visible_lanes(cx);
        if lanes.is_empty() {
            return;
        }
        let index = self
            .focused_lane
            .as_ref()
            .and_then(|key| lanes.iter().position(|lane| &lane.key == key))
            .unwrap_or(0);
        let next = (index as isize + delta).clamp(0, lanes.len() as isize - 1) as usize;
        let lanes = self.arranged(&lanes, cx);
        let lane = &lanes[next];
        self.focused_row = None;
        let keep = self.focused_agent.as_ref().is_some_and(|(host, agent)| {
            lane.cards
                .iter()
                .any(|card| &card.host == host && &card.agent_id == agent)
        });
        if !keep {
            self.focused_agent = lane
                .cards
                .first()
                .map(|card| (card.host.clone(), card.agent_id.clone()));
        }
        self.focused_lane = Some(lane.key.clone());
        self.draft_focused = false;
        self.h_scroll.scroll_to_item(next);
        cx.notify();
    }

    fn move_card(&mut self, delta: isize, cx: &mut Context<Self>) {
        let lanes = self.arranged(&self.current_lanes(cx), cx);
        if self.focused_lane.is_none() {
            self.move_column(0, cx);
            return;
        }
        let Some(lane) = lanes
            .iter()
            .find(|lane| Some(&lane.key) == self.focused_lane.as_ref())
            .cloned()
        else {
            return;
        };
        if lane.collapsed {
            self.move_column(0, cx);
            return;
        }
        let draft_here = self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.lane == lane.key);
        /// Where focus can rest, with the list row it is drawn in.
        enum Slot {
            Draft,
            Card((String, String), usize),
            Row(RowFocus, usize),
        }
        let mut slots = Vec::new();
        if draft_here {
            slots.push(Slot::Draft);
        }
        let box_single = self.core.nav.read(cx).box_single_workspaces;
        let rows = self.rows(&lane, box_single, cx);
        for (ix, row) in rows.into_iter().enumerate() {
            match row {
                Row::Header { .. } => {}
                Row::Card { card, .. } => {
                    let card = &lane.cards[card];
                    slots.push(Slot::Card((card.host.clone(), card.agent_id.clone()), ix));
                }
                Row::Fold { .. } => slots.push(Slot::Row(RowFocus::Fold, ix)),
                Row::Subagents { card, .. } => {
                    let card = &lane.cards[card];
                    let focus = RowFocus::Subagents(card.host.clone(), card.agent_id.clone());
                    slots.push(Slot::Row(focus, ix));
                }
                Row::Terminal { owner, index, .. } => {
                    let (host, workspace) = owner.workspace(&lane);
                    if let Some(id) = self
                        .core
                        .store
                        .read(cx)
                        .workspace_terminal_ids(host, workspace)
                        .nth(index)
                    {
                        let focus = RowFocus::Terminal(host.to_string(), id.clone());
                        slots.push(Slot::Row(focus, ix));
                    }
                }
                Row::Empty { workspace, .. } => {
                    let ws = &lane.workspaces[workspace];
                    let focus = RowFocus::Workspace(ws.host.clone(), ws.workspace_id.clone());
                    slots.push(Slot::Row(focus, ix));
                }
            }
        }
        if slots.is_empty() {
            return;
        }
        let current = if self.draft_focused && draft_here {
            0
        } else if let Some(focus) = &self.focused_row {
            slots
                .iter()
                .position(|slot| matches!(slot, Slot::Row(row, _) if row == focus))
                .unwrap_or(0)
        } else {
            self.focused_agent
                .as_ref()
                .and_then(|agent| {
                    slots
                        .iter()
                        .position(|slot| matches!(slot, Slot::Card(card, _) if card == agent))
                })
                .unwrap_or(0)
        };
        let next = (current as isize + delta).clamp(0, slots.len() as isize - 1) as usize;
        let row = match &slots[next] {
            Slot::Draft => {
                self.draft_focused = true;
                self.focused_row = None;
                None
            }
            Slot::Card(agent, row) => {
                self.draft_focused = false;
                self.focused_row = None;
                self.focused_agent = Some(agent.clone());
                Some(*row)
            }
            Slot::Row(focus, row) => {
                self.draft_focused = false;
                self.focused_row = Some(focus.clone());
                Some(*row)
            }
        };
        if let Some(row) = row
            && let Some(list) = self.lists.get(&lane.key)
        {
            list.state.scroll_to_reveal_item(row);
        }
        cx.notify();
    }

    /// Focuses a lane's empty workspace or fold toggle.
    fn focus_row(&mut self, lane: &str, focus: RowFocus, cx: &mut Context<Self>) {
        self.focused_lane = Some(lane.to_string());
        self.focused_row = Some(focus);
        self.draft_focused = false;
        cx.notify();
    }

    fn toggle_empty(&mut self, lane: &str, cx: &mut Context<Self>) {
        if !self.empty_open.remove(lane) {
            self.empty_open.insert(lane.to_string());
        }
        self.focus_row(lane, RowFocus::Fold, cx);
    }

    fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.draft_focused {
            if let Some(draft) = &self.draft {
                draft.input.update(cx, |input, cx| input.focus(window, cx));
            }
            return;
        }
        let Some(lane_key) = self.focused_lane.clone() else {
            self.move_column(0, cx);
            return;
        };
        self.reveal_lane(&lane_key, cx);
        match self.focused_row.clone() {
            Some(RowFocus::Fold) => {
                self.toggle_empty(&lane_key, cx);
                return;
            }
            Some(RowFocus::Subagents(host, agent)) => {
                self.toggle_subagents(&lane_key, &host, &agent, cx);
                return;
            }
            Some(RowFocus::Terminal(host, id)) => {
                self.expand_terminal(&lane_key, &host, &id, window, cx);
                return;
            }
            Some(RowFocus::Workspace(host, workspace)) => {
                // An empty workspace's action is a new agent in it.
                if let Some(lane) = self
                    .current_lanes(cx)
                    .iter()
                    .find(|lane| lane.key == lane_key)
                    .cloned()
                {
                    self.open_draft_in(&lane, Some((host, workspace)), window, cx);
                }
                return;
            }
            None => {}
        }
        let Some((host, agent)) = self.focused_agent.clone() else {
            return;
        };
        self.expand(&lane_key, &host, &agent, window, cx);
    }

    /// Escape closes a blank composer, a blank draft, or the focused column's card.
    fn on_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let expanded: Vec<_> = self
            .expanded
            .iter()
            .map(|(lane, pair)| (lane.clone(), pair.clone()))
            .collect();
        for (lane, (host, agent)) in &expanded {
            let Some(view) = self.conversations.get(&(host.clone(), agent.clone())) else {
                continue;
            };
            if !view.read(cx).composer_is_focused(window, cx) {
                continue;
            }
            if view.read(cx).composer_is_blank(cx) {
                self.collapse_lane(lane, window, cx);
                return true;
            }
            return false;
        }
        if self.draft_focused
            && let Some(draft) = &self.draft
            && draft.input.read(cx).focus_handle(cx).is_focused(window)
        {
            // Closed whatever it holds; the text comes back with the next
            // draft in its lane.
            self.dismiss_draft(true, window, cx);
            return true;
        }
        if self.focus.is_focused(window)
            && let Some(lane) = self.focused_lane.clone()
            && self.expanded.contains_key(&lane)
        {
            self.collapse_lane(&lane, window, cx);
            return true;
        }
        false
    }

    /// Moves the columns' scroll offset (zero or negative), clamped to the content.
    fn set_scroll_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let current = self.h_scroll.offset();
        let min = -self.h_scroll.max_offset().x;
        let next = if x > px(0.) {
            px(0.)
        } else if x < min {
            min
        } else {
            x
        };
        if next != current.x {
            self.h_scroll.set_offset(point(next, current.y));
            cx.notify();
        }
    }

    /// Positive `dx` reveals later columns.
    fn nudge_scroll(&mut self, dx: f32, cx: &mut Context<Self>) {
        let x = self.h_scroll.offset().x - px(dx);
        self.set_scroll_x(x, cx);
    }

    fn middle_down(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.middle = match self.middle {
            Some(MiddleScroll::Auto { .. }) => None,
            _ => Some(MiddleScroll::Held {
                origin: position,
                offset: self.h_scroll.offset().x,
                panning: false,
            }),
        };
        cx.notify();
    }

    fn middle_up(&mut self, cx: &mut Context<Self>) {
        match self.middle {
            Some(MiddleScroll::Held {
                origin,
                panning: false,
                ..
            }) => {
                self.middle = Some(MiddleScroll::Auto { origin });
                self.run_autoscroll(cx);
            }
            Some(MiddleScroll::Held { .. }) => self.middle = None,
            _ => return,
        }
        cx.notify();
    }

    fn edge_hover(&self) -> bool {
        self.window_active && self.pointer_inside && edges::hovered(&self.h_scroll, self.pointer)
    }

    fn pointer_moved(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let was_hover = self.edge_hover();
        self.pointer = position;
        self.pointer_inside = true;
        if self.edge_hover() != was_hover {
            cx.notify();
        }
        if let Some(resize) = &self.resizing {
            let widest = (self.h_scroll.bounds().size.width - px(COLUMNS_PADDING * 2.))
                .max(px(EXPANDED_RESIZE_MIN));
            let width = (resize.start_width + (position.x - resize.start_x))
                .clamp(px(EXPANDED_RESIZE_MIN), widest)
                .round();
            if self.expanded_widths.get(&resize.lane) != Some(&width) {
                self.expanded_widths.insert(resize.lane.clone(), width);
                cx.notify();
            }
            return;
        }
        let Some(MiddleScroll::Held {
            origin,
            offset,
            panning,
        }) = self.middle
        else {
            return;
        };
        let dx = position.x - origin.x;
        let panning = panning || (dx / px(1.)).abs() > PAN_THRESHOLD;
        if panning {
            self.middle = Some(MiddleScroll::Held {
                origin,
                offset,
                panning,
            });
            // The board follows the pointer, like dragging paper.
            self.set_scroll_x(offset + dx, cx);
            cx.notify();
        }
    }

    fn end_resize(&mut self, cx: &mut Context<Self>) {
        if self.resizing.take().is_some() {
            cx.notify();
        }
    }

    fn stop_middle(&mut self, cx: &mut Context<Self>) -> bool {
        if self.middle.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    /// Ticks while in autoscroll, scrolling by how far the pointer is from
    /// where the middle button was clicked.
    fn run_autoscroll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut last = Instant::now();
            loop {
                cx.background_executor().timer(AUTOSCROLL_TICK).await;
                let now = Instant::now();
                let elapsed = (now - last).as_secs_f32();
                last = now;
                let running = this
                    .update(cx, |this, cx| {
                        let Some(MiddleScroll::Auto { origin }) = this.middle else {
                            return false;
                        };
                        let distance = (this.pointer.x - origin.x) / px(1.);
                        let past = distance.abs() - AUTOSCROLL_DEAD_ZONE;
                        if past > 0. {
                            // Pointer to the right of the origin reveals later columns.
                            let dx = distance.signum() * past * AUTOSCROLL_GAIN * elapsed;
                            this.nudge_scroll(dx, cx);
                        }
                        true
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            return;
        }
        let mods = &event.keystroke.modifiers;
        if mods.control || mods.platform || mods.alt {
            return;
        }
        let key = event.keystroke.key.as_str();
        match (key, mods.shift) {
            ("left", true) => self.nudge_scroll(-(self.column_width / px(1.)), cx),
            ("right", true) => self.nudge_scroll(self.column_width / px(1.), cx),
            ("left", false) => self.move_column(-1, cx),
            ("right", false) => self.move_column(1, cx),
            ("up", false) => self.move_card(-1, cx),
            ("down", false) => self.move_card(1, cx),
            ("enter", false) => self.activate(window, cx),
            ("f", false) => {
                if let (Some(lane), Some((host, agent))) =
                    (self.focused_lane.clone(), self.focused_agent.clone())
                {
                    self.toggle_focus_mode(&lane, &host, &agent, window, cx);
                }
            }
            ("escape", false) => {
                self.on_escape(window, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn card_focused(&self, lane: &str, card: &LaneCard) -> bool {
        !self.draft_focused
            && self.focused_row.is_none()
            && self.focused_lane.as_deref() == Some(lane)
            && self
                .focused_agent
                .as_ref()
                .is_some_and(|(host, agent)| host == &card.host && agent == &card.agent_id)
    }
}

impl Render for Lanes {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let lanes = self.lanes(cx);
        // Cache rebuilds request one header-focus check in the next update.
        // Apply relocation outside render and skip protected batch/pending focus.
        if self.focus_check_needed.replace(false)
            && placement::focus_lane_missing(&self.visible_lanes(cx), self.focused_lane.as_deref())
        {
            let view = cx.entity().downgrade();
            cx.defer(move |cx| {
                let _ = view.update(cx, |this, cx| {
                    let follow = this.core.nav.read(cx).follow_relabel;
                    this.reconcile_view(follow, cx);
                    cx.notify();
                });
            });
        }
        let viewport = window.viewport_size().width;
        let expanded_width = {
            let room = viewport - px(200.);
            if room < px(EXPANDED_MIN) {
                px(EXPANDED_MIN)
            } else if room > px(EXPANDED_MAX) {
                px(EXPANDED_MAX)
            } else {
                room
            }
        };

        // A lane whose card collapsed returns to the shared width.
        let expanded = &self.expanded;
        self.expanded_widths
            .retain(|lane, _| expanded.contains_key(lane));
        if self
            .resizing
            .as_ref()
            .is_some_and(|resize| !self.expanded.contains_key(&resize.lane))
        {
            self.resizing = None;
        }

        let open: Vec<(String, String)> = self.expanded.values().cloned().collect();
        for (host, agent) in &open {
            self.conversation(host, agent, window, cx);
        }
        if let Some((host, agent)) = self.focus_composer.take()
            && let Some(view) = self.conversations.get(&(host, agent)).cloned()
        {
            view.update(cx, |conversation, cx| {
                conversation.focus_composer(window, cx)
            });
        }

        let visible_width = match self.h_scroll.bounds().size.width {
            width if width > px(0.) => width,
            _ => viewport,
        };
        let visible = self.arranged(&self.visible_lanes(cx), cx);
        // While a lane is dragged, the lanes show where it would land.
        let visible = match &self.lane_drag {
            Some(drag) => in_order(&visible, &drag.order),
            None => visible,
        };
        let mut columns = div()
            .id("lanes-columns")
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .flex()
            .gap(px(COLUMN_GAP))
            .px(px(COLUMNS_PADDING))
            .pb(px(16.))
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.h_scroll)
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                if this.wheel_resize(event, cx) {
                    cx.stop_propagation();
                    return;
                }
                // Headers scroll across columns with a plain wheel, and so do
                // the gaps between them, so a wheel along the header row never
                // falls through a gap. Lower gaps stay inert.
                let board = this.h_scroll.bounds();
                let header_row = event.position.y >= board.top()
                    && event.position.y < board.top() + px(HEADER_HEIGHT);
                if !event.modifiers.shift && !header_row {
                    return;
                }
                let delta = event.delta.pixel_delta(px(16.));
                let dy = delta.y / px(1.);
                if dy == 0. {
                    return;
                }
                // Wheel down is a negative delta; that should reveal later columns.
                this.nudge_scroll(-dy, cx);
                cx.stop_propagation();
            }));
        if visible.is_empty() {
            columns = columns.child(div().p(px(24.)).text_color(p.muted).child(
                if lanes.is_empty() {
                    "No workstreams yet"
                } else if !self.core.nav.read(cx).lane_filter.is_empty() {
                    "Nothing matches the filters. Clear them in the bar above."
                } else {
                    "All lanes are collapsed. Restore one from the tabs above."
                },
            ));
        }
        let keys: HashSet<&str> = lanes.iter().map(|lane| lane.key.as_str()).collect();
        // Only columns near the viewport are built. The rest are empty boxes
        // of the same width, so the scroll extent and scroll_to_item hold.
        // Whole device pixels, so a column that only moved can reuse what it
        // drew: its glyphs stay on the subpixel offsets they were drawn at.
        let offset = self.h_scroll.offset();
        let scale = window.scale_factor();
        let snapped = px((offset.x.as_f32() * scale).round() / scale);
        if snapped != offset.x {
            self.h_scroll.set_offset(point(snapped, offset.y));
        }
        let mut scroll_x = -snapped;
        self.column_width = self.fit_width(&visible, expanded_width, visible_width, cx);
        // Scroll the least that shows the whole column, with the board's
        // padding beside it; a column wider than the view shows its left.
        if let Some(key) = self.reveal_column.take() {
            let mut x = px(COLUMNS_PADDING);
            for lane in visible.iter() {
                let width = self.lane_width(lane, expanded_width);
                if lane.key == key {
                    let left = x - px(COLUMNS_PADDING);
                    let right = x + width + px(COLUMNS_PADDING);
                    let target = if right - left > visible_width || left < scroll_x {
                        left
                    } else if right > scroll_x + visible_width {
                        right - visible_width
                    } else {
                        scroll_x
                    };
                    let target = px((target.as_f32() * scale).round() / scale).max(px(0.));
                    if target != scroll_x {
                        scroll_x = target;
                        self.h_scroll.set_offset(point(-target, offset.y));
                    }
                    break;
                }
                x += width + px(COLUMN_GAP);
            }
        }
        let (near_from, near_to) = (
            scroll_x - self.column_width,
            scroll_x + visible_width + self.column_width,
        );
        let mut x = px(COLUMNS_PADDING);
        // Workspaces in lanes near the view, whose terminals they list.
        let mut watched: Vec<(String, String)> = Vec::new();
        for (ix, lane) in visible.iter().enumerate() {
            let width = self.lane_width(lane, expanded_width);
            let near = x + width >= near_from && x <= near_to;
            if near {
                watched.extend(
                    lane.workspaces
                        .iter()
                        .map(|ws| (ws.host.clone(), ws.workspace_id.clone())),
                );
            }
            x += width + px(COLUMN_GAP);
            columns = columns.child(if near {
                let style = self.lane_style(lane, cx);
                let inputs = ColumnInputs {
                    lanes: visible.clone(),
                    ix,
                    width,
                    style,
                    fingerprint: self.column_fingerprint(lane, &style, cx),
                };
                // An expanded card or a draft records where it was drawn
                // (popover anchors, text input), so only plain card columns
                // may reuse their drawing when scrolling moves them. See
                // vendor/gpui-pre/PATCHES.md for relocation restrictions.
                let relocatable = !self.expanded.contains_key(&lane.key)
                    && !self.expanded_terminals.contains_key(&lane.key)
                    && !self
                        .draft
                        .as_ref()
                        .is_some_and(|draft| draft.lane == lane.key);
                let view = self.column_view(&lane.key, inputs, cx);
                // A cached view is laid out from this style, not its content.
                AnyView::from(view)
                    .cached(StyleRefinement::default().w(width).h_full().flex_shrink_0())
                    .relocatable(relocatable)
                    .into_any_element()
            } else {
                div().w(width).flex_shrink_0().h_full().into_any_element()
            });
        }
        // A lane narrowed with the board scrolled to its end would shorten
        // the board, and the view would jump right as the scroll clamped,
        // leaving the lane's edge away from the pointer. Room at the end
        // holds the view still; it goes as the view scrolls back over it.
        if self.hold_board_length {
            let length = x - px(COLUMN_GAP) + px(COLUMNS_PADDING);
            let slack = scroll_x + visible_width - length;
            if slack > px(0.) {
                columns = columns.child(
                    div()
                        .w((slack - px(COLUMN_GAP)).max(px(0.)))
                        .flex_shrink_0()
                        .h_full(),
                );
            } else if self.resizing.is_none() {
                self.hold_board_length = false;
            }
        }
        self.watch_terminals(watched, cx);
        self.lists.retain(|key, _| keys.contains(key.as_str()));
        let visible_keys: HashSet<&str> = visible.iter().map(|lane| lane.key.as_str()).collect();
        self.columns
            .retain(|key, _| visible_keys.contains(key.as_str()));

        // Passive fades indicate actual overflow. The thumb reveals on edge
        // or bottom hover, and the stock scrollbar holds it during interaction.
        let board = div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(columns)
            .child(edges::fades(&self.h_scroll, p.surface0))
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::horizontal(&self.h_scroll)
                        .mode(if self.edge_hover() {
                            ScrollbarMode::Always
                        } else {
                            ScrollbarMode::Hover
                        })
                        .viewport_from_layout(),
                ),
            )
            .child(self.width_control(cx));
        let autoscroll_origin = match self.middle {
            Some(MiddleScroll::Auto { origin }) => Some(origin),
            _ => None,
        };
        let scrolling = autoscroll_origin.is_some()
            || matches!(self.middle, Some(MiddleScroll::Held { panning: true, .. }))
            || self.resizing.is_some();

        let find_bar = self.find_bar(cx);
        div()
            .id("lanes-root")
            .key_context("Lanes")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &FindOnBoard, window, cx| {
                this.open_find(window, cx);
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    let was_hover = this.edge_hover();
                    this.pointer_inside = false;
                    if was_hover {
                        cx.notify();
                    }
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface0)
            .when(scrolling, |root| root.cursor(CursorStyle::ResizeLeftRight))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key != "escape" || event.keystroke.modifiers.modified() {
                    return;
                }
                // The find bar closes first.
                if this.find.is_some() {
                    this.close_find(window, cx);
                    cx.stop_propagation();
                    return;
                }
                // Escape cancels a title edit; the editor handles it.
                if this.renaming.is_some() {
                    return;
                }
                // Then it leaves focus mode, before anything else.
                if this.focus_mode.take().is_some() {
                    this.focus.focus(window, cx);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if this.stop_middle(cx) || this.on_escape(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.middle_down(event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.middle_up(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Middle,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.stop_middle(cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.pointer_moved(event.position, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, window, cx| {
                    this.end_resize(cx);
                    this.cancel_lane_drag(window, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, window, cx| {
                    this.end_resize(cx);
                    this.cancel_lane_drag(window, cx);
                }),
            )
            // A click anywhere but on the draft closes it. Its menus block
            // clicks from reaching the board, so using them does not.
            .on_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                let outside = this.draft.as_ref().is_some_and(|draft| {
                    draft
                        .bounds
                        .get()
                        .is_some_and(|bounds| !bounds.contains(&event.position))
                });
                if outside {
                    this.dismiss_draft(true, window, cx);
                }
            }))
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.dismiss_draft(false, window, cx);
            }))
            // A click ends autoscroll without acting on what was under it,
            // as in browsers.
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                if event.button != MouseButton::Middle
                    && matches!(this.middle, Some(MiddleScroll::Auto { .. }))
                    && this.stop_middle(cx)
                {
                    cx.stop_propagation();
                }
            }))
            .relative()
            .child(board)
            .children(self.focus_overlay(window, cx))
            .children(self.card_menu_layer(window, cx))
            .children(find_bar)
            .children(self.state_bar(cx))
            .when_some(autoscroll_origin, |root, origin| {
                root.child(deferred(
                    anchored().position(origin - point(px(14.), px(14.))).child(
                        div()
                            .size(px(28.))
                            .rounded_full()
                            .border_1()
                            .border_color(p.border)
                            .bg(p.surface2)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(p.muted)
                            .child("↔"),
                    ),
                ))
            })
    }
}

/// `lanes` in `order`, by key; any not in it keep their order after.
fn in_order(lanes: &LaneSet, order: &[String]) -> LaneSet {
    let rank = |lane: &Rc<Lane>| {
        order
            .iter()
            .position(|key| *key == lane.key)
            .unwrap_or(order.len())
    };
    let mut sorted = lanes.to_vec();
    sorted.sort_by_key(rank);
    Rc::new(sorted)
}

/// The tooltip on a workspace's host name: the host in full, the
/// workspace's directory, and when its first agent started (Paseo does not
/// say when a workspace was made). Read when shown, so it is never stale.
fn workspace_host_tooltip(
    core: &Core,
    host: &str,
    workspace: &str,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let (core, host, workspace) = (core.clone(), host.to_string(), workspace.to_string());
    move |window, cx| {
        let store = core.store.read(cx);
        let fleet = &store.fleet;
        let mut rows: Vec<(&str, String)> = vec![("Host", host.clone())];
        let record = fleet.host(&host);
        if let Some(name) = record
            .and_then(|record| record.server.as_ref())
            .map(|server| server.hostname.clone())
            .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case(&host))
        {
            rows.push(("Hostname", name));
        }
        if let Some(directory) = fleet
            .workspace(&host, &workspace)
            .map(|ws| ws.directory().to_string())
            .filter(|directory| !directory.is_empty())
        {
            rows.push(("Directory", directory));
        }
        let first_started = record.and_then(|record| {
            record
                .directory
                .agents
                .values()
                .filter(|agent| agent.workspace_id.as_deref() == Some(workspace.as_str()))
                .filter_map(|agent| {
                    let iso = agent.created_at.as_deref()?;
                    Some((crate::app::store::parse_iso(iso)?, iso))
                })
                .min_by_key(|(at, _)| *at)
                .map(|(_, iso)| iso.to_string())
        });
        if let Some(iso) = first_started {
            let date = iso.get(..10).unwrap_or(&iso);
            let ago = crate::app::store::time_ago(&iso);
            rows.push(("First agent", format!("{ago} · {date}")));
        }
        rows_tooltip(rows, window, cx)
    }
}

/// A tooltip of label and value rows.
fn rows_tooltip(rows: Vec<(&str, String)>, window: &mut Window, cx: &mut App) -> AnyView {
    let rows: Vec<(String, String)> = rows
        .into_iter()
        .map(|(label, value)| (label.to_string(), value))
        .collect();
    Tooltip::element(move |_, _| {
        let p = palette();
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(rows.iter().map(|(label, value)| {
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .w(px(84.))
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_color(p.muted)
                            .child(label.clone()),
                    )
                    .child(div().max_w(px(420.)).child(value.clone()))
            }))
    })
    .build(window, cx)
}

/// The tooltip on a lane's name: the name in full, and for a label lane
/// which hosts define each label and where its workspaces are. Paseo keeps
/// only a label's name and colour, not who made it or when. Read when
/// shown, so it is never stale.
fn lane_title_tooltip(
    core: &Core,
    lane: &Rc<Lane>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let (core, lane) = (core.clone(), Rc::clone(lane));
    move |window, cx| {
        let fleet = &core.store.read(cx).fleet;
        let mut rows: Vec<(&str, String)> = Vec::new();
        match lane.kind {
            LaneKind::Label | LaneKind::Combination => {
                let single = lane.label_keys.len() == 1;
                rows.push((if single { "Label" } else { "Labels" }, lane.title.clone()));
                for (ix, key) in lane.label_keys.iter().enumerate() {
                    // One "Defined on" heads the rows for all the labels.
                    let label = if ix == 0 { "Defined on" } else { "" };
                    let hosts: Vec<&str> = fleet
                        .hosts
                        .iter()
                        .filter(|host| {
                            host.directory
                                .labels
                                .iter()
                                .any(|def| fleet::directory::label_key(&def.name) == *key)
                        })
                        .map(|host| host.name())
                        .collect();
                    let hosts = if hosts.is_empty() {
                        "no host".to_string()
                    } else {
                        hosts.join(", ")
                    };
                    if single {
                        rows.push((label, hosts));
                    } else {
                        let name = fleet
                            .hosts
                            .iter()
                            .flat_map(|host| &host.directory.labels)
                            .find(|def| fleet::directory::label_key(&def.name) == *key)
                            .map_or(key.as_str(), |def| def.name.as_str());
                        rows.push((label, format!("{name}: {hosts}")));
                    }
                }
            }
            _ => rows.push(("Lane", lane.title.clone())),
        }
        // Workspaces per host, most first.
        let mut per_host: Vec<(&str, usize)> = Vec::new();
        for workspace in &lane.workspaces {
            match per_host
                .iter_mut()
                .find(|(host, _)| *host == workspace.host)
            {
                Some((_, count)) => *count += 1,
                None => per_host.push((&workspace.host, 1)),
            }
        }
        per_host.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        if !per_host.is_empty() {
            let spread: Vec<String> = per_host
                .iter()
                .map(|(host, count)| format!("{count} on {host}"))
                .collect();
            rows.push(("Workspaces", spread.join(" · ")));
        }
        rows_tooltip(rows, window, cx)
    }
}

/// A menu item that copies `text`, greyed out when there is none, such as
/// the resume command of an agent whose provider has not started a session.
fn copy_item(menu: PopupMenu, core: &Core, label: &str, text: Option<String>) -> PopupMenu {
    let store = core.store.clone();
    let what = label.trim_start_matches("Copy ").to_string();
    menu.item(
        PopupMenuItem::new(label.to_string())
            .disabled(text.is_none())
            .on_click(move |_, _, cx| {
                let Some(text) = text.clone() else {
                    return;
                };
                cx.write_to_clipboard(gpui_kit::gpui::ClipboardItem::new_string(text));
                store.update(cx, |store, cx| store.toast(format!("Copied {what}"), cx));
            }),
    )
}

impl Lanes {
    /// Opens the card menu for `host`/`agent` at `position`, replacing any
    /// menu already open. Dismissing it returns focus to the board.
    fn open_card_menu(
        &mut self,
        host: &str,
        agent: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let (host, agent) = (host.to_string(), agent.to_string());
        let menu = PopupMenu::build(window, cx, move |menu, _, cx| {
            let record = core.store.read(cx).fleet.agent(&host, &agent);
            let resume = record.and_then(|record| record.resume_command());
            let workspace = record.and_then(|record| record.workspace_id.clone());
            let menu = copy_item(menu, &core, "Copy Paseo agent id", Some(agent.clone()));
            let menu = copy_item(menu, &core, "Copy resume command", resume);
            let menu = copy_item(menu, &core, "Copy Paseo workspace id", workspace);
            crate::workspace_archive::agent_menu(menu.separator(), &core, &host, &agent, cx)
        });
        let _dismiss = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
            this.card_menu = None;
            this.focus.focus(window, cx);
            cx.notify();
        });
        self.card_menu = Some(CardMenu {
            position,
            menu,
            _dismiss,
        });
        cx.notify();
    }

    /// Opens a workspace's menu at `position`, replacing any menu open.
    fn open_workspace_menu(
        &mut self,
        lane: &Rc<Lane>,
        host: &str,
        workspace: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let board = cx.entity().downgrade();
        let lane = Rc::clone(lane);
        let (host, workspace) = (host.to_string(), workspace.to_string());
        let menu = PopupMenu::build(window, cx, move |menu, _, cx| {
            let target = (host.clone(), workspace.clone());
            let menu = menu.item(PopupMenuItem::new("New agent in this workspace").on_click({
                let (board, lane, target) = (board.clone(), lane.clone(), target.clone());
                move |_, window, cx| {
                    let _ = board.update(cx, |board, cx| {
                        board.open_draft_in(&lane, Some(target.clone()), window, cx);
                    });
                }
            }));
            let menu = menu.item(
                PopupMenuItem::new("New terminal in this workspace").on_click({
                    let (board, lane_key, (host, workspace)) =
                        (board.clone(), lane.key.clone(), target.clone());
                    move |_, window, cx| {
                        let _ = board.update(cx, |board, cx| {
                            board.new_terminal(&lane_key, &host, &workspace, window, cx);
                        });
                    }
                }),
            );
            // Where the box sits, and the order of its cards.
            let key = workspace_key(&host, &workspace);
            let (pinned, ordered) = {
                let nav = core.nav.read(cx);
                (
                    nav.pinned_workspaces.contains(&key),
                    nav.card_order.contains_key(&key),
                )
            };
            let menu = menu.separator().item(
                PopupMenuItem::new(if pinned {
                    "Unpin from the top of the lane"
                } else {
                    "Pin to the top of the lane"
                })
                .on_click({
                    let (nav, host, workspace) =
                        (core.nav.clone(), host.clone(), workspace.clone());
                    move |_, _, cx| {
                        nav.update(cx, |nav, cx| {
                            nav.toggle_workspace_pin(&host, &workspace, cx)
                        });
                    }
                }),
            );
            let menu = if ordered {
                menu.item(PopupMenuItem::new("Reset card order").on_click({
                    let (nav, host, workspace) =
                        (core.nav.clone(), host.clone(), workspace.clone());
                    move |_, _, cx| {
                        nav.update(cx, |nav, cx| {
                            nav.set_card_order(&host, &workspace, Vec::new(), cx)
                        });
                    }
                }))
            } else {
                menu
            };
            let menu = copy_item(
                menu.separator(),
                &core,
                "Copy Paseo workspace id",
                Some(workspace.clone()),
            );
            crate::workspace_archive::workspace_menu(menu.separator(), &core, &host, &workspace, cx)
        });
        let _dismiss = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
            this.card_menu = None;
            this.focus.focus(window, cx);
            cx.notify();
        });
        self.card_menu = Some(CardMenu {
            position,
            menu,
            _dismiss,
        });
        cx.notify();
    }

    /// The open card menu, floated above everything at its click position.
    fn card_menu_layer(&self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let open = self.card_menu.as_ref()?;
        let focus = open.menu.focus_handle(cx);
        if !focus.contains_focused(window, cx) {
            focus.focus(window, cx);
        }
        Some(
            deferred(
                anchored()
                    .position(open.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(open.menu.clone()),
            )
            .with_priority(gpui_kit::base::POPUP_PRIORITY)
            .into_any_element(),
        )
    }

    /// One of Lanes' toolbar controls, wherever the user put it.
    fn toolbar_item(&mut self, kind: LanesControl, cx: &mut Context<Self>) -> Option<AnyElement> {
        let nav = self.core.nav.clone();
        let grouping = nav.read(cx).lane_grouping;
        match kind {
            LanesControl::Grouping => Some(
                Button::new("lanes-group")
                    .label(format!("Group by {}", grouping.label()))
                    .flex_shrink_0()
                    .ghost()
                    .small()
                    .dropdown_menu(move |menu, _, _| {
                        let mut menu = menu;
                        for choice in [
                            LaneGrouping::Label,
                            LaneGrouping::Project,
                            LaneGrouping::Host,
                            LaneGrouping::Workspace,
                        ] {
                            let nav = nav.clone();
                            menu = menu.item(
                                PopupMenuItem::new(choice.label())
                                    .checked(grouping == choice)
                                    .on_click(move |_, _, cx| {
                                        nav.update(cx, |nav, cx| nav.set_lane_grouping(choice, cx));
                                    }),
                            );
                        }
                        menu
                    })
                    .into_any_element(),
            ),
            LanesControl::LabelMode => {
                if grouping != LaneGrouping::Label {
                    return None;
                }
                let label_mode = nav.read(cx).lane_label_mode;
                Some(
                    ButtonGroup::new("lane-label-mode")
                        .flex_shrink_0()
                        .small()
                        .compact()
                        .children(LabelLaneMode::ALL.into_iter().map(|mode| {
                            Button::new(mode.as_str())
                                .label(mode.label())
                                .selected(mode == label_mode)
                                .accessibility_label(format!("Label lanes: {}", mode.label()))
                                .tooltip(match mode {
                                    LabelLaneMode::Labels => "One lane per label",
                                    LabelLaneMode::Combinations => {
                                        "One lane per exact set of labels"
                                    }
                                    LabelLaneMode::Both => {
                                        "Individual labels and exact combinations"
                                    }
                                })
                        }))
                        .on_click(move |indices, _, cx| {
                            if let Some(mode) = indices
                                .first()
                                .and_then(|index| LabelLaneMode::ALL.get(*index))
                                .copied()
                            {
                                nav.update(cx, |nav, cx| nav.set_lane_label_mode(mode, cx));
                            }
                        })
                        .into_any_element(),
                )
            }
            LanesControl::Filters => Some(self.filters_bar(cx)),
            LanesControl::CollapsedLanes => {
                let lanes = self.current_lanes(cx);
                lanes
                    .iter()
                    .any(|lane| lane.collapsed)
                    .then(|| self.collapsed_tabs(&lanes, cx))
            }
        }
    }

    fn collapsed_tabs(&self, lanes: &LaneSet, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        // While finding: tabs that match lit up, the rest dimmed.
        let lit = self
            .find
            .as_ref()
            .filter(|find| find.active)
            .map(|find| find.collapsed.clone());
        let pointed = self
            .find
            .as_ref()
            .and_then(|find| find.pointed_lane().map(str::to_string));
        let tabs = lanes.iter().filter(|lane| lane.collapsed).map(|lane| {
            let style = self.lane_style(lane, cx);
            let found = lit.as_ref().map(|lit| lit.contains(&lane.key));
            let singled = pointed.as_deref() == Some(lane.key.as_str());
            let dot = style
                .swatch
                .map(|index| rgb(theme::LANE_COLORS[index].1))
                .unwrap_or(p.extra_muted);
            let key = lane.key.clone();
            let description = format!("Restore {} ({} cards)", lane.title, lane.cards.len());
            Button::new(eid("lane-restore", &lane.key))
                .small()
                .ghost()
                .flex_shrink_0()
                .bg(style.column)
                .border_1()
                .border_color(p.border)
                .rounded(px(6.))
                .when(found == Some(true), |tab| tab.border_color(p.accent))
                .when(singled, |tab| {
                    tab.border_2()
                        .border_color(p.accent)
                        .bg(with_alpha(p.accent, 0.25))
                })
                .when(found == Some(false), |tab| tab.opacity(0.35))
                .accessibility_label(description.clone())
                .tooltip(description)
                .child(div().size(px(6.)).flex_shrink_0().rounded_full().bg(dot))
                .child(
                    div()
                        .min_w(px(0.))
                        .max_w(px(152.))
                        .truncate()
                        .child(lane.title.clone()),
                )
                .child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.extra_muted)
                        .child(lane.cards.len().to_string()),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.restore_lane(&key, window, cx);
                }))
        });
        div()
            .relative()
            .flex_1()
            .min_w(px(0.))
            .h(px(30.))
            .child(
                div()
                    .id("collapsed-lane-tabs")
                    .size_full()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&self.collapsed_scroll)
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        let delta = event.delta.pixel_delta(px(16.));
                        if delta.x == px(0.) && delta.y != px(0.) {
                            let old = this.collapsed_scroll.offset();
                            let maximum = this.collapsed_scroll.max_offset().x;
                            this.collapsed_scroll.set_offset(point(
                                (old.x + delta.y).clamp(-maximum, px(0.)),
                                old.y,
                            ));
                            cx.stop_propagation();
                            // The tabs sit in a toolbar the shell draws.
                            this.core.toolbars.update(cx, |_, cx| cx.notify());
                        }
                    }))
                    .children(tabs),
            )
            .child(
                Scrollbar::horizontal(&self.collapsed_scroll)
                    .mode(ScrollbarMode::Hover)
                    .viewport_from_layout(),
            )
            .into_any_element()
    }

    fn width_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let nav_state = self.core.nav.read(cx);
        let width_label = if nav_state.lane_fit {
            format!("Fit · {:.0}", self.column_width / px(1.))
        } else {
            format!("Width · {:.0}", nav_state.lane_width)
        };
        let slider = self.width_slider.clone();
        let nav = self.core.nav.clone();
        let any_expanded = !self.expanded.is_empty();
        div()
            .absolute()
            .right(px(16.))
            .bottom(px(22.))
            .bg(p.surface2)
            .rounded(px(6.))
            .border_1()
            .border_color(p.border)
            .flex()
            .items_center()
            .when(any_expanded, |pill| {
                pill.child(
                    Button::new("lanes-collapse-all")
                        .small()
                        .ghost()
                        .label("Collapse all cards")
                        .icon(Icon::empty().path("icons/chevrons-down-up.svg"))
                        .accessibility_label("Collapse all expanded cards")
                        .tooltip("Collapse every expanded agent card")
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.collapse_all(window, cx)
                        })),
                )
            })
            .child(
                Popover::new("lane-width-popover")
                    .anchor(Anchor::BottomRight)
                    .trigger(
                        Button::new("lane-width-settings")
                            .small()
                            .ghost()
                            .label(width_label)
                            .icon(Icon::empty().path("icons/settings-2.svg"))
                            .accessibility_label("Column width and Fit")
                            .tooltip("Column width and Fit · Cmd/Ctrl+wheel also resizes"),
                    )
                    .content(move |_, _, cx| {
                        let fit = nav.read(cx).lane_fit;
                        let nav = nav.clone();
                        div()
                            .w(px(240.))
                            .flex()
                            .flex_col()
                            .gap(px(12.))
                            .child(
                                div()
                                    .text_size(size::sm())
                                    .text_color(p.muted)
                                    .child("Column width"),
                            )
                            .child(Slider::new(&slider))
                            .child(
                                Button::new("lanes-fit")
                                    .small()
                                    .ghost()
                                    .label("Fit columns")
                                    .selected(fit)
                                    .tooltip("Narrow columns so they all fit in view")
                                    .on_click(move |_, _, cx| {
                                        nav.update(cx, |nav, cx| nav.set_lane_fit(!fit, cx))
                                    }),
                            )
                    }),
            )
            .into_any_element()
    }

    fn lane_width(&self, lane: &Lane, expanded_width: Pixels) -> Pixels {
        if self.expanded.contains_key(&lane.key) {
            self.expanded_widths
                .get(&lane.key)
                .copied()
                .unwrap_or(expanded_width)
        } else {
            self.column_width
        }
    }

    /// The ordinary column width: the user's, or narrower when fitting every
    /// column into `visible` (never below [`FIT_MIN_WIDTH`]; past that, scroll).
    fn fit_width(
        &self,
        lanes: &[Rc<Lane>],
        expanded_width: Pixels,
        visible: Pixels,
        cx: &App,
    ) -> Pixels {
        let nav = self.core.nav.read(cx);
        let chosen = px(nav.lane_width);
        if !nav.lane_fit {
            return chosen;
        }
        let mut fixed = px(0.);
        let mut ordinary = 0usize;
        for lane in lanes {
            if self.expanded.contains_key(&lane.key) {
                fixed += self.lane_width(lane, expanded_width);
            } else {
                ordinary += 1;
            }
        }
        if ordinary == 0 {
            return chosen;
        }
        let gaps = px(COLUMN_GAP) * lanes.len().saturating_sub(1) as f32;
        let room = visible - px(COLUMNS_PADDING * 2.) - gaps - fixed;
        let fit = room / ordinary as f32;
        if fit > chosen {
            chosen
        } else if fit < px(FIT_MIN_WIDTH) {
            px(FIT_MIN_WIDTH)
        } else {
            fit
        }
    }

    /// Ctrl+wheel makes every column wider or narrower, like zooming.
    /// Returns whether the event was a resize.
    fn wheel_resize(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) -> bool {
        if !(event.modifiers.control || event.modifiers.platform) {
            return false;
        }
        let dy = event.delta.pixel_delta(px(16.)).y / px(1.);
        if dy == 0. {
            return false;
        }
        let lines = (dy / 16.).clamp(-3., 3.);
        // Save wheel notches now; coalesce trackpad updates into a trailing save.
        let persist = matches!(event.delta, ScrollDelta::Lines(_));
        let nav = self.core.nav.clone();
        let width = nav.read(cx).lane_width + lines * WHEEL_WIDTH_STEP;
        nav.update(cx, |nav, cx| nav.set_lane_width(width, persist, cx));
        self.width_save_generation += 1;
        if !persist {
            let generation = self.width_save_generation;
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.width_save_generation == generation {
                        this.core.nav.read(cx).save();
                    }
                });
            })
            .detach();
        }
        true
    }

    fn lane_decor(&self, lane: &Lane) -> LaneDecor {
        self.lanes_cache
            .borrow()
            .as_ref()
            .and_then(|cache| cache.decor.get(&lane.key))
            .copied()
            .unwrap_or_default()
    }

    fn lane_style(&self, lane: &Lane, cx: &App) -> LaneStyle {
        let p = palette();
        let nav = self.core.nav.read(cx);
        let decor = self.lane_decor(lane);
        let base = decor
            .swatch
            .filter(|_| nav.colored_lanes)
            .map(|index| rgb(theme::LANE_COLORS[index].1));
        let tint =
            |surface, amount| base.map_or(surface, |base| colors::tint(surface, base, amount));
        let label_header = tint(p.surface1, theme::LANE_HEADER_TINT);
        // Recency owns the header hue; label identity remains in the dot/column.
        let header = if nav.recency_headers {
            decor
                .activity
                .score(now_millis())
                .map(colors::recency)
                .map_or(label_header, |age| {
                    colors::tint(p.surface1, age, theme::LANE_RECENCY_TINT)
                })
        } else {
            label_header
        };
        LaneStyle {
            column: tint(p.surface1, theme::LANE_COLUMN_TINT),
            column_hover: tint(p.surface2, theme::LANE_COLUMN_TINT),
            header,
            card: tint(p.surface0, theme::LANE_CARD_TINT),
            card_hover: tint(p.surface2, theme::LANE_CARD_TINT),
            swatch: decor.swatch,
            editable: nav.colored_lanes,
        }
    }

    fn lane_dot(&self, lane: &Lane, width: Pixels, style: LaneStyle) -> AnyElement {
        if let Some(index) = style.swatch {
            if style.editable {
                let nav = self.core.nav.clone();
                let key = lane.key.clone();
                lane_color_picker::picker(&lane.key, index, width, move |index, _, cx| {
                    nav.update(cx, |nav, cx| nav.set_lane_color(&key, index, cx));
                })
                .into_any_element()
            } else {
                dot(rgb(theme::LANE_COLORS[index].1))
            }
        } else {
            color_dot(lane)
        }
    }

    /// The column view for `key`, notified only when `inputs` change what it
    /// shows. Notifying here, mid-draw, still counts for this frame: the
    /// cached view checks its dirty flag when it is prepainted, after this.
    fn column_view(
        &mut self,
        key: &str,
        inputs: ColumnInputs,
        cx: &mut Context<Self>,
    ) -> Entity<ColumnView> {
        let board = cx.entity().downgrade();
        let view = self
            .columns
            .entry(key.to_string())
            .or_insert_with(|| {
                cx.new(|_| ColumnView {
                    board,
                    inputs: None,
                    session_watches: Vec::new(),
                    watched_lane: None,
                })
            })
            .clone();
        let store_handle = self.core.store.clone();
        view.update(cx, |column, cx| {
            column.watch_sessions(&inputs.lanes[inputs.ix], &store_handle, cx);
            let changed = !column
                .inputs
                .as_ref()
                .is_some_and(|old| old.shows_the_same_as(&inputs));
            column.inputs = Some(inputs);
            if changed {
                cx.notify();
            }
        });
        view
    }

    /// A hash of the board state a column reads besides its lane and style.
    /// Anything a column renders from `self` must be in here, or the column
    /// shows it stale.
    fn column_fingerprint(&self, lane: &Lane, style: &LaneStyle, cx: &App) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        let key = lane.key.as_str();
        let focused_here = self.focused_lane.as_deref() == Some(key);
        focused_here.hash(&mut hash);
        if focused_here {
            self.focused_agent.hash(&mut hash);
            self.focused_row.hash(&mut hash);
            self.draft_focused.hash(&mut hash);
        }
        let expanded = self.expanded.get(key);
        expanded.hash(&mut hash);
        // Pins show on box headers even when nothing moved.
        self.core.nav.read(cx).ordering_revision.hash(&mut hash);
        // A card dragged inside its box shows where it would land.
        self.card_drop
            .as_ref()
            .filter(|(host, agent, _)| {
                lane.cards
                    .iter()
                    .any(|card| card.host == *host && card.agent_id == *agent)
            })
            .hash(&mut hash);
        // A state singled out from the title bar outlines its agents.
        self.core.nav.read(cx).state_highlight.hash(&mut hash);
        // Find outlines matches and dims the rest.
        self.find
            .as_ref()
            .map(|find| {
                (
                    Rc::as_ptr(&find.matches) as usize,
                    find.cover.as_ref().map(|cover| Rc::as_ptr(cover) as usize),
                    find.pointed(),
                    find.active,
                )
            })
            .hash(&mut hash);
        self.lane_drag
            .as_ref()
            .is_some_and(|drag| drag.key == key)
            .hash(&mut hash);
        // The selected workspace's box is outlined or tinted wherever it is.
        self.selected_workspace(cx).hash(&mut hash);
        self.empty_open.contains(key).hash(&mut hash);
        self.row_generation(lane, cx).hash(&mut hash);
        self.expanded_terminals.get(key).hash(&mut hash);
        self.focus_mode
            .as_ref()
            .filter(|(lane, ..)| lane == key)
            .hash(&mut hash);
        self.core.nav.read(cx).box_single_workspaces.hash(&mut hash);
        self.box_picker
            .as_ref()
            .filter(|open| open.lane == key)
            .map(|open| (&open.host, &open.workspace))
            .hash(&mut hash);
        self.relabel_pending.is_some().hash(&mut hash);
        expanded
            .is_some_and(|(host, agent)| self.rename_editor(key, host, agent, cx).is_some())
            .hash(&mut hash);
        // A workspace's box shows its rename editor.
        self.renaming
            .as_ref()
            .map(|(editor, _)| editor.entity_id())
            .hash(&mut hash);
        let draft = self.draft.as_ref().filter(|draft| draft.lane == key);
        if let Some(draft) = draft {
            draft.workspace.hash(&mut hash);
            draft.provider.hash(&mut hash);
            draft.model.hash(&mut hash);
            draft.thinking.hash(&mut hash);
            draft.creating.hash(&mut hash);
            draft.error.hash(&mut hash);
        }
        if expanded.is_some() || draft.is_some() {
            // An open card or draft reads live store state (archive
            // availability, providers) beyond its lane's cards.
            self.relabel_pending.is_some().hash(&mut hash);
            self.core.store.read(cx).revision.hash(&mut hash);
        }
        // Session membership changes the preview's source: streamed text
        // when present, otherwise the attention/activity fallback.
        self.core.store.read(cx).sessions_revision().hash(&mut hash);
        // Cards say how long ago they last changed.
        (now_millis() / 60_000).hash(&mut hash);
        for color in [
            style.column,
            style.column_hover,
            style.header,
            style.card,
            style.card_hover,
        ] {
            for channel in [color.r, color.g, color.b, color.a] {
                channel.to_bits().hash(&mut hash);
            }
        }
        style.swatch.hash(&mut hash);
        style.editable.hash(&mut hash);
        hash.finish()
    }

    fn column(
        &mut self,
        lanes: &LaneSet,
        lane_ix: usize,
        width: Pixels,
        style: LaneStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let lane = &lanes[lane_ix];
        let expanded = self.expanded.get(&lane.key).cloned();
        // Lanes keep their padding between each other; inside one, boxes
        // and cards run nearly edge to edge.
        let mut body = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap(px(CARD_GAP))
            .px(px(4.))
            .py(px(6.))
            .relative();
        if self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.lane == lane.key)
        {
            body = body.child(self.draft_card(lane, style, cx));
        }
        let open_terminal = self.expanded_terminals.get(&lane.key).cloned();
        if let Some((host, terminal)) = open_terminal {
            let box_single = self.core.nav.read(cx).box_single_workspaces;
            let rows = self.rows(lane, box_single, cx);
            let at = {
                let store = self.core.store.read(cx);
                rows.iter().position(|row| match *row {
                    Row::Terminal { owner, index, .. } => {
                        let (h, w) = owner.workspace(lane);
                        h == host
                            && store.workspace_terminal_ids(h, w).nth(index) == Some(&terminal)
                    }
                    _ => false,
                })
            };
            match at {
                Some(at) => {
                    // The terminal opens in place inside its workspace's box.
                    let start = rows[..at]
                        .iter()
                        .rposition(|row| {
                            matches!(row, Row::Header { .. } | Row::Empty { boxed: true, .. })
                        })
                        .unwrap_or(at);
                    let end = rows[at..]
                        .iter()
                        .position(|row| row.ends_box())
                        .map_or(rows.len(), |ix| at + ix + 1);
                    let lane = &lanes[lane_ix];
                    if start > 0 {
                        body = body.child(self.compact_strip(
                            lane,
                            &rows[..start],
                            "above",
                            style,
                            window,
                            cx,
                        ));
                    }
                    body = body.child(self.terminal_box(
                        lane,
                        &rows[start..end],
                        at - start,
                        &host,
                        &terminal,
                        style,
                        window,
                        cx,
                    ));
                    if end < rows.len() {
                        body = body.child(self.compact_strip(
                            lane,
                            &rows[end..],
                            "below",
                            style,
                            window,
                            cx,
                        ));
                    }
                }
                None => {
                    // It closed or its list has not loaded: the lane shows
                    // its cards.
                    body = body.child(self.card_list(lanes, lane_ix, style, cx));
                }
            }
        } else if let Some((host, agent)) = expanded {
            let box_single = self.core.nav.read(cx).box_single_workspaces;
            let rows = self.rows(lane, box_single, cx);
            // The expanded card opens in place inside its workspace's box,
            // with the rest of the lane compact above and below.
            let at = rows.iter().position(|row| {
                matches!(row, Row::Card { card, .. }
                    if lane.cards[*card].host == host && lane.cards[*card].agent_id == agent)
            });
            let (start, end) = match at {
                Some(at) => {
                    let start = match rows[at] {
                        Row::Card { boxed: true, .. } => rows[..at]
                            .iter()
                            .rposition(|row| matches!(row, Row::Header { .. }))
                            .unwrap_or(at),
                        _ => at,
                    };
                    let end = rows[at..]
                        .iter()
                        .position(|row| row.ends_box())
                        .map_or(rows.len(), |ix| at + ix + 1);
                    (start, end)
                }
                None => (0, 0),
            };
            let lane = &lanes[lane_ix];
            if start > 0 {
                body = body.child(self.compact_strip(
                    lane,
                    &rows[..start],
                    "above",
                    style,
                    window,
                    cx,
                ));
            }
            body = body.child(self.expanded_box(
                lane,
                &rows[start..end],
                &host,
                &agent,
                style,
                window,
                cx,
            ));
            if end < rows.len() {
                body =
                    body.child(self.compact_strip(lane, &rows[end..], "below", style, window, cx));
            }
        } else {
            body = body.child(self.card_list(lanes, lane_ix, style, cx));
        }
        // A box's label picker sizes itself to the lane.
        if let Some(open) = self
            .box_picker
            .as_ref()
            .filter(|open| open.lane == lane.key)
        {
            let picker = open.picker.clone();
            body = body.child(
                canvas(
                    move |rect, _, cx| {
                        picker.update(cx, |picker, cx| picker.set_bounds(rect, cx));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        }

        let dragged_here = self
            .lane_drag
            .as_ref()
            .is_some_and(|drag| drag.key == lane.key);
        let resizable = self.expanded.contains_key(&lane.key);
        let resize = {
            let key = lane.key.clone();
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.resizing = Some(ColumnResize {
                    lane: key.clone(),
                    start_x: event.position.x,
                    start_width: width,
                });
                this.hold_board_length = true;
                cx.stop_propagation();
                cx.notify();
            })
        };
        div()
            .id(eid("lane", &lane.key))
            .w(width)
            .flex_shrink_0()
            .h_full()
            .rounded(px(8.))
            .border_1()
            .border_color(p.border)
            .bg(style.column)
            .flex()
            .flex_col()
            .overflow_hidden()
            .relative()
            .child(self.column_header(lane, width, style, cx))
            .child(body)
            // A lane dragged over this one takes its place, and the lanes
            // between move aside; where it will land shows as a faded slot.
            .when(dragged_here, |column| {
                column.opacity(0.45).border_dashed().border_color(p.accent)
            })
            .on_drag_move({
                let over = lane.key.clone();
                cx.listener(move |this, event: &DragMoveEvent<LaneDrag>, _, cx| {
                    if event.bounds.contains(&event.event.position) {
                        let key = event.drag(cx).key.clone();
                        this.preview_lane_drag(&key, &over, cx);
                    }
                })
            })
            .on_drop(cx.listener(move |this, dragged: &LaneDrag, _, cx| {
                this.drop_lane(&dragged.key, cx);
            }))
            // An expanded column's own width: drag its right edge.
            .when(resizable, |column| {
                column.child(
                    div()
                        .id(eid("lane-resize", &lane.key))
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .w(px(6.))
                        .cursor(CursorStyle::ResizeLeftRight)
                        .hover(|style| style.bg(p.border))
                        .on_mouse_down(MouseButton::Left, resize)
                        .tooltip(|window, cx| {
                            Tooltip::new("Drag to resize this lane while its card is expanded")
                                .build(window, cx)
                        }),
                )
            })
            .into_any_element()
    }

    /// Moves a lane one place left or right in the board's current order.
    fn reorder_lane(&mut self, key: &str, step: isize, cx: &mut Context<Self>) {
        let keys: Vec<String> = self.lanes(cx).iter().map(|lane| lane.key.clone()).collect();
        self.core
            .nav
            .update(cx, |nav, cx| nav.reorder_lane(&keys, key, step, cx));
    }

    /// While a lane is dragged over `over`, shows it there: it takes that
    /// lane's place and the lanes between move aside.
    fn preview_lane_drag(&mut self, key: &str, over: &str, cx: &mut Context<Self>) {
        if key == over {
            return;
        }
        let shown = match &self.lane_drag {
            Some(drag) if drag.key == key => drag.order.clone(),
            _ => self
                .arranged(&self.visible_lanes(cx), cx)
                .iter()
                .map(|lane| lane.key.clone())
                .collect(),
        };
        if let Some(order) = crate::app::nav::move_lane_to(&shown, key, over) {
            self.lane_drag = Some(LaneDragPreview {
                key: key.to_string(),
                order,
            });
            cx.notify();
        }
    }

    /// Keeps the order a dragged lane was shown in when it is dropped.
    fn drop_lane(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(drag) = self.lane_drag.take().filter(|drag| drag.key == key) else {
            return;
        };
        let keys: Vec<String> = self.lanes(cx).iter().map(|lane| lane.key.clone()).collect();
        self.core.nav.update(cx, |nav, cx| {
            nav.place_lane_as_shown(&keys, &drag.order, key, cx)
        });
        cx.notify();
    }

    /// A lane let go anywhere but on a lane goes back where it was. Deferred
    /// so a drop on a lane, handled by the same release, is seen first.
    fn cancel_lane_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lane_drag.is_none() && self.card_drop.is_none() {
            return;
        }
        cx.defer_in(window, |this, _, cx| {
            let lane = this.lane_drag.take().is_some();
            let card = this.card_drop.take().is_some();
            if lane || card {
                cx.notify();
            }
        });
    }

    /// A card in a workspace's box can be dragged to another place in that
    /// box; the order is kept. Subagents go with the agent that started them.
    fn reorderable(
        &self,
        shell: Stateful<Div>,
        lane: &Lane,
        card: &LaneCard,
        frame: Frame,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let Some(workspace) = card
            .workspace_id
            .clone()
            .filter(|_| matches!(frame, Frame::Boxed { .. }))
        else {
            return shell;
        };
        let drag = CardDrag {
            lane: lane.key.clone(),
            host: card.host.clone(),
            agent: card.agent_id.clone(),
            workspace: workspace.clone(),
            title: card.title.clone().into(),
        };
        let (lane_key, host, agent) = (lane.key.clone(), card.host.clone(), card.agent_id.clone());
        shell
            .on_drag(drag, |dragged, offset, _, cx| {
                let title = dragged.title.clone();
                cx.new(|_| crate::toolbars::customize::DragChip::new(title, offset))
            })
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<CardDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let dragged = event.drag(cx);
                    let here = dragged.lane == lane_key
                        && dragged.host == host
                        && dragged.workspace == workspace
                        && dragged.agent != agent;
                    let target = here.then(|| {
                        let before = event.event.position.y < event.bounds.center().y;
                        (host.clone(), agent.clone(), before)
                    });
                    if this.card_drop != target {
                        this.card_drop = target;
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(|this, dragged: &CardDrag, _, cx| {
                this.drop_card(dragged, cx);
            }))
    }

    /// Puts a dropped card where the line showed, keeping that order.
    fn drop_card(&mut self, dragged: &CardDrag, cx: &mut Context<Self>) {
        let Some((host, target, before)) = self.card_drop.take() else {
            return;
        };
        let lanes = self.arranged(&self.visible_lanes(cx), cx);
        let Some(lane) = lanes.iter().find(|lane| lane.key == dragged.lane) else {
            return;
        };
        let family = Family::of(lane);
        let mut order: Vec<String> = lane
            .cards
            .iter()
            .enumerate()
            .filter(|(ix, card)| {
                family.root[*ix]
                    && card.host == host
                    && card.workspace_id.as_deref() == Some(dragged.workspace.as_str())
            })
            .map(|(_, card)| card.agent_id.clone())
            .filter(|id| *id != dragged.agent)
            .collect();
        let at = order
            .iter()
            .position(|id| *id == target)
            .map_or(order.len(), |at| if before { at } else { at + 1 });
        order.insert(at, dragged.agent.clone());
        let workspace = dragged.workspace.clone();
        self.core.nav.update(cx, |nav, cx| {
            nav.set_card_order(&host, &workspace, order, cx)
        });
        cx.notify();
    }

    /// A row with the line where a dragged card would land, above or below.
    fn drop_line(&self, lane: &Lane, row: &Row, element: AnyElement) -> AnyElement {
        let Some((host, agent, before)) = &self.card_drop else {
            return element;
        };
        let Row::Card { card, .. } = row else {
            return element;
        };
        let card = &lane.cards[*card];
        if card.host != *host || card.agent_id != *agent {
            return element;
        }
        let line = div()
            .absolute()
            .left(px(4.))
            .right(px(4.))
            .h(px(2.))
            .rounded(px(1.))
            .bg(palette().accent);
        div()
            .relative()
            .child(element)
            .child(if *before {
                line.top(px(-2.))
            } else {
                line.bottom(px(-2.))
            })
            .into_any_element()
    }

    /// Moves a lane to the front or the back of the board's current order.
    fn move_lane_to_end(&mut self, key: &str, first: bool, cx: &mut Context<Self>) {
        let keys: Vec<String> = self.lanes(cx).iter().map(|lane| lane.key.clone()).collect();
        self.core
            .nav
            .update(cx, |nav, cx| nav.move_lane_to_end(&keys, key, first, cx));
    }

    fn column_header(
        &self,
        lane: &Rc<Lane>,
        width: Pixels,
        style: LaneStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let narrow = width < px(NARROW_WIDTH);
        let counts = lane.bucket_counts();
        let key = lane.key.clone();
        let count = lane.cards.len().to_string();
        let fit = HeaderFit::for_width(width.as_f32(), lane.is_combination(), count.len());
        // The order is read when clicked, so a header need not redraw each
        // time another lane moves.
        let move_first = {
            let key = key.clone();
            Button::new(eid("lane-first", &lane.key))
                .icon(Icon::empty().path("icons/chevron-first.svg"))
                .ghost()
                .xsmall()
                .tooltip("Move lane to the front")
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.move_lane_to_end(&key, true, cx);
                }))
        };
        let move_last = {
            let key = key.clone();
            Button::new(eid("lane-last", &lane.key))
                .icon(Icon::empty().path("icons/chevron-last.svg"))
                .ghost()
                .xsmall()
                .tooltip("Move lane to the end")
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.move_lane_to_end(&key, false, cx);
                }))
        };
        let move_left = {
            let key = key.clone();
            Button::new(eid("lane-left", &lane.key))
                .icon(Icon::empty().path("icons/chevron-left.svg"))
                .ghost()
                .xsmall()
                .tooltip("Move lane left")
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.reorder_lane(&key, -1, cx);
                }))
        };
        let move_right = {
            let key = key.clone();
            Button::new(eid("lane-right", &lane.key))
                .icon(Icon::empty().path("icons/chevron-right.svg"))
                .ghost()
                .xsmall()
                .tooltip("Move lane right")
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.reorder_lane(&key, 1, cx);
                }))
        };
        // Between left and right: drag the lane onto another to move it there.
        let grip = div()
            .id(eid("lane-grip", &lane.key))
            .w(px(14.))
            .h(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .cursor(ui::grab_cursor())
            .hover(|grip| grip.bg(p.surface3))
            .child(ui::icon("grip-vertical", 12., p.muted))
            .tooltip(|window, cx| Tooltip::new("Drag to move this lane").build(window, cx))
            .on_drag(
                LaneDrag {
                    key: key.clone(),
                    title: lane.title.clone().into(),
                },
                |dragged, offset, _, cx| {
                    let title = dragged.title.clone();
                    cx.new(|_| crate::toolbars::customize::DragChip::new(title, offset))
                },
            );
        let collapse = {
            let key = key.clone();
            Button::new(eid("lane-collapse", &lane.key))
                .icon(Icon::empty().path("icons/fold-horizontal.svg"))
                .ghost()
                .xsmall()
                .accessibility_label(format!("Collapse {} to tabs", lane.title))
                .tooltip("Collapse lane into the tabs above")
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.hide_lane(&key, window, cx);
                }))
        };
        let add = {
            // Shared, not copied: a lane holds all its cards.
            let lane = Rc::clone(lane);
            Button::new(eid("lane-add", &lane.key))
                .icon(Icon::empty().path("icons/plus.svg"))
                .ghost()
                .xsmall()
                .tooltip("New agent in this lane")
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_draft(&lane, window, cx);
                }))
        };
        div()
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            .overflow_hidden()
            .bg(style.header)
            .p(px(HEADER_PADDING))
            .border_b_1()
            .border_color(p.border)
            .flex()
            .flex_col()
            .gap(px(HEADER_ROW_GAP))
            // A plain wheel over a header scrolls across columns; over the
            // cards it scrolls the lane.
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                if this.wheel_resize(event, cx) {
                    cx.stop_propagation();
                    return;
                }
                let dy = event.delta.pixel_delta(px(16.)).y / px(1.);
                if dy != 0. {
                    this.nudge_scroll(-dy, cx);
                    cx.stop_propagation();
                }
            }))
            .group("lane-header")
            .relative()
            .child(
                div()
                    .h(px(HEADER_TITLE_HEIGHT))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(HEADER_ROW_GAP))
                    .child(self.lane_dot(lane, width, style))
                    .when(lane.is_combination(), |row| {
                        row.child(
                            div()
                                .id(eid("lane-combination", &lane.key))
                                .flex_shrink_0()
                                .child(ui::icon("layers", 12., p.extra_muted))
                                .tooltip(|window, cx| {
                                    Tooltip::new("Exact label combination").build(window, cx)
                                }),
                        )
                    })
                    // New agent follows the name; a long name truncates
                    // before it rather than pushing it out.
                    .child(
                        div()
                            .id(eid("lane-title", &lane.key))
                            .min_w(px(0.))
                            .truncate()
                            .px(px(3.))
                            .mx(px(-3.))
                            .rounded(px(4.))
                            .text_color(p.foreground)
                            .hover(|title| title.bg(p.surface3))
                            .tooltip(lane_title_tooltip(&self.core, lane))
                            .child(lane.title.clone()),
                    )
                    .when(fit.add, |row| row.child(add))
                    .child(div().flex_1())
                    .when(fit.count, |row| {
                        row.child(
                            div()
                                .flex_shrink_0()
                                .text_size(size::sm())
                                .text_color(p.extra_muted)
                                .child(count),
                        )
                    })
                    .child(
                        div()
                            .id(eid("lane-header-buttons", &lane.key))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .when(fit.ends, |buttons| buttons.child(move_first))
                            .when(fit.steps, |buttons| buttons.child(move_left))
                            .child(grip)
                            .when(fit.steps, |buttons| buttons.child(move_right))
                            .when(fit.ends, |buttons| buttons.child(move_last))
                            .when(fit.collapse, |buttons| buttons.child(collapse)),
                    ),
            )
            .child(
                div()
                    .h(px(HEADER_STATUS_HEIGHT))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .line_height(px(16.))
                    .flex()
                    .gap(px(8.))
                    .children(counts.iter().filter(|(_, count)| *count > 0).map(
                        |(bucket, count)| {
                            let color = ui::bucket_color(*bucket).unwrap_or(p.extra_muted);
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .text_size(size::sm())
                                .text_color(color)
                                .child(div().size(px(6.)).rounded_full().bg(color))
                                .child(count.to_string())
                        },
                    )),
            )
            .child(header_hosts(lane, narrow))
            .into_any_element()
    }

    /// A virtual list: only the cards in view (plus overdraw) are built and
    /// laid out, however long the lane is.
    fn card_list(
        &mut self,
        lanes: &LaneSet,
        lane_ix: usize,
        style: LaneStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let box_single = self.core.nav.read(cx).box_single_workspaces;
        let key = &lanes[lane_ix].key;
        let mut entry = self.lists.remove(key).unwrap_or_else(LaneList::new);
        entry.sync(&lanes[lane_ix], self, box_single, cx);
        let state = entry.state.clone();
        let rows = entry.rows.clone();
        self.lists.insert(key.clone(), entry);
        let lanes = lanes.clone();
        list(
            state,
            cx.processor(move |this, ix: usize, window, cx| {
                let lane = &lanes[lane_ix];
                let Some(&row) = rows.get(ix) else {
                    return div().into_any_element();
                };
                div()
                    .w_full()
                    .when(row.ends_box(), |item| item.pb(px(CARD_GAP)))
                    // Items handle the wheel before the list does, so this is
                    // where Ctrl+wheel over cards can resize instead of scroll.
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        if this.wheel_resize(event, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .child(this.row_element(lane, row, false, true, style, window, cx))
                    .into_any_element()
            }),
        )
        .flex_1()
        .min_h(px(0.))
        .w_full()
        .into_any_element()
    }

    fn full_card(
        &self,
        lane: &Lane,
        card: &LaneCard,
        style: LaneStyle,
        frame: Frame,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let look = self.box_look(&lane.key, card_workspace(card), style, cx);
        let store = self.core.store.read(cx);
        // Borrowed, not cloned: an agent carries its raw JSON, and this runs
        // for every visible card on every frame.
        let agent = store.fleet.agent(&card.host, &card.agent_id);
        let blurb = blurb(store, card, cx);
        let focused = self.card_focused(&lane.key, card);
        let lane_key = lane.key.clone();
        let host = card.host.clone();
        let agent_id = card.agent_id.clone();
        let scope = element_scope(&[&lane.key, &card.host, &card.agent_id]);
        let activity = activity::label(
            card.updated_at
                .as_deref()
                .or_else(|| agent.and_then(|agent| agent.created_at.as_deref())),
            now_millis(),
        );
        let footer = blurb.clone().unwrap_or(activity);
        let actions = agent.and_then(|agent| {
            crate::widgets::agent_controls::permission_buttons(
                &self.core, &card.host, agent, &scope,
            )
            .or_else(|| {
                crate::widgets::agent_controls::controls(&self.core, &card.host, agent, &scope)
            })
        });
        let content = div()
            .min_w(px(0.))
            .p(px(10.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .h(px(20.))
                    .flex_shrink_0()
                    .line_height(px(20.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(ui::agent_dot(card.bucket))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(p.foreground)
                            .child(card.title.clone()),
                    ),
            )
            .child(
                div()
                    .h(px(16.))
                    .flex_shrink_0()
                    .line_height(px(16.))
                    .text_size(size::sm())
                    .text_color(p.muted)
                    .truncate()
                    .child(meta(card, matches!(frame, Frame::Boxed { .. }))),
            )
            .child(
                div()
                    .h(px(24.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .line_height(px(16.))
                            .text_size(size::sm())
                            .truncate()
                            .text_color(if blurb.is_some() {
                                p.foreground
                            } else {
                                p.extra_muted
                            })
                            .when(blurb.is_none(), |d| d.italic())
                            .child(footer),
                    )
                    .when_some(actions, |d, actions| {
                        d.child(
                            div()
                                .id(eid_parts(
                                    "lane-actions",
                                    &[&lane.key, &card.host, &card.agent_id],
                                ))
                                .min_w(px(0.))
                                .max_w(gpui_kit::gpui::relative(0.75))
                                .overflow_x_scroll()
                                .restrict_scroll_to_axis()
                                .child(actions),
                        )
                    }),
            );
        let id = eid_parts("lane-card", &[&lane.key, &card.host, &card.agent_id]);
        let shell = card_frame(id, CARD_HEIGHT, focused, frame, look, content);
        self.reorderable(shell, lane, card, frame, cx)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.expand(&lane_key, &host, &agent_id, window, cx);
            }))
            .on_mouse_down(MouseButton::Right, self.card_menu_listener(card, cx))
            .into_any_element()
    }

    /// Right-click on a card opens the board's card menu for it.
    fn card_menu_listener(
        &self,
        card: &LaneCard,
        cx: &mut Context<Self>,
    ) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        let (host, agent) = (card.host.clone(), card.agent_id.clone());
        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
            this.open_card_menu(&host, &agent, event.position, window, cx);
            cx.stop_propagation();
        })
    }

    /// Rows of a lane with an expanded card, compact, in a short scrolling
    /// strip.
    fn compact_strip(
        &mut self,
        lane: &Rc<Lane>,
        rows: &[Row],
        which: &str,
        style: LaneStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut strip = div()
            .id(eid_parts("lane-strip-cards", &[&lane.key, which]))
            // In a short lane the strips give way to the open card, down to
            // a row each.
            .min_h(px(COMPACT_HEIGHT))
            .max_h(px(160.))
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for (ix, &row) in rows.iter().enumerate() {
            let gap = row.ends_box() && ix + 1 < rows.len();
            strip = strip.child(
                div()
                    .w_full()
                    .when(gap, |item| item.pb(px(CARD_GAP)))
                    .child(self.row_element(lane, row, true, true, style, window, cx)),
            );
        }
        strip.into_any_element()
    }

    /// One row of a lane, in the card list or, `compact`, beside an
    /// expanded card. `framed` rows draw their part of their box's outline;
    /// the expanded card's box draws its own.
    #[allow(clippy::too_many_arguments)]
    fn row_element(
        &mut self,
        lane: &Rc<Lane>,
        row: Row,
        compact: bool,
        framed: bool,
        style: LaneStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let element = self.row_content(lane, row, compact, framed, style, window, cx);
        let state = self.core.nav.read(cx).state_highlight;
        let element = state_row(lane, &row, element, state);
        let element = self.drop_line(lane, &row, element);
        self.find_row(lane, &row, element)
    }

    #[allow(clippy::too_many_arguments)]
    fn row_content(
        &mut self,
        lane: &Rc<Lane>,
        row: Row,
        compact: bool,
        framed: bool,
        style: LaneStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match row {
            Row::Header { card, .. } => self.box_header(lane, card, framed, style, window, cx),
            Row::Card {
                card,
                boxed,
                last,
                depth,
            } => {
                let frame = if boxed {
                    Frame::Boxed { last, framed }
                } else {
                    Frame::Alone
                };
                let card = &lane.cards[card];
                if compact {
                    self.compact_card(lane, card, style, frame, depth, cx)
                } else if depth > 0 {
                    self.subagent_card(lane, card, style, frame, depth, cx)
                } else {
                    self.full_card(lane, card, style, frame, cx)
                }
            }
            Row::Subagents {
                card,
                count,
                open,
                boxed,
                last,
            } => {
                let frame = if boxed {
                    Frame::Boxed { last, framed }
                } else {
                    Frame::Alone
                };
                self.subagents_fold(lane, card, count, open, style, frame, cx)
            }
            Row::Fold { count, open } => self.fold_row(lane, count, open, cx),
            Row::Empty { workspace, boxed } => {
                self.empty_row(lane, workspace, boxed, framed, style, window, cx)
            }
            Row::Terminal {
                owner,
                index,
                boxed,
                last,
            } => {
                let frame = if boxed {
                    Frame::Boxed { last, framed }
                } else {
                    Frame::Alone
                };
                self.terminal_row(lane, owner, index, style, frame, cx)
            }
        }
    }

    fn compact_card(
        &self,
        lane: &Lane,
        card: &LaneCard,
        style: LaneStyle,
        frame: Frame,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let look = self.box_look(&lane.key, card_workspace(card), style, cx);
        let focused = self.card_focused(&lane.key, card);
        let lane_key = lane.key.clone();
        let host = card.host.clone();
        let agent_id = card.agent_id.clone();
        let content = div()
            .pl(px(8. + SUBAGENT_INDENT * depth.saturating_sub(1) as f32))
            .pr(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .when(depth > 0, |row| row.child(subagent_mark()))
            .child(ui::agent_dot(card.bucket))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(size::sm())
                    .text_color(p.foreground)
                    .child(card.title.clone()),
            );
        let id = eid_parts("lane-compact", &[&lane.key, &card.host, &card.agent_id]);
        let shell = card_frame(id, COMPACT_HEIGHT, focused, frame, look, content);
        let shell = if depth == 0 {
            self.reorderable(shell, lane, card, frame, cx)
        } else {
            shell
        };
        shell
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.expand(&lane_key, &host, &agent_id, window, cx);
            }))
            .on_mouse_down(MouseButton::Right, self.card_menu_listener(card, cx))
            .into_any_element()
    }

    /// A subagent in the card list: indented under the agent that started
    /// it, on two lines, its title and its latest words or controls.
    fn subagent_card(
        &self,
        lane: &Lane,
        card: &LaneCard,
        style: LaneStyle,
        frame: Frame,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let look = self.box_look(&lane.key, card_workspace(card), style, cx);
        let store = self.core.store.read(cx);
        let agent = store.fleet.agent(&card.host, &card.agent_id);
        let blurb = blurb(store, card, cx);
        let focused = self.card_focused(&lane.key, card);
        let scope = element_scope(&[&lane.key, &card.host, &card.agent_id]);
        let footer = blurb.clone().unwrap_or_else(|| {
            activity::label(
                card.updated_at
                    .as_deref()
                    .or_else(|| agent.and_then(|agent| agent.created_at.as_deref())),
                now_millis(),
            )
        });
        let actions = agent.and_then(|agent| {
            crate::widgets::agent_controls::permission_buttons(
                &self.core, &card.host, agent, &scope,
            )
            .or_else(|| {
                crate::widgets::agent_controls::controls(&self.core, &card.host, agent, &scope)
            })
        });
        let (lane_key, host, agent_id) =
            (lane.key.clone(), card.host.clone(), card.agent_id.clone());
        let content = div()
            .min_w(px(0.))
            .pl(px(10. + SUBAGENT_INDENT * (depth - 1) as f32))
            .pr(px(10.))
            .py(px(4.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .h(px(20.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(subagent_mark())
                    .child(ui::agent_dot(card.bucket))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(size::sm())
                            .text_color(p.foreground)
                            .child(card.title.clone()),
                    ),
            )
            .child(
                div()
                    .h(px(20.))
                    .flex_shrink_0()
                    .pl(px(18.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(size::sm())
                            .text_color(if blurb.is_some() {
                                p.muted
                            } else {
                                p.extra_muted
                            })
                            .when(blurb.is_none(), |d| d.italic())
                            .child(footer),
                    )
                    .when_some(actions, |row, actions| {
                        row.child(
                            div()
                                .id(eid_parts(
                                    "lane-sub-actions",
                                    &[&lane.key, &card.host, &card.agent_id],
                                ))
                                .min_w(px(0.))
                                .max_w(gpui_kit::gpui::relative(0.75))
                                .overflow_x_scroll()
                                .restrict_scroll_to_axis()
                                .child(actions),
                        )
                    }),
            );
        let id = eid_parts("lane-subagent", &[&lane.key, &card.host, &card.agent_id]);
        card_frame(id, SUBAGENT_HEIGHT, focused, frame, look, content)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.expand(&lane_key, &host, &agent_id, window, cx);
            }))
            .on_mouse_down(MouseButton::Right, self.card_menu_listener(card, cx))
            .into_any_element()
    }

    /// Folds away an agent's subagents: how many, and the colour of the
    /// most urgent. Those that need the user stay shown below it.
    #[allow(clippy::too_many_arguments)]
    fn subagents_fold(
        &self,
        lane: &Lane,
        card: usize,
        count: usize,
        open: bool,
        style: LaneStyle,
        frame: Frame,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let parent = &lane.cards[card];
        let look = self.box_look(&lane.key, card_workspace(parent), style, cx);
        let focus = RowFocus::Subagents(parent.host.clone(), parent.agent_id.clone());
        let focused = self.focused_lane.as_deref() == Some(lane.key.as_str())
            && self.focused_row.as_ref() == Some(&focus);
        let urgent = lane.cards[card + 1..card + 1 + count]
            .iter()
            .map(|sub| sub.bucket)
            .min_by_key(|bucket| {
                fleet::Bucket::GROUP_ORDER
                    .iter()
                    .position(|other| other == bucket)
            });
        let label = match count {
            1 => "1 subagent".to_string(),
            n => format!("{n} subagents"),
        };
        let content = div()
            .pl(px(8.))
            .pr(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(size::sm())
            .text_color(p.muted)
            .child(subagent_mark())
            .child(ui::icon(
                if open {
                    "chevron-down"
                } else {
                    "chevron-right"
                },
                12.,
                p.extra_muted,
            ))
            .children(urgent.filter(|_| !open).map(ui::agent_dot))
            .child(div().flex_1().min_w(px(0.)).truncate().child(label));
        let id = eid_parts(
            "lane-subagents",
            &[&lane.key, &parent.host, &parent.agent_id],
        );
        let (lane_key, host, agent) = (
            lane.key.clone(),
            parent.host.clone(),
            parent.agent_id.clone(),
        );
        card_frame(id, COMPACT_HEIGHT, focused, frame, look, content)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_subagents(&lane_key, &host, &agent, cx);
            }))
            .into_any_element()
    }

    /// A workspace's terminal: its name, and whether it has exited. A click
    /// opens it in the lane.
    fn terminal_row(
        &self,
        lane: &Lane,
        owner: Owner,
        index: usize,
        style: LaneStyle,
        frame: Frame,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let (host, workspace) = owner.workspace(lane);
        let store = self.core.store.read(cx);
        let Some(id) = store
            .workspace_terminal_ids(host, workspace)
            .nth(index)
            .cloned()
        else {
            return div().h(px(COMPACT_HEIGHT)).into_any_element();
        };
        let label = store.terminal_label(host, &id, cx);
        let gone = store
            .terminal_session(host, &id)
            .and_then(|session| session.read(cx).error.clone());
        let look = self.box_look(&lane.key, Some((host, workspace)), style, cx);
        let open = self.expanded_terminals.get(&lane.key) == Some(&(host.to_string(), id.clone()));
        let focused = open
            || (self.focused_lane.as_deref() == Some(lane.key.as_str())
                && self.focused_row == Some(RowFocus::Terminal(host.to_string(), id.clone())));
        let content = div()
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(size::sm())
            .child(ui::icon("square-terminal", 12., p.muted))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_color(p.foreground)
                    .child(label),
            )
            .when_some(gone, |row, _| {
                row.child(div().text_color(p.extra_muted).child("exited"))
            });
        let (lane_key, host, terminal) = (lane.key.clone(), host.to_string(), id.clone());
        let element_id = eid_parts("lane-terminal", &[&lane_key, &host, &terminal]);
        card_frame(element_id, COMPACT_HEIGHT, focused, frame, look, content)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.expand_terminal(&lane_key, &host, &terminal, window, cx);
            }))
            .into_any_element()
    }

    /// How a workspace's box looks in `lane`: outlined where it is
    /// selected, tinted where it is selected in another lane.
    fn box_look(
        &self,
        lane: &str,
        workspace: Option<(&str, &str)>,
        style: LaneStyle,
        cx: &App,
    ) -> BoxLook {
        let p = palette();
        let selected = workspace.is_some()
            && self
                .selected_workspace(cx)
                .as_ref()
                .map(|(host, workspace)| (host.as_str(), workspace.as_str()))
                == workspace;
        let here = self.focused_lane.as_deref() == Some(lane);
        let tinted = selected && !here;
        let tint = |color| {
            if tinted {
                colors::tint(color, p.accent, theme::LANE_BOX_TINT)
            } else {
                color
            }
        };
        BoxLook {
            border: if selected && here {
                p.accent.into()
            } else {
                p.border.into()
            },
            bg: tint(style.card),
            hover: tint(style.card_hover),
            outlined: selected && here,
            tinted,
        }
    }

    /// Who a workspace box relabels from: the lane's expanded card when it
    /// is in the box, so it is followed to its new lane; else the box's
    /// first card.
    fn box_agent(&self, lane: &Lane, card: &LaneCard) -> String {
        self.expanded
            .get(&lane.key)
            .filter(|(host, agent)| {
                lane.cards.iter().any(|other| {
                    &other.host == host
                        && &other.agent_id == agent
                        && card_workspace(other) == card_workspace(card)
                })
            })
            .map_or_else(|| card.agent_id.clone(), |(_, agent)| agent.clone())
    }

    /// A workspace box's header: its name and host, and on hover or while
    /// selected, its actions.
    fn box_header(
        &mut self,
        lane: &Rc<Lane>,
        card: usize,
        framed: bool,
        style: LaneStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let first = &lane.cards[card];
        let host = first.host.clone();
        let workspace = first.workspace_id.clone().unwrap_or_default();
        let look = self.box_look(&lane.key, card_workspace(first), style, cx);
        let name = lane
            .workspaces
            .iter()
            .find(|ws| ws.host == host && ws.workspace_id == workspace)
            .map(|ws| ws.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| first.workspace_name.clone());
        let agent = self.box_agent(lane, first);
        let actions = self.workspace_actions(
            lane,
            &host,
            &workspace,
            Some(&agent),
            look.outlined,
            window,
            cx,
        );
        div()
            .id(eid_parts("lane-box", &[&lane.key, &host, &workspace]))
            .h(px(BOX_HEADER_HEIGHT))
            .flex_shrink_0()
            .when(framed, |header| {
                header
                    .border_t_1()
                    .border_l_1()
                    .border_r_1()
                    .border_color(look.border)
                    .rounded_t(px(BOX_RADIUS))
            })
            .bg(look.bg)
            .pl(px(8.))
            .pr(px(2.))
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(size::sm())
            .group("lane-box-header")
            .hover(move |header| header.bg(look.hover))
            .when(
                self.core
                    .nav
                    .read(cx)
                    .pinned_workspaces
                    .contains(&workspace_key(&host, &workspace)),
                |header| {
                    header.child(
                        div()
                            .id(eid_parts(
                                "lane-box-pinned",
                                &[&lane.key, &host, &workspace],
                            ))
                            .flex_shrink_0()
                            .child(ui::icon("pin", 11., p.muted))
                            .tooltip(|window, cx| {
                                Tooltip::new("Pinned to the top of the lane").build(window, cx)
                            }),
                    )
                },
            )
            .child(self.workspace_name(&lane.key, &host, &workspace, name, p.muted, cx))
            .child(
                div()
                    .id(eid_parts("lane-box-host", &[&lane.key, &host, &workspace]))
                    .flex_shrink_0()
                    .max_w(px(96.))
                    .truncate()
                    .text_color(p.extra_muted)
                    .tooltip(workspace_host_tooltip(&self.core, &host, &workspace))
                    .child(host.clone()),
            )
            .child(actions)
            .on_mouse_down(
                MouseButton::Right,
                self.workspace_menu_listener(lane, &host, &workspace, cx),
            )
            .into_any_element()
    }

    /// A workspace's actions: a new agent in it, relabel, and its menu, or
    /// for an empty workspace, archive. Shown on hover, or always when
    /// `shown`.
    #[allow(clippy::too_many_arguments)]
    fn workspace_actions(
        &mut self,
        lane: &Rc<Lane>,
        host: &str,
        workspace: &str,
        agent: Option<&str>,
        shown: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let parts = [lane.key.as_str(), host, workspace];
        let add = {
            let lane = Rc::clone(lane);
            let target = (host.to_string(), workspace.to_string());
            Button::new(eid_parts("lane-box-add", &parts))
                .icon(Icon::empty().path("icons/plus.svg"))
                .ghost()
                .xsmall()
                .tooltip("New agent in this workspace")
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_draft_in(&lane, Some(target.clone()), window, cx);
                }))
        };
        let supported = self
            .core
            .store
            .read(cx)
            .fleet
            .host(host)
            .is_some_and(|host| host.has("workspaceLabels"));
        let tag = Button::new(eid_parts("lane-box-relabel-button", &parts))
            .icon(Icon::empty().path("icons/tag.svg"))
            .ghost()
            .xsmall()
            .tooltip("Relabel this workspace: all its agents move together")
            .disabled(!supported || self.relabel_pending.is_some());
        let open_picker = self
            .box_picker
            .as_ref()
            .filter(|open| {
                open.lane == lane.key && open.host == host && open.workspace == workspace
            })
            .map(|open| open.picker.clone());
        let relabel = match open_picker {
            Some(picker) => {
                let focus = picker.read(cx).focus_handle(cx);
                let open = picker.read(cx).is_open();
                let closing = picker.clone();
                let board = cx.entity().downgrade();
                Popover::new(eid_parts("lane-box-relabel", &parts))
                    .anchor(Anchor::TopRight)
                    .open(open)
                    .p(px(0.))
                    .track_focus(&focus)
                    .trigger(tag)
                    .on_open_change(move |open, window, cx| {
                        if *open {
                            return;
                        }
                        closing.update(cx, |picker, cx| picker.set_open(false, window, cx));
                        let _ = board.update(cx, |board, cx| {
                            board.box_picker = None;
                            cx.notify();
                        });
                    })
                    .content(move |_, _, _| picker.clone())
                    .into_any_element()
            }
            None => {
                let (lane_key, host, workspace) =
                    (lane.key.clone(), host.to_string(), workspace.to_string());
                let agent = agent.map(str::to_string);
                tag.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_box_picker(
                        &lane_key,
                        &host,
                        agent.as_deref(),
                        &workspace,
                        window,
                        cx,
                    );
                }))
                .into_any_element()
            }
        };
        let picking = self.box_picker.as_ref().is_some_and(|open| {
            open.lane == lane.key && open.host == host && open.workspace == workspace
        });
        let last = if agent.is_some() {
            let lane = Rc::clone(lane);
            let (host, workspace) = (host.to_string(), workspace.to_string());
            Button::new(eid_parts("lane-box-menu", &parts))
                .icon(Icon::empty().path("icons/ellipsis-vertical.svg"))
                .ghost()
                .xsmall()
                .tooltip("Workspace actions")
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    let at = this.pointer;
                    this.open_workspace_menu(&lane, &host, &workspace, at, window, cx);
                }))
                .into_any_element()
        } else {
            let enabled =
                crate::workspace_archive::available(&self.core, host, workspace, None, cx);
            let (core, host, workspace) =
                (self.core.clone(), host.to_string(), workspace.to_string());
            Button::new(eid_parts("lane-box-archive", &parts))
                .icon(Icon::empty().path("icons/archive.svg"))
                .ghost()
                .xsmall()
                .disabled(!enabled)
                .tooltip("Archive this workspace")
                .on_click(move |_: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    crate::workspace_archive::show(&core, &host, &workspace, None, window, cx);
                })
                .into_any_element()
        };
        let _ = window;
        div()
            .id(eid_parts("lane-box-actions", &parts))
            .flex_shrink_0()
            .flex()
            .items_center()
            .text_color(p.muted)
            .when(!shown && !picking, |actions| {
                actions
                    .invisible()
                    .group_hover("lane-box-header", |style| style.visible())
            })
            .child(add)
            .child(relabel)
            .child(last)
            .into_any_element()
    }

    /// Right-click on a workspace opens its menu where the pointer is.
    fn workspace_menu_listener(
        &self,
        lane: &Rc<Lane>,
        host: &str,
        workspace: &str,
        cx: &mut Context<Self>,
    ) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        let lane = Rc::clone(lane);
        let (host, workspace) = (host.to_string(), workspace.to_string());
        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
            this.open_workspace_menu(&lane, &host, &workspace, event.position, window, cx);
            cx.stop_propagation();
        })
    }

    /// A workspace without agents: a dashed one-line box at the end of the
    /// lane.
    /// A workspace without agents. Alone it is a dashed line; with
    /// terminals (`boxed`) it heads their box, and unless `framed` the box
    /// draws its own outline.
    #[allow(clippy::too_many_arguments)]
    fn empty_row(
        &mut self,
        lane: &Rc<Lane>,
        ix: usize,
        boxed: bool,
        framed: bool,
        style: LaneStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let ws = &lane.workspaces[ix];
        let (host, workspace) = (ws.host.clone(), ws.workspace_id.clone());
        let look = self.box_look(&lane.key, Some((&host, &workspace)), style, cx);
        let actions =
            self.workspace_actions(lane, &host, &workspace, None, look.outlined, window, cx);
        let focus = RowFocus::Workspace(host.clone(), workspace.clone());
        let lane_key = lane.key.clone();
        let row = div()
            .id(eid_parts("lane-empty", &[&lane.key, &host, &workspace]))
            .flex_shrink_0();
        let row = if boxed {
            row.h(px(BOX_HEADER_HEIGHT))
                .bg(look.bg)
                .when(framed, |row| {
                    row.border_t_1()
                        .border_l_1()
                        .border_r_1()
                        .border_color(look.border)
                        .rounded_t(px(BOX_RADIUS))
                })
        } else {
            row.h(px(EMPTY_HEIGHT))
                .rounded(px(BOX_RADIUS))
                .border_1()
                .when(!look.outlined, |row| row.border_dashed())
                // A dashed line reads fainter than a solid one of the same colour.
                .border_color(if look.outlined {
                    look.border
                } else {
                    with_alpha(p.extra_muted, 0.6)
                })
                .when(look.tinted, |row| row.bg(look.bg))
        };
        row.pl(px(8.))
            .pr(px(2.))
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(size::sm())
            .text_color(p.extra_muted)
            .group("lane-box-header")
            .cursor_pointer()
            .hover(move |row| row.bg(look.hover).text_color(p.muted))
            .child(self.workspace_name(
                &lane.key,
                &host,
                &workspace,
                ws.name.clone(),
                p.extra_muted,
                cx,
            ))
            .child(
                div()
                    .id(eid_parts(
                        "lane-empty-host",
                        &[&lane.key, &host, &workspace],
                    ))
                    .flex_shrink_0()
                    .max_w(px(96.))
                    .truncate()
                    .tooltip(workspace_host_tooltip(&self.core, &host, &workspace))
                    .child(host.clone()),
            )
            .child(actions)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.focus_row(&lane_key, focus.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                self.workspace_menu_listener(lane, &host, &workspace, cx),
            )
            .into_any_element()
    }

    /// Folds away a lane's empty workspaces when there are many.
    fn fold_row(
        &self,
        lane: &Lane,
        count: usize,
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let focused = self.focused_lane.as_deref() == Some(lane.key.as_str())
            && self.focused_row == Some(RowFocus::Fold);
        let muted = p.muted;
        let lane_key = lane.key.clone();
        div()
            .id(eid("lane-fold", &lane.key))
            .h(px(FOLD_HEIGHT))
            .flex_shrink_0()
            .px(px(6.))
            .rounded(px(BOX_RADIUS))
            .border_1()
            .border_color(if focused {
                p.accent.into()
            } else {
                with_alpha(p.border, 0.)
            })
            .flex()
            .items_center()
            .gap(px(4.))
            .text_size(size::sm())
            .text_color(p.extra_muted)
            .cursor_pointer()
            .hover(move |style| style.text_color(muted))
            .child(ui::icon(
                if open {
                    "chevron-down"
                } else {
                    "chevron-right"
                },
                12.,
                p.extra_muted,
            ))
            .child(format!("{count} empty workspaces"))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_empty(&lane_key, cx);
            }))
            .into_any_element()
    }

    /// An open terminal inside its workspace's box: the box's header and
    /// other rows, compact, around it. `rows` are the box's rows and `at`
    /// the terminal's among them.
    #[allow(clippy::too_many_arguments)]
    fn terminal_box(
        &mut self,
        lane: &Rc<Lane>,
        rows: &[Row],
        at: usize,
        host: &str,
        terminal: &str,
        style: LaneStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let (box_host, box_workspace) = match rows[at] {
            Row::Terminal { owner, .. } => owner.workspace(lane),
            _ => (host, ""),
        };
        let look = self.box_look(&lane.key, Some((box_host, box_workspace)), style, cx);
        let strip = |this: &mut Self,
                     rows: &[Row],
                     which: &str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            (!rows.is_empty()).then(|| {
                let mut strip = div()
                    .id(eid_parts("lane-terminal-mates", &[&lane.key, which]))
                    .min_h(px(COMPACT_HEIGHT))
                    .max_h(px(COMPACT_HEIGHT * 3.5))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col();
                for &row in rows {
                    strip =
                        strip.child(this.row_element(lane, row, true, false, style, window, cx));
                }
                strip
            })
        };
        let header = self.row_element(lane, rows[0], true, false, style, window, cx);
        let before = strip(self, &rows[1.min(at)..at], "before", window, cx);
        let after = strip(self, &rows[(at + 1).min(rows.len())..], "after", window, cx);
        let view = self.terminal_view(host, terminal, window, cx);
        let label = self.core.store.read(cx).terminal_label(host, terminal, cx);
        let close = {
            let (core, lane_key, host, terminal) = (
                self.core.clone(),
                lane.key.clone(),
                host.to_string(),
                terminal.to_string(),
            );
            let board = cx.entity().downgrade();
            Button::new(eid_parts(
                "lane-terminal-kill",
                &[&lane.key, &host, &terminal],
            ))
            .icon(Icon::empty().path("icons/circle-x.svg"))
            .ghost()
            .xsmall()
            .tooltip("Close terminal (ends its shell)")
            .on_click(move |_: &ClickEvent, window, cx| {
                cx.stop_propagation();
                let (core, lane_key, host, terminal, board) = (
                    core.clone(),
                    lane_key.clone(),
                    host.clone(),
                    terminal.clone(),
                    board.clone(),
                );
                ui::confirm(
                    window,
                    cx,
                    "Close terminal?",
                    "This ends the terminal's shell and anything running in it.",
                    "Close terminal",
                    move |cx| {
                        core.store
                            .update(cx, |store, cx| store.kill_terminal(&host, &terminal, cx));
                        let _ = board.update(cx, |board, cx| {
                            board.expanded_terminals.remove(&lane_key);
                            board
                                .terminal_views
                                .remove(&(host.clone(), terminal.clone()));
                            board.sync_pins(cx);
                            cx.notify();
                        });
                    },
                );
            })
        };
        let collapse = {
            let lane_key = lane.key.clone();
            Button::new(eid_parts(
                "lane-terminal-collapse",
                &[&lane.key, host, terminal],
            ))
            .icon(Icon::empty().path("icons/x.svg"))
            .ghost()
            .xsmall()
            .tooltip("Collapse terminal")
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.collapse_terminal(&lane_key, window, cx);
            }))
        };
        let panel = div()
            .flex_1()
            .min_h(px(EXPANDED_MIN_HEIGHT))
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(p.border)
            .child(
                div()
                    .h(px(32.))
                    .flex_shrink_0()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(p.border)
                    .child(ui::icon("square-terminal", 13., p.muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(p.foreground)
                            .child(label),
                    )
                    .child(close)
                    .child(collapse),
            )
            .child(div().flex_1().min_h(px(0.)).child(view));
        div()
            .flex_1()
            .min_h(px(EXPANDED_MIN_HEIGHT + BOX_HEADER_HEIGHT))
            .rounded(px(BOX_RADIUS))
            .border_1()
            .border_color(p.border_accent)
            .bg(look.bg)
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .children(before)
            .child(panel)
            .children(after)
            .into_any_element()
    }

    /// The expanded card, inside its workspace's box when it has one: the
    /// box's header and the card's mates, compact, around it. `rows` are the
    /// box's rows.
    #[allow(clippy::too_many_arguments)]
    fn expanded_box(
        &mut self,
        lane: &Rc<Lane>,
        rows: &[Row],
        host: &str,
        agent: &str,
        style: LaneStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let boxed = rows.iter().any(|row| matches!(row, Row::Header { .. }));
        if !boxed {
            if self.in_focus_mode(&lane.key, host, agent) {
                return self.focus_placeholder(&lane.key, host, agent, true, cx);
            }
            return self.expanded_card(lane, host, agent, style, true, window, cx);
        }
        let p = palette();
        let at = rows
            .iter()
            .position(|row| {
                matches!(row, Row::Card { card, .. }
                    if lane.cards[*card].host == host && lane.cards[*card].agent_id == agent)
            })
            .unwrap_or(rows.len());
        let first = rows.iter().find_map(|row| match row {
            Row::Card { card, .. } => Some(&lane.cards[*card]),
            _ => None,
        });
        let look = self.box_look(&lane.key, first.and_then(card_workspace), style, cx);
        let mates = |this: &mut Self,
                     rows: &[Row],
                     which: &str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            (!rows.is_empty()).then(|| {
                let mut strip = div()
                    .id(eid_parts("lane-box-mates", &[&lane.key, which]))
                    .min_h(px(COMPACT_HEIGHT))
                    .max_h(px(COMPACT_HEIGHT * 3.5))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col();
                for &row in rows {
                    strip =
                        strip.child(this.row_element(lane, row, true, false, style, window, cx));
                }
                strip
            })
        };
        let header = rows
            .first()
            .map(|&row| self.row_element(lane, row, true, false, style, window, cx));
        let before = mates(self, &rows[1.min(at)..at], "before", window, cx);
        let card = if self.in_focus_mode(&lane.key, host, agent) {
            self.focus_placeholder(&lane.key, host, agent, false, cx)
        } else {
            self.expanded_card(lane, host, agent, style, false, window, cx)
        };
        let after = mates(self, &rows[(at + 1).min(rows.len())..], "after", window, cx);
        // While its card is in focus mode the box holds only a placeholder.
        let away = self.in_focus_mode(&lane.key, host, agent);
        div()
            .when(!away, |d| {
                d.flex_1()
                    .min_h(px(EXPANDED_MIN_HEIGHT + BOX_HEADER_HEIGHT))
            })
            .rounded(px(BOX_RADIUS))
            .border_1()
            .border_color(if look.outlined {
                look.border
            } else {
                p.border_accent.into()
            })
            .bg(look.bg)
            .flex()
            .flex_col()
            .overflow_hidden()
            .children(header)
            .children(before)
            .child(card)
            .children(after)
            .into_any_element()
    }

    /// An agent's card, open on its conversation. Unless `framed`, its
    /// workspace's box outlines it.
    #[allow(clippy::too_many_arguments)]
    fn expanded_card(
        &mut self,
        lane: &Lane,
        host: &str,
        agent: &str,
        style: LaneStyle,
        framed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let title = self
            .core
            .store
            .read(cx)
            .fleet
            .agent(host, agent)
            .map(|agent| agent.display_title())
            .unwrap_or_else(|| "New agent".into());
        let lane_key = lane.key.clone();
        let conversation = self.conversation(host, agent, window, cx);
        let workspace = self
            .core
            .store
            .read(cx)
            .fleet
            .agent(host, agent)
            .and_then(|agent| agent.workspace_id.clone());
        let supported = self
            .core
            .store
            .read(cx)
            .fleet
            .host(host)
            .is_some_and(|host| host.has("workspaceLabels"));
        // Inside its workspace's box, the box's header relabels it.
        let picker = workspace
            .clone()
            .filter(|_| supported && framed)
            .map(|workspace| self.label_picker(&lane.key, host, agent, &workspace, window, cx));
        // Closing the tab archives the agent; for a workspace's last agent it
        // offers to archive the workspace instead.
        let close_button = {
            let enabled = crate::workspace_archive::can_close_agent(&self.core, host, agent, cx);
            // A subagent is archived from its card, as from Paseo's track.
            let (icon, label, tip) =
                if crate::workspace_archive::is_subagent(&self.core, host, agent, cx) {
                    ("icons/archive.svg", "Archive subagent", "Archive subagent")
                } else {
                    (
                        "icons/circle-x.svg",
                        "Close agent tab",
                        "Close agent tab (archives the agent)",
                    )
                };
            let (core, host, agent) = (self.core.clone(), host.to_string(), agent.to_string());
            Button::new(eid_parts(
                "lane-card-close-tab",
                &[&lane.key, &host, &agent],
            ))
            .icon(Icon::empty().path(icon))
            .ghost()
            .xsmall()
            .disabled(!enabled)
            .accessibility_label(label)
            .tooltip(tip)
            .on_click(move |_: &ClickEvent, window, cx| {
                cx.stop_propagation();
                crate::workspace_archive::close_agent(&core, &host, &agent, window, cx);
            })
        };
        // Labels belong to the workspace: say so, and how many agents move.
        let relabel_tip = {
            let store = self.core.store.read(cx);
            let workspace = workspace
                .as_deref()
                .and_then(|workspace| store.fleet.workspace(host, workspace));
            let name = workspace
                .map(|w| w.title.clone().unwrap_or_else(|| w.name.clone()))
                .unwrap_or_default();
            let agents = workspace.map_or(0, |w| {
                store.fleet.host(host).map_or(0, |h| {
                    h.directory
                        .agents
                        .values()
                        .filter(|a| a.workspace_id.as_deref() == Some(w.id.as_str()))
                        .count()
                })
            });
            match agents {
                0 | 1 => format!("Relabel the workspace \"{name}\""),
                n => {
                    format!("Relabel the workspace \"{name}\": all {n} of its agents move together")
                }
            }
        };
        let measured_picker = picker.clone();
        let relabel_popover = picker.map(|picker| {
            let focus = picker.read(cx).focus_handle(cx);
            let open = picker.read(cx).is_open();
            let opened = picker.clone();
            Popover::new(eid_parts("lane-relabel", &[&lane.key, host, agent]))
                .anchor(Anchor::TopRight)
                .open(open)
                .p(px(0.))
                .track_focus(&focus)
                .trigger(
                    Button::new(eid_parts("lane-relabel-button", &[&lane.key, host, agent]))
                        .icon(Icon::empty().path("icons/tag.svg"))
                        .ghost()
                        .xsmall()
                        .tooltip(relabel_tip.clone())
                        .disabled(self.relabel_pending.is_some()),
                )
                .on_open_change(move |open, window, cx| {
                    opened.update(cx, |picker, cx| picker.set_open(*open, window, cx));
                })
                .content(move |_, _, _| picker.clone())
                .into_any_element()
        });
        div()
            .relative()
            .flex_1()
            // The conversation keeps room to read and type in a short lane.
            .min_h(px(EXPANDED_MIN_HEIGHT))
            .when(framed, |card| {
                card.rounded(px(BOX_RADIUS))
                    .border_1()
                    .border_color(p.border_accent)
            })
            // In its box, a line as strong as the box's outline sets the
            // agent's bar apart from the workspace's above it.
            .when(!framed, |card| {
                card.border_t_1().border_color(p.border_accent)
            })
            .bg(style.card)
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .id(eid_parts("lane-card-bar", &[&lane.key, host, agent]))
                    .px(px(10.))
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(p.border)
                    .hover(move |bar| bar.bg(style.card_hover))
                    .on_mouse_down(MouseButton::Right, {
                        let (host, agent) = (host.to_string(), agent.to_string());
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.open_card_menu(&host, &agent, event.position, window, cx);
                            cx.stop_propagation();
                        })
                    })
                    .child(match self.rename_editor(&lane.key, host, agent, cx) {
                        Some(editor) => editor.into_any_element(),
                        None => {
                            let (host, agent) = (host.to_string(), agent.to_string());
                            let rename_lane = lane.key.clone();
                            // Only the name itself starts a rename, not the
                            // room beside it.
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .child(
                                    div()
                                        .id(eid_parts(
                                            "lane-card-title",
                                            &[&lane.key, &host, &agent],
                                        ))
                                        .min_w(px(0.))
                                        .truncate()
                                        .px(px(4.))
                                        .mx(px(-4.))
                                        .rounded(px(4.))
                                        .text_color(p.foreground)
                                        .cursor(CursorStyle::IBeam)
                                        .hover(move |style| style.bg(p.surface2))
                                        .child(title)
                                        .tooltip(|window, cx| {
                                            Tooltip::new("Rename agent").build(window, cx)
                                        })
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                cx.stop_propagation();
                                                this.start_rename(
                                                    &rename_lane,
                                                    &host,
                                                    &agent,
                                                    window,
                                                    cx,
                                                );
                                            },
                                        )),
                                )
                                .into_any_element()
                        }
                    })
                    .children(relabel_popover)
                    .child({
                        let in_focus = self.in_focus_mode(&lane.key, host, agent);
                        let (lane_key, host, agent) =
                            (lane.key.clone(), host.to_string(), agent.to_string());
                        Button::new(eid_parts("lane-card-focus", &[&lane_key, &host, &agent]))
                            .icon(Icon::empty().path(if in_focus {
                                "icons/minimize-2.svg"
                            } else {
                                "icons/maximize-2.svg"
                            }))
                            .ghost()
                            .xsmall()
                            .tooltip(if in_focus {
                                "Back to its lane (Esc)"
                            } else {
                                "Focus: show this card large (F)"
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.toggle_focus_mode(&lane_key, &host, &agent, window, cx);
                            }))
                    })
                    .child(close_button)
                    .child(
                        Button::new(eid_parts("lane-card-collapse", &[&lane.key, host, agent]))
                            .icon(Icon::empty().path("icons/x.svg"))
                            .ghost()
                            .xsmall()
                            .accessibility_label("Collapse card")
                            .tooltip("Collapse card")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.collapse_lane(&lane_key, window, cx);
                            })),
                    ),
            )
            .child(div().flex_1().min_h(px(0.)).child(conversation))
            .when_some(measured_picker, |d, picker| {
                d.child(
                    canvas(
                        move |rect, _, cx| {
                            picker.update(cx, |picker, cx| picker.set_bounds(rect, cx));
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .into_any_element()
    }

    fn draft_card(&self, lane: &Lane, style: LaneStyle, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let Some(draft) = &self.draft else {
            return div().into_any_element();
        };
        let providers = self.providers(cx);
        let resolved = self.resolved(cx);
        let loading = draft
            .workspace
            .as_ref()
            .is_some_and(|(host, _)| !self.core.store.read(cx).providers.contains_key(host));
        let provider_label = resolved
            .as_ref()
            .map(|(provider, _, _)| provider.label.clone())
            .unwrap_or_else(|| {
                if loading {
                    "Loading providers…".into()
                } else {
                    "No providers".into()
                }
            });
        let model_label = resolved
            .as_ref()
            .and_then(|(provider, model, _)| {
                let id = model.as_ref()?;
                provider
                    .models
                    .iter()
                    .find(|model| &model.id == id)
                    .map(|model| model.label.clone())
            })
            .unwrap_or_else(|| "Default model".into());
        let this = cx.entity().downgrade();
        let provider_menu = {
            let providers = providers.clone();
            let this = this.clone();
            draft_picker(
                eid("lane-draft-provider", &lane.key),
                "Tool",
                provider_label,
            )
            .dropdown_menu(move |mut menu, _, _| {
                for provider in &providers {
                    let this = this.clone();
                    let id = provider.id.clone();
                    menu = menu.item(PopupMenuItem::new(provider.label.clone()).on_click(
                        move |_, _, cx| {
                            let id = id.clone();
                            let _ = this.update(cx, |this, cx| {
                                if let Some(draft) = &mut this.draft {
                                    draft.provider = Some(id);
                                    draft.model = None;
                                    draft.thinking = None;
                                }
                                cx.notify();
                            });
                        },
                    ));
                }
                menu
            })
        };
        let model_menu = {
            let models = resolved
                .as_ref()
                .map(|(provider, _, _)| provider.models.clone())
                .unwrap_or_default();
            let this = this.clone();
            draft_picker(eid("lane-draft-model", &lane.key), "Model", model_label).dropdown_menu(
                move |mut menu, _, _| {
                    for model in &models {
                        let this = this.clone();
                        let id = model.id.clone();
                        menu = menu.item(PopupMenuItem::new(model.label.clone()).on_click(
                            move |_, _, cx| {
                                let id = id.clone();
                                let _ = this.update(cx, |this, cx| {
                                    if let Some(draft) = &mut this.draft {
                                        draft.model = Some(id);
                                        draft.thinking = None;
                                    }
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu.max_h(px(320.))
                },
            )
        };
        let (workspace_menu, new_workspace_row) = self.draft_workspace_controls(lane, draft, cx);
        let error = draft.error.clone();
        let creating = draft.creating;
        let input = draft.input.clone();
        let bounds = draft.bounds.clone();
        div()
            .relative()
            .p(px(8.))
            .rounded(px(8.))
            .bg(style.card)
            // Outlined while it waits: Enter creates the agent; Escape or a
            // click anywhere else closes it.
            .border_1()
            .border_color(p.accent)
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                canvas(move |rect, _, _| bounds.set(Some(rect)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(div().flex().min_w(px(0.)).child(workspace_menu))
            .children(new_workspace_row)
            .child(
                Textarea::new(&input)
                    .appearance(false)
                    .text_size(size::sm()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .flex_wrap()
                    .child(provider_menu)
                    .child(model_menu)
                    .child(div().flex_1())
                    .child(
                        Button::new(eid("lane-draft-create", &lane.key))
                            .icon(Icon::empty().path("icons/arrow-up.svg"))
                            .primary()
                            .xsmall()
                            .loading(creating)
                            .tooltip("Create agent")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.create_draft(window, cx);
                            })),
                    ),
            )
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.status_danger)
                        .child(error),
                )
            })
            .into_any_element()
    }
}

impl Lanes {
    /// The draft's workspace dropdown, always shown: the lane's workspaces
    /// and "New workspace…". With a new workspace picked, also the row that
    /// says where it goes.
    fn draft_workspace_controls(
        &self,
        lane: &Lane,
        draft: &DraftState,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Option<AnyElement>) {
        let p = palette();
        let this = cx.entity().downgrade();
        let input = draft.input.clone();
        let label = match &draft.new_workspace {
            Some(_) => "New workspace".to_string(),
            None => draft
                .workspace
                .as_ref()
                .and_then(|(host, id)| {
                    lane.workspaces
                        .iter()
                        .find(|ws| ws.host == *host && ws.workspace_id == *id)
                        .map(|ws| format!("{} · {}", ws.name, ws.host))
                })
                .unwrap_or_else(|| "Workspace".into()),
        };
        let menu = {
            let workspaces = lane.workspaces.clone();
            let (this, input) = (this.clone(), input.clone());
            let lane = lane.clone();
            draft_picker(eid("lane-draft-workspace", &lane.key), "Workspace", label)
                .tooltip("The workspace the agent works in")
                .dropdown_menu(move |mut menu, _, _| {
                    for workspace in &workspaces {
                        let (this, input) = (this.clone(), input.clone());
                        let (host, id) = (workspace.host.clone(), workspace.workspace_id.clone());
                        menu = menu.item(
                            PopupMenuItem::new(format!("{} · {}", workspace.name, workspace.host))
                                .on_click(move |_, window, cx| {
                                    let (host, id) = (host.clone(), id.clone());
                                    let _ = this.update(cx, |this, cx| {
                                        if let Some(draft) = &mut this.draft {
                                            draft.workspace = Some((host.clone(), id));
                                            draft.new_workspace = None;
                                            draft.error = None;
                                        }
                                        this.core.store.update(cx, |store, cx| {
                                            store.ensure_providers(&host, cx);
                                        });
                                        cx.notify();
                                    });
                                    input.update(cx, |input, cx| input.focus(window, cx));
                                }),
                        );
                    }
                    if !workspaces.is_empty() {
                        menu = menu.separator();
                    }
                    let (this, input, lane) = (this.clone(), input.clone(), lane.clone());
                    menu.item(PopupMenuItem::new("New workspace…").on_click(
                        move |_, window, cx| {
                            let _ = this.update(cx, |this, cx| {
                                let from = this.draft.as_ref().and_then(|d| d.workspace.clone());
                                let new = this.default_new_workspace(&lane, from.as_ref(), cx);
                                if let Some(new) = &new {
                                    let host = new.host.clone();
                                    this.core.store.update(cx, |store, cx| {
                                        store.ensure_providers(&host, cx);
                                    });
                                }
                                if let Some(draft) = &mut this.draft {
                                    draft.error = new
                                        .is_none()
                                        .then(|| "No projects on an online host".to_string());
                                    draft.new_workspace = new;
                                }
                                cx.notify();
                            });
                            input.update(cx, |input, cx| input.focus(window, cx));
                        },
                    ))
                })
                .into_any_element()
        };
        let Some(new) = draft.new_workspace.clone() else {
            return (menu, None);
        };
        let choices = self.project_choices(lane, cx);
        let project_label = choices
            .iter()
            .find(|(host, project, _)| *host == new.host && *project == new.project)
            .map(|(_, _, label)| label.clone())
            .unwrap_or_else(|| "Project".into());
        let is_git = self
            .core
            .store
            .read(cx)
            .fleet
            .host(&new.host)
            .and_then(|host| host.directory.projects.get(&new.project))
            .is_some_and(|project| project.project_kind == "git");
        let projects = {
            let (this, input) = (this.clone(), input.clone());
            draft_picker(
                eid("lane-draft-project", &lane.key),
                "Project",
                project_label,
            )
            .tooltip("The project the new workspace is made in")
            .dropdown_menu(move |mut menu, _, _| {
                for (host, project, label) in &choices {
                    let (this, input) = (this.clone(), input.clone());
                    let (host, project) = (host.clone(), project.clone());
                    menu = menu.item(PopupMenuItem::new(label.clone()).on_click(
                        move |_, window, cx| {
                            let (host, project) = (host.clone(), project.clone());
                            let _ = this.update(cx, |this, cx| {
                                this.core.store.update(cx, |store, cx| {
                                    store.ensure_providers(&host, cx);
                                });
                                if let Some(new) =
                                    this.draft.as_mut().and_then(|d| d.new_workspace.as_mut())
                                {
                                    new.host = host;
                                    new.project = project;
                                }
                                cx.notify();
                            });
                            input.update(cx, |input, cx| input.focus(window, cx));
                        },
                    ));
                }
                menu.max_h(px(320.))
            })
        };
        let worktree = {
            let (this, input) = (this.clone(), input.clone());
            div()
                .id(eid("lane-draft-worktree", &lane.key))
                .flex()
                .items_center()
                .gap(px(4.))
                .px(px(4.))
                .rounded(px(4.))
                .text_size(size::sm())
                .text_color(p.muted)
                .cursor_pointer()
                .hover(|style| style.text_color(p.foreground))
                .child(ui::icon(
                    if new.worktree {
                        "square-check"
                    } else {
                        "square"
                    },
                    12.,
                    p.muted,
                ))
                .child("New worktree")
                .tooltip(|window, cx| {
                    Tooltip::new("Work on a branch in a worktree of its own").build(window, cx)
                })
                .on_click(move |_: &ClickEvent, window, cx| {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(new) =
                            this.draft.as_mut().and_then(|d| d.new_workspace.as_mut())
                        {
                            new.worktree = !new.worktree;
                        }
                        cx.notify();
                    });
                    input.update(cx, |input, cx| input.focus(window, cx));
                })
        };
        let row = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .flex_wrap()
            .text_size(size::sm())
            .child(projects)
            .when(is_git, |row| row.child(worktree))
            .into_any_element();
        (menu, Some(row))
    }
}

/// Picks one kind of a [`LaneFilter`]: hosts, tools or labels.
type FilterField = fn(&mut LaneFilter) -> &mut BTreeSet<String>;

impl Lanes {
    /// Filters for the board: which hosts, tools and labels it shows.
    fn filters_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let nav = self.core.nav.clone();
        let filter = nav.read(cx).lane_filter.clone();
        let store = self.core.store.read(cx);
        let hosts: Vec<(String, String)> = store
            .fleet
            .hosts
            .iter()
            .map(|host| (host.name().to_string(), host.name().to_string()))
            .collect();
        // Tools by what the hosts call them, for those any agent uses.
        let mut tools: BTreeMap<String, String> = BTreeMap::new();
        for (_, agent) in store.fleet.agents() {
            let id = agent.provider_id();
            if id.is_empty() || tools.contains_key(id) {
                continue;
            }
            let name = store
                .providers
                .values()
                .flatten()
                .find(|provider| provider.id == id)
                .map(|provider| provider.label.clone())
                .unwrap_or_else(|| id.to_string());
            tools.insert(id.to_string(), name);
        }
        let mut labels: BTreeMap<String, String> = BTreeMap::new();
        for host in &store.fleet.hosts {
            for def in &host.directory.labels {
                labels
                    .entry(fleet::directory::label_key(&def.name))
                    .or_insert_with(|| def.name.clone());
            }
        }
        let mut labels: Vec<(String, String)> = labels.into_iter().collect();
        labels.sort_by_key(|(_, name)| name.to_lowercase());
        let p = palette();
        let clear = {
            let nav = nav.clone();
            Button::new("lane-filter-clear")
                .icon(Icon::empty().path("icons/x.svg"))
                .ghost()
                .xsmall()
                .tooltip("Clear the filters")
                .on_click(move |_, _, cx| {
                    nav.update(cx, |nav, cx| nav.set_lane_filter(LaneFilter::default(), cx));
                })
        };
        let find = Button::new("lane-find")
            .icon(Icon::empty().path("icons/search.svg"))
            .ghost()
            .xsmall()
            .tooltip(if cfg!(target_os = "macos") {
                "Find on the board (Cmd+F)"
            } else {
                "Find on the board (Ctrl+F)"
            })
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_find(window, cx)));
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(find)
            .child(
                // As bright and as large as "Group by", its neighbour.
                div()
                    .text_size(size::base())
                    .text_color(p.foreground)
                    .px(px(4.))
                    .child("Filter"),
            )
            .child(filter_picker(
                "lane-filter-hosts",
                "Host",
                "hosts",
                hosts,
                &filter,
                |f| &mut f.hosts,
                &nav,
            ))
            .child(filter_picker(
                "lane-filter-tools",
                "Tool",
                "tools",
                tools.into_iter().collect(),
                &filter,
                |f| &mut f.tools,
                &nav,
            ))
            .child(filter_picker(
                "lane-filter-states",
                "State",
                "states",
                fleet::Bucket::GROUP_ORDER
                    .iter()
                    .map(|state| (state.as_str().to_string(), state.label().to_string()))
                    .collect(),
                &filter,
                |f| &mut f.states,
                &nav,
            ))
            .child(filter_picker(
                "lane-filter-labels",
                "Label",
                "labels",
                labels,
                &filter,
                |f| &mut f.labels,
                &nav,
            ))
            .when(!filter.is_empty(), |bar| bar.child(clear))
            .into_any_element()
    }
}

/// A row as the board shows it while a state is singled out from the title
/// bar's counts: that state's cards outlined, everything else dimmed.
fn state_row(
    lane: &Lane,
    row: &Row,
    element: AnyElement,
    state: Option<fleet::Bucket>,
) -> AnyElement {
    let Some(state) = state else {
        return element;
    };
    match row {
        Row::Card { card, .. } if lane.cards[*card].bucket == state => div()
            .relative()
            .child(element)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(palette().accent),
            )
            .into_any_element(),
        _ => div().opacity(0.3).child(element).into_any_element(),
    }
}

impl Lanes {
    /// While a state is singled out: how many, and the offer to filter to
    /// them. Clicking anything else, or any key, goes back to normal.
    fn state_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.core.nav.read(cx).state_highlight?;
        let p = palette();
        // Agents in that state on the board, and those only in collapsed
        // lanes, which the title bar's count includes.
        let in_state = |lanes: &LaneSet| -> HashSet<(String, String)> {
            lanes
                .iter()
                .flat_map(|lane| lane.cards.iter())
                .filter(|card| card.bucket == state)
                .map(|card| (card.host.clone(), card.agent_id.clone()))
                .collect()
        };
        let shown = in_state(&self.visible_lanes(cx));
        let count = shown.len();
        let hidden = in_state(&self.current_lanes(cx)).difference(&shown).count();
        let label = state.label().to_lowercase();
        let text = match (count, hidden) {
            (1, 0) => format!("1 {label} agent highlighted"),
            (n, 0) => format!("{n} {label} agents highlighted"),
            (n, k) => format!("{n} {label} agents highlighted · {k} more in collapsed lanes"),
        };
        let nav = self.core.nav.clone();
        let filter = Button::new("state-filter")
            .label("Filter to them")
            .primary()
            .xsmall()
            .on_click(move |_, _, cx| {
                nav.update(cx, |nav, cx| {
                    let mut filter = nav.lane_filter.clone();
                    filter.states = [state.as_str().to_string()].into();
                    nav.set_lane_filter(filter, cx);
                    nav.set_state_highlight(None, cx);
                });
            });
        Some(
            div()
                .absolute()
                .top(px(8.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("state-bar")
                        .occlude()
                        // Its own clicks do not count as clicking away.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .px(px(12.))
                        .py(px(6.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(with_alpha(p.accent, 0.6))
                        .bg(p.surface1)
                        .shadow_lg()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .text_size(size::sm())
                        .child(ui::agent_dot(state))
                        .child(div().text_color(p.foreground).child(text))
                        .child(filter)
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(p.extra_muted)
                                .child("Click anywhere or press a key to go back"),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// One kind of filter as a dropdown: its name, what is chosen ("All", one
/// name, or how many), and a caret. Each choice toggles; "All" clears.
fn filter_picker(
    id: &'static str,
    name: &str,
    many: &str,
    choices: Vec<(String, String)>,
    filter: &LaneFilter,
    field: FilterField,
    nav: &Entity<crate::app::Nav>,
) -> AnyElement {
    let p = palette();
    let chosen = field(&mut filter.clone()).clone();
    let value = match chosen.len() {
        0 => "All".to_string(),
        1 => {
            let key = chosen.iter().next().expect("one");
            choices
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| key.clone())
        }
        n => format!("{n} {many}"),
    };
    let active = !chosen.is_empty();
    let nav = nav.clone();
    Button::new(id)
        .ghost()
        .xsmall()
        .child(div().text_color(p.muted).child(name.to_string()))
        .child(
            div()
                .text_color(if active { p.accent } else { p.foreground })
                .child(value),
        )
        .child(ui::icon("chevron-down", 12., p.muted))
        .dropdown_menu(move |mut menu, _, cx| {
            let current = field(&mut nav.read(cx).lane_filter.clone()).clone();
            let edit = move |nav: &Entity<crate::app::Nav>,
                             cx: &mut App,
                             change: &dyn Fn(&mut BTreeSet<String>)| {
                nav.update(cx, |nav, cx| {
                    let mut filter = nav.lane_filter.clone();
                    change(field(&mut filter));
                    nav.set_lane_filter(filter, cx);
                });
            };
            let all = nav.clone();
            menu = menu.item(
                PopupMenuItem::new("All")
                    .checked(current.is_empty())
                    .on_click(move |_, _, cx| edit(&all, cx, &|set| set.clear())),
            );
            if !choices.is_empty() {
                menu = menu.separator();
            }
            for (key, name) in &choices {
                let (nav, key) = (nav.clone(), key.clone());
                menu = menu.item(
                    PopupMenuItem::new(name.clone())
                        .checked(current.contains(&key))
                        .on_click(move |_, _, cx| {
                            edit(&nav, cx, &|set| {
                                if !set.remove(&key) {
                                    set.insert(key.clone());
                                }
                            })
                        }),
                );
            }
            menu.max_h(px(360.))
        })
        .into_any_element()
}

/// One of the draft's choices, as a dropdown: what it chooses, in grey,
/// the current choice, and a caret that says it opens.
fn draft_picker(id: SharedString, name: &str, value: impl Into<SharedString>) -> Button {
    let p = palette();
    Button::new(id)
        .ghost()
        .xsmall()
        .child(div().text_color(p.muted).child(name.to_string()))
        .child(
            div()
                .min_w(px(0.))
                .truncate()
                .text_color(p.foreground)
                .child(value.into()),
        )
        .child(ui::icon("chevron-down", 12., p.muted))
}

/// The labels a workspace made from `lane` takes so it lands in that lane:
/// the lane's labels, named and coloured as `host` has them. None for a
/// lane that is not a label's.
fn lane_labels(lane: &Lane, host: &str, fleet: &fleet::Fleet) -> Vec<LabelChange> {
    if !matches!(lane.kind, LaneKind::Label | LaneKind::Combination) {
        return Vec::new();
    }
    let defs = fleet
        .host(host)
        .map(|host| host.directory.labels.clone())
        .unwrap_or_default();
    lane.label_keys
        .iter()
        .map(|key| {
            let def = defs
                .iter()
                .find(|def| fleet::directory::label_key(&def.name) == *key);
            LabelChange {
                name: def.map(|def| def.name.clone()).unwrap_or_else(|| {
                    if lane.label_keys.len() == 1 {
                        lane.title.clone()
                    } else {
                        key.clone()
                    }
                }),
                color: def
                    .map(|def| def.color.clone())
                    .or_else(|| lane.color.clone())
                    .unwrap_or_else(|| "violet".into()),
                assigned: true,
            }
        })
        .collect()
}

fn prefer_model<'a>(provider: &'a Provider, picked: Option<&String>) -> Option<&'a Model> {
    if let Some(id) = picked
        && let Some(model) = provider.models.iter().find(|model| &model.id == id)
    {
        return Some(model);
    }
    provider
        .models
        .iter()
        .find(|model| {
            let id = model.id.to_ascii_lowercase();
            let label = model.label.to_ascii_lowercase();
            id.contains("haiku") || label.contains("haiku")
        })
        .or_else(|| provider.default_model())
}

fn assistant_blurb(timeline: &fleet::timeline::Timeline) -> Option<String> {
    timeline
        .head
        .iter()
        .rev()
        .chain(timeline.tail.iter().rev())
        .find_map(|item| {
            let ItemKind::Assistant { text, .. } = &item.kind else {
                return None;
            };
            text.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_string)
        })
}

fn blurb(store: &crate::app::FleetStore, card: &LaneCard, cx: &App) -> Option<String> {
    if let Some(session) = store.session(&card.host, &card.agent_id) {
        let text = assistant_blurb(&session.read(cx).timeline);
        if text.is_some() {
            return text;
        }
    }
    match card.attention_reason.as_deref() {
        Some("finished") => Some("Finished".into()),
        Some("permission") => Some("Needs permission".into()),
        Some("error") => Some("Error".into()),
        Some(other) if !other.is_empty() => Some(other.to_string()),
        _ => None,
    }
}

/// Marks a subagent's row as started by the row above.
fn subagent_mark() -> AnyElement {
    ui::icon("corner-down-right", 12., palette().extra_muted).into_any_element()
}

/// A card's workspace, host and model; in a box, whose header names the
/// workspace and host, only its model.
fn meta(card: &LaneCard, boxed: bool) -> String {
    let mut parts = Vec::new();
    if !boxed {
        if !card.workspace_name.is_empty() {
            parts.push(card.workspace_name.clone());
        }
        parts.push(card.host.clone());
    }
    let model = match &card.model {
        Some(model) if !card.provider.is_empty() => format!("{}/{}", card.provider, model),
        Some(model) => model.clone(),
        None => card.provider.clone(),
    };
    if !model.is_empty() {
        parts.push(model);
    }
    parts.join(" · ")
}

/// Host metadata occupies one bounded row even when a lane has no cards yet.
fn header_hosts(lane: &Lane, narrow: bool) -> AnyElement {
    let p = palette();
    let row = div()
        .h(px(HEADER_HOST_HEIGHT))
        .flex_shrink_0()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap(px(4.))
        .text_size(size::sm())
        .line_height(px(16.))
        .whitespace_nowrap()
        .text_color(p.muted);
    if lane.hosts.is_empty() {
        return row
            .child(div().italic().child("Hosts not listed"))
            .into_any_element();
    }
    if narrow && lane.hosts.len() > 1 {
        let names = lane.hosts.join(", ");
        return row
            .child(
                div()
                    .id(eid("lane-host-count", &lane.key))
                    .min_w(px(0.))
                    .truncate()
                    .child(format!("{} hosts", lane.hosts.len()))
                    .tooltip(move |window, cx| {
                        Tooltip::new(names.clone())
                            .max_w(px(360.))
                            .build(window, cx)
                    }),
            )
            .into_any_element();
    }
    let row = row.children(lane.hosts.iter().take(2).map(|host| {
        let name = host.clone();
        div()
            .id(eid_parts("lane-host", &[&lane.key, host]))
            .h(px(HEADER_HOST_HEIGHT))
            .min_w(px(0.))
            .px(px(6.))
            .rounded(px(4.))
            .bg(p.surface2)
            .flex()
            .items_center()
            .child(div().min_w(px(0.)).truncate().child(host.clone()))
            .tooltip(move |window, cx| Tooltip::new(name.clone()).max_w(px(360.)).build(window, cx))
    }));
    let row = if lane.hosts.len() > 2 {
        let names = lane.hosts[2..].join(", ");
        row.child(
            div()
                .id(eid("lane-host-more", &lane.key))
                .h(px(HEADER_HOST_HEIGHT))
                .flex_shrink_0()
                .px(px(6.))
                .rounded(px(4.))
                .bg(p.surface2)
                .flex()
                .items_center()
                .child(format!("+{}", lane.hosts.len() - 2))
                .tooltip(move |window, cx| {
                    Tooltip::new(names.clone())
                        .max_w(px(360.))
                        .build(window, cx)
                }),
        )
    } else {
        row
    };
    row.into_any_element()
}

fn color_dot(lane: &Lane) -> AnyElement {
    let p = palette();
    let color = if lane.key == UNLABELLED {
        p.extra_muted
    } else if let Some(name) = lane.color.as_deref()
        && let Some((fill, _)) = theme::identity_named(name)
    {
        rgb(fill)
    } else {
        rgb(theme::identity_for(&lane.title).0)
    };
    dot(color)
}

fn dot(color: Rgba) -> AnyElement {
    div()
        .size(px(8.))
        .rounded_full()
        .flex_shrink_0()
        .bg(color)
        .into_any_element()
}

/// A cached snapshot for board geometry and its item indices.
/// `lanes` with only what `filter` lets through: cards whose host, tool
/// and workspace labels match, and workspaces likewise; a lane left with
/// neither goes. `labels` gives a workspace's label keys. Lanes the filter
/// leaves whole keep their identity.
fn filter_lanes(
    lanes: &[Rc<Lane>],
    filter: &LaneFilter,
    labels: impl Fn(&str, &str) -> Vec<String>,
) -> Vec<Rc<Lane>> {
    if filter.is_empty() {
        return lanes.to_vec();
    }
    let host_ok = |host: &str| filter.hosts.is_empty() || filter.hosts.contains(host);
    let labels_ok = |host: &str, workspace: Option<&str>| {
        filter.labels.is_empty()
            || workspace.is_some_and(|workspace| {
                labels(host, workspace)
                    .iter()
                    .any(|label| filter.labels.contains(label))
            })
    };
    let card_ok = |card: &LaneCard| {
        let tool = card.provider.split('/').next().unwrap_or(&card.provider);
        host_ok(&card.host)
            && (filter.tools.is_empty() || filter.tools.contains(tool))
            && (filter.states.is_empty() || filter.states.contains(card.bucket.as_str()))
            && labels_ok(&card.host, card.workspace_id.as_deref())
    };
    lanes
        .iter()
        .filter_map(|lane| {
            let cards: Vec<LaneCard> = lane.cards.iter().filter(|c| card_ok(c)).cloned().collect();
            // With a tool or state chosen, a workspace shows only for its
            // matching agents.
            let workspaces: Vec<_> = lane
                .workspaces
                .iter()
                .filter(|ws| {
                    host_ok(&ws.host)
                        && labels_ok(&ws.host, Some(&ws.workspace_id))
                        && ((filter.tools.is_empty() && filter.states.is_empty())
                            || cards.iter().any(|card| {
                                card.host == ws.host
                                    && card.workspace_id.as_deref() == Some(&ws.workspace_id)
                            }))
                })
                .cloned()
                .collect();
            if cards.is_empty() && workspaces.is_empty() {
                return None;
            }
            if cards.len() == lane.cards.len() && workspaces.len() == lane.workspaces.len() {
                return Some(lane.clone());
            }
            let mut kept = (**lane).clone();
            kept.hosts.retain(|host| {
                host_ok(host)
                    && (cards.iter().any(|card| card.host == *host)
                        || workspaces.iter().any(|ws| ws.host == *host))
            });
            kept.cards = cards;
            kept.workspaces = workspaces;
            Some(Rc::new(kept))
        })
        .collect()
}

fn displayed_lanes(lanes: &[Rc<Lane>]) -> LaneSet {
    Rc::new(
        lanes
            .iter()
            .filter(|lane| !lane.collapsed)
            .cloned()
            .collect(),
    )
}

/// Hiding the focused lane prefers the next displayed neighbour, then the
/// previous one. It never transfers focus to another collapsed tab.
fn next_visible_lane(lanes: &[Rc<Lane>], hidden_index: usize) -> Option<String> {
    lanes[hidden_index + 1..]
        .iter()
        .chain(lanes[..hidden_index].iter().rev())
        .find(|lane| !lane.collapsed)
        .map(|lane| lane.key.clone())
}

fn visible_focus_lane(lanes: &[Rc<Lane>], focused: Option<&str>) -> Option<String> {
    let key = focused?;
    match lanes.iter().position(|lane| lane.key == key) {
        Some(index) if lanes[index].collapsed => next_visible_lane(lanes, index),
        Some(_) => Some(key.to_string()),
        None => lanes
            .iter()
            .find(|lane| !lane.collapsed)
            .map(|lane| lane.key.clone()),
    }
}

// IDs must distinguish punctuation and synthetic label-set keys; replacing
// punctuation with '-' aliases different lanes and their button/input state.
fn safe_key(key: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(key.len() * 2);
    for byte in key.bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 15) as usize] as char);
    }
    encoded
}

fn element_scope(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| safe_key(part))
        .collect::<Vec<_>>()
        .join("_")
}
fn eid_parts(prefix: &str, parts: &[&str]) -> SharedString {
    SharedString::from(format!("{prefix}-{}", element_scope(parts)))
}

fn eid(prefix: &str, key: &str) -> SharedString {
    SharedString::from(format!("{prefix}-{}", safe_key(key)))
}

#[cfg(test)]
mod box_tests {
    use super::*;
    use fleet::Bucket;
    use fleet::lanes::LaneWorkspace;

    fn card(agent: &str, workspace: Option<&str>) -> LaneCard {
        LaneCard {
            host: "h".into(),
            agent_id: agent.into(),
            workspace_id: workspace.map(str::to_string),
            workspace_name: workspace.unwrap_or_default().into(),
            title: agent.into(),
            provider: "claude".into(),
            model: None,
            updated_at: None,
            bucket: Bucket::Done,
            attention_reason: None,
            parent: None,
        }
    }

    fn lane(cards: Vec<LaneCard>, workspaces: &[&str]) -> Lane {
        Lane {
            key: "l".into(),
            kind: LaneKind::Other,
            title: "l".into(),
            color: None,
            label_keys: Vec::new(),
            hosts: vec!["h".into()],
            cards,
            workspaces: workspaces
                .iter()
                .map(|id| LaneWorkspace {
                    host: "h".into(),
                    workspace_id: (*id).into(),
                    name: (*id).into(),
                })
                .collect(),
            collapsed: false,
        }
    }

    /// Grouped as the user has said nothing about where cards go.
    fn group(lane: &Lane) -> Option<Lane> {
        group_workspaces(
            lane,
            &Placement {
                pinned: &BTreeSet::new(),
                order: &BTreeMap::new(),
            },
        )
    }

    #[test]
    fn pinned_workspaces_lead_and_given_card_order_holds() {
        let lane = lane(
            vec![
                card("a1", Some("a")),
                card("a2", Some("a")),
                card("a3", Some("a")),
                card("b1", Some("b")),
            ],
            &["a", "b"],
        );
        let pinned = BTreeSet::from(["h:b".to_string()]);
        let order = BTreeMap::from([("h:a".to_string(), vec!["a3".to_string(), "a1".to_string()])]);
        let grouped = group_workspaces(
            &lane,
            &Placement {
                pinned: &pinned,
                order: &order,
            },
        )
        .unwrap();
        // b is pinned; in a, the given order first, then a2 as it sorts.
        assert_eq!(agents(&grouped), ["b1", "a3", "a1", "a2"]);
    }

    fn agents(lane: &Lane) -> Vec<&str> {
        lane.cards
            .iter()
            .map(|card| card.agent_id.as_str())
            .collect()
    }

    #[test]
    fn a_workspace_gathers_where_its_first_card_sorts() {
        let interleaved = lane(
            vec![
                card("a1", Some("a")),
                card("b1", Some("b")),
                card("loose", None),
                card("a2", Some("a")),
                card("b2", Some("b")),
            ],
            &["a", "b"],
        );
        let grouped = group(&interleaved).unwrap();
        assert_eq!(agents(&grouped), ["a1", "a2", "b1", "b2", "loose"]);
        assert!(group(&grouped).is_none());
    }

    #[test]
    fn rows_box_workspaces_and_leave_loose_cards_alone() {
        let lane = lane(
            vec![
                card("a1", Some("a")),
                card("a2", Some("a")),
                card("b1", Some("b")),
                card("loose", None),
            ],
            &["a", "b"],
        );
        assert_eq!(
            lane_rows(&lane, true, false, &|_| false, &|_, _| 0),
            [
                Row::Header { card: 0, count: 2 },
                Row::Card {
                    card: 0,
                    boxed: true,
                    last: false,
                    depth: 0
                },
                Row::Card {
                    card: 1,
                    boxed: true,
                    last: true,
                    depth: 0
                },
                Row::Header { card: 2, count: 1 },
                Row::Card {
                    card: 2,
                    boxed: true,
                    last: true,
                    depth: 0
                },
                Row::Card {
                    card: 3,
                    boxed: false,
                    last: true,
                    depth: 0
                },
            ]
        );
        // Without boxing single workspaces, only "a" keeps its box.
        let rows = lane_rows(&lane, false, false, &|_| false, &|_, _| 0);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, Row::Header { .. }))
                .count(),
            1
        );
        assert_eq!(
            rows[3],
            Row::Card {
                card: 2,
                boxed: false,
                last: true,
                depth: 0
            }
        );
    }

    #[test]
    fn empty_workspaces_follow_and_fold_beyond_three() {
        let few = lane(vec![card("a1", Some("a"))], &["a", "e1", "e2", "e3"]);
        assert_eq!(
            lane_rows(&few, true, false, &|_| false, &|_, _| 0)[2..],
            [
                Row::Empty {
                    workspace: 1,
                    boxed: false
                },
                Row::Empty {
                    workspace: 2,
                    boxed: false
                },
                Row::Empty {
                    workspace: 3,
                    boxed: false
                },
            ]
        );
        let many = lane(vec![card("a1", Some("a"))], &["a", "e1", "e2", "e3", "e4"]);
        assert_eq!(
            lane_rows(&many, true, false, &|_| false, &|_, _| 0)[2..],
            [Row::Fold {
                count: 4,
                open: false
            }]
        );
        let open = lane_rows(&many, true, true, &|_| false, &|_, _| 0);
        assert_eq!(open.len(), 2 + 1 + 4);
        assert!(open[3..].iter().all(|row| matches!(row, Row::Empty { .. })));
    }

    fn sub(agent: &str, workspace: &str, parent: &str) -> LaneCard {
        LaneCard {
            parent: Some(parent.into()),
            ..card(agent, Some(workspace))
        }
    }

    #[test]
    fn subagents_sit_under_the_agent_that_started_them() {
        let lane = lane(
            vec![
                card("p", Some("a")),
                card("x", Some("b")),
                sub("c2", "a", "c1"),
                sub("c1", "a", "p"),
            ],
            &["a", "b"],
        );
        let grouped = group(&lane).unwrap();
        assert_eq!(agents(&grouped), ["p", "c1", "c2", "x"]);
        let rows = lane_rows(&grouped, true, false, &|_| false, &|_, _| 0);
        let depths: Vec<usize> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Card { depth, .. } => Some(*depth),
                _ => None,
            })
            .collect();
        assert_eq!(depths, [0, 1, 2, 0]);
    }

    #[test]
    fn an_urgent_subagent_lifts_its_parent() {
        let lane = lane(
            vec![
                card("q", Some("a")),
                sub("c", "a", "p"),
                card("p", Some("a")),
            ],
            &["a"],
        );
        assert_eq!(agents(&group(&lane).unwrap()), ["q", "p", "c"]);
    }

    #[test]
    fn many_subagents_fold_but_those_needing_the_user_stay() {
        let mut waiting = sub("c3", "a", "p");
        waiting.bucket = Bucket::NeedsInput;
        let lane = lane(
            vec![
                card("p", Some("a")),
                sub("c1", "a", "p"),
                sub("c2", "a", "p"),
                waiting,
            ],
            &["a"],
        );
        let folded = lane_rows(&lane, true, false, &|_| false, &|_, _| 0);
        assert!(matches!(
            folded[2],
            Row::Subagents {
                card: 0,
                count: 3,
                open: false,
                ..
            }
        ));
        assert!(matches!(
            folded[3],
            Row::Card {
                card: 3,
                depth: 1,
                last: true,
                ..
            }
        ));
        assert_eq!(folded.len(), 4);
        let open = lane_rows(&lane, true, false, &|card| card.agent_id == "p", &|_, _| 0);
        assert_eq!(open.len(), 1 + 1 + 1 + 3);
    }

    #[test]
    fn terminals_close_their_workspaces_box() {
        let lane = lane(vec![card("a1", Some("a"))], &["a", "shell", "bare"]);
        let terminals = |_: &str, workspace: &str| match workspace {
            "a" => 2,
            "shell" => 1,
            _ => 0,
        };
        // Alone, "a"'s one card would be unboxed; its terminals box it.
        let rows = lane_rows(&lane, false, false, &|_| false, &terminals);
        assert_eq!(
            rows,
            [
                Row::Header { card: 0, count: 1 },
                Row::Card {
                    card: 0,
                    boxed: true,
                    last: false,
                    depth: 0
                },
                Row::Terminal {
                    owner: Owner::Card(0),
                    index: 0,
                    boxed: true,
                    last: false
                },
                Row::Terminal {
                    owner: Owner::Card(0),
                    index: 1,
                    boxed: true,
                    last: true
                },
                // A workspace with a terminal but no agents heads a box.
                Row::Empty {
                    workspace: 1,
                    boxed: true
                },
                Row::Terminal {
                    owner: Owner::Workspace(1),
                    index: 0,
                    boxed: true,
                    last: true
                },
                Row::Empty {
                    workspace: 2,
                    boxed: false
                },
            ]
        );
        assert!(!rows[4].ends_box());
        assert!(rows[5].ends_box());
        assert_eq!(Owner::Workspace(1).workspace(&lane), ("h", "shell"));
    }

    #[test]
    fn a_parent_cycle_hides_no_card() {
        let lane = lane(vec![sub("a1", "a", "a2"), sub("a2", "a", "a1")], &["a"]);
        let rows = lane_rows(&lane, true, false, &|_| false, &|_, _| 0);
        let cards = rows
            .iter()
            .filter(|row| matches!(row, Row::Card { .. }))
            .count();
        assert_eq!(cards, 2);
    }

    #[test]
    fn rows_match_by_what_they_show_not_where_they_sit() {
        let before = lane(
            vec![card("a1", Some("a")), card("b1", Some("b"))],
            &["a", "b"],
        );
        let after = lane(
            vec![
                card("new", Some("c")),
                card("a1", Some("a")),
                card("b1", Some("b")),
            ],
            &["a", "b", "c"],
        );
        let (old, new) = (
            lane_rows(&before, true, false, &|_| false, &|_, _| 0),
            lane_rows(&after, true, false, &|_| false, &|_, _| 0),
        );
        // "a"'s header moved down two rows but shows the same.
        assert!(old[0].same_as(&before, new[2], &after));
        assert!(!old[0].same_as(&before, new[0], &after));
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    use fleet::lanes::LaneWorkspace;

    fn card(host: &str, provider: &str, workspace: &str) -> LaneCard {
        LaneCard {
            host: host.into(),
            agent_id: format!("{host}-{provider}-{workspace}"),
            workspace_id: Some(workspace.into()),
            workspace_name: workspace.into(),
            title: String::new(),
            provider: provider.into(),
            model: None,
            updated_at: None,
            bucket: fleet::Bucket::Done,
            attention_reason: None,
            parent: None,
        }
    }

    fn lane(key: &str, cards: Vec<LaneCard>, workspaces: &[(&str, &str)]) -> Rc<Lane> {
        Rc::new(Lane {
            key: key.into(),
            kind: LaneKind::Label,
            title: key.into(),
            color: None,
            label_keys: vec![key.into()],
            collapsed: false,
            hosts: vec!["mac".into(), "box".into()],
            cards,
            workspaces: workspaces
                .iter()
                .map(|(host, id)| LaneWorkspace {
                    host: (*host).into(),
                    workspace_id: (*id).into(),
                    name: (*id).into(),
                })
                .collect(),
        })
    }

    #[test]
    fn filters_keep_matching_cards_and_drop_emptied_lanes() {
        let lanes = vec![
            lane(
                "a",
                vec![card("mac", "claude/opus", "w1"), card("box", "codex", "w2")],
                &[("mac", "w1"), ("box", "w2"), ("mac", "empty")],
            ),
            lane("b", vec![card("box", "codex", "w3")], &[("box", "w3")]),
        ];
        let labels = |_: &str, workspace: &str| -> Vec<String> {
            if workspace == "w1" {
                vec!["infra".into()]
            } else {
                vec![]
            }
        };
        let none = LaneFilter::default();
        let all = filter_lanes(&lanes, &none, labels);
        assert!(Rc::ptr_eq(&all[0], &lanes[0]) && Rc::ptr_eq(&all[1], &lanes[1]));

        // A host: only its cards and workspaces, and lane b goes.
        let mac = LaneFilter {
            hosts: ["mac".to_string()].into(),
            ..Default::default()
        };
        let shown = filter_lanes(&lanes, &mac, labels);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].cards.len(), 1);
        assert_eq!(shown[0].workspaces.len(), 2);
        assert_eq!(shown[0].hosts, ["mac"]);

        // A tool: by provider id, ignoring the model; an empty workspace
        // has no agent of that tool, so it goes.
        let claude = LaneFilter {
            tools: ["claude".to_string()].into(),
            ..Default::default()
        };
        let shown = filter_lanes(&lanes, &claude, labels);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].workspaces.len(), 1);

        // A label: by the workspace's labels.
        let infra = LaneFilter {
            labels: ["infra".to_string()].into(),
            ..Default::default()
        };
        let shown = filter_lanes(&lanes, &infra, labels);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].cards[0].workspace_id.as_deref(), Some("w1"));

        // Kinds combine: codex on mac matches nothing.
        let both = LaneFilter {
            hosts: ["mac".to_string()].into(),
            tools: ["codex".to_string()].into(),
            ..Default::default()
        };
        assert!(filter_lanes(&lanes, &both, labels).is_empty());
    }
}

#[cfg(test)]
mod header_fit_tests {
    use super::*;

    #[test]
    fn header_controls_go_in_order_as_a_lane_narrows() {
        let at = |width: f32| HeaderFit::for_width(width, false, 1);
        let all = HeaderFit {
            count: true,
            add: true,
            collapse: true,
            steps: true,
            ends: true,
        };
        assert_eq!(at(LANE_WIDTH_MAX), all);
        let mut last = all;
        // Narrowing never brings a control back, and the ends go first.
        for width in (LANE_WIDTH_MIN as i32..LANE_WIDTH_MAX as i32).rev() {
            let fit = at(width as f32);
            let shown = [fit.count, fit.add, fit.collapse, fit.steps, fit.ends];
            let before = [last.count, last.add, last.collapse, last.steps, last.ends];
            assert!(shown.iter().zip(before).all(|(now, was)| !now | was));
            // Shown ones are always a prefix of the order.
            assert!(shown.windows(2).all(|pair| pair[0] | !pair[1]));
            last = fit;
        }
        // The narrowest lane still shows its title, count and new agent.
        let narrowest = at(LANE_WIDTH_MIN);
        assert!(narrowest.count && narrowest.add && !narrowest.ends);
    }
}

#[cfg(test)]
mod id_tests {
    use super::*;
    #[test]
    fn punctuation_and_combination_keys_have_distinct_element_scopes() {
        let keys = [
            "a + b",
            "a - b",
            "\u{0}label-set:[\"a+b\",\"c\"]",
            "\u{0}label-set:[\"a-b\",\"c\"]",
            "中文",
            "--",
        ];
        assert_eq!(
            keys.into_iter().map(safe_key).collect::<HashSet<_>>().len(),
            keys.len()
        );
    }

    #[test]
    fn composite_scopes_preserve_boundaries_and_host_identity() {
        assert_ne!(element_scope(&["x-y", "z"]), element_scope(&["x", "y-z"]));
        assert_ne!(element_scope(&["x_y", "z"]), element_scope(&["x", "y_z"]));
        assert_ne!(
            eid_parts("card", &["lane", "mac", "same-agent"]),
            eid_parts("card", &["lane", "windows", "same-agent"])
        );
        assert_ne!(element_scope(&["", "a"]), element_scope(&["a", ""]));
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use fleet::timeline::{Item, Timeline};

    #[test]
    fn latest_summary_ignores_body_only_stream_updates_and_uses_history_when_live_is_empty() {
        let assistant = |id: &str, text: &str| Item {
            id: id.into(),
            turn_id: None,
            timestamp: String::new(),
            kind: ItemKind::Assistant {
                text: text.into(),
                message_id: None,
            },
        };
        let mut timeline = Timeline::default();
        timeline.tail.push(assistant("older", "Historical summary"));
        timeline
            .head
            .push(assistant("live", "\n  Latest summary  \nBody"));
        let initial = assistant_blurb(&timeline);
        assert_eq!(initial.as_deref(), Some("Latest summary"));
        timeline.head[0] = assistant("live", "Latest summary\nBody changed by a stream update");
        assert_eq!(assistant_blurb(&timeline), initial);
        timeline.head[0] = assistant("live", "New summary\nBody");
        assert_ne!(assistant_blurb(&timeline), initial);
        timeline.head[0] = assistant("live", "   ");
        assert_eq!(
            assistant_blurb(&timeline).as_deref(),
            Some("Historical summary")
        );
    }
}

#[cfg(test)]
mod collapsed_tab_tests {
    use super::*;

    fn lane(key: &str, collapsed: bool) -> Rc<Lane> {
        Rc::new(Lane {
            key: key.into(),
            title: key.into(),
            kind: LaneKind::Other,
            color: None,
            label_keys: Vec::new(),
            hosts: Vec::new(),
            cards: Vec::new(),
            workspaces: Vec::new(),
            collapsed,
        })
    }

    #[test]
    fn displayed_snapshot_excludes_hidden_lanes_and_preserves_order() {
        let lanes = vec![
            lane("hidden-first", true),
            lane("a", false),
            lane("hidden-middle", true),
            lane("b", false),
        ];
        let visible = displayed_lanes(&lanes);
        assert_eq!(
            visible
                .iter()
                .map(|lane| lane.key.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(Rc::ptr_eq(&visible[1], &lanes[3]));
    }

    #[test]
    fn displayed_snapshot_tracks_restored_flags_in_an_empty_board() {
        let mut lanes = vec![lane("a", true), lane("b", true), lane("c", true)];
        assert!(displayed_lanes(&lanes).is_empty());
        Rc::make_mut(&mut lanes[1]).collapsed = false;
        assert_eq!(displayed_lanes(&lanes)[0].key, "b");
        Rc::make_mut(&mut lanes[0]).collapsed = false;
        Rc::make_mut(&mut lanes[2]).collapsed = false;
        let visible = displayed_lanes(&lanes);
        assert_eq!(visible[1].key, "b");
        assert_eq!(visible[2].key, "c");
    }

    #[test]
    fn reactive_focus_resolves_hidden_targets_without_revealing_them() {
        let mut lanes = vec![lane("a", false), lane("hidden", true), lane("b", false)];
        assert_eq!(
            visible_focus_lane(&lanes, Some("hidden")).as_deref(),
            Some("b")
        );
        assert_eq!(visible_focus_lane(&lanes, Some("a")).as_deref(), Some("a"));
        assert_eq!(
            visible_focus_lane(&lanes, Some("missing")).as_deref(),
            Some("a")
        );
        assert_eq!(visible_focus_lane(&lanes, None), None);
        Rc::make_mut(&mut lanes[2]).collapsed = true;
        assert_eq!(
            visible_focus_lane(&lanes, Some("hidden")).as_deref(),
            Some("a")
        );
        Rc::make_mut(&mut lanes[0]).collapsed = true;
        assert_eq!(visible_focus_lane(&lanes, Some("hidden")), None);
        assert!(lanes[1].collapsed);
    }

    #[test]
    fn hide_focus_prefers_next_visible_then_previous_and_handles_all_hidden() {
        let mut lanes = vec![
            lane("a", false),
            lane("hidden", true),
            lane("b", false),
            lane("c", false),
        ];
        assert_eq!(next_visible_lane(&lanes, 0).as_deref(), Some("b"));
        assert_eq!(next_visible_lane(&lanes, 3).as_deref(), Some("b"));
        Rc::make_mut(&mut lanes[2]).collapsed = true;
        Rc::make_mut(&mut lanes[3]).collapsed = true;
        assert_eq!(next_visible_lane(&lanes, 3).as_deref(), Some("a"));
        assert_eq!(next_visible_lane(&lanes, 0), None);
    }
}

#[cfg(test)]
mod new_lane_order_tests {
    use super::order_with_new_lanes;

    fn keys(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|key| key.to_string()).collect()
    }

    #[test]
    fn a_new_lane_goes_right_after_the_one_relabeled_from() {
        let known = keys(&["a", "b", "c", "d"]);
        // The model sorted the new lane to the end, as it does today.
        let current = keys(&["a", "b", "c", "d", "new"]);
        assert_eq!(
            order_with_new_lanes(&known, "b", &current),
            Some(keys(&["a", "b", "new", "c", "d"]))
        );
    }

    #[test]
    fn from_the_last_lane_it_goes_last() {
        let known = keys(&["a", "b"]);
        let current = keys(&["new", "a", "b"]);
        assert_eq!(
            order_with_new_lanes(&known, "b", &current),
            Some(keys(&["a", "b", "new"]))
        );
    }

    #[test]
    fn several_new_lanes_keep_their_order_and_gone_lanes_drop_out() {
        // Moving off the only card of "b" emptied it.
        let known = keys(&["a", "b", "c"]);
        let current = keys(&["a", "x", "c", "y"]);
        assert_eq!(
            order_with_new_lanes(&known, "b", &current),
            Some(keys(&["a", "x", "y", "c"]))
        );
    }

    #[test]
    fn no_new_lane_leaves_the_order_alone() {
        let known = keys(&["a", "b"]);
        assert_eq!(order_with_new_lanes(&known, "a", &keys(&["b", "a"])), None);
    }
}
