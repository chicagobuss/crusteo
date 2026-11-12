//! One lane's column: the cached view and its fingerprint, the lists of
//! cards it holds, and how wide it is drawn.

use fleet::HostId;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use fleet::lanes::{Lane, LaneKind, LaneModel, open_lane_order};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::gpui::{
    AnyElement, App, AppContext, Context, CursorStyle, DragMoveEvent, Entity, InteractiveElement,
    IntoElement, ListAlignment, ListState, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Render, Rgba, ScrollWheelEvent, StatefulInteractiveElement, Styled, Subscription, WeakEntity,
    Window, canvas, div, px, rgb,
};

use gpui_kit::prelude::FluentBuilder;

use crate::app::nav::LaneFilter;
use crate::app::store::{FleetStore, now_millis};
use crate::layouts::columns::{self};
use crate::theme::{self, palette};
use crate::widgets::lane_color_picker;

use super::*;

/// One lane column as its own cached view. It tracks the live summaries it
/// depends on; its render asks the board to build the column. GPUI
/// reuses a cached view's subtree until that entity is notified, so hovering
/// a card or scrolling a lane re-renders one column, and the board notifies
/// a column only when what it shows changed (see [`ColumnInputs`]).
pub(super) struct ColumnView {
    pub(super) board: WeakEntity<Lanes>,
    pub(super) inputs: Option<ColumnInputs>,
    /// Stream changes need to invalidate this cached view independently of
    /// directory changes, including another copy of an expanded agent.
    session_watches: Vec<Subscription>,
    watched_lane: Option<(Rc<Lane>, u64)>,
}

/// Everything a column shows, as last handed to it by the board.
#[derive(Clone)]
pub(super) struct ColumnInputs {
    pub(super) lanes: LaneSet,
    pub(super) ix: usize,
    pub(super) width: Pixels,
    pub(super) style: LaneStyle,
    /// A hash of the board state the column reads: focus, expansion, draft,
    /// relabeling, live-session membership, store revision where it matters,
    /// and the minute for "3m ago" labels. See [`Lanes::column_fingerprint`].
    pub(super) fingerprint: u64,
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

/// Aggregated once per lane change, rather than per scroll or hover frame.
pub(super) fn lane_decor(lane: &Lane, colors: &BTreeMap<String, String>) -> LaneDecor {
    LaneDecor {
        activity: activity::Summary::from_cards(&lane.cards),
        swatch: matches!(lane.kind, LaneKind::Label | LaneKind::Combination)
            .then(|| colors::index(&lane.key, lane.color.as_deref(), colors)),
    }
}

/// The incrementally maintained lane model, its latest snapshot, and display
/// data derived per lane.
pub(super) struct LaneCache {
    pub(super) model: LaneModel,
    /// Store change-log position the model reflects.
    pub(super) cursor: u64,
    pub(super) nav_key: LaneModelKey,
    pub(super) lanes: LaneSet,
    pub(super) visible: LaneSet,
    /// The filter `visible` was made with; `None` when it must be remade.
    pub(super) visible_filter: Option<LaneFilter>,
    pub(super) decor: HashMap<String, LaneDecor>,
}

#[derive(Clone, Copy, Default)]
pub(super) struct LaneDecor {
    pub(super) activity: activity::Summary,
    pub(super) swatch: Option<usize>,
}

/// Resolved once per rendered lane, then copied into virtual card builders.
#[derive(Clone, Copy)]
pub(super) struct LaneStyle {
    pub(super) column: Rgba,
    column_hover: Rgba,
    pub(super) header: Rgba,
    pub(super) card: Rgba,
    pub(super) card_hover: Rgba,
    pub(super) swatch: Option<usize>,
    editable: bool,
}

/// A lane's card list only builds the rows in view. `source` is the lane
/// and row settings the list state was last told about, to see what
/// changed; `rows` are that lane's rows.
/// What a lane's rows were made from: the lane, whether single-agent
/// workspaces are boxed, whether its empty workspaces are unfolded, and the
/// subagent-fold and terminal-list generations.
type ListSource = (Rc<Lane>, bool, bool, (u64, u64));

pub(super) struct LaneList {
    pub(super) state: ListState,
    pub(super) source: Option<ListSource>,
    pub(super) rows: Rc<Vec<Row>>,
}

impl LaneList {
    pub(super) fn new() -> Self {
        Self {
            state: ListState::new(0, ListAlignment::Top, px(240.)),
            source: None,
            rows: Rc::new(Vec::new()),
        }
    }

