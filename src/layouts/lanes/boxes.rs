//! Workspace boxes in a column: their header and actions, empty and
//! folded rows, and the terminal.

use fleet::HostId;
use std::rc::Rc;

use fleet::lanes::{Lane, LaneCard, card_workspace};
use gpui_kit::base::Disableable as _;
use gpui_kit::component::Icon;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::gpui::{
    Anchor, AnyElement, App, ClickEvent, Context, Focusable, InteractiveElement, IntoElement,
    MouseButton, ParentElement, StatefulInteractiveElement, Styled, Window, div, px,
};

use gpui_kit::prelude::FluentBuilder;

use crate::app::nav::workspace_key;
use crate::theme::{self, palette, size, with_alpha};
use crate::ui;

use super::*;

impl Lanes {
    /// A workspace's terminal: its name, and whether it has exited. A click
    /// opens it in the lane.
    pub(super) fn terminal_row(
        &self,
        lane: &Lane,
        owner: Owner,
        index: usize,
        style: LaneStyle,
        frame: Frame,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let Some((host, workspace)) = owner.workspace(lane) else {
            return div().h(px(COMPACT_HEIGHT)).into_any_element();
        };
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
        let open = self.expanded_terminals.get(&lane.key) == Some(&(host.clone(), id.clone()));
        let focused = open
            || (self.focused_lane.as_deref() == Some(lane.key.as_str())
                && self.focused_row == Some(RowFocus::Terminal(host.clone(), id.clone())));
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
        let (lane_key, host, terminal) = (lane.key.clone(), host.clone(), id.clone());
        let element_id = eid_parts("lane-terminal", &[&lane_key, host.as_str(), &terminal]);
        card_frame(element_id, COMPACT_HEIGHT, focused, frame, look, content)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.expand_terminal(&lane_key, &host, &terminal, window, cx);
            }))
            .into_any_element()
    }

    /// How a workspace's box looks in `lane`: outlined where it is
    /// selected, tinted where it is selected in another lane.
    pub(super) fn box_look(
        &self,
        lane: &str,
        workspace: Option<(&HostId, &str)>,
        style: LaneStyle,
        cx: &App,
    ) -> BoxLook {
        let p = palette();
        let selected = workspace.is_some()
            && self
                .selected_workspace(cx)
                .as_ref()
                .map(|(host, workspace)| (host, workspace.as_str()))
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
    pub(super) fn box_header(
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
        let host_name = self.core.store.read(cx).fleet.host_name(&host).to_string();
        let workspace = first.workspace_id.clone().unwrap_or_default();
        let look = self.box_look(&lane.key, card_workspace(first), style, cx);
        let entry =
            (lane.workspaces.iter()).find(|ws| ws.host == host && ws.workspace_id == workspace);
        let background = tag_background(lane, entry.is_some_and(|ws| ws.background));
        let name = entry
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
            .id(eid_parts(
                "lane-box",
                &[&lane.key, host.as_str(), &workspace],
            ))
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
                                &[&lane.key, host.as_str(), &workspace],
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
            .when(background, |header| header.child(ui::background_tag()))
            .child(
                div()
                    .id(eid_parts(
                        "lane-box-host",
                        &[&lane.key, host.as_str(), &workspace],
                    ))
                    .flex_shrink_0()
                    .max_w(px(96.))
                    .truncate()
                    .text_color(p.extra_muted)
                    .tooltip(workspace_host_tooltip(&self.core, &host, &workspace))
                    .child(host_name.clone()),
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
        host: &HostId,
        workspace: &str,
        agent: Option<&str>,
        shown: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let parts = [lane.key.as_str(), host.as_str(), workspace];
        let add = {
            let lane = Rc::clone(lane);
            let target = (host.clone(), workspace.to_string());
            Button::new(eid_parts("lane-box-add", &parts))
                .icon(Icon::empty().path("icons/plus.svg"))
                .ghost()
                .xsmall()
                .tooltip("New agent in this workspace")
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_draft_in(&lane, target.clone(), window, cx);
                }))
        };
        let supported = self
            .core
            .store
            .read(cx)
            .fleet
            .host(host)
            .is_some_and(|host| host.has("workspaceLabels"));
        let reef_mode = self.core.store.read(cx).fleet.labels.is_some();
        let tip = match reef_mode {
            true => "Relabel this workspace's agents in this lane",
            false => "Relabel this workspace: all its agents move together",
        };
        let tag = Button::new(eid_parts("lane-box-relabel-button", &parts))
            .icon(Icon::empty().path("icons/tag.svg"))
            .ghost()
            .xsmall()
            .tooltip(tip)
            .disabled(
                !(supported || reef_mode && agent.is_some()) || self.relabel_pending.is_some(),
            );
        let open_picker = self
            .box_picker
            .as_ref()
            .filter(|open| {
                open.lane == lane.key && &open.host == host && open.workspace == workspace
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
                    (lane.key.clone(), host.clone(), workspace.to_string());
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
            open.lane == lane.key && &open.host == host && open.workspace == workspace
        });
        let last = if agent.is_some() {
            let lane = Rc::clone(lane);
            let (host, workspace) = (host.clone(), workspace.to_string());
            Button::new(eid_parts("lane-box-menu", &parts))
                .icon(Icon::empty().path("icons/ellipsis-vertical.svg"))
                .ghost()
                .xsmall()
                .tooltip("Workspace actions")
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    let at = this.side.pointer();
                    this.open_workspace_menu(&lane, &host, &workspace, at, window, cx);
                }))
                .into_any_element()
        } else {
            let enabled =
                crate::workspace_archive::available(&self.core, host, workspace, None, cx);
            let (core, host, workspace) = (self.core.clone(), host.clone(), workspace.to_string());
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

    /// A workspace without agents: a dashed one-line box at the end of the
    /// lane.
    /// A workspace without agents. Alone it is a dashed line; with
    /// terminals (`boxed`) it heads their box, and unless `framed` the box
    /// draws its own outline.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn empty_row(
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
        let host_name = self.core.store.read(cx).fleet.host_name(&host).to_string();
        let look = self.box_look(&lane.key, Some((&host, &workspace)), style, cx);
        let actions =
            self.workspace_actions(lane, &host, &workspace, None, look.outlined, window, cx);
        let focus = RowFocus::Workspace(host.clone(), workspace.clone());
        let lane_key = lane.key.clone();
        let row = div()
            .id(eid_parts(
                "lane-empty",
                &[&lane.key, host.as_str(), &workspace],
            ))
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
            .when(tag_background(lane, ws.background), |row| {
                row.child(ui::background_tag())
            })
            .child(
                div()
                    .id(eid_parts(
                        "lane-empty-host",
                        &[&lane.key, host.as_str(), &workspace],
                    ))
                    .flex_shrink_0()
                    .max_w(px(96.))
                    .truncate()
                    .tooltip(workspace_host_tooltip(&self.core, &host, &workspace))
                    .child(host_name.clone()),
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
    pub(super) fn fold_row(
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
    pub(super) fn terminal_box(
        &mut self,
        lane: &Rc<Lane>,
        rows: &[Row],
        at: usize,
        host: &HostId,
        terminal: &str,
        style: LaneStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let owner_workspace = match rows[at] {
            Row::Terminal { owner, .. } => owner.workspace(lane),
            _ => Some((host, "")),
        };
        let look = self.box_look(&lane.key, owner_workspace, style, cx);
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
                host.clone(),
                terminal.to_string(),
            );
            let board = cx.entity().downgrade();
            Button::new(eid_parts(
                "lane-terminal-kill",
                &[&lane.key, host.as_str(), &terminal],
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
                &[&lane.key, host.as_str(), terminal],
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
            .child(self.measured_card(&terminal_height_key(&lane.key), panel.into_any_element()))
            .children(after)
            .into_any_element()
    }
}
