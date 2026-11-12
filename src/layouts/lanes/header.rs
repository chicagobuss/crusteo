//! A column's header: title, hosts that fit, and state counts.

use fleet::HostId;
use std::rc::Rc;

use fleet::lanes::{BACKGROUND, Lane, LaneKind, UNLABELLED};
use gpui_kit::component::Icon;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::gpui::{
    AnyElement, AppContext, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Rgba, ScrollDelta, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, div, px, rgb,
};

use gpui_kit::prelude::FluentBuilder;

use crate::theme::{self, palette, size};
use crate::ui;

use super::*;

/// Which of a lane header's controls fit beside its title. As the lane
/// narrows they go in this order: move to the front and back, move one
/// left and right, collapse, new agent, the card count. The grip stays: the
/// narrowest lane leaves room for it.
#[derive(Debug, PartialEq, Eq)]
struct HeaderFit {
    pub(super) count: bool,
    pub(super) add: bool,
    pub(super) collapse: bool,
    pub(super) steps: bool,
    pub(super) ends: bool,
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

/// The names of hosts, comma-separated.
pub(super) fn host_names(fleet: &fleet::Fleet, hosts: &[HostId]) -> String {
    let names: Vec<&str> = hosts.iter().map(|host| fleet.host_name(host)).collect();
    names.join(", ")
}

/// Roughly how wide a header's state counts draw: a dot, a gap and the
/// digits for each, with gaps between.
fn state_counts_width(counts: &[(fleet::Bucket, usize)]) -> f32 {
    let each: f32 = (counts.iter())
        .map(|(_, count)| 10. + 7. * count.to_string().len() as f32)
        .sum();
    each + 8. * counts.len().saturating_sub(1) as f32
}

/// How many host chips fit in `room` beside a "+N" for the rest, at most
/// two; none means the hosts go as a count.
fn hosts_that_fit(names: &[&str], room: f32) -> usize {
    let chip = |name: &str| 12. + 6.5 * name.chars().count() as f32;
    let more = |rest: usize| match rest {
        0 => 0.,
        rest => 4. + 12. + 7. * rest.to_string().len() as f32,
    };
    (1..=names.len().min(2))
        .rev()
        .find(|&shown| {
            let chips: f32 = names[..shown].iter().map(|name| chip(name)).sum();
            chips + 4. * (shown - 1) as f32 + more(names.len() - shown) <= room
        })
        .unwrap_or(0)
}

/// Host metadata occupies one bounded row even when a lane has no cards
/// yet: up to two chips and "+N" for the rest, as many as fit in `room`,
/// else "N hosts". Each says its hosts in full on hover.
fn header_hosts(lane: &Lane, room: f32, fleet: &fleet::Fleet) -> AnyElement {
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
    let names: Vec<&str> = lane
        .hosts
        .iter()
        .map(|host| fleet.host_name(host))
        .collect();
    let fit = hosts_that_fit(&names, room);
    if fit == 0 && lane.hosts.len() > 1 {
        let names = host_names(fleet, &lane.hosts);
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
    // A single host keeps its chip, cut short if it must be.
    let fit = fit.max(1);
    let row = row.children(lane.hosts.iter().take(fit).map(|host| {
        let name = fleet.host_name(host).to_string();
        div()
            .id(eid_parts("lane-host", &[&lane.key, host.as_str()]))
            .h(px(HEADER_HOST_HEIGHT))
            .min_w(px(0.))
            .px(px(6.))
            .rounded(px(4.))
            .bg(p.surface2)
            .flex()
            .items_center()
            .child(div().min_w(px(0.)).truncate().child(name.clone()))
            .tooltip(move |window, cx| Tooltip::new(name.clone()).max_w(px(360.)).build(window, cx))
    }));
    let row = if lane.hosts.len() > fit {
        let names = host_names(fleet, &lane.hosts[fit..]);
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
                .child(format!("+{}", lane.hosts.len() - fit))
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

pub(super) fn color_dot(lane: &Lane) -> AnyElement {
    let p = palette();
    let color = if lane.key == UNLABELLED || lane.key == BACKGROUND {
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

/// Whether a background workspace's header in `lane` says "background":
/// not in the Background lane, which says it already.
pub(super) fn tag_background(lane: &Lane, background: bool) -> bool {
    background && lane.kind != LaneKind::Background
}

pub(super) fn dot(color: Rgba) -> AnyElement {
    div()
        .size(px(8.))
        .rounded_full()
        .flex_shrink_0()
        .bg(color)
        .into_any_element()
}

impl Lanes {
    pub(super) fn column_header(
        &self,
        lane: &Rc<Lane>,
        width: Pixels,
        style: LaneStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
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
            .px(px(HEADER_PADDING))
            .py(px(HEADER_PADDING_Y))
            .border_b_1()
            .border_color(p.border)
            .flex()
            .flex_col()
            .gap(px(HEADER_ROW_GAP))
            // A plain mouse wheel over a header scrolls across columns; over
            // the cards it scrolls the lane.
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                if this.wheel_resize(event, cx) {
                    cx.stop_propagation();
                    return;
                }
                // A trackpad's swipe moves the board by its sideways part
                // alone, which the board does itself: a swipe's drift up or
                // down must not move it sideways as well.
                if !matches!(event.delta, ScrollDelta::Lines(_)) {
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
            .child({
                // The state counts keep the row's right end; the hosts give
                // way to them.
                let shown: Vec<(fleet::Bucket, usize)> =
                    counts.into_iter().filter(|(_, count)| *count > 0).collect();
                let counts_width = state_counts_width(&shown);
                let room = width.as_f32() - 2. - 2. * HEADER_PADDING - counts_width - 8.;
                div()
                    .h(px(HEADER_HOST_HEIGHT))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .child(header_hosts(lane, room, &self.core.store.read(cx).fleet)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .line_height(px(16.))
                            .flex()
                            .gap(px(8.))
                            .children(shown.iter().map(|(bucket, count)| {
                                let color = ui::bucket_color(*bucket).unwrap_or(p.extra_muted);
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .text_size(size::sm())
                                    .text_color(color)
                                    .child(div().size(px(6.)).rounded_full().bg(color))
                                    .child(count.to_string())
                            })),
                    )
            })
            .on_mouse_down(MouseButton::Right, {
                let lane = Rc::clone(lane);
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_lane_menu(&lane, event.position, window, cx);
                })
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod header_fit_tests {
    use super::*;

    #[test]
    fn host_chips_give_way_to_a_count_as_room_runs_out() {
        let names = ["office-mac.local", "lab-box-zt", "storage"];
        assert_eq!(hosts_that_fit(&names, 400.), 2);
        assert_eq!(hosts_that_fit(&names, 160.), 1);
        assert_eq!(hosts_that_fit(&names, 60.), 0);
        // With nothing left over there is no "+N" to make room for.
        assert_eq!(hosts_that_fit(&names[..1], 130.), 1);
        assert_eq!(hosts_that_fit(&[], 400.), 0);
        let counts = [(fleet::Bucket::Running, 8), (fleet::Bucket::Done, 13)];
        assert_eq!(state_counts_width(&counts), 17. + 24. + 8.);
    }

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
        assert_eq!(at(crate::app::nav::COLUMN_WIDTH_MAX), all);
        let mut last = all;
        // Narrowing never brings a control back, and the ends go first.
        for width in (crate::app::nav::COLUMN_WIDTH_MIN as i32
            ..crate::app::nav::COLUMN_WIDTH_MAX as i32)
            .rev()
        {
            let fit = at(width as f32);
            let shown = [fit.count, fit.add, fit.collapse, fit.steps, fit.ends];
            let before = [last.count, last.add, last.collapse, last.steps, last.ends];
            assert!(shown.iter().zip(before).all(|(now, was)| !now | was));
            // Shown ones are always a prefix of the order.
            assert!(shown.windows(2).all(|pair| pair[0] | !pair[1]));
            last = fit;
        }
        // The narrowest lane still shows its title, count and new agent.
        let narrowest = at(crate::app::nav::COLUMN_WIDTH_MIN);
        assert!(narrowest.count && narrowest.add && !narrowest.ends);
    }
}
