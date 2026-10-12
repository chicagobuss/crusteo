//! Full-pane screens reached from the sidebar: hosts, new workspace, settings.

use fleet::providers::Provider;
use fleet::{Connection, HostConfig};
use gpui_kit::base::Selectable as _;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::gpui::{
    App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;
use serde_json::json;

use crate::app::notifications::{Corner, Route};
use crate::app::store::uuid_v4;
use crate::app::{Core, Screen, Tab};
use crate::theme::{palette, size};
use crate::ui;

fn page(title: &str) -> gpui_kit::gpui::Div {
    let p = palette();
    div().size_full().bg(p.surface0).flex().flex_col().child(
        div()
            .h(px(size::HEADER_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .px(px(16.))
            .border_b_1()
            .border_color(p.border)
            .text_color(p.foreground)
            .child(title.to_string()),
    )
}

fn card() -> gpui_kit::gpui::Div {
    let p = palette();
    div()
        .rounded(px(8.))
        .border_1()
        .border_color(p.border)
        .bg(p.surface1)
        .flex()
        .flex_col()
}

// ---- hosts -------------------------------------------------------------------

pub struct HostsScreen {
    core: Core,
    name: Entity<InputState>,
    endpoint: Entity<InputState>,
}

impl HostsScreen {
    pub fn new(core: Core, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        Self {
            core,
            name: cx.new(|cx| InputState::new(window, cx).placeholder("Name, e.g. workstation")),
            endpoint: cx.new(|cx| {
                InputState::new(window, cx).placeholder("host:port, e.g. workstation.local:6767")
            }),
        }
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        let endpoint = self.endpoint.read(cx).value().trim().to_string();
        if name.is_empty() || endpoint.is_empty() {
            return;
        }
        let mut hosts: Vec<HostConfig> = self
            .core
            .store
            .read(cx)
            .fleet
            .hosts
            .iter()
            .map(|h| h.config.clone())
            .collect();
        hosts.retain(|h| h.name != name);
        hosts.push(HostConfig {
            name,
            endpoint: Some(endpoint),
            password: None,
        });
        self.core
            .store
            .update(cx, |store, cx| store.set_hosts(hosts, cx));
        self.name.update(cx, |i, cx| i.set_value("", window, cx));
        self.endpoint
            .update(cx, |i, cx| i.set_value("", window, cx));
    }
}

impl Render for HostsScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let hosts = &self.core.store.read(cx).fleet.hosts;
        let mut list = card();
        for (i, host) in hosts.iter().enumerate() {
            let name = host.config.name.clone();
            let core = self.core.clone();
            let core_remove = self.core.clone();
            let detail = match &host.connection {
                Connection::Online => host
                    .server
                    .as_ref()
                    .map(|s| {
                        format!(
                            "{} · v{} · {} workspaces · {} agents",
                            s.hostname,
                            s.version,
                            host.directory.workspaces.len(),
                            host.directory.agents.len()
                        )
                    })
                    .unwrap_or_default(),
                Connection::Offline(reason) | Connection::Refused(reason) => reason.clone(),
                other => other.label().to_string(),
            };
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .p(px(16.))
                    .when(i > 0, |d| d.border_t_1().border_color(p.border))
                    .child(ui::host_status_dot(&host.connection))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().text_color(p.foreground).child(format!(
                                "{}  {}",
                                host.config.name,
                                host.config.endpoint.clone().unwrap_or_else(|| "no address".into())
                            )))
                            .child(
                                div()
                                    .text_size(size::sm())
                                    .text_color(p.muted)
                                    .truncate()
                                    .child(detail),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("reconnect-{i}")))
                            .label("Reconnect")
                            .ghost()
                            .small()
                            .on_click(move |_, _, cx| {
                                core.store.update(cx, |store, _| store.reconnect(&name));
                            }),
                    )
                    .child({
                        let remove = host.config.name.clone();
                        Button::new(SharedString::from(format!("remove-{i}")))
                            .label("Remove")
                            .ghost()
                            .small()
                            .on_click(move |_, _, cx| {
                                let hosts: Vec<HostConfig> = core_remove
                                    .store
                                    .read(cx)
                                    .fleet
                                    .hosts
                                    .iter()
                                    .map(|h| h.config.clone())
                                    .filter(|h| h.name != remove)
                                    .collect();
                                core_remove
                                    .store
                                    .update(cx, |store, cx| store.set_hosts(hosts, cx));
                            })
                    }),
            );
        }
        page("Hosts").child(
            div()
                .id("hosts-scroll")
                .flex_1()
                .overflow_y_scroll()
                .p(px(24.))
                .flex()
                .flex_col()
                .gap(px(16.))
                .child(div().text_size(size::sm()).text_color(p.muted).child(
                    "Each host is a Paseo daemon. Crusteo keeps a live connection to all of them at once.",
                ))
                .child(list)
                .child(
                    card()
                        .p(px(16.))
                        .gap(px(8.))
                        .child(div().text_color(p.foreground).child("Add host"))
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(div().w(px(200.)).child(Input::new(&self.name).small()))
                                .child(div().flex_1().child(Input::new(&self.endpoint).small()))
                                .child(
                                    Button::new("add-host")
                                        .label("Add")
                                        .primary()
                                        .small()
                                        .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                                ),
                        ),
                ),
        )
    }
}

