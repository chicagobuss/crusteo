//! The left sidebar: nav rows, pinned workspaces, projects with their
//! workspaces (or status groups), and the footer.
//!
//! The workspace list is virtual: it is flattened into [`Item`]s and only
//! the ones in view are built and laid out, each on its own, however many
//! workspaces the fleet has.

use fleet::sidebar::{Grouping, SidebarRow, SidebarView};
use fleet::{Bucket, HostId, requests};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, Sizable};
use gpui_kit::gpui::{
    AnyElement, App, ClickEvent, Context, DismissEvent, Entity, Focusable, InteractiveElement,
    IntoElement, ListAlignment, ListState, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Point, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    anchored, deferred, div, list, px,
};
use gpui_kit::prelude::FluentBuilder;

use crate::app::{Core, Screen};
use crate::theme::{palette, size, with_alpha};
use crate::ui;

pub struct Sidebar {
    core: Core,
    view: SidebarView,
    /// Hosts online, which the footer and the empty state show.
    online: usize,
    /// The list's lines, as last handed to `list`.
    items: Vec<Item>,
    list: ListState,
    menu: Option<RowMenu>,
}

/// An open workspace row menu. Rows only carry a button; the sidebar owns
/// the one menu, so no row lays out menu machinery of its own.
struct RowMenu {
    position: Point<Pixels>,
    menu: Entity<PopupMenu>,
    _dismiss: Subscription,
}

/// One line of the workspace list.
#[derive(Clone, PartialEq)]
enum Item {
    PinnedHeader {
        collapsed: bool,
    },
    SectionHeader,
    Empty,
    Project {
        key: String,
        name: String,
        collapsed: bool,
        aggregate: Bucket,
        /// (host, projectId) to create a workspace in.
        creatable: Option<(HostId, String)>,
        /// (host, projectId) of each host's project merged here.
        projects: Vec<(HostId, String)>,
    },
    Row {
        row: Box<SidebarRow>,
        selected: bool,
        with_project: bool,
        indent: f32,
    },
    ShowMore {
        key: String,
        showing_all: bool,
        total: usize,
    },
    /// "New workspace" in a project that has none.
    Ghost {
        key: String,
        host: HostId,
        project_id: String,
    },
    /// Space below an expanded project.
    ProjectEnd,
    Status {
        bucket: Bucket,
        count: usize,
        collapsed: bool,
    },
    /// Space below the last line.
    End,
}