    /// Tells the list which rows changed, keeping the scroll position.
    pub(super) fn sync(&mut self, lane: &Rc<Lane>, board: &Lanes, box_single: bool, cx: &App) {
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

/// Rows the list under an open card shows before its edge is dragged.
pub(super) const OPEN_LIST_ROWS: usize = 6;

/// The list under a lane's open card: its scroll, and which cards it held
/// last, to keep the scroll where it was as cards come, go and move.
pub(super) struct OpenList {
    pub(super) state: ListState,
    pub(super) cards: Vec<(HostId, String)>,
}

impl OpenList {
    pub(super) fn new() -> Self {
        Self {
            state: ListState::new(0, ListAlignment::Top, px(240.)),
            cards: Vec::new(),
        }
    }

    /// Tells the list which rows changed, keeping the scroll position.
    pub(super) fn sync(&mut self, cards: Vec<(HostId, String)>) {
        let old = &self.cards;
        let prefix = old.iter().zip(&cards).take_while(|(a, b)| a == b).count();
        let suffix = (old[prefix..].iter().rev())
            .zip(cards[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let (old_end, new_end) = (old.len() - suffix, cards.len() - suffix);
        if old_end > prefix || new_end > prefix {
            self.state.splice(prefix..old_end, new_end - prefix);
        }
        self.cards = cards;
    }
}

/// A lane's vertical scrollbar: always shown while there is more to scroll
/// to, thin, so where the lane is in its cards is plain at a glance.
pub(super) fn lane_scrollbar(bar: Scrollbar) -> impl IntoElement {
    div().absolute().inset_0().child(
        bar.mode(ScrollbarMode::Always)
            .styles(|styles| styles.thumb(|thumb| thumb.width(px(4.)).inset(px(1.)))),
    )
}

impl Lanes {
    /// Whether lane has a card or a terminal open in it, which makes it
    /// wider than the rest and lets its width be dragged.
    pub(super) fn lane_open(&self, lane: &str) -> bool {
        self.expanded.contains_key(lane) || self.expanded_terminals.contains_key(lane)
    }

    pub(super) fn lane_width(&self, lane: &Lane, expanded_width: Pixels) -> Pixels {
        if self.lane_open(&lane.key) {
            self.expanded_widths
                .get(&lane.key)
                .copied()
                .unwrap_or(expanded_width)
        } else {
            self.column_width
        }
    }

    /// The ordinary column width: the user's, or narrower when fitting every
    /// column into `visible`. Open lanes keep their own widths.
    pub(super) fn fit_width(
        &self,
        lanes: &[Rc<Lane>],
        expanded_width: Pixels,
        visible: Pixels,
        cx: &App,
    ) -> Pixels {
        let mut fixed = px(0.);
        let mut ordinary = 0usize;
        for lane in lanes {
            if self.lane_open(&lane.key) {
                fixed += self.lane_width(lane, expanded_width);
            } else {
                ordinary += 1;
            }
        }
        columns::fit_width(
            self.width.chosen(cx),
            self.width.fit(cx),
            ordinary,
            lanes.len(),
            fixed,
            visible,
        )
    }

    /// Ctrl+wheel makes every column wider or narrower, like zooming.
    /// Returns whether the event was a resize.
    pub(super) fn wheel_resize(
        &mut self,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        self.width.wheel(event, |this| &mut this.width, cx)
    }

    pub(super) fn lane_decor(&self, lane: &Lane) -> LaneDecor {
        self.lanes_cache
            .borrow()
            .as_ref()
            .and_then(|cache| cache.decor.get(&lane.key))
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn lane_style(&self, lane: &Lane, cx: &App) -> LaneStyle {
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

    pub(super) fn lane_dot(&self, lane: &Lane, width: Pixels, style: LaneStyle) -> AnyElement {
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
    pub(super) fn column_view(
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
    pub(super) fn column_fingerprint(&self, lane: &Lane, style: &LaneStyle, cx: &App) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        let key = lane.key.as_str();
        let focused_here = self.focused_lane.as_deref() == Some(key);
        focused_here.hash(&mut hash);
        if focused_here {
            self.focused_agent.hash(&mut hash);
            self.focused_row.hash(&mut hash);
        }
        let expanded = self.expanded.get(key);
        expanded.hash(&mut hash);
        // Its open card's bar shows a colour pinned for its agent.
        (expanded.and_then(|(host, agent)| self.core.nav.read(cx).agent_color(host, agent)))
            .hash(&mut hash);
        // Cards out in their own windows are marked.
        (self.core.nav.read(cx).popouts.keys())
            .filter(|(host, agent)| {
                lane.cards
                    .iter()
                    .any(|c| &c.host == host && &c.agent_id == agent)
            })
            .for_each(|key| key.hash(&mut hash));
        // Its open card blinking after a notification.
        expanded
            .is_some_and(|(host, agent)| self.flash_lit(host, agent))
            .hash(&mut hash);
        for key in [key.to_string(), terminal_height_key(key)] {
            (self.expanded_heights.get(&key))
                .map(|height| height.as_f32().to_bits())
                .hash(&mut hash);
        }
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
        if expanded.is_some() {
            // An open card reads live store state (archive availability)
            // beyond its lane's cards.
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

    pub(super) fn column(
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
        let open_terminal = self.expanded_terminals.get(&lane.key).cloned();
        if let Some((host, terminal)) = open_terminal {
            let box_single = self.core.nav.read(cx).box_single_workspaces;
            let rows = self.rows(lane, box_single, cx);
            let at = {
                let store = self.core.store.read(cx);
                rows.iter().position(|row| match *row {
                    Row::Terminal { owner, index, .. } => {
                        owner.workspace(lane).is_some_and(|(h, w)| {
                            *h == host
                                && store.workspace_terminal_ids(h, w).nth(index) == Some(&terminal)
                        })
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
                            false,
                            style,
                            window,
                            cx,
                        ));
                    }
                    let open = self.terminal_box(
                        lane,
                        &rows[start..end],
                        at - start,
                        &host,
                        &terminal,
                        style,
                        window,
                        cx,
                    );
                    // A terminal keeps a height of its own, apart from the
                    // lane's open card.
                    let key = terminal_height_key(&lane.key);
                    let height = self.expanded_heights.get(&key).copied();
                    let below = end < rows.len();
                    body = body
                        .child(self.measure_body(&key))
                        .child(self.sized_open(&key, open, height, below, false, cx));
                    if below {
                        body = body.child(self.compact_strip(
                            lane,
                            &rows[end..],
                            "below",
                            height.is_some(),
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
            // The open card at the top of its lane, outside its workspace's
            // box, and the lane's other cards under it as one-line rows:
            // those needing the user first, then running ones, then the rest.
            let lane = &lanes[lane_ix];
            let in_focus = self.in_focus_mode(&lane.key, &host, &agent);
            let open = if in_focus {
                self.focus_placeholder(&lane.key, &host, &agent, true, cx)
            } else {
                let card = self.expanded_card(lane, &host, &agent, style, true, window, cx);
                self.measured_card(&lane.key, card)
            };
            let others = open_lane_order(lane, &(host.clone(), agent.clone()));
            let below = !others.is_empty();
            // Its placeholder in focus mode keeps its own size.
            let height = (!in_focus).then(|| self.expanded_heights.get(&lane.key).copied());
            body = match height {
                Some(height) => body
                    .child(self.measure_body(&lane.key))
                    .child(self.sized_open(
                        &lane.key,
                        open,
                        height,
                        below,
                        self.flash_lit(&host, &agent),
                        cx,
                    )),
                None => body.child(open),
            };
            if below {
                // A card made shorter leaves the rest of the lane to the list.
                let fill = matches!(height, Some(Some(_)) | None);
                body = body.child(self.open_lane_list(lanes, lane_ix, others, fill, style, cx));
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
        let resizable = self.lane_open(&lane.key);
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
                            Tooltip::new(
                                "Drag to resize this lane while a card or terminal is open",
                            )
                            .build(window, cx)
                        }),
                )
            })
            .into_any_element()
    }
}
