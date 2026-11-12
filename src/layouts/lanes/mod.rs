//! Lanes: one column per workstream, each agent a card. Expanding a card
//! embeds the Paseo conversation in the column.

mod activity;
mod bands;
mod boxes;
mod cards;
pub(crate) mod colors;
mod column;
mod expand;
mod find;
mod header;
mod interactions;
mod menus;
mod model;
mod place;
mod placement;
mod relabel;
mod rows;
mod toolbar;

pub use place::{agent_folder, agent_swatch, place_label, swatch_tint};
use {
    cards::*, column::*, header::*, interactions::*, menus::*, model::*, place::*, relabel::*,
    rows::*, toolbar::*,
};

use fleet::HostId;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use fleet::lanes::Lane;
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::gpui::{
    AnyView, App, AppContext, Bounds, Context, CursorStyle, Entity, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    StyleRefinement, Styled, Subscription, Window, div, point, px,
};

use gpui_kit::prelude::FluentBuilder;

use crate::app::Core;
use crate::layouts::columns::{self, COLUMN_GAP, COLUMNS_PADDING, ColumnWidth, SideScroll};
use crate::layouts::paseo::{Conversation, TerminalView};
use crate::theme::palette;
use crate::widgets::agent_rename::AgentRename;
use crate::widgets::label_picker::LabelPicker;

const EXPANDED_MIN: f32 = 360.;
const EXPANDED_MAX: f32 = 720.;
/// Narrowest an expanded column can be dragged to.
const EXPANDED_RESIZE_MIN: f32 = 300.;
const CARD_GAP: f32 = 6.;
const CARD_HEIGHT: f32 = 90.;
/// Beside a header's rows; above and below them is [`HEADER_PADDING_Y`].
const HEADER_PADDING: f32 = 10.;
const HEADER_PADDING_Y: f32 = 8.;
const HEADER_TITLE_HEIGHT: f32 = 24.;
/// The hosts, and the lane's state counts at its right end.
const HEADER_HOST_HEIGHT: f32 = 20.;
const HEADER_ROW_GAP: f32 = 6.;
/// The strip over the columns while any lane is under a band.
const BAND_HEIGHT: f32 = 28.;
const HEADER_HEIGHT: f32 =
    HEADER_PADDING_Y * 2. + HEADER_TITLE_HEIGHT + HEADER_HOST_HEIGHT + HEADER_ROW_GAP + 1.;

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

/// A lane as grouped for display: the lane it came from, and the grouped
/// lane.
type ArrangedLane = (Rc<Lane>, Rc<Lane>);

/// A drag of an expanded column's right edge.
struct ColumnResize {
    lane: String,
    /// Pointer x and column width when the drag began.
    start_x: Pixels,
    start_width: Pixels,
}

/// An open card blinking its outline, as one opened from a notification
/// does, so the eye finds it.
struct CardFlash {
    host: HostId,
    agent: String,
    /// In the lit half of a blink.
    lit: bool,
}

/// How long each half of a card's blink lasts.
const FLASH_STEP: Duration = Duration::from_millis(400);

/// A drag of an open card's bottom edge.
struct HeightResize {
    lane: String,
    /// Pointer y and the open card's height when the drag began.
    start_y: Pixels,
    start_height: Pixels,
    /// The shortest it can be: its conversation at its least, its box's
    /// header and mates kept whole.
    min_height: Pixels,
    /// The tallest it can be: down to the lane's bottom, less a row for the
    /// cards below it. Dragged there, it fills the lane again.
    max_height: Pixels,
}

/// The key a lane's open terminal keeps its height under, apart from the
/// lane's open card's.
fn terminal_height_key(lane: &str) -> String {
    format!("{lane}\u{1f}terminal")
}