impl Sidebar {
    pub fn new(core: Core, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |this, _, cx| this.rebuild(false, cx))
            .detach();
        cx.observe(&core.nav, |this, _, cx| this.rebuild(true, cx))
            .detach();
        let mut this = Self {
            core,
            view: SidebarView::default(),
            online: usize::MAX,
            items: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(240.)),
            menu: None,
        };
        this.rebuild(true, cx);
        this
    }

    /// Reprojects the fleet. Redraws only when the projection or the hosts
    /// online changed, or `always` (navigation changes what is drawn too):
    /// most fleet changes touch nothing the sidebar shows.
    fn rebuild(&mut self, always: bool, cx: &mut Context<Self>) {
        let store = self.core.store.read(cx);
        let nav = self.core.nav.read(cx);
        let view = SidebarView::build(&store.fleet, nav.grouping, &nav.filters);
        let online = store
            .fleet
            .hosts
            .iter()
            .filter(|host| host.connection.is_online())
            .count();
        if always || view != self.view || online != self.online {
            self.view = view;
            self.online = online;
            cx.notify();
        }
    }

    /// Rows in visual order, for keyboard jumps.
    pub fn visible_rows(&self, cx: &App) -> Vec<(HostId, String)> {
        let nav = self.core.nav.read(cx);
        let collapsed = |key: &str| nav.is_collapsed(key);
        let mut rows: Vec<(HostId, String)> = Vec::new();
        if !nav.collapsed_pinned {
            rows.extend(
                self.view
                    .pinned
                    .iter()
                    .map(|r| (r.host.clone(), r.workspace_id.clone())),
            );
        }
        for row in self
            .view
            .visible_rows(nav.grouping, &collapsed)
            .into_iter()
            .skip(self.view.pinned.len())
        {
            rows.push((row.host.clone(), row.workspace_id.clone()));
        }
        rows
    }

    /// The list's lines for the current projection and navigation.
    fn items(&self, cx: &App) -> Vec<Item> {
        let nav = self.core.nav.read(cx);
        let on_workspace = nav.screen == Screen::Workspace;
        let selected = |row: &SidebarRow| {
            on_workspace
                && nav
                    .selection
                    .as_ref()
                    .is_some_and(|s| s.host == row.host && s.workspace == row.workspace_id)
        };
        let row = |row: &SidebarRow, with_project: bool, indent: f32| Item::Row {
            row: Box::new(row.clone()),
            selected: selected(row),
            with_project,
            indent,
        };
        let mut items = Vec::new();
        if !self.view.pinned.is_empty() {
            items.push(Item::PinnedHeader {
                collapsed: nav.collapsed_pinned,
            });
            if !nav.collapsed_pinned {
                items.extend(self.view.pinned.iter().map(|r| row(r, true, 0.)));
            }
        }
        items.push(Item::SectionHeader);
        match nav.grouping {
            Grouping::Project => {
                if self.view.projects.is_empty() {
                    items.push(Item::Empty);
                }
                for project in &self.view.projects {
                    let collapsed = nav.is_collapsed(&project.key);
                    let creatable = project
                        .placements
                        .first()
                        .map(|(host, project_id, _, _)| (host.clone(), project_id.clone()));
                    items.push(Item::Project {
                        key: project.key.clone(),
                        name: project.name.clone(),
                        collapsed,
                        aggregate: project.aggregate(),
                        creatable: creatable.clone(),
                        projects: (project.placements.iter())
                            .map(|(host, id, _, _)| (host.clone(), id.clone()))
                            .collect(),
                    });
                    if collapsed {
                        continue;
                    }
                    let showing_all = nav.show_all.contains(&project.key);
                    let limit = if showing_all { usize::MAX } else { GROUP_LIMIT };
                    items.extend(project.rows.iter().take(limit).map(|r| row(r, false, 0.)));
                    if project.rows.len() > GROUP_LIMIT {
                        items.push(Item::ShowMore {
                            key: project.key.clone(),
                            showing_all,
                            total: project.rows.len(),
                        });
                    }
                    if project.rows.is_empty()
                        && let Some((host, project_id)) = creatable
                    {
                        items.push(Item::Ghost {
                            key: project.key.clone(),
                            host,
                            project_id,
                        });
                    }
                    items.push(Item::ProjectEnd);
                }
            }
            Grouping::Status => {
                for (bucket, rows) in &self.view.groups {
                    let collapsed = nav.is_collapsed(bucket.label());
                    items.push(Item::Status {
                        bucket: *bucket,
                        count: rows.len(),
                        collapsed,
                    });
                    if !collapsed {
                        items.extend(rows.iter().map(|r| row(r, true, 8.)));
                    }
                }
            }
        }
        items.push(Item::End);
        items
    }

    fn render_item(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.items.get(ix) else {
            return div().into_any_element();
        };
        match item {
            Item::PinnedHeader { collapsed } => self.pinned_header(*collapsed, cx),
            Item::SectionHeader => self
                .section_header(self.core.nav.read(cx).grouping, cx)
                .into_any_element(),
            Item::Empty => empty_state(&self.core, cx).into_any_element(),
            Item::Project {
                key,
                name,
                collapsed,
                aggregate,
                creatable,
                projects,
            } => self.project_header(
                key,
                name,
                *collapsed,
                *aggregate,
                creatable.clone(),
                projects,
                cx,
            ),
            Item::Row {
                row,
                selected,
                with_project,
                indent,
            } => self.workspace_row(row, *selected, *with_project, *indent, cx),
            Item::ShowMore {
                key,
                showing_all,
                total,
            } => show_more(&self.core, key, *showing_all, *total).into_any_element(),
            Item::Ghost {
                key,
                host,
                project_id,
            } => ghost_row(&self.core, key, host, project_id),
            Item::ProjectEnd => div().h(px(12.)).into_any_element(),
            Item::Status { bucket, count, .. } => self.status_header(*bucket, *count, cx),
            Item::End => div().h(px(16.)).into_any_element(),
        }
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let items = self.items(cx);
        ui::sync_list(&self.list, &mut self.items, items);
        // A list lays each line out on its own, sized to its content unless
        // told to fill the width, as the rows did in a column.
        let rows = list(
            self.list.clone(),
            cx.processor(|this, ix: usize, _window, cx| {
                div()
                    .w_full()
                    .child(this.render_item(ix, cx))
                    .into_any_element()
            }),
        )
        .size_full();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.sidebar)
            .border_r_1()
            .border_color(p.border)
            .child(nav_rows(&self.core, cx))
            .child(
                div()
                    .id("sidebar-list")
                    .flex_1()
                    .min_h(px(0.))
                    .px(px(8.))
                    .pt(px(2.))
                    .child(rows),
            )
            .child(footer(&self.core, cx))
            .children(self.menu_layer(window, cx))
    }
}