// ---- new workspace -------------------------------------------------------

pub struct NewWorkspaceScreen {
    core: Core,
    host: Option<String>,
    project: Option<String>,
    worktree: bool,
    provider: Option<String>,
    prompt: Entity<TextareaState>,
    creating: bool,
    error: Option<String>,
}

impl NewWorkspaceScreen {
    pub fn new(core: Core, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        cx.observe(&core.nav, |this, nav, cx| {
            if let Screen::NewWorkspace { host, project } = nav.read(cx).screen.clone()
                && host.is_some()
                && host != this.host
            {
                this.host = host;
                this.project = project;
                cx.notify();
            }
        })
        .detach();
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 12)
                .placeholder("Describe the work for the first agent")
        });
        let (host, project) = match core.nav.read(cx).screen.clone() {
            Screen::NewWorkspace { host, project } => (host, project),
            _ => (None, None),
        };
        Self {
            core,
            host,
            project,
            worktree: true,
            provider: None,
            prompt,
            creating: false,
            error: None,
        }
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        let picked = {
            let store = self.core.store.read(cx);
            let host = self.host.clone().filter(|h| {
                store
                    .fleet
                    .host(h)
                    .is_some_and(|h| h.connection.is_online())
            });
            let project = host.as_ref().and_then(|host| {
                let id = self.project.as_ref()?;
                store.fleet.host(host)?.directory.projects.get(id).cloned()
            });
            let providers: Vec<Provider> = host
                .as_ref()
                .and_then(|h| store.providers.get(h).cloned())
                .unwrap_or_default();
            (host, project, providers)
        };
        let (Some(host), project, providers) = picked else {
            self.error = Some("Pick an online host".into());
            cx.notify();
            return;
        };
        let Some(project) = project else {
            self.error = Some("Pick a project".into());
            cx.notify();
            return;
        };
        let provider = self
            .provider
            .clone()
            .or_else(|| {
                providers
                    .iter()
                    .find(|p| p.ready() && p.id == "claude")
                    .map(|p| p.id.clone())
            })
            .or_else(|| providers.iter().find(|p| p.ready()).map(|p| p.id.clone()))
            .unwrap_or_else(|| "claude".into());
        let prompt = self.prompt.read(cx).value().trim().to_string();
        let worktree = self.worktree && project.project_kind == "git";
        let source = if worktree {
            json!({ "kind": "worktree", "cwd": project.project_root_path, "projectId": project.project_id })
        } else {
            json!({ "kind": "directory", "path": project.project_root_path, "projectId": project.project_id })
        };
        let mut message = json!({
            "type": "workspace.create.request",
            "idempotencyKey": uuid_v4(),
            "source": source,
        });
        if !prompt.is_empty() {
            message["agent"] = json!({
                "config": { "provider": provider, "cwd": project.project_root_path },
                "initialPrompt": prompt,
                "clientMessageId": uuid_v4(),
                "labels": {},
            });
            message["firstAgentContext"] = json!({ "prompt": prompt });
        }
        self.creating = true;
        self.error = None;
        cx.notify();
        let task = self.core.store.read(cx).request(&host, message, cx);
        let nav = self.core.nav.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.creating = false;
                match result {
                    Ok(payload) if payload["error"].is_string() => {
                        this.error = payload["error"].as_str().map(str::to_string);
                    }
                    Ok(payload) => {
                        let workspace = payload["workspace"]["id"].as_str().map(str::to_string);
                        let agent = payload["agent"]["id"].as_str().map(str::to_string);
                        if let Some(workspace) = workspace {
                            nav.update(cx, |nav, cx| {
                                nav.select(&host, &workspace, cx);
                                if let Some(agent) = agent {
                                    nav.focus_tab(
                                        &format!("{host}:{workspace}"),
                                        Tab::Agent(agent),
                                        cx,
                                    );
                                }
                            });
                        }
                    }
                    Err(err) => this.error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for NewWorkspaceScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let store = self.core.store.read(cx);
        let online: Vec<String> = store
            .fleet
            .hosts
            .iter()
            .filter(|h| h.connection.is_online())
            .map(|h| h.config.name.clone())
            .collect();
        if self.host.is_none() {
            self.host = online.first().cloned();
        }
        let host = self.host.clone();
        let projects: Vec<(String, String, String)> = host
            .as_ref()
            .and_then(|h| store.fleet.host(h))
            .map(|h| {
                let mut list: Vec<_> = h
                    .directory
                    .projects
                    .values()
                    .map(|p| (p.project_id.clone(), p.name(), p.project_kind.clone()))
                    .collect();
                list.sort_by(|a, b| fleet::sidebar::natural(&a.1, &b.1));
                list
            })
            .unwrap_or_default();
        let project_label = self
            .project
            .as_ref()
            .and_then(|id| projects.iter().find(|p| &p.0 == id))
            .map(|p| p.1.clone())
            .unwrap_or_else(|| "Choose a project".into());
        let is_git = self
            .project
            .as_ref()
            .and_then(|id| projects.iter().find(|p| &p.0 == id))
            .is_some_and(|p| p.2 == "git");
        let providers: Vec<Provider> = host
            .as_ref()
            .and_then(|h| store.providers.get(h).cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(Provider::ready)
            .collect();
        if let Some(h) = &host
            && !store.providers.contains_key(h)
        {
            let h = h.clone();
            let core = self.core.clone();
            cx.defer(move |cx| {
                core.store
                    .update(cx, |store, cx| store.ensure_providers(&h, cx))
            });
        }
        let provider_label = self
            .provider
            .clone()
            .or_else(|| {
                providers
                    .iter()
                    .find(|p| p.id == "claude")
                    .map(|p| p.id.clone())
            })
            .or_else(|| providers.first().map(|p| p.id.clone()))
            .and_then(|id| {
                providers
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.label.clone())
            })
            .unwrap_or_else(|| "Provider".into());

        let this = cx.entity().downgrade();
        let host_menu = {
            let this = this.clone();
            let online = online.clone();
            Button::new("nw-host")
                .label(host.clone().unwrap_or_else(|| "No host online".into()))
                .small()
                .dropdown_menu(move |mut menu, _, _| {
                    for name in &online {
                        let this = this.clone();
                        let name = name.clone();
                        menu = menu.item(PopupMenuItem::new(name.clone()).on_click(
                            move |_, _, cx| {
                                let name = name.clone();
                                let _ = this.update(cx, |this, cx| {
                                    this.host = Some(name);
                                    this.project = None;
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu
                })
        };
        let project_menu = {
            let this = this.clone();
            Button::new("nw-project")
                .label(project_label)
                .small()
                .dropdown_menu(move |mut menu, _, _| {
                    for (id, name, _) in &projects {
                        let this = this.clone();
                        let id = id.clone();
                        menu = menu.item(PopupMenuItem::new(name.clone()).on_click(
                            move |_, _, cx| {
                                let id = id.clone();
                                let _ = this.update(cx, |this, cx| {
                                    this.project = Some(id);
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu.max_h(px(400.))
                })
        };
        let provider_menu = {
            let this = this.clone();
            Button::new("nw-provider")
                .label(provider_label)
                .small()
                .dropdown_menu(move |mut menu, _, _| {
                    for provider in &providers {
                        let this = this.clone();
                        let id = provider.id.clone();
                        menu = menu.item(PopupMenuItem::new(provider.label.clone()).on_click(
                            move |_, _, cx| {
                                let id = id.clone();
                                let _ = this.update(cx, |this, cx| {
                                    this.provider = Some(id);
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu
                })
        };
        let worktree = self.worktree;

        page("New workspace").child(
            div().flex_1().flex().justify_center().p(px(24.)).child(
                div()
                    .w_full()
                    .max_w(px(size::COMPOSER_MAX_WIDTH))
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().w(px(80.)).text_color(p.muted).child("Host"))
                            .child(host_menu),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().w(px(80.)).text_color(p.muted).child("Project"))
                            .child(project_menu),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().w(px(80.)).text_color(p.muted).child("Isolation"))
                            .child(
                                Button::new("nw-local")
                                    .label("Local checkout")
                                    .small()
                                    .when(!worktree || !is_git, |b| b.primary())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.worktree = false;
                                        cx.notify();
                                    })),
                            )
                            .when(is_git, |d| {
                                d.child(
                                    Button::new("nw-worktree")
                                        .label("New worktree")
                                        .small()
                                        .when(worktree, |b| b.primary())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.worktree = true;
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().w(px(80.)).text_color(p.muted).child("Agent"))
                            .child(provider_menu),
                    )
                    .child(
                        div()
                            .rounded(px(16.))
                            .border_1()
                            .border_color(p.border_accent)
                            .bg(p.surface1)
                            .p(px(12.))
                            .child(
                                Textarea::new(&self.prompt)
                                    .appearance(false)
                                    .text_size(size::content()),
                            ),
                    )
                    .when_some(self.error.clone(), |d, e| {
                        d.child(div().text_color(p.status_danger).child(e))
                    })
                    .child(
                        div().flex().justify_end().child(
                            Button::new("nw-create")
                                .label("Create workspace")
                                .primary()
                                .loading(self.creating)
                                .on_click(cx.listener(|this, _, _, cx| this.create(cx))),
                        ),
                    ),
            ),
        )
    }
}

// ---- settings ------------------------------------------------------------

fn preference_toggle(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    checked: bool,
    on_click: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> gpui_kit::gpui::Div {
    let p = palette();
    div()
        .p(px(16.))
        .flex()
        .items_center()
        .gap(px(12.))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(title)
                .child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.muted)
                        .child(description),
                ),
        )
        .child(
            Switch::new(id)
                .accessibility_label(title)
                .checked(checked)
                .on_click(on_click),
        )
}

/// A preference with a few named options, one selected.
fn preference_choice(
    title: &'static str,
    description: &'static str,
    group: ButtonGroup,
) -> gpui_kit::gpui::Div {
    let p = palette();
    div()
        .p(px(16.))
        .flex()
        .items_center()
        .gap(px(12.))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(title)
                .child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.muted)
                        .child(description),
                ),
        )
        .child(group.flex_shrink_0().small().compact())
}

pub struct SettingsScreen {
    core: Core,
}

impl SettingsScreen {
    pub fn new(core: Core, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        cx.observe(&core.nav, |_, _, cx| cx.notify()).detach();
        Self { core }
    }
}

impl Render for SettingsScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let data_dir = crate::app::config::data_dir().display().to_string();
        let client_id = crate::app::config::client_id();
        let hosts = &self.core.store.read(cx).fleet.hosts;
        let row = |label: &str, value: String| {
            div()
                .flex()
                .gap(px(12.))
                .p(px(16.))
                .border_b_1()
                .border_color(p.border)
                .child(
                    div()
                        .w(px(160.))
                        .text_color(p.muted)
                        .child(label.to_string()),
                )
                .child(div().flex_1().text_color(p.foreground).child(value))
        };
        let versions = hosts
            .iter()
            .map(|h| {
                format!(
                    "{}: {}",
                    h.config.name,
                    h.server
                        .as_ref()
                        .map(|s| format!("v{}", s.version))
                        .unwrap_or_else(|| "offline".into())
                )
            })
            .collect::<Vec<_>>()
            .join("   ");
        let nav = self.core.nav.clone();
        let follow = nav.read(cx).follow_relabel;
        let colors = nav.read(cx).colored_lanes;
        let recency = nav.read(cx).recency_headers;
        let box_single = nav.read(cx).box_single_workspaces;
        let colors_nav = nav.clone();
        let recency_nav = nav.clone();
        let box_nav = nav.clone();
        let preferences = card()
            .child(preference_toggle(
                "follow-relabel",
                "Follow cards after relabeling",
                "Move the view to the expanded card’s new lane. Turn off to keep your place.",
                follow,
                move |checked, _, cx| {
                    nav.update(cx, |nav, cx| nav.set_follow_relabel(*checked, cx))
                },
            ))
            .child(preference_toggle(
                "colored-lanes",
                "Colored lanes",
                "Tint label lanes. Click a lane’s color dot to choose from 32 colors.",
                colors,
                move |checked, _, cx| {
                    colors_nav.update(cx, |nav, cx| nav.set_colored_lanes(*checked, cx))
                },
            ))
            .child(preference_toggle(
                "recency-headers",
                "Smart recency headers",
                "Balance recent activity with waiting permissions and errors: blue → green → yellow → red.",
                recency,
                move |checked, _, cx| {
                    recency_nav.update(cx, |nav, cx| nav.set_recency_headers(*checked, cx))
                },
            ))
            .child(preference_toggle(
                "box-single-workspaces",
                "Box single-agent workspaces",
                "In Lanes, outline every workspace. Turn off to box only workspaces with several agents or none.",
                box_single,
                move |checked, _, cx| {
                    box_nav.update(cx, |nav, cx| nav.set_box_single_workspaces(*checked, cx))
                },
            ));
        let (corner, route) = {
            let nav = self.core.nav.read(cx);
            (nav.notification_corner, nav.notification_route)
        };
        let corner_nav = self.core.nav.clone();
        let route_nav = self.core.nav.clone();
        let notifications = card()
            .child(preference_choice(
                "Show notifications",
                "In the app, in the system's notification center, or both.",
                ButtonGroup::new("notification-route")
                    .children(Route::ALL.into_iter().map(|option| {
                        Button::new(option.as_str())
                            .label(option.label())
                            .selected(option == route)
                    }))
                    .on_click(move |indices, _, cx| {
                        if let Some(route) = indices.first().and_then(|ix| Route::ALL.get(*ix)) {
                            route_nav.update(cx, |nav, cx| nav.set_notification_route(*route, cx));
                        }
                    }),
            ))
            .child(preference_choice(
                "Notification corner",
                "Where notifications in the app appear.",
                ButtonGroup::new("notification-corner")
                    .children(Corner::ALL.into_iter().map(|option| {
                        Button::new(option.as_str())
                            .label(option.label())
                            .selected(option == corner)
                    }))
                    .on_click(move |indices, _, cx| {
                        if let Some(corner) = indices.first().and_then(|ix| Corner::ALL.get(*ix)) {
                            corner_nav
                                .update(cx, |nav, cx| nav.set_notification_corner(*corner, cx));
                        }
                    }),
            ));
        page("Settings").child(
            div()
                .id("settings-content")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .restrict_scroll_to_axis()
                .p(px(24.))
                .flex()
                .flex_col()
                .gap(px(16.))
                .child(div().text_color(p.muted).child("Preferences"))
                .child(preferences)
                .child(div().text_color(p.muted).child("Notifications"))
                .child(notifications)
                .child(
                    card()
                        .child(row("Crusteo", format!("v{}", env!("CARGO_PKG_VERSION"))))
                        .child(row("Data directory", data_dir))
                        .child(row("Client id", client_id))
                        .child(row("Daemons", versions)),
                ),
        )
    }
}

// ---- history -----------------------------------------------------------

#[derive(Clone)]
struct HistoryRow {
    host: String,
    agent: fleet::Agent,
    workspace_name: String,
}

/// Every host's agent history, merged newest first. Opening an archived
/// agent restores it first, the way Paseo's "Unarchive" does.
pub struct HistoryScreen {
    core: Core,
    search: Entity<InputState>,
    rows: Vec<HistoryRow>,
    loading: usize,
    generation: u64,
}

impl HistoryScreen {
    pub fn new(core: Core, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search agents, workspaces, projects…")
        });
        cx.subscribe(
            &search,
            |this, _, event: &gpui_kit::component::input::InputEvent, cx| {
                if let gpui_kit::component::input::InputEvent::Change = event {
                    this.reload(cx);
                }
            },
        )
        .detach();
        let mut this = Self {
            core,
            search,
            rows: Vec::new(),
            loading: 0,
            generation: 0,
        };
        this.reload(cx);
        this
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let query = self.search.read(cx).value().trim().to_string();
        let hosts: Vec<String> = self
            .core
            .store
            .read(cx)
            .fleet
            .hosts
            .iter()
            .filter(|h| h.connection.is_online())
            .map(|h| h.config.name.clone())
            .collect();
        self.rows.clear();
        self.loading = hosts.len();
        for host in hosts {
            let mut message = json!({
                "type": "fetch_agent_history_request",
                "sort": [{"key": "updated_at", "direction": "desc"}],
                "page": {"limit": 100},
            });
            if !query.is_empty() {
                message["search"] = json!(query);
            }
            let task = self.core.store.read(cx).request(&host, message, cx);
            cx.spawn(async move |this: WeakEntity<Self>, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.loading = this.loading.saturating_sub(1);
                    if let Ok(payload) = result {
                        for entry in payload["entries"].as_array().into_iter().flatten() {
                            if let Some(agent) = fleet::Agent::from_value(entry["agent"].clone()) {
                                let workspace_name = entry["project"]["workspaceName"]
                                    .as_str()
                                    .or(entry["project"]["projectName"].as_str())
                                    .unwrap_or_default()
                                    .to_string();
                                this.rows.push(HistoryRow {
                                    host: host.clone(),
                                    agent,
                                    workspace_name,
                                });
                            }
                        }
                        this.rows
                            .sort_by(|a, b| b.agent.updated_at.cmp(&a.agent.updated_at));
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn open(&mut self, row: HistoryRow, cx: &mut Context<Self>) {
        let Some(workspace) = row.agent.workspace_id.clone() else {
            return;
        };
        let nav = self.core.nav.clone();
        let key = format!("{}:{workspace}", row.host);
        let agent_id = row.agent.id.clone();
        let host = row.host.clone();
        let go = move |cx: &mut gpui_kit::gpui::App| {
            nav.update(cx, |nav, cx| {
                nav.select(&host, &workspace, cx);
                nav.focus_tab(&key, Tab::Agent(agent_id.clone()), cx);
            });
        };
        if row.agent.archived_at.is_none() {
            go(cx);
            return;
        }
        let task = self.core.store.read(cx).request(
            &row.host,
            json!({"type": "refresh_agent_request", "agentId": row.agent.id}),
            cx,
        );
        cx.spawn(async move |_, cx| {
            if task.await.is_ok() {
                cx.update(|cx| go(cx));
            }
        })
        .detach();
    }
}

impl Render for HistoryScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let multi = self.core.store.read(cx).fleet.hosts.len() > 1;
        let mut list = div()
            .id("history-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for (i, row) in self.rows.clone().into_iter().enumerate() {
            let archived = row.agent.archived_at.is_some();
            let subtitle = [
                multi.then(|| row.host.clone()),
                Some(row.workspace_name.clone()).filter(|s| !s.is_empty()),
                Some(row.agent.provider_id().to_string()),
                row.agent
                    .updated_at
                    .as_deref()
                    .map(crate::app::store::time_ago),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            let clicked = row.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("history-{i}")))
                    .px(px(24.))
                    .py(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .border_b_1()
                    .border_color(p.border)
                    .hover(|s| s.bg(p.surface1))
                    .child(ui::agent_dot(if archived {
                        fleet::Bucket::Done
                    } else {
                        row.agent.bucket()
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_color(p.foreground)
                                    .child(row.agent.display_title()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(size::sm())
                                    .text_color(p.muted)
                                    .child(subtitle),
                            ),
                    )
                    .when(archived, |d| {
                        let restore = clicked.clone();
                        d.child(
                            div()
                                .px(px(6.))
                                .rounded(px(4.))
                                .bg(p.surface2)
                                .text_size(size::sm())
                                .text_color(p.muted)
                                .child("archived"),
                        )
                        .child(
                            Button::new(SharedString::from(format!("restore-{i}")))
                                .label("Restore")
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open(restore.clone(), cx)
                                })),
                        )
                    })
                    .when(!archived, |d| {
                        d.on_click(cx.listener(
                            move |this, _: &gpui_kit::gpui::ClickEvent, _, cx| {
                                this.open(clicked.clone(), cx)
                            },
                        ))
                    }),
            );
        }
        let status = if self.loading > 0 {
            "Loading…".to_string()
        } else {
            format!("{} agents", self.rows.len())
        };
        page("History")
            .child(
                div()
                    .px(px(24.))
                    .py(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .border_b_1()
                    .border_color(p.border)
                    .child(div().flex_1().child(Input::new(&self.search)))
                    .child(
                        div()
                            .text_size(size::sm())
                            .text_color(p.muted)
                            .child(status),
                    ),
            )
            .child(list)
    }
}