/// Where a lane's body and its open card were last drawn, for a
/// [`HeightResize`] to start from.
#[derive(Clone, Copy, Default)]
struct OpenBounds {
    body: Option<Bounds<Pixels>>,
    /// The open card with its workspace's box, when it has one.
    open: Option<Bounds<Pixels>>,
    /// The open card alone: only it gives way as the box gets shorter.
    card: Option<Bounds<Pixels>>,
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
    /// Scrolling across the lanes.
    side: SideScroll,
    width: ColumnWidth,
    collapsed_scroll: ScrollHandle,
    /// Lane key → (host, agent) expanded in that column.
    expanded: BTreeMap<String, (HostId, String)>,
    /// Lane key → (host, terminal) open in that column, in place of an
    /// expanded agent.
    expanded_terminals: BTreeMap<String, (HostId, String)>,
    terminal_views: HashMap<(HostId, String), Entity<TerminalView>>,
    /// The workspaces whose terminal lists Lanes last asked to watch.
    terminal_watch_key: u64,
    conversations: HashMap<(HostId, String), Entity<Conversation>>,
    /// Agent just created, kept expanded until the directory lists it.
    /// Lane, host and agent.
    pending: Option<(String, HostId, String)>,
    focused_lane: Option<String>,
    focused_agent: Option<(HostId, String)>,
    /// Focus this composer's input once it exists.
    focus_composer: Option<(HostId, String)>,
    /// Per lane key: the virtual list of its cards.
    lists: HashMap<String, LaneList>,
    /// Per lane with an open card: the list of its other cards under it.
    open_lists: HashMap<String, OpenList>,
    /// Per lane key: the cached view the column renders in.
    columns: HashMap<String, Entity<ColumnView>>,
    /// The lane model, patched from the store's change log and rebuilt only
    /// when the nav's grouping, order or colours change.
    lanes_cache: RefCell<Option<LaneCache>>,
    /// Last observed nav transition for focus reconciliation and stopping pan.
    /// Model validity itself belongs to LaneCache::nav_key.
    observed_nav_key: Option<LaneModelKey>,
    /// Width of an ordinary column in the last render, after fitting.
    column_width: Pixels,
    /// The one right-click menu for every card, open over the card it was
    /// opened on.
    card_menu: Option<CardMenu>,
    /// Widths the user dragged expanded columns to, by lane. A lane drops
    /// its width when it no longer has an expanded card, so it returns to
    /// the width the other lanes share.
    expanded_widths: HashMap<String, Pixels>,
    /// An expanded column's right edge being dragged.
    resizing: Option<ColumnResize>,
    /// Heights the user dragged open cards to, by lane, leaving the rest of
    /// the lane to the cards below; open terminals' by
    /// [`terminal_height_key`]. Without one, it fills the lane.
    expanded_heights: HashMap<String, Pixels>,
    /// An open card's bottom edge being dragged.
    resizing_height: Option<HeightResize>,
    /// Per lane, where its body and open card were last drawn.
    open_bounds: Rc<RefCell<HashMap<String, OpenBounds>>>,
    /// The card blinking after a notification opened it, and which blink
    /// it is, so a newer one stops an older one's timer.
    flash: Option<CardFlash>,
    flash_generation: u64,
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
    focus_mode: Option<(String, HostId, String)>,
    /// The lane whose box a workspace rename was started from; the same
    /// workspace's boxes in other lanes keep showing its name.
    rename_lane: Option<String>,
    /// Agents showing their subagents unfolded, by host and agent id.
    subagents_open: HashMap<HostId, HashSet<String>>,
    label_pickers: HashMap<(String, HostId, String, String), Entity<LabelPicker>>,
    /// The catalog revision whose ids the saved lane keys were last
    /// brought to, in reef mode.
    labels_adopted: Option<u64>,
    relabel_pending: Option<RelabelPending>,
    next_relabel: u64,
    view_membership: Option<placement::ViewMembership>,
    focus_check_needed: Cell<bool>,
    /// A lane being dragged, and where the board shows it.
    lane_drag: Option<LaneDragPreview>,
    /// The find bar, while open: see [`find`].
    find: Option<find::Find>,
    /// Where a card dragged inside its box would land: before or after
    /// this card, as host, agent and whether before.
    card_drop: Option<(HostId, String, bool)>,
    /// Loaded conversations lowercased for find, kept between searches.
    find_index: RefCell<HashMap<(HostId, String), find::Indexed>>,
}

/// The board's lanes. Each lane is shared, so a snapshot is cheap and an
/// unchanged lane keeps its identity across updates.
type LaneSet = Rc<Vec<Rc<Lane>>>;