impl Sidebar {
    fn pinned_header(&self, collapsed: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        div()
            .id("pinned-header")
            .flex()
            .items_center()
            .gap(px(4.))
            .min_h(px(36.))
            .px(px(8.))
            .text_size(size::sm())
            .text_color(p.muted)
            .child("Pinned")
            .child(ui::icon(
                if collapsed {
                    "chevron-right"
                } else {
                    "chevron-down"
                },
                12.,
                p.muted,
            ))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.core.nav.update(cx, |nav, cx| {
                    nav.collapsed_pinned = !nav.collapsed_pinned;
                    nav.save();
                    cx.notify();
                });
            }))
            .into_any_element()
    }

    /// Opens the row menu for a workspace at `position`, replacing any menu
    /// already open.
    #[allow(clippy::too_many_arguments)]
    fn open_row_menu(
        &mut self,
        host: &HostId,
        workspace: &str,
        pinned: bool,
        bucket: Bucket,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let (host, workspace) = (host.clone(), workspace.to_string());
        let menu = PopupMenu::build(window, cx, move |menu, _, cx| {
            row_menu(menu, &core, &host, &workspace, pinned, bucket, cx)
        });
        let _dismiss = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
            this.menu = None;
            cx.notify();
        });
        self.menu = Some(RowMenu {
            position,
            menu,
            _dismiss,
        });
        cx.notify();
    }

    /// The open row menu, floated above everything at its click position.
    fn menu_layer(&self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let open = self.menu.as_ref()?;
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
}

