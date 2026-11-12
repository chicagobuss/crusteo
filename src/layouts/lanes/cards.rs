//! Cards in a column: collapsed, expanded with their conversation, and the
//! subagents under them.

use fleet::HostId;
use std::rc::Rc;

use fleet::lanes::{Lane, LaneCard, card_workspace};
use fleet::timeline::ItemKind;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::Icon;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::gpui::{
    Anchor, AnyElement, App, ClickEvent, Context, CursorStyle, Focusable, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, Window, canvas, div, list, px,
};

use gpui_kit::prelude::FluentBuilder;

use crate::app::store::now_millis;
use crate::layouts::columns::edges;
use crate::theme::{palette, size};
use crate::ui;

use super::*;

pub(super) fn assistant_blurb(timeline: &fleet::timeline::Timeline) -> Option<String> {
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
    let asks = || {
        (store.fleet.agent(&card.host, &card.agent_id))
            .and_then(|agent| agent.pending_permissions.first())
            .is_some_and(fleet::questions::is_question)
    };
    match card.attention_reason.as_deref() {
        Some("finished") => Some("Finished".into()),
        Some("permission") if asks() => Some("Has a question".into()),
        Some("permission") => Some("Needs permission".into()),
        Some("error") => Some("Error".into()),
        Some(other) if !other.is_empty() => Some(other.to_string()),
        _ => None,
    }
}

/// How long ago a card's agent last did something, kept whole at the end of
/// its footer while the blurb before it is cut short.
fn card_age(age: String) -> AnyElement {
    div()
        .flex_shrink_0()
        .line_height(px(16.))
        .text_size(size::sm())
        .text_color(palette().extra_muted)
        .child(age)
        .into_any_element()
}

/// Marks a subagent's row as started by the row above.
fn subagent_mark() -> AnyElement {
    ui::icon("corner-down-right", 12., palette().extra_muted).into_any_element()
}