gpui_kit::actions!(lanes, [FindOnBoard]);

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

        // A lane whose card or terminal closed returns to the shared width.
        let (expanded, terminals) = (&self.expanded, &self.expanded_terminals);
        self.expanded_widths
            .retain(|lane, _| expanded.contains_key(lane) || terminals.contains_key(lane));
        self.open_lists
            .retain(|lane, _| expanded.contains_key(lane));
        if self
            .resizing
            .as_ref()
            .is_some_and(|resize| !self.lane_open(&resize.lane))
        {
            self.resizing = None;
        }

        let open: Vec<(HostId, String)> = self.expanded.values().cloned().collect();
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

        let visible_width = columns::visible_width(&self.side, window);
        let visible = self.arranged(&self.visible_lanes(cx), cx);
        // While a lane is dragged, the lanes show where it would land.
        let visible = match &self.lane_drag {
            Some(drag) => in_order(&visible, &drag.order),
            None => visible,
        };
        // While any lane is under a band, every column has a strip on top.
        let banded = visible.iter().any(|lane| lane.band.is_some());
        let strip = if banded { BAND_HEIGHT } else { 0. };
        let mut columns = columns::wheel(
            div()
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
                .track_scroll(&self.side.handle),
            px(HEADER_HEIGHT + strip),
            |this| &mut this.side,
            |this| &mut this.width,
            cx,
        );
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
        let offset = self.side.handle.offset();
        let scale = window.scale_factor();
        let snapped = px((offset.x.as_f32() * scale).round() / scale);
        if snapped != offset.x {
            self.side.handle.set_offset(point(snapped, offset.y));
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
                        self.side.handle.set_offset(point(-target, offset.y));
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
        // Where each column sits on the board, and the part of the board in
        // view: a band's name and buttons stay in the part of it you see.
        let spans: Vec<(Pixels, Pixels)> = if banded {
            let mut at = px(COLUMNS_PADDING);
            (visible.iter())
                .map(|lane| {
                    let width = self.lane_width(lane, expanded_width);
                    let span = (at, width);
                    at += width + px(COLUMN_GAP);
                    span
                })
                .collect()
        } else {
            Vec::new()
        };
        let in_view = (scroll_x, scroll_x + visible_width);
        let mut x = px(COLUMNS_PADDING);
        // Workspaces in lanes near the view, whose terminals they list.
        let mut watched: Vec<(HostId, String)> = Vec::new();
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
            let column = if near {
                let style = self.lane_style(lane, cx);
                let inputs = ColumnInputs {
                    lanes: visible.clone(),
                    ix,
                    width,
                    style,
                    fingerprint: self.column_fingerprint(lane, &style, cx),
                };
                // An expanded card records where it was drawn (popover
                // anchors, text input), so only plain card columns may reuse
                // their drawing when scrolling moves them. See
                // vendor/gpui-pre/PATCHES.md for relocation restrictions.
                let relocatable = !self.expanded.contains_key(&lane.key)
                    && !self.expanded_terminals.contains_key(&lane.key);
                let view = self.column_view(&lane.key, inputs, cx);
                // A cached view is laid out from this style, not its content.
                let style = StyleRefinement::default().w(width).flex_shrink_0();
                let style = if banded {
                    style.flex_1().min_h(px(0.))
                } else {
                    style.h_full()
                };
                AnyView::from(view)
                    .cached(style)
                    .relocatable(relocatable)
                    .into_any_element()
            } else if banded {
                div().w(width).flex_1().min_h(px(0.)).into_any_element()
            } else {
                div().w(width).flex_shrink_0().h_full().into_any_element()
            };
            columns = columns.child(if banded {
                div()
                    .w(width)
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .child(self.band_strip(&visible, ix, width, cx))
                    .child(column)
                    .into_any_element()
            } else {
                column
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
        // Bands' names and buttons, over their strips, where they are seen.
        if banded {
            columns = columns.children(self.band_bars(&visible, &spans, in_view, cx));
        }
        self.watch_terminals(watched, cx);
        self.lists.retain(|key, _| keys.contains(key.as_str()));
        let visible_keys: HashSet<&str> = visible.iter().map(|lane| lane.key.as_str()).collect();
        self.columns
            .retain(|key, _| visible_keys.contains(key.as_str()));

        let board = div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(columns)
            .children(self.side.overlays(p.surface0))
            .child(self.width_control(cx));
        let scrolling = self.side.scrolling() || self.resizing.is_some();

        let find_bar = self.find_bar(cx);
        let root = div()
            .id("lanes-root")
            .key_context("Lanes")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &FindOnBoard, window, cx| {
                this.open_find(window, cx);
            }));
        columns::listen(root, |this| &mut this.side, cx)
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface0)
            .when(scrolling, |root| root.cursor(CursorStyle::ResizeLeftRight))
            .when(self.resizing_height.is_some(), |root| {
                root.cursor(CursorStyle::ResizeUpDown)
            })
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
            .relative()
            .child(board)
            .children(self.focus_overlay(window, cx))
            .children(self.card_menu_layer(window, cx))
            .children(find_bar)
            .children(self.state_bar(cx))
            .children(self.side.autoscroll_marker())
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

impl Lanes {
    fn new(core: Core, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |this, _, cx| {
            this.adopt_label_ids(cx);
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
                this.flash_card(&host, &agent, cx);
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
            cx.notify();
        })
        .detach();
        cx.observe_window_activation(window, |this, window, cx| {
            this.side.set_window_active(window.is_window_active(), cx);
        })
        .detach();
        let width = core.nav.read(cx).lane_width;
        let column_width = ColumnWidth::new("lanes", &core, window, cx);
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
                        if this.side.window_active() && this.core.nav.read(cx).layout == "lanes" {
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
            side: SideScroll::new(window),
            width: column_width,
            collapsed_scroll: ScrollHandle::new(),
            expanded: BTreeMap::new(),
            expanded_terminals: BTreeMap::new(),
            terminal_views: HashMap::new(),
            terminal_watch_key: 0,
            conversations: HashMap::new(),
            pending: None,
            focused_lane: None,
            focused_agent: None,
            focus_composer: None,
            lists: HashMap::new(),
            open_lists: HashMap::new(),
            columns: HashMap::new(),
            lanes_cache: RefCell::new(None),
            observed_nav_key: Some(initial_model_key),
            column_width: px(width),
            card_menu: None,
            expanded_widths: HashMap::new(),
            resizing: None,
            expanded_heights: HashMap::new(),
            resizing_height: None,
            open_bounds: Rc::default(),
            flash: None,
            flash_generation: 0,
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
            labels_adopted: None,
            relabel_pending: None,
            next_relabel: 0,
            view_membership: None,
            focus_check_needed: Cell::new(false),
            lane_drag: None,
            find: None,
            card_drop: None,
            find_index: RefCell::new(HashMap::new()),
        }
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