impl Sidebar {
    fn section_header(&self, grouping: Grouping, _cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let nav = self.core.nav.clone();
        let store = self.core.store.clone();
        div()
            .flex_shrink_0()
            .mt(px(4.))
            .flex()
            .items_center()
            .justify_between()
            .pl(px(8.))
            .pr(px(4.))
            .py(px(4.))
            .child(
                div()
                    .text_size(size::sm())
                    .text_color(p.muted)
                    .child("Workspaces"),
            )
            .child(
                Button::new("display-preferences")
                    .icon(Icon::empty().path("icons/settings-2.svg"))
                    .ghost()
                    .xsmall()
                    .tooltip("Display preferences")
                    .dropdown_menu(move |menu, _, cx| {
                        let nav_a = nav.clone();
                        let nav_b = nav.clone();
                        let nav_bg = nav.clone();
                        let show_background = nav.read(cx).show_background();
                        let mut menu = menu
                            // First, as in Paseo's display menu.
                            .item(
                                PopupMenuItem::new("Show background")
                                    .checked(show_background)
                                    .on_click(move |_, _, cx| {
                                        nav_bg.update(cx, |nav, cx| {
                                            nav.set_show_background(!show_background, cx)
                                        })
                                    }),
                            )
                            .separator()
                            .label("Grouping")
                            .item(
                                PopupMenuItem::new("Project")
                                    .checked(grouping == Grouping::Project)
                                    .on_click(move |_, _, cx| {
                                        nav_a.update(cx, |nav, cx| {
                                            nav.grouping = Grouping::Project;
                                            nav.save();
                                            cx.notify();
                                        })
                                    }),
                            )
                            .item(
                                PopupMenuItem::new("Status")
                                    .checked(grouping == Grouping::Status)
                                    .on_click(move |_, _, cx| {
                                        nav_b.update(cx, |nav, cx| {
                                            nav.grouping = Grouping::Status;
                                            nav.save();
                                            cx.notify();
                                        })
                                    }),
                            );
                        let hosts = store.read(cx).host_labels();
                        if hosts.len() > 1 {
                            let filter = nav.read(cx).filters.hosts.clone();
                            let nav_all = nav.clone();
                            menu = menu.separator().label("Host").item(
                                PopupMenuItem::new("All hosts")
                                    .checked(filter.is_empty())
                                    .on_click(move |_, _, cx| {
                                        nav_all.update(cx, |nav, cx| {
                                            nav.filters.hosts.clear();
                                            nav.save();
                                            cx.notify();
                                        })
                                    }),
                            );
                            for (host, label) in hosts {
                                let nav = nav.clone();
                                let checked = filter.contains(&host);
                                let name = host.clone();
                                menu =
                                    menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                                        move |_, _, cx| {
                                            let name = name.clone();
                                            nav.update(cx, |nav, cx| {
                                                if let Some(i) = nav
                                                    .filters
                                                    .hosts
                                                    .iter()
                                                    .position(|h| *h == name)
                                                {
                                                    nav.filters.hosts.remove(i);
                                                } else {
                                                    nav.filters.hosts.push(name);
                                                }
                                                nav.save();
                                                cx.notify();
                                            })
                                        },
                                    ));
                            }
                        }
                        menu
                    }),
            )
    }

    #[allow(clippy::too_many_arguments)]
    fn project_header(
        &self,
        key: &str,
        name: &str,
        collapsed: bool,
        aggregate: Bucket,
        creatable: Option<(HostId, String)>,
        projects: &[(HostId, String)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let group: SharedString = format!("project-{key}").into();
        let core = self.core.clone();
        let toggle = key.to_string();
        // A menu where a host can remove its project.
        let has_menu = projects.iter().any(|(host, _)| {
            (core.store.read(cx).fleet.host(host)).is_some_and(fleet::project_remove::supported)
        });
        let menu = {
            let projects = projects.to_vec();
            Button::new(SharedString::from(format!("project-menu-{key}")))
                .icon(Icon::empty().path("icons/ellipsis-vertical.svg"))
                .ghost()
                .xsmall()
                .tooltip("Project actions")
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_project_menu(&projects, event.position(), window, cx);
                }))
        };
        let right_click = {
            let projects = projects.to_vec();
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.open_project_menu(&projects, event.position, window, cx);
            })
        };

        div()
            .id(SharedString::from(format!("project-row-{key}")))
            .group(group.clone())
            .relative()
            .min_h(px(size::ROW_MIN_HEIGHT))
            .py(px(8.))
            .px(px(8.))
            .mb(px(4.))
            .rounded(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .hover(|s| s.bg(p.sidebar_hover))
            .child(
                div()
                    .w(px(16.))
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .relative()
                    .child(
                        div()
                            .id(SharedString::from(format!("{group}-badge")))
                            .group_hover(group.clone(), |s| s.invisible())
                            .child(ui::project_badge(name, key)),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("{group}-chevron")))
                            .absolute()
                            .invisible()
                            .group_hover(group.clone(), |s| s.visible())
                            .child(ui::icon(
                                if collapsed {
                                    "chevron-right"
                                } else {
                                    "chevron-down"
                                },
                                14.,
                                gpui_kit::gpui::rgb(0x9ca3af),
                            )),
                    )
                    .when(collapsed && aggregate != Bucket::Done, |slot| {
                        slot.child(
                            div()
                                .absolute()
                                .right(px(-4.))
                                .bottom(px(-2.))
                                .size(px(12.))
                                .rounded_full()
                                .bg(p.sidebar)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(ui::agent_dot(aggregate)),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(size::base())
                    .text_color(p.muted)
                    .child(name.to_string()),
            )
            .child(
                div()
                    .id(SharedString::from(format!("{group}-actions")))
                    .flex()
                    .gap(px(2.))
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .when_some(creatable, |row, (host, project_id)| {
                        let nav = core.nav.clone();
                        row.child(
                            Button::new(SharedString::from(format!("new-ws-{key}")))
                                .icon(Icon::empty().path("icons/plus.svg"))
                                .ghost()
                                .xsmall()
                                .tooltip("New workspace")
                                .on_click(move |_, _, cx| {
                                    // Not also a click on the row, which
                                    // would fold the project away.
                                    cx.stop_propagation();
                                    let screen = Screen::NewWorkspace {
                                        host: Some(host.clone()),
                                        project: Some(project_id.clone()),
                                    };
                                    nav.update(cx, |nav, cx| nav.show(screen, cx));
                                }),
                        )
                    })
                    .when(has_menu, |row| row.child(menu)),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.core
                    .nav
                    .update(cx, |nav, cx| nav.toggle_collapsed(&toggle, cx));
            }))
            .when(has_menu, |row| {
                row.on_mouse_down(MouseButton::Right, right_click)
            })
            .into_any_element()
    }

    /// Opens a project's menu at `position`, replacing any menu open.
    fn open_project_menu(
        &mut self,
        projects: &[(HostId, String)],
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let projects = projects.to_vec();
        let menu = PopupMenu::build(window, cx, move |menu, _, cx| {
            crate::widgets::remove_project::menu_items(menu, &core, &projects, cx)
        });
        let _dismiss = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
            this.menu = None;
            cx.notify();
        });
        self.menu = Some(RowMenu {
            position,
            menu,
            _dismiss,
        });
        cx.notify();
    }

    fn status_header(&self, bucket: Bucket, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let label = bucket.label();
        let (icon_name, color) = match bucket {
            Bucket::NeedsInput => ("circle-alert", p.dot_warning),
            Bucket::Failed => ("circle-x", p.dot_danger),
            Bucket::Attention => ("circle-check", p.dot_success),
            Bucket::Running => ("circle-dot", p.dot_running),
            Bucket::Done => ("circle-check", p.muted),
        };
        div()
            .id(SharedString::from(format!("status-{label}")))
            .min_h(px(36.))
            .p(px(8.))
            .mb(px(8.))
            .rounded(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .hover(|s| s.bg(p.sidebar_hover))
            .child(
                div()
                    .w(px(16.))
                    .flex()
                    .justify_center()
                    .child(ui::icon(icon_name, 14., color)),
            )
            .child(div().text_color(p.muted).child(format!("{label}  {count}")))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.core
                    .nav
                    .update(cx, |nav, cx| nav.toggle_collapsed(label, cx));
            }))
            .into_any_element()
    }

    fn workspace_row(
        &self,
        row: &SidebarRow,
        selected: bool,
        with_project: bool,
        indent: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let id = format!("{}:{}", row.host, row.workspace_id);
        let group: SharedString = format!("ws-{id}").into();
        let host = row.host.clone();
        let host_name = self.core.store.read(cx).fleet.host_name(&host).to_string();
        let workspace = row.workspace_id.clone();

        let leading: AnyElement = if with_project {
            div()
                .w(px(16.))
                .h(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .relative()
                .child(ui::project_badge(&row.project_name, &row.project_key))
                .when(row.bucket != Bucket::Done, |slot| {
                    slot.child(
                        div()
                            .absolute()
                            .right(px(-5.))
                            .bottom(px(-1.))
                            .size(px(12.))
                            .rounded_full()
                            .bg(p.sidebar)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(ui::agent_dot(row.bucket)),
                    )
                })
                .into_any_element()
        } else if row.archiving {
            div()
                .w(px(16.))
                .flex()
                .justify_center()
                .child(ui::running_ring())
                .into_any_element()
        } else {
            ui::status_indicator(row.bucket, &row.workspace_kind).into_any_element()
        };

        let multi_host = self.view.multi_host;
        let mut meta: Vec<AnyElement> = Vec::new();
        if multi_host {
            // Coloured by id, so a renamed host keeps its colour.
            let host_color = crate::theme::identity_for(row.host.as_str()).1;
            meta.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .child(ui::icon("server", 12., gpui_kit::gpui::rgb(host_color)))
                    .child(
                        div()
                            .text_size(size::sm())
                            .text_color(gpui_kit::gpui::rgb(host_color))
                            .child(host_name),
                    )
                    .into_any_element(),
            );
        }
        if let Some((number, state)) = &row.pull_request {
            let (name, color) = match state.as_str() {
                "merged" => ("git-merge", p.status_merged),
                "closed" => ("git-pull-request", p.status_danger),
                _ => ("git-pull-request", p.status_success),
            };
            meta.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .child(ui::icon(name, 12., color))
                    .child(
                        div()
                            .text_size(size::sm())
                            .text_color(p.muted)
                            .child(format!("#{number}")),
                    )
                    .into_any_element(),
            );
        }
        if !row.labels.is_empty() {
            meta.push(
                div()
                    .flex()
                    .gap(px(4.))
                    .children(row.labels.iter().map(|(name, color)| {
                        let (fill, fg) =
                            crate::theme::identity_named(color).unwrap_or((0x7a6aa8, 0xa392d5));
                        div()
                            .px(px(6.))
                            .rounded_full()
                            .bg(with_alpha(gpui_kit::gpui::rgb(fill), 0.1))
                            .text_size(size::sm())
                            .text_color(gpui_kit::gpui::rgb(fg))
                            .child(name.clone())
                    }))
                    .into_any_element(),
            );
        }

        let mut meta_row = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .min_w(px(0.))
            .overflow_hidden();
        for (i, item) in meta.into_iter().enumerate() {
            if i > 0 {
                meta_row = meta_row.child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.extra_muted)
                        .child("·"),
                );
            }
            meta_row = meta_row.child(item);
        }

        let trailing = div()
            .id(SharedString::from(format!("{group}-trailing")))
            .flex()
            .items_center()
            .gap(px(4.))
            .h(px(20.))
            .text_size(size::sm())
            .group_hover(group.clone(), |s| s.invisible())
            .when(row.additions > 0 || row.deletions > 0, |d| {
                d.child(
                    div()
                        .text_color(p.status_success)
                        .child(format!("+{}", row.additions)),
                )
                .child(
                    div()
                        .text_color(p.status_danger)
                        .child(format!("-{}", row.deletions)),
                )
            });

        let menu = {
            let (host, workspace) = (row.host.clone(), row.workspace_id.clone());
            let (pinned, bucket) = (row.pinned_at.is_some(), row.bucket);
            Button::new(SharedString::from(format!("ws-menu-{id}")))
                .icon(Icon::empty().path("icons/ellipsis-vertical.svg"))
                .ghost()
                .xsmall()
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_row_menu(
                        &host,
                        &workspace,
                        pinned,
                        bucket,
                        event.position(),
                        window,
                        cx,
                    );
                }))
        };

        div()
            .id(SharedString::from(format!("ws-row-{id}")))
            .group(group.clone())
            .relative()
            // Scroll contents keep their height instead of squeezing the
            // metadata into the next row. Pinned rows stay deliberately compact.
            .flex_shrink_0()
            .min_h(px(size::ROW_MIN_HEIGHT))
            .mb(px(2.))
            .py(px(if row.pinned_at.is_some() && indent == 0. {
                2.
            } else {
                8.
            }))
            .pl(px(8. + indent))
            .pr(px(12.))
            .rounded(px(8.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .when(selected, |d| d.bg(p.sidebar_selected))
            .when(!selected, |d| d.hover(|s| s.bg(p.sidebar_hover)))
            .child(
                div().flex().items_start().gap(px(8.)).child(leading).child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .line_height(px(20.))
                                        .text_color(with_alpha(
                                            p.foreground,
                                            if selected { 1.0 } else { 0.76 },
                                        ))
                                        .group_hover(group.clone(), |s| s.text_color(p.foreground))
                                        .id(SharedString::from(format!("{group}-title")))
                                        .child(row.title.clone()),
                                )
                                .when(row.background, |line| line.child(ui::background_tag()))
                                .child(trailing),
                        )
                        .when(meta_row_has_children(multi_host, row), |col| {
                            col.child(meta_row)
                        }),
                ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("{group}-menu")))
                    .absolute()
                    .top(px(6.))
                    .right(px(6.))
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .child(menu),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                let (host, workspace) = (host.clone(), workspace.clone());
                this.core
                    .nav
                    .update(cx, |nav, cx| nav.select(&host, &workspace, cx));
            }))
            .into_any_element()
    }
}

