//! Context menus and tooltips for cards, workspaces, lanes and bands.

use fleet::HostId;
use std::collections::BTreeSet;
use std::rc::Rc;

use fleet::lanes::{Lane, LaneCard, LaneGrouping, LaneKind};
use fleet::project_remove;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::gpui::{
    AnyElement, AnyView, App, Context, DismissEvent, Focusable, InteractiveElement, IntoElement,
    MouseDownEvent, ParentElement, Pixels, Point, StatefulInteractiveElement, Styled, Window,
    anchored, deferred, div, px,
};

use gpui_kit::prelude::FluentBuilder;

use crate::app::Core;
use crate::app::nav::workspace_key;
use crate::theme::{palette, size};

use super::*;

/// The tooltip on a workspace's host name: the host in full, the
/// workspace's directory, and when its first agent started (Paseo does not
/// say when a workspace was made). Read when shown, so it is never stale.
pub(super) fn workspace_host_tooltip(
    core: &Core,
    host: &HostId,
    workspace: &str,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let (core, host, workspace) = (core.clone(), host.clone(), workspace.to_string());
    move |window, cx| {
        let store = core.store.read(cx);
        let fleet = &store.fleet;
        let host_name = fleet.host_name(&host).to_string();
        let mut rows: Vec<(&str, String)> = vec![("Host", host_name.clone())];
        let record = fleet.host(&host);
        if let Some(name) = record
            .and_then(|record| record.server.as_ref())
            .map(|server| server.hostname.clone())
            .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case(&host_name))
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
pub(super) fn lane_title_tooltip(
    core: &Core,
    lane: &Rc<Lane>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let (core, lane) = (core.clone(), Rc::clone(lane));
    move |window, cx| {
        let fleet = &core.store.read(cx).fleet;
        let mut rows: Vec<(&str, String)> = Vec::new();
        match lane.kind {
            LaneKind::Label | LaneKind::Combination if fleet.labels.is_some() => {
                // Reef keeps these labels; agents carry them.
                let single = lane.label_keys.len() == 1;
                rows.push((if single { "Label" } else { "Labels" }, lane.title.clone()));
                rows.push(("Kept by", "Reef".to_string()));
            }
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
        let mut per_host: Vec<(&HostId, usize)> = Vec::new();
        for workspace in &lane.workspaces {
            match per_host
                .iter_mut()
                .find(|(host, _)| **host == workspace.host)
            {
                Some((_, count)) => *count += 1,
                None => per_host.push((&workspace.host, 1)),
            }
        }
        per_host.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        if !per_host.is_empty() {
            let spread: Vec<String> = per_host
                .iter()
                .map(|(host, count)| format!("{count} on {}", fleet.host_name(host)))
                .collect();
            rows.push(("Workspaces", spread.join(" · ")));
        }
        rows_tooltip(rows, window, cx)
    }
}

/// A menu item that copies `text`, greyed out when there is none, such as
/// the resume command of an agent whose provider has not started a session.
/// The longest a path shows in a menu; longer ones lose their middle.
pub(super) const MENU_PATH_CHARS: usize = 48;

/// A menu's first line: where the workspace lives on its host, dimmed, as
/// something to read rather than choose; the whole path on hover when it is
/// cut short.
fn path_item(path: Option<String>) -> PopupMenuItem {
    let full = path
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| "unknown path".to_string());
    let shown = short_path(&full);
    let cut = shown != full;
    PopupMenuItem::element(move |_, _| {
        let full = full.clone();
        div()
            .id("menu-workspace-path")
            .text_size(size::sm())
            .text_color(palette().muted)
            .child(shown.clone())
            .when(cut, |line| {
                line.tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
            })
    })
    .disabled(true)
}

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
    pub(super) fn open_card_menu(
        &mut self,
        host: &HostId,
        agent: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let (host, agent) = (host.clone(), agent.to_string());
        let menu = PopupMenu::build(window, cx, move |menu, _, cx| {
            let fleet = &core.store.read(cx).fleet;
            let record = fleet.agent(&host, &agent);
            let resume = record.and_then(|record| record.resume_command());
            let workspace = record.and_then(|record| record.workspace_id.clone());
            // Its workspace's folder, or failing that its own.
            let path = (workspace.as_deref())
                .and_then(|workspace| fleet.workspace(&host, workspace))
                .map(|ws| ws.directory().to_string())
                .filter(|path| !path.is_empty())
                .or_else(|| record.map(|record| record.cwd.clone()));
            let menu = menu.item(path_item(path)).separator();
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
    pub(super) fn open_workspace_menu(
        &mut self,
        lane: &Rc<Lane>,
        host: &HostId,
        workspace: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let board = cx.entity().downgrade();
        let lane = Rc::clone(lane);
        let (host, workspace) = (host.clone(), workspace.to_string());
        let menu = PopupMenu::build(window, cx, move |menu, _, cx| {
            let target = (host.clone(), workspace.clone());
            let path = (core.store.read(cx).fleet.workspace(&host, &workspace))
                .map(|ws| ws.directory().to_string());
            let menu = menu.item(path_item(path)).separator();
            let menu = menu.item(PopupMenuItem::new("New agent in this workspace").on_click({
                let (board, lane, target) = (board.clone(), lane.clone(), target.clone());
                move |_, window, cx| {
                    let _ = board.update(cx, |board, cx| {
                        board.open_draft_in(&lane, target.clone(), window, cx);
                    });
                }
            }));
            let menu = menu.item(
                PopupMenuItem::new("New workspace in this project…").on_click({
                    let (board, lane, target) = (board.clone(), lane.clone(), target.clone());
                    move |_, window, cx| {
                        let _ = board.update(cx, |board, cx| {
                            board.open_new_workspace_draft(&lane, target.clone(), window, cx);
                        });
                    }
                }),
            );
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
    pub(super) fn card_menu_layer(&self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
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

    /// Opens a lane's menu at `position`, replacing any menu open: grouped
    /// by project, removing the lane's project from a host.
    pub(super) fn open_lane_menu(
        &mut self,
        lane: &Rc<Lane>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.core.nav.read(cx).lane_grouping == LaneGrouping::Label {
            return self.open_band_menu(lane, position, window, cx);
        }
        if self.core.nav.read(cx).lane_grouping != LaneGrouping::Project {
            return;
        }
        // Its workspaces' projects, one per host it spans.
        let mut projects: Vec<(HostId, String)> = Vec::new();
        for ws in &lane.workspaces {
            let fleet = &self.core.store.read(cx).fleet;
            if let Some(workspace) = fleet.workspace(&ws.host, &ws.workspace_id) {
                let pair = (ws.host.clone(), workspace.project_id.clone());
                if !projects.contains(&pair) {
                    projects.push(pair);
                }
            }
        }
        let fleet = &self.core.store.read(cx).fleet;
        if !(projects.iter())
            .any(|(host, _)| fleet.host(host).is_some_and(project_remove::supported))
        {
            return;
        }
        let core = self.core.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _, cx| {
            crate::widgets::remove_project::menu_items(menu, &core, &projects, cx)
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

    /// Grouped by label, a lane's menu shows each of its labels as a band,
    /// or stops showing its band.
    fn open_band_menu(
        &mut self,
        lane: &Rc<Lane>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The lane keys of its labels, named: a label's lane is keyed as
        // bands keep it; a combination's members by the catalog.
        let fleet = &self.core.store.read(cx).fleet;
        let banded: BTreeSet<&str> = (lane.band.iter())
            .flat_map(|band| band.labels.iter().map(String::as_str))
            .collect();
        let mut offers: Vec<(String, String)> = Vec::new();
        if lane.kind == LaneKind::Label && lane.band.is_none() {
            offers.push((lane.key.clone(), lane.title.clone()));
        } else {
            for key in &lane.label_keys {
                if banded.contains(key.as_str()) {
                    continue;
                }
                let name = (fleet.labels.as_ref())
                    .and_then(|catalog| catalog.get(&fleet::labels::LabelId::new(key)))
                    .map_or_else(|| key.clone(), |label| label.name.clone());
                offers.push((key.clone(), name));
            }
        }
        let band = lane.band.clone();
        if offers.is_empty() && band.is_none() {
            return;
        }
        let nav = self.core.nav.clone();
        let menu = PopupMenu::build(window, cx, move |mut menu, _, _| {
            for (key, name) in &offers {
                let (nav, key) = (nav.clone(), key.clone());
                menu = menu.item(PopupMenuItem::new(format!("Show {name} as band")).on_click(
                    move |_, _, cx| {
                        nav.update(cx, |nav, cx| nav.set_lane_band(&key, true, cx));
                    },
                ));
            }
            if let Some(band) = &band {
                let (nav, labels) = (nav.clone(), band.labels.clone());
                menu = menu.item(
                    PopupMenuItem::new(format!("Stop showing {} as band", band.title)).on_click(
                        move |_, _, cx| {
                            nav.update(cx, |nav, cx| {
                                for label in &labels {
                                    nav.set_lane_band(label, false, cx);
                                }
                            });
                        },
                    ),
                );
            }
            menu
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

    /// Right-click on a card opens the board's card menu for it.
    pub(super) fn card_menu_listener(
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

    /// Right-click on a workspace opens its menu where the pointer is.
    pub(super) fn workspace_menu_listener(
        &self,
        lane: &Rc<Lane>,
        host: &HostId,
        workspace: &str,
        cx: &mut Context<Self>,
    ) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        let lane = Rc::clone(lane);
        let (host, workspace) = (host.clone(), workspace.to_string());
        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
            this.open_workspace_menu(&lane, &host, &workspace, event.position, window, cx);
            cx.stop_propagation();
        })
    }
}
