//! Lanes' controls in the toolbars, and the filter and state bars they open.

use fleet::HostId;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use fleet::lanes::{BACKGROUND, LabelLaneMode, Lane, LaneGrouping, UNLABELLED};
use gpui_kit::base::Selectable as _;
use gpui_kit::component::Icon;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::gpui::{
    AnyElement, App, ClickEvent, Context, Entity, InteractiveElement, IntoElement, MouseButton,
    ParentElement, SharedString, Styled, WeakEntity, Window, div, px,
};

use gpui_kit::prelude::FluentBuilder;

use crate::app::nav::LaneFilter;
use crate::theme::{palette, size, with_alpha};
use crate::ui;

use super::*;

/// Lanes' controls in the toolbars.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LanesControl {
    Grouping,
    LabelMode,
    Filters,
    CollapsedLanes,
}

/// A Lanes control as a toolbar item. It shows only while Lanes does.
pub(super) struct LanesItem {
    pub(super) kind: LanesControl,
    pub(super) board: WeakEntity<Lanes>,
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

/// Picks one kind of a [`LaneFilter`]: hosts, tools or labels.
type FilterField<T> = fn(&mut LaneFilter) -> &mut BTreeSet<T>;

/// A row as the board shows it while a state is singled out from the title
/// bar's counts: that state's cards outlined, everything else dimmed.
pub(super) fn state_row(
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

/// One kind of filter as a dropdown: its name, what is chosen ("All", one
/// name, or how many), and a caret. Each choice toggles; "All" clears.
/// With `background`, the menu ends with the "Show background" setting,
/// which the Background choice follows, and says when it is off.
#[allow(clippy::too_many_arguments)]
fn filter_picker<T: Ord + Clone + std::fmt::Display + 'static>(
    id: &'static str,
    name: &str,
    many: &str,
    choices: Vec<(T, String)>,
    filter: &LaneFilter,
    field: FilterField<T>,
    background: bool,
    nav: &Entity<crate::app::Nav>,
    cx: &App,
) -> AnyElement {
    let p = palette();
    let chosen = field(&mut filter.clone()).clone();
    let hidden = background && !nav.read(cx).show_background();
    let value = match chosen.len() {
        0 if hidden => "All but background".to_string(),
        0 => "All".to_string(),
        1 => {
            let key = chosen.iter().next().expect("one");
            choices
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| key.to_string())
        }
        n => format!("{n} {many}"),
    };
    let active = !chosen.is_empty() || hidden;
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
                             change: &dyn Fn(&mut BTreeSet<T>)| {
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
            if background {
                // One setting for every view, as in the sidebar's display menu.
                let shown = nav.read(cx).show_background();
                let nav = nav.clone();
                menu = menu.separator().item(
                    PopupMenuItem::new("Show background")
                        .checked(shown)
                        .on_click(move |_, _, cx| {
                            nav.update(cx, |nav, cx| nav.set_show_background(!shown, cx))
                        }),
                );
            }
            // Scrolls, or a long label list would hide its foot.
            menu.scrollable(true).max_h(px(360.))
        })
        .into_any_element()
}

impl Lanes {
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

    /// The width pill, led by Collapse all while any card is open.
    pub(super) fn width_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let collapse = (!self.expanded.is_empty()).then(|| {
            Button::new("lanes-collapse-all")
                .small()
                .ghost()
                .label("Collapse all cards")
                .icon(Icon::empty().path("icons/chevrons-down-up.svg"))
                .accessibility_label("Collapse all expanded cards")
                .tooltip("Collapse every expanded agent card")
                .on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.collapse_all(window, cx)),
                )
                .into_any_element()
        });
        self.width.control(self.column_width, collapse, cx)
    }

    /// Filters for the board: which hosts, tools and labels it shows.
    fn filters_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let nav = self.core.nav.clone();
        let filter = nav.read(cx).lane_filter.clone();
        let store = self.core.store.read(cx);
        // Chosen by id, shown by name.
        let hosts: Vec<(HostId, String)> = store
            .fleet
            .hosts
            .iter()
            .map(|host| (host.id().clone(), host.name().to_string()))
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
        // By lane key: in reef mode Reef's labels by id, else each name once.
        let mut labels: BTreeMap<String, String> = BTreeMap::new();
        if let Some(catalog) = &store.fleet.labels {
            for label in &catalog.labels {
                labels.insert(label.id.to_string(), label.name.clone());
            }
        }
        for host in store
            .fleet
            .hosts
            .iter()
            .filter(|_| store.fleet.labels.is_none())
        {
            for def in &host.directory.labels {
                labels
                    .entry(fleet::directory::label_key(&def.name))
                    .or_insert_with(|| def.name.clone());
            }
        }
        let mut labels: Vec<(String, String)> = labels.into_iter().collect();
        labels.sort_by_key(|(_, name)| name.to_lowercase());
        // After the labels, as their lanes are.
        labels.push((UNLABELLED.into(), "Unlabelled".into()));
        if nav.read(cx).show_background() {
            labels.push((BACKGROUND.into(), "Background".into()));
        }
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
                false,
                &nav,
                cx,
            ))
            .child(filter_picker(
                "lane-filter-tools",
                "Tool",
                "tools",
                tools.into_iter().collect(),
                &filter,
                |f| &mut f.tools,
                false,
                &nav,
                cx,
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
                false,
                &nav,
                cx,
            ))
            .child(filter_picker(
                "lane-filter-labels",
                "Label",
                "labels",
                labels,
                &filter,
                |f| &mut f.labels,
                true,
                &nav,
                cx,
            ))
            .when(!filter.is_empty(), |bar| bar.child(clear))
            .into_any_element()
    }

    /// While a state is singled out: how many, and the offer to filter to
    /// them. Clicking anything else, or any key, goes back to normal.
    pub(super) fn state_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.core.nav.read(cx).state_highlight?;
        let p = palette();
        // Agents in that state on the board, and those only in collapsed
        // lanes, which the title bar's count includes.
        let in_state = |lanes: &LaneSet| -> HashSet<(HostId, String)> {
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