/// The workspace row menu's items.
fn row_menu(
    menu: PopupMenu,
    core: &Core,
    host: &HostId,
    workspace: &str,
    pinned: bool,
    bucket: Bucket,
    cx: &App,
) -> PopupMenu {
    let store = core.store.clone();
    let directory = store
        .read(cx)
        .fleet
        .workspace(host, workspace)
        .map(|w| w.directory().to_string())
        .unwrap_or_default();
    let branch = store
        .read(cx)
        .fleet
        .workspace(host, workspace)
        .and_then(|w| w.branch().map(str::to_string));
    let mut menu = menu.item(PopupMenuItem::new("Copy path").on_click(move |_, _, cx| {
        cx.write_to_clipboard(gpui_kit::gpui::ClipboardItem::new_string(directory.clone()));
    }));
    if let Some(branch) = branch {
        menu = menu.item(
            PopupMenuItem::new("Copy branch name").on_click(move |_, _, cx| {
                cx.write_to_clipboard(gpui_kit::gpui::ClipboardItem::new_string(branch.clone()));
            }),
        );
    }
    let act = |label: &str, message: serde_json::Value| {
        let store = store.clone();
        let host = host.clone();
        PopupMenuItem::new(label.to_string()).on_click(move |_, _, cx| {
            let message = message.clone();
            let host = host.clone();
            store.update(cx, |store, cx| store.act(&host, message, cx));
        })
    };
    if matches!(bucket, Bucket::Attention | Bucket::Failed) {
        menu = menu.item(act("Mark as read", requests::clear_attention(workspace)));
    }
    if bucket == Bucket::Done {
        menu = menu.item(act("Mark as unread", requests::mark_unread(workspace)));
    }
    menu = menu.item(act(
        if pinned { "Unpin" } else { "Pin to top" },
        requests::pin_workspace(workspace, !pinned),
    ));
    {
        let store = store.clone();
        let host = host.clone();
        let workspace = workspace.to_string();
        let current = store
            .read(cx)
            .fleet
            .workspace(&host, &workspace)
            .map(|w| w.title.clone().unwrap_or_else(|| w.name.clone()))
            .unwrap_or_default();
        menu = menu.item(
            PopupMenuItem::new("Rename workspace").on_click(move |_, window, cx| {
                let store = store.clone();
                let host = host.clone();
                let workspace = workspace.clone();
                ui::prompt_text(
                    window,
                    cx,
                    "Rename workspace",
                    &current,
                    "Rename",
                    move |value, cx| {
                        let title = (!value.is_empty()).then_some(value.as_str());
                        let message = requests::rename_workspace(&workspace, title);
                        store.update(cx, |store, cx| store.act(&host, message, cx));
                    },
                );
            }),
        );
    }
    let host_labels = store
        .read(cx)
        .fleet
        .host(host)
        .map(|h| h.directory.labels.clone())
        .unwrap_or_default();
    let assigned: Vec<String> = store
        .read(cx)
        .fleet
        .workspace(host, workspace)
        .map(|w| {
            w.labels
                .iter()
                .map(|l| fleet::directory::label_key(l))
                .collect()
        })
        .unwrap_or_default();
    if store
        .read(cx)
        .fleet
        .host(host)
        .is_some_and(|h| h.has("workspaceLabels"))
    {
        menu = menu.separator().label("Labels");
        for label in host_labels {
            let on = assigned.contains(&fleet::directory::label_key(&label.name));
            menu = menu.item(
                act(
                    &label.name,
                    requests::set_label(workspace, &label.name, &label.color, !on),
                )
                .checked(on),
            );
        }
        let store = store.clone();
        let host = host.clone();
        let workspace = workspace.to_string();
        menu = menu.item(
            PopupMenuItem::new("New label…").on_click(move |_, window, cx| {
                let store = store.clone();
                let host = host.clone();
                let workspace = workspace.clone();
                ui::prompt_text(window, cx, "New label", "", "Add", move |value, cx| {
                    if value.is_empty() {
                        return;
                    }
                    let message = requests::set_label(&workspace, &value, "violet", true);
                    store.update(cx, |store, cx| store.act(&host, message, cx));
                });
            }),
        );
    }
    crate::workspace_archive::workspace_menu(menu.separator(), core, host, workspace, cx)
}

