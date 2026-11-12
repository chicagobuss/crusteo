//! Collapsed lanes as tabs, and the bands that group lanes.

use fleet::HostId;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use fleet::lanes::{Lane, LaneBand};
use gpui_kit::component::Icon;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::gpui::{
    AnyElement, App, ClickEvent, Context, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Pixels, Rgba, ScrollWheelEvent, StatefulInteractiveElement, Styled, Window, div,
    point, px, rgb,
};

use gpui_kit::prelude::FluentBuilder;

use crate::layouts::columns::COLUMN_GAP;
use crate::theme::{self, palette, size, with_alpha};
use crate::ui;

use super::*;

impl Lanes {
    pub(super) fn collapsed_tabs(&self, lanes: &LaneSet, cx: &mut Context<Self>) -> AnyElement {
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
        // A collapsed band is one tab, where its first lane would be.
        let mut bands_seen: HashSet<&str> = HashSet::new();
        let band_tabs: HashMap<&str, AnyElement> = (lanes.iter())
            .filter_map(|lane| lane.band.as_ref().filter(|band| band.collapsed))
            .filter(|band| bands_seen.insert(band.key.as_str()))
            .map(|band| (band.key.as_str(), self.band_tab(lanes, band, cx)))
            .collect();
        let mut band_tabs = band_tabs;
        let tabs = lanes
            .iter()
            .filter(|lane| lane.collapsed)
            .filter_map(|lane| {
                if let Some(band) = lane.band.as_ref().filter(|band| band.collapsed) {
                    return band_tabs.remove(band.key.as_str());
                }
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
                    .into_any_element()
                    .into()
            });
        let tabs: Vec<AnyElement> = tabs.collect();
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

    /// A collapsed band's tab: its name and agents, in its colour.
    fn band_tab(&self, lanes: &LaneSet, band: &LaneBand, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let nav = self.core.nav.read(cx);
        let swatch = colors::index(&band.key, band.color.as_deref(), &nav.lane_color_choices);
        let base = rgb(theme::LANE_COLORS[swatch].1);
        let agents: HashSet<(&HostId, &str)> = (lanes.iter())
            .filter(|lane| lane.band.as_ref().is_some_and(|b| b.key == band.key))
            .flat_map(|lane| &lane.cards)
            .map(|card| (&card.host, card.agent_id.as_str()))
            .collect();
        let description = format!("Restore the {} band ({} agents)", band.title, agents.len());
        let collapsed = fleet::lanes::band_collapse_key(&band.key);
        Button::new(eid("band-restore", &band.key))
            .small()
            .ghost()
            .flex_shrink_0()
            .bg(colors::tint(p.surface1, base, 0.32))
            .border_1()
            .border_color(base)
            .rounded(px(6.))
            .accessibility_label(description.clone())
            .tooltip(description)
            .child(ui::icon("layers", 12., p.muted))
            .child(
                div()
                    .min_w(px(0.))
                    .max_w(px(152.))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(band.title.clone()),
            )
            .child(
                div()
                    .text_size(size::sm())
                    .text_color(p.extra_muted)
                    .child(agents.len().to_string()),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.core
                    .nav
                    .update(cx, |nav, cx| nav.set_lane_collapsed(&collapsed, false, cx));
                cx.notify();
            }))
            .into_any_element()
    }

    /// The strip over a column while any lane is under a band: its band's,
    /// unbroken across the band's lanes and the gaps between them; blank
    /// over a lane under none, so every header stays level. The band's name
    /// and buttons are not in it but over it ([`Self::band_bars`]).
    pub(super) fn band_strip(
        &self,
        lanes: &LaneSet,
        ix: usize,
        width: Pixels,
        cx: &App,
    ) -> AnyElement {
        let p = palette();
        let strip = div().h(px(BAND_HEIGHT)).flex_shrink_0().pb(px(4.));
        let Some(band) = lanes[ix].band.as_ref() else {
            return strip.into_any_element();
        };
        let ours = |other: Option<&Rc<Lane>>| {
            (other.and_then(|lane| lane.band.as_ref())).is_some_and(|other| other.key == band.key)
        };
        let first = !ours(ix.checked_sub(1).and_then(|ix| lanes.get(ix)));
        let last = !ours(lanes.get(ix + 1));
        let base = self.band_color(band, cx);
        let mut segment = div()
            .h_full()
            // Over the gap to the next lane of the band too.
            .w(if last { width } else { width + px(COLUMN_GAP) })
            .bg(colors::tint(p.surface1, base, 0.32))
            .border_t_2()
            .border_color(base);
        if first {
            segment = segment.rounded_tl(px(6.)).rounded_bl(px(6.));
        }
        if last {
            segment = segment.rounded_tr(px(6.)).rounded_br(px(6.));
        }
        strip.child(segment).into_any_element()
    }

    fn band_color(&self, band: &LaneBand, cx: &App) -> Rgba {
        let nav = self.core.nav.read(cx);
        let swatch = colors::index(&band.key, band.color.as_deref(), &nav.lane_color_choices);
        rgb(theme::LANE_COLORS[swatch].1)
    }

    /// Each band's name and buttons, laid over its strips across the part
    /// of it in view: the name at the left of that part and the buttons at
    /// its right, so both stay in sight however far the board is scrolled
    /// along a band wider than the view, and at the band's own ends once
    /// those are in view. `spans` is each column's start and width on the
    /// board, `in_view` the board's part in view.
    pub(super) fn band_bars(
        &self,
        lanes: &LaneSet,
        spans: &[(Pixels, Pixels)],
        in_view: (Pixels, Pixels),
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let p = palette();
        let mut bars = Vec::new();
        let mut ix = 0;
        while ix < lanes.len() {
            let Some(band) = lanes[ix].band.clone() else {
                ix += 1;
                continue;
            };
            let from = ix;
            while ix + 1 < lanes.len()
                && (lanes[ix + 1].band.as_ref()).is_some_and(|other| other.key == band.key)
            {
                ix += 1;
            }
            let to = ix;
            ix += 1;
            // The part in view, a little in from the view's edges.
            let edge = px(10.);
            let (band_left, band_right) = (spans[from].0, spans[to].0 + spans[to].1);
            let seen_left = band_left.max(in_view.0 + edge);
            let seen_right = band_right.min(in_view.1 - edge);
            if seen_right - seen_left < px(40.) {
                continue;
            }
            let base = self.band_color(&band, cx);
            // Each agent once, however many of the band's lanes it is in.
            let agents: HashSet<(&HostId, &str)> = (lanes[from..=to].iter())
                .flat_map(|lane| &lane.cards)
                .map(|card| (&card.host, card.agent_id.as_str()))
                .collect();
            let button = |id: &str, icon: &str, tip: &'static str| {
                Button::new(eid_parts(id, &[&band.key]))
                    .icon(Icon::empty().path(icon))
                    .ghost()
                    .xsmall()
                    .tooltip(tip)
            };
            let key = band.key.clone();
            let left = button("band-left", "icons/chevron-left.svg", "Move band left").on_click(
                cx.listener({
                    let key = key.clone();
                    move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.move_band(&key, -1, cx);
                    }
                }),
            );
            let right = button("band-right", "icons/chevron-right.svg", "Move band right")
                .on_click(cx.listener({
                    let key = key.clone();
                    move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.move_band(&key, 1, cx);
                    }
                }));
            let fold = button(
                "band-collapse",
                "icons/fold-horizontal.svg",
                "Collapse band into the tabs above",
            )
            .on_click(cx.listener({
                let key = key.clone();
                move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.hide_band(&key, window, cx);
                }
            }));
            let labels = band.labels.clone();
            let remove = button("band-remove", "icons/x.svg", "Stop showing as a band").on_click(
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.core.nav.update(cx, |nav, cx| {
                        for label in &labels {
                            nav.set_lane_band(label, false, cx);
                        }
                    });
                }),
            );
            bars.push(
                div()
                    .absolute()
                    // Under the strip's top border, over the strip.
                    .top(px(2.))
                    .h(px(BAND_HEIGHT - 6.))
                    .left(seen_left)
                    .w(seen_right - seen_left)
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    // The strip's own padding at the band's ends; at the
                    // view's edges the edge's inset is enough.
                    .pl(if seen_left == band_left {
                        px(8.)
                    } else {
                        px(2.)
                    })
                    .pr(if seen_right == band_right {
                        px(4.)
                    } else {
                        px(0.)
                    })
                    .child(div().size(px(8.)).flex_shrink_0().rounded_full().bg(base))
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.foreground)
                            .child(band.title.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(size::sm())
                            .text_color(p.muted)
                            .child(agents.len().to_string()),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .child(left)
                            .child(right)
                            .child(fold)
                            .child(remove),
                    )
                    .into_any_element(),
            );
        }
        bars
    }

    /// Moves a band's lanes together one place left or right.
    fn move_band(&mut self, band: &str, step: isize, cx: &mut Context<Self>) {
        let keys: Vec<(String, Option<String>)> = (self.lanes(cx).iter())
            .map(|lane| (lane.key.clone(), lane.band.as_ref().map(|b| b.key.clone())))
            .collect();
        self.core
            .nav
            .update(cx, |nav, cx| nav.move_lane_run(&keys, band, step, cx));
    }

    /// Collapses a band's lanes into one tab.
    fn hide_band(&mut self, band: &str, window: &mut Window, cx: &mut Context<Self>) {
        let collapsed = fleet::lanes::band_collapse_key(band);
        let focused_in_band = self.focused_lane.as_ref().is_some_and(|focused| {
            (self.current_lanes(cx).iter()).any(|lane| {
                &lane.key == focused && lane.band.as_ref().is_some_and(|b| b.key == band)
            })
        });
        self.core
            .nav
            .update(cx, |nav, cx| nav.set_lane_collapsed(&collapsed, true, cx));
        if focused_in_band {
            self.focused_lane = None;
            self.focused_agent = None;
            self.move_column(0, cx);
        }
        self.focus.focus(window, cx);
        cx.notify();
    }
}