/// A card's workspace, host and model; in a box, whose header names the
/// workspace and host, only its model.
fn meta(card: &LaneCard, boxed: bool, fleet: &fleet::Fleet) -> String {
    let mut parts = Vec::new();
    if !boxed {
        if !card.workspace_name.is_empty() {
            parts.push(card.workspace_name.clone());
        }
        parts.push(fleet.host_name(&card.host).to_string());
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

/// The mark on a card whose agent is out in a window of its own.
fn popped_mark() -> AnyElement {
    div()
        .flex_shrink_0()
        .id("popped-out")
        .child(ui::icon("square-arrow-out-up-right", 12., palette().muted))
        .tooltip(|window, cx| {
            Tooltip::new("In its own window: click to bring it forward").build(window, cx)
        })
        .into_any_element()
}

impl Lanes {
    /// A virtual list: only the cards in view (plus overdraw) are built and
    /// laid out, however long the lane is.
    pub(super) fn card_list(
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
        let fades = edges::list_fades(state.clone());
        let bar = Scrollbar::vertical(&state);
        let list = list(
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
        .w_full();
        // Fades where the lane has more cards to scroll to.
        div()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .flex()
            .flex_col()
            .relative()
            .child(list)
            .child(fades)
            .child(lane_scrollbar(bar))
            .into_any_element()
    }

    /// The cards under a lane's open card, one line each in `order` (see
    /// [`open_lane_order`]), scrolling with fades and a thin bar. It holds
    /// a few rows, or with `fill` the rest of the lane.
    pub(super) fn open_lane_list(
        &mut self,
        lanes: &LaneSet,
        lane_ix: usize,
        order: Vec<usize>,
        fill: bool,
        style: LaneStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lane = &lanes[lane_ix];
        let cards: Vec<(HostId, String)> = (order.iter())
            .map(|&ix| (lane.cards[ix].host.clone(), lane.cards[ix].agent_id.clone()))
            .collect();
        let entry = self
            .open_lists
            .entry(lane.key.clone())
            .or_insert_with(OpenList::new);
        entry.sync(cards);
        let state = entry.state.clone();
        let count = order.len();
        let fades = edges::list_fades(state.clone());
        let bar = Scrollbar::vertical(&state);
        let (lanes, order) = (lanes.clone(), Rc::new(order));
        let list = list(
            state,
            cx.processor(move |this, ix: usize, window, cx| {
                let lane = &lanes[lane_ix];
                let Some(&card) = order.get(ix) else {
                    return div().into_any_element();
                };
                let row = Row::Card {
                    card,
                    boxed: false,
                    last: true,
                    depth: 0,
                };
                div()
                    .w_full()
                    .pb(px(CARD_GAP))
                    .child(this.row_element(lane, row, true, true, style, window, cx))
                    .into_any_element()
            }),
        )
        .flex_1()
        .min_h(px(0.))
        .w_full();
        let rows = count.min(OPEN_LIST_ROWS) as f32;
        div()
            .w_full()
            .flex()
            .flex_col()
            .relative()
            .map(|box_| match fill {
                true => box_.flex_1().min_h(px(COMPACT_HEIGHT + CARD_GAP)),
                false => box_
                    .flex_shrink_0()
                    .h(px(rows * (COMPACT_HEIGHT + CARD_GAP))),
            })
            .child(list)
            .child(fades)
            .child(lane_scrollbar(bar))
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
        let scope = element_scope(&[&lane.key, card.host.as_str(), &card.agent_id]);
        let at = (card.updated_at.as_deref())
            .or_else(|| agent.and_then(|agent| agent.created_at.as_deref()));
        let activity = activity::label(at, now_millis());
        // A blurb takes the line, and the age keeps to its right edge.
        let age = (blurb.is_some() && at.is_some()).then(|| activity.clone());
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
                    )
                    .when(self.popped_out(card, cx), |row| row.child(popped_mark())),
            )
            .child(
                div()
                    .h(px(16.))
                    .flex_shrink_0()
                    .line_height(px(16.))
                    .text_size(size::sm())
                    .text_color(p.muted)
                    .truncate()
                    .child(meta(
                        card,
                        matches!(frame, Frame::Boxed { .. }),
                        &store.fleet,
                    )),
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
                    .when_some(age, |d, age| d.child(card_age(age)))
                    .when_some(actions, |d, actions| {
                        d.child(
                            div()
                                .id(eid_parts(
                                    "lane-actions",
                                    &[&lane.key, card.host.as_str(), &card.agent_id],
                                ))
                                .min_w(px(0.))
                                .max_w(gpui_kit::gpui::relative(0.75))
                                .overflow_x_scroll()
                                .restrict_scroll_to_axis()
                                .child(actions),
                        )
                    }),
            );
        let id = eid_parts(
            "lane-card",
            &[&lane.key, card.host.as_str(), &card.agent_id],
        );
        let shell = card_frame(id, CARD_HEIGHT, focused, frame, look, content);
        self.reorderable(shell, lane, card, frame, cx)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.expand(&lane_key, &host, &agent_id, window, cx);
            }))
            .on_mouse_down(MouseButton::Right, self.card_menu_listener(card, cx))
            .into_any_element()
    }

    /// Whether `card`'s agent is out in a window of its own.
    fn popped_out(&self, card: &LaneCard, cx: &App) -> bool {
        (self.core.nav.read(cx).popouts).contains_key(&(card.host.clone(), card.agent_id.clone()))
    }

    /// An open card, recording where it is drawn, for dragging its height.
    pub(super) fn measured_card(&self, lane: &str, card: AnyElement) -> AnyElement {
        let bounds = self.open_bounds.clone();
        let lane = lane.to_string();
        div()
            .flex_1()
            .min_h(px(EXPANDED_MIN_HEIGHT))
            .flex()
            .flex_col()
            .relative()
            .child(card)
            .child(
                canvas(
                    move |rect, _, _| {
                        bounds.borrow_mut().entry(lane).or_default().card = Some(rect);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }

    /// Records where a lane's body is drawn, for dragging its open card's
    /// height.
    pub(super) fn measure_body(&self, lane: &str) -> AnyElement {
        let bounds = self.open_bounds.clone();
        let lane = lane.to_string();
        canvas(
            move |rect, _, _| {
                bounds.borrow_mut().entry(lane).or_default().body = Some(rect);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    /// The open card `height` tall, or filling the lane when none, with an
    /// edge along its bottom to drag. With cards `below` it, dragged as far
    /// down as it goes it still leaves a row of them. `flashing` outlines it
    /// brightly, in the lit half of a blink.
    pub(super) fn sized_open(
        &self,
        lane: &str,
        open: AnyElement,
        height: Option<Pixels>,
        below: bool,
        flashing: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let measure = {
            let bounds = self.open_bounds.clone();
            let lane = lane.to_string();
            canvas(
                move |rect, _, _| {
                    bounds.borrow_mut().entry(lane).or_default().open = Some(rect);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        };
        let start = {
            let bounds = self.open_bounds.clone();
            let lane = lane.to_string();
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                let Some(OpenBounds {
                    body: Some(body),
                    open: Some(open),
                    card,
                }) = bounds.borrow().get(&lane).copied()
                else {
                    return;
                };
                // Only the conversation gives way, down to its least.
                let give = card.map_or(px(0.), |card| {
                    (card.size.height - px(EXPANDED_MIN_HEIGHT)).max(px(0.))
                });
                // The body's bottom padding stays, and a row of the cards below.
                let reserve = px(6.)
                    + if below {
                        px(COMPACT_HEIGHT + CARD_GAP)
                    } else {
                        px(0.)
                    };
                this.resizing_height = Some(HeightResize {
                    lane: lane.clone(),
                    start_y: event.position.y,
                    start_height: open.size.height,
                    min_height: open.size.height - give,
                    max_height: body.bottom() - open.top() - reserve,
                });
                cx.stop_propagation();
                cx.notify();
            })
        };
        let reset = {
            let lane = lane.to_string();
            cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 && this.expanded_heights.remove(&lane).is_some() {
                    cx.notify();
                }
            })
        };
        div()
            .relative()
            .flex()
            .flex_col()
            .map(|open| match height {
                // It gives way in a lane too short for it, down to its least.
                Some(height) => open.h(height).min_h(px(EXPANDED_MIN_HEIGHT)),
                None => open.flex_1(),
            })
            .child(open)
            .child(measure)
            // Over the card's own outline, wider and in a color it never
            // has, so the eye goes to it; with no hitbox, it takes no clicks.
            .when(flashing, |open| {
                open.child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(px(BOX_RADIUS))
                        .border_3()
                        .border_color(p.status_warning),
                )
            })
            .child(
                div()
                    .id(eid("lane-open-resize", lane))
                    .absolute()
                    .left_0()
                    .right_0()
                    // Over the card's bottom edge and the gap under it.
                    .bottom(px(-5.))
                    .h(px(8.))
                    .cursor(CursorStyle::ResizeUpDown)
                    .hover(|style| style.bg(p.border))
                    .on_mouse_down(MouseButton::Left, start)
                    .on_click(reset)
                    .tooltip(|window, cx| {
                        Tooltip::new(
                            "Drag to make it shorter or taller; \
                             double-click to fill the lane again",
                        )
                        .build(window, cx)
                    }),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn compact_strip(
        &mut self,
        lane: &Rc<Lane>,
        rows: &[Row],
        which: &str,
        fill: bool,
        style: LaneStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut strip = div()
            .id(eid_parts("lane-strip-cards", &[&lane.key, which]))
            // In a short lane the strips give way to the open card, down to
            // a row each. Under a card made shorter, one takes the rest.
            .min_h(px(COMPACT_HEIGHT))
            .when(fill, |strip| strip.flex_1())
            .when(!fill, |strip| strip.max_h(px(160.)))
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
    pub(super) fn row_element(
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
        let alone = matches!(frame, Frame::Alone);
        // How long ago it last did something, as a conversation's turns say.
        let ago = {
            let store = self.core.store.read(cx);
            (card.updated_at.clone())
                .or_else(|| {
                    (store.fleet.agent(&card.host, &card.agent_id))
                        .and_then(|agent| agent.created_at.clone())
                })
                .map(|at| crate::app::store::time_ago(&at))
                .filter(|ago| !ago.is_empty())
        };
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
                    .min_w(px(0.))
                    .truncate()
                    .text_size(size::sm())
                    .text_color(p.foreground)
                    .child(card.title.clone()),
            )
            // Outside a box, its workspace follows its name, smaller; the
            // title gives way first.
            .when(alone && !card.workspace_name.is_empty(), |row| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .max_w(gpui_kit::gpui::relative(0.45))
                        .truncate()
                        .text_size(px(11.))
                        .text_color(p.muted)
                        .child(card.workspace_name.clone()),
                )
            })
            .child(div().flex_1())
            .when(self.popped_out(card, cx), |row| row.child(popped_mark()))
            .when_some(ago, |row, ago| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(p.extra_muted)
                        .child(ago),
                )
            });
        let id = eid_parts(
            "lane-compact",
            &[&lane.key, card.host.as_str(), &card.agent_id],
        );
        // Where it is, on hover: a one-line row has no room for it.
        let place = {
            let store = self.core.store.read(cx);
            let host = store.fleet.host_name(&card.host).to_string();
            let model = (store.fleet.agent(&card.host, &card.agent_id))
                .and_then(|agent| agent.model_label())
                .map(str::to_string);
            let workspace = (!card.workspace_name.is_empty()).then(|| card.workspace_name.clone());
            let line = [workspace, Some(host), model]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            match agent_folder(&self.core, &card.host, &card.agent_id, cx) {
                Some((folder, _)) => format!("{line}\n{}", tilde(&folder)),
                None => line,
            }
        };
        let shell = card_frame(id, COMPACT_HEIGHT, focused, frame, look, content)
            .tooltip(move |window, cx| Tooltip::new(place.clone()).build(window, cx));
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
        let scope = element_scope(&[&lane.key, card.host.as_str(), &card.agent_id]);
        let at = (card.updated_at.as_deref())
            .or_else(|| agent.and_then(|agent| agent.created_at.as_deref()));
        let activity = activity::label(at, now_millis());
        let age = (blurb.is_some() && at.is_some()).then(|| activity.clone());
        let footer = blurb.clone().unwrap_or(activity);
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
                    .when_some(age, |row, age| row.child(card_age(age)))
                    .when_some(actions, |row, actions| {
                        row.child(
                            div()
                                .id(eid_parts(
                                    "lane-sub-actions",
                                    &[&lane.key, card.host.as_str(), &card.agent_id],
                                ))
                                .min_w(px(0.))
                                .max_w(gpui_kit::gpui::relative(0.75))
                                .overflow_x_scroll()
                                .restrict_scroll_to_axis()
                                .child(actions),
                        )
                    }),
            );
        let id = eid_parts(
            "lane-subagent",
            &[&lane.key, card.host.as_str(), &card.agent_id],
        );
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
            &[&lane.key, parent.host.as_str(), &parent.agent_id],
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

    /// An agent's card, open on its conversation. Unless `framed`, its
    /// workspace's box outlines it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn expanded_card(
        &mut self,
        lane: &Lane,
        host: &HostId,
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
        // Inside its workspace's box, the box's header relabels it; in reef
        // mode labels are the agent's own, so its card has a tag too, and
        // any host can carry them.
        let reef_mode = self.core.store.read(cx).fleet.labels.is_some();
        let picker = workspace
            .clone()
            .filter(|_| reef_mode || (supported && framed))
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
            let (core, host, agent) = (self.core.clone(), host.clone(), agent.to_string());
            Button::new(eid_parts(
                "lane-card-close-tab",
                &[&lane.key, host.as_str(), &agent],
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
                // In reef mode labels are the agent's own.
                _ if store.fleet.labels.is_some() => "Relabel this agent".to_string(),
                0 | 1 => format!("Relabel the workspace \"{name}\""),
                n => {
                    format!("Relabel the workspace \"{name}\": all {n} of its agents move together")
                }
            }
        };
        let place = framed.then(|| {
            let store = self.core.store.read(cx);
            let name = (workspace.as_deref())
                .and_then(|w| store.fleet.workspace(host, w))
                .map(|w| w.title.clone().unwrap_or_else(|| w.name.clone()));
            (name, store.fleet.host_name(host).to_string())
        });
        let pinned = self.core.nav.read(cx).agent_color(host, agent);
        let measured_picker = picker.clone();
        let relabel_popover = picker.map(|picker| {
            let focus = picker.read(cx).focus_handle(cx);
            let open = picker.read(cx).is_open();
            let opened = picker.clone();
            Popover::new(eid_parts(
                "lane-relabel",
                &[&lane.key, host.as_str(), agent],
            ))
            .anchor(Anchor::TopRight)
            .open(open)
            .p(px(0.))
            .track_focus(&focus)
            .trigger(
                Button::new(eid_parts(
                    "lane-relabel-button",
                    &[&lane.key, host.as_str(), agent],
                ))
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
                    .id(eid_parts(
                        "lane-card-bar",
                        &[&lane.key, host.as_str(), agent],
                    ))
                    .px(px(10.))
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(p.border)
                    // A colour pinned for the agent shows here too, so its
                    // card matches its popped-out window.
                    .when_some(pinned, |bar, index| bar.bg(swatch_tint(p.surface1, index)))
                    .hover(move |bar| bar.bg(style.card_hover))
                    .on_mouse_down(MouseButton::Right, {
                        let (host, agent) = (host.clone(), agent.to_string());
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.open_card_menu(&host, &agent, event.position, window, cx);
                            cx.stop_propagation();
                        })
                    })
                    .child(match self.rename_editor(&lane.key, host, agent, cx) {
                        Some(editor) => editor.into_any_element(),
                        None => {
                            let (host, agent) = (host.clone(), agent.to_string());
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
                                            &[&lane.key, host.as_str(), &agent],
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
                    // Outside its box it says where it is, as the box's
                    // header would.
                    .when_some(place, |bar, place| {
                        bar.child(place_label(&self.core, host, agent, place, px(180.), cx))
                    })
                    .children(relabel_popover)
                    .child({
                        let (lane_key, host, agent) =
                            (lane.key.clone(), host.clone(), agent.to_string());
                        Button::new(eid_parts(
                            "lane-card-pop-out",
                            &[&lane_key, host.as_str(), &agent],
                        ))
                        .icon(Icon::empty().path("icons/square-arrow-out-up-right.svg"))
                        .ghost()
                        .xsmall()
                        .tooltip("Pop out into its own window")
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.pop_out(&lane_key, &host, &agent, window, cx);
                            },
                        ))
                    })
                    .child({
                        let in_focus = self.in_focus_mode(&lane.key, host, agent);
                        let (lane_key, host, agent) =
                            (lane.key.clone(), host.clone(), agent.to_string());
                        Button::new(eid_parts(
                            "lane-card-focus",
                            &[&lane_key, host.as_str(), &agent],
                        ))
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
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.toggle_focus_mode(&lane_key, &host, &agent, window, cx);
                            },
                        ))
                    })
                    .child(close_button)
                    .child(
                        Button::new(eid_parts(
                            "lane-card-collapse",
                            &[&lane.key, host.as_str(), agent],
                        ))
                        .icon(Icon::empty().path("icons/x.svg"))
                        .ghost()
                        .xsmall()
                        .accessibility_label("Collapse card")
                        .tooltip("Collapse card")
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                this.collapse_lane(&lane_key, window, cx);
                            },
                        )),
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