/// "New workspace" in a project that has none yet.
fn ghost_row(core: &Core, key: &str, host: &HostId, project_id: &str) -> AnyElement {
    let p = palette();
    let nav = core.nav.clone();
    let (host, project_id) = (host.clone(), project_id.to_string());
    div()
        .id(SharedString::from(format!("ghost-{key}")))
        .min_h(px(size::ROW_MIN_HEIGHT))
        .mb(px(2.))
        .py(px(8.))
        .pl(px(16.))
        .pr(px(12.))
        .rounded(px(8.))
        .flex()
        .items_center()
        .gap(px(8.))
        .text_color(p.muted)
        .hover(|s| s.bg(p.sidebar_hover).text_color(p.foreground))
        .child(
            div()
                .w(px(16.))
                .flex()
                .justify_center()
                .child(ui::icon("plus", 14., p.muted)),
        )
        .child("New workspace")
        .on_click(move |_, _, cx| {
            let screen = Screen::NewWorkspace {
                host: Some(host.clone()),
                project: Some(project_id.clone()),
            };
            nav.update(cx, |nav, cx| nav.show(screen, cx));
        })
        .into_any_element()
}

fn meta_row_has_children(multi_host: bool, row: &SidebarRow) -> bool {
    multi_host || row.pull_request.is_some() || !row.labels.is_empty()
}

fn nav_rows(core: &Core, cx: &mut Context<Sidebar>) -> impl IntoElement {
    let p = palette();
    let screen = core.nav.read(cx).screen.clone();
    let row = |id: &'static str, icon: &'static str, label: &'static str, active: bool| {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(8.))
            .min_h(px(28.))
            .py(px(4.))
            .px(px(8.))
            .rounded(px(8.))
            .text_color(if active { p.foreground } else { p.muted })
            .when(active, |d| d.bg(p.sidebar_hover))
            .hover(|s| s.bg(p.sidebar_hover).text_color(p.foreground))
            .child(ui::icon(
                icon,
                14.,
                if active { p.foreground } else { p.muted },
            ))
            .child(label)
    };
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .px(px(8.))
        .pt(px(8.))
        .pb(px(6.))
        .border_b_1()
        .border_color(p.border)
        .child(
            row(
                "nav-new-workspace",
                "plus",
                "New workspace",
                matches!(screen, Screen::NewWorkspace { .. }),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                let screen = Screen::NewWorkspace {
                    host: this
                        .core
                        .nav
                        .read(cx)
                        .selection
                        .as_ref()
                        .map(|s| s.host.clone()),
                    project: None,
                };
                this.core.nav.update(cx, |nav, cx| nav.show(screen, cx));
            })),
        )
        .child(
            row(
                "nav-history",
                "list-clock",
                "History",
                screen == Screen::History,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.core
                    .nav
                    .update(cx, |nav, cx| nav.show(Screen::History, cx));
            })),
        )
        .child(
            row("nav-search", "search", "Search", false).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.core.nav.update(cx, |nav, cx| {
                        nav.command_open = true;
                        cx.notify();
                    });
                },
            )),
        )
}

fn empty_state(core: &Core, cx: &mut Context<Sidebar>) -> impl IntoElement {
    let p = palette();
    let store = core.store.read(cx);
    let online = store
        .fleet
        .hosts
        .iter()
        .filter(|h| h.connection.is_online())
        .count();
    let (title, body) = if online == 0 {
        (
            "Connecting to your hosts",
            "Workspaces appear here once a daemon answers.",
        )
    } else {
        ("No projects yet", "Add a project to get started")
    };
    div()
        .mx(px(8.))
        .mt(px(16.))
        .pt(px(24.))
        .pb(px(16.))
        .px(px(16.))
        .rounded(px(8.))
        .bg(p.surface0)
        .flex()
        .flex_col()
        .items_center()
        .gap(px(12.))
        .child(
            div()
                .text_size(size::sm())
                .text_color(p.foreground)
                .child(title),
        )
        .child(div().text_size(size::sm()).text_color(p.muted).child(body))
        .when(online > 0, |d| {
            d.child(
                Button::new("empty-add-project")
                    .label("Add project")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        add_project(&this.core, window, cx);
                    })),
            )
        })
}

/// Add project, on the selected workspace's host if any; the new project
/// then opens in New workspace, as in Paseo.
pub fn add_project(core: &Core, window: &mut Window, cx: &mut App) {
    let host = core.nav.read(cx).selection.as_ref().map(|s| s.host.clone());
    let nav = core.nav.clone();
    crate::widgets::add_project::open(core, host, window, cx, move |host, project, _, cx| {
        let screen = Screen::NewWorkspace {
            host: Some(host.clone()),
            project: Some(project.to_string()),
        };
        nav.update(cx, |nav, cx| nav.show(screen, cx));
    });
}

fn footer(core: &Core, cx: &mut Context<Sidebar>) -> impl IntoElement {
    let p = palette();
    let hosts = &core.store.read(cx).fleet.hosts;
    let (online, total) = (
        hosts.iter().filter(|h| h.connection.is_online()).count(),
        hosts.len(),
    );
    let icon_button = |id: &'static str, icon: &'static str| {
        div()
            .id(id)
            .size(px(28.))
            .p(px(4.))
            .rounded(px(6.))
            .flex()
            .items_center()
            .justify_center()
            .hover(|s| s.bg(p.sidebar_hover))
            .child(ui::icon(icon, 16., p.muted))
    };
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(8.))
        .py(px(12.))
        .border_t_1()
        .border_color(p.border)
        .child(
            div()
                .id("footer-hosts")
                .flex_1()
                .min_h(px(32.))
                .py(px(6.))
                .px(px(8.))
                .rounded(px(8.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_color(p.muted)
                .hover(|s| s.bg(p.sidebar_hover).text_color(p.foreground))
                .child(ui::icon("server", 14., p.muted))
                .child(format!("{online}/{total} hosts online"))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.core
                        .nav
                        .update(cx, |nav, cx| nav.show(Screen::Hosts, cx));
                })),
        )
        .child(
            icon_button("footer-add-project", "folder-plus")
                .tooltip(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new("Add project").build(window, cx)
                })
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    add_project(&this.core, window, cx);
                })),
        )
        .child(
            icon_button("footer-settings", "settings").on_click(cx.listener(
                |this, event: &ClickEvent, window, cx| {
                    let side = crate::pane::side_at(event.position().x, window);
                    this.core.nav.update(cx, |nav, cx| {
                        nav.set_pane(Some((crate::app::Pane::Settings, side)), cx)
                    });
                },
            )),
        )
}

pub fn _unused(_: Entity<Sidebar>) {}

/// Paseo shows 20 rows per group, then a "Show more" row.
const GROUP_LIMIT: usize = 20;

fn show_more(core: &Core, key: &str, showing_all: bool, total: usize) -> impl IntoElement {
    let p = palette();
    let nav = core.nav.clone();
    let key = key.to_string();
    div()
        .id(SharedString::from(format!("show-more-{key}")))
        .min_h(px(28.))
        .pl(px(8.))
        .rounded(px(8.))
        .flex()
        .items_center()
        .gap(px(8.))
        .text_size(size::sm())
        .text_color(p.muted)
        .hover(|s| s.bg(p.sidebar_hover).text_color(p.foreground))
        .child(div().w(px(16.)).flex().justify_center().child(ui::icon(
            if showing_all {
                "chevron-up"
            } else {
                "chevron-down"
            },
            14.,
            p.muted,
        )))
        .child(if showing_all {
            "Show less".to_string()
        } else {
            format!("Show more ({})", total - GROUP_LIMIT)
        })
        .on_click(move |_, _, cx| {
            let key = key.clone();
            nav.update(cx, |nav, cx| {
                if !nav.show_all.remove(&key) {
                    nav.show_all.insert(key);
                }
                cx.notify();
            });
        })
}
