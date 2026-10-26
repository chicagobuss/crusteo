//! Full-pane screens reached from the sidebar: hosts, new workspace, settings.

use fleet::providers::Provider;
use fleet::{Connection, HostConfig, HostId};
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants};
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::switch::Switch;
use gpui_kit::gpui::{
    App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;
use serde_json::json;

use crate::app::notifications::{Corner, Route};
use crate::app::store::ProvidersState;
use crate::app::{Core, Screen, Tab};
use crate::theme::{palette, size};
use crate::ui;
use crate::widgets::workspace_form::{FormChanged, FormPicked, FormSubmit, WorkspaceForm};

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
        // A name already saved edits that host, keeping its id; a new one
        // gets an id when saved.
        let id =
            (hosts.iter().find(|h| h.name == name)).map_or_else(HostId::default, |h| h.id.clone());
        hosts.retain(|h| h.name != name);
        hosts.push(HostConfig {
            id,
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
        let reef = self.core.store.read(cx).reef.clone();
        let hosts = &self.core.store.read(cx).fleet.hosts;
        let mut list = card();
        for (i, host) in hosts.iter().enumerate() {
            let name = host.id().clone();
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
                    .when(reef.is_none(), |row| {
                        row.child({
                            let remove = host.id().clone();
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
                                        .filter(|h| h.id != remove)
                                        .collect();
                                    core_remove
                                        .store
                                        .update(cx, |store, cx| store.set_hosts(hosts, cx));
                                })
                        })
                    }),
            );
        }
        // In reef mode the list is Reef's: say whose, and how the last ask went.
        let intro = match &reef {
            None => "Each host is a Paseo daemon. Crusteo keeps a live connection to all of them at once.".to_string(),
            Some(reef) => {
                let synced = reef.synced_at.map_or_else(
                    || "not yet".to_string(),
                    |at| crate::app::store::time_ago(&crate::app::store::iso_from_millis(at)),
                );
                match &reef.error {
                    Some(error) => format!("Hosts come from Reef at {}, which did not answer: {error}. These are the hosts it gave last ({synced}).", reef.url),
                    None => format!("Hosts come from Reef at {} (last asked {synced}). Add or remove them there; this desktop connects to each daemon itself.", reef.url),
                }
            }
        };
        page("Hosts").child(
            div()
                .id("hosts-scroll")
                .flex_1()
                .overflow_y_scroll()
                .p(px(24.))
                .flex()
                .flex_col()
                .gap(px(16.))
                .child(div().text_size(size::sm()).text_color(p.muted).child(intro))
                .child(list)
                .when(reef.is_none(), |page| {
                    page.child(
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
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.add(window, cx)
                                            })),
                                    ),
                            ),
                    )
                }),
        )
    }
}

// ---- new workspace -------------------------------------------------------

pub struct NewWorkspaceScreen {
    core: Core,
    form: Entity<WorkspaceForm>,
    provider: Option<String>,
    prompt: Entity<TextareaState>,
    creating: bool,
    error: Option<String>,
}

/// The project a New workspace screen opens on: the one asked for, else,
/// on the host asked for, the selected workspace's.
fn screen_target(core: &Core, cx: &App) -> (Option<HostId>, Option<String>) {
    let Screen::NewWorkspace { host, project } = core.nav.read(cx).screen.clone() else {
        return (None, None);
    };
    if project.is_some() {
        return (host, project);
    }
    let selected = core.nav.read(cx).selection.clone().and_then(|selection| {
        let workspace =
            (core.store.read(cx).fleet).workspace(&selection.host, &selection.workspace)?;
        Some((selection.host.clone(), workspace.project_id.clone()))
    });
    match (host, selected) {
        (Some(host), Some((on, project))) if host == on => (Some(host), Some(project)),
        (None, Some((on, project))) => (Some(on), Some(project)),
        (host, _) => (host, None),
    }
}

impl NewWorkspaceScreen {
    pub fn new(core: Core, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        cx.observe(&core.nav, |this, _, cx| {
            // Opened again for another project, from a project's "+".
            let (host, project) = screen_target(&this.core, cx);
            if let (Some(host), Some(project)) = (host, project) {
                let form = this.form.read(cx);
                if form.host() != Some(&host) || form.project() != Some(project.as_str()) {
                    this.form
                        .update(cx, |form, cx| form.set_project(&host, &project, cx));
                }
            }
        })
        .detach();
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 12)
                .submit_on_enter(true)
                .placeholder("Describe the work for the first agent, or leave it blank")
        });
        let (host, project) = screen_target(&core, cx);
        let form = {
            let core = core.clone();
            cx.new(|cx| WorkspaceForm::new(core, host, project, Vec::new(), window, cx))
        };
        cx.subscribe(&form, |this, form, _: &FormChanged, cx| {
            if let Some(host) = form.read(cx).host().cloned() {
                this.core
                    .store
                    .update(cx, |store, cx| store.ensure_providers(&host, cx));
            }
            this.error = None;
            cx.notify();
        })
        .detach();
        cx.subscribe_in(&form, window, |this, _, _: &FormPicked, window, cx| {
            this.prompt.update(cx, |input, cx| input.focus(window, cx));
        })
        .detach();
        cx.subscribe(&form, |this, _, _: &FormSubmit, cx| this.create(cx))
            .detach();
        cx.subscribe(
            &prompt,
            |this, prompt, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    let text = prompt.read(cx).value().to_string();
                    this.form.update(cx, |form, cx| form.set_prompt(&text, cx));
                }
                InputEvent::PressEnter { shift: false, .. } => this.create(cx),
                _ => {}
            },
        )
        .detach();
        if let Some(host) = form.read(cx).host().cloned() {
            core.store
                .update(cx, |store, cx| store.ensure_providers(&host, cx));
        }
        Self {
            core,
            form,
            provider: None,
            prompt,
            creating: false,
            error: None,
        }
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        let plan = match self.form.read(cx).plan(cx) {
            Ok(plan) => plan,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let prompt = self.prompt.read(cx).value().trim().to_string();
        let agent = if prompt.is_empty() {
            None
        } else {
            let store = self.core.store.read(cx);
            let providers: Vec<Provider> = (store.providers.get(&plan.host).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter(Provider::ready)
                .collect();
            let provider = (self.provider.as_ref())
                .and_then(|id| providers.iter().find(|p| &p.id == id))
                .or_else(|| providers.iter().find(|p| p.id == "claude"))
                .or_else(|| providers.first());
            let Some(provider) = provider else {
                self.error = Some(match store.providers_state(&plan.host) {
                    ProvidersState::Ready => format!("No ready providers on {}", plan.host_name),
                    state => format!("{}: {}", plan.host_name, state.label()),
                });
                cx.notify();
                return;
            };
            let model = provider.default_model();
            let config = fleet::creation::agent_config(
                provider,
                model.map(|m| m.id.as_str()),
                model.and_then(|m| m.default_thinking.as_deref()),
                None,
                &plan.root,
            );
            Some((
                config,
                fleet::prompt::Prompt {
                    text: prompt,
                    ..Default::default()
                },
            ))
        };
        self.creating = true;
        self.error = None;
        cx.notify();
        let task = self.core.store.update(cx, |store, cx| {
            store.create_workspace(&plan, agent, None, cx)
        });
        let (nav, host) = (self.core.nav.clone(), plan.host.clone());
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.creating = false;
                match result {
                    Ok(created) => {
                        this.form.update(cx, |form, cx| form.remember(cx));
                        let workspace = created.workspace.clone();
                        nav.update(cx, |nav, cx| {
                            nav.select(&host, &workspace, cx);
                            if let Ok(Some(agent)) = &created.agent {
                                nav.focus_tab(
                                    &format!("{host}:{workspace}"),
                                    Tab::Agent(agent.clone()),
                                    cx,
                                );
                            }
                        });
                        if let Err(error) = created.agent {
                            this.core.store.update(cx, |store, cx| {
                                store.toast(
                                    format!("The workspace is made, but its agent did not start: {error}"),
                                    cx,
                                )
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
        let host = self.form.read(cx).host().cloned();
        let store = self.core.store.read(cx);
        let providers: Vec<Provider> = host
            .as_ref()
            .and_then(|h| store.providers.get(h).cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(Provider::ready)
            .collect();
        let state = host.as_ref().map(|h| store.providers_state(h));
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
            .unwrap_or_else(|| {
                state
                    .as_ref()
                    .map_or("Provider", ProvidersState::label)
                    .to_string()
            });

        let this = cx.entity().downgrade();
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

        page("New workspace").child(
            div().flex_1().flex().justify_center().p(px(24.)).child(
                div()
                    .w_full()
                    .max_w(px(size::COMPOSER_MAX_WIDTH))
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .child(self.form.clone())
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
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(size::sm())
                                    .text_color(p.extra_muted)
                                    .child("Enter creates · Shift+Enter for a new line · a blank prompt makes the workspace alone"),
                            )
                            .child(
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
    preference_row(title, description, group.small().compact())
}

/// A preference's title and description, with the control that sets it.
fn preference_row(
    title: &'static str,
    description: &'static str,
    control: impl IntoElement,
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
        .child(div().flex_shrink_0().child(control))
}

pub struct SettingsScreen {
    core: Core,
    /// The window border's color, chosen with a picker.
    border_color: Entity<ColorPickerState>,
    /// How long an agent's notification stays up, 0 until closed.
    notification_seconds: Entity<SliderState>,
}

/// Bright colors offered first for the window border.
const BORDER_COLORS: [u32; 10] = [
    0x3b9eff, 0x00d4ff, 0x30d158, 0xffd60a, 0xff9f0a, 0xff453a, 0xff375f, 0xbf5af2, 0x5e5ce6,
    0xffffff,
];

impl SettingsScreen {
    pub fn new(core: Core, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        cx.observe(&core.nav, |_, _, cx| cx.notify()).detach();
        let current = crate::app::nav::parse_hex(&core.nav.read(cx).window_border_color)
            .unwrap_or(BORDER_COLORS[0]);
        let border_color = cx.new(|cx| {
            ColorPickerState::new(window, cx).default_value(gpui_kit::gpui::rgb(current))
        });
        let nav = core.nav.clone();
        cx.subscribe(&border_color, move |_, _, event: &ColorPickerEvent, cx| {
            let ColorPickerEvent::Change(Some(color)) = event else {
                return;
            };
            let rgba = gpui_kit::gpui::Rgba::from(*color);
            let byte = |channel: f32| (channel.clamp(0., 1.) * 255.).round() as u8;
            let hex = format!(
                "#{:02x}{:02x}{:02x}",
                byte(rgba.r),
                byte(rgba.g),
                byte(rgba.b)
            );
            nav.update(cx, |nav, cx| nav.set_window_border_color(&hex, cx));
        })
        .detach();
        let seconds = core.nav.read(cx).notification_seconds;
        let notification_seconds = cx.new(|_| {
            // Max first: `min` clamps against the current max.
            SliderState::new()
                .max(crate::app::notifications::SECONDS_MAX)
                .min(0.)
                .step(0.5)
                .default_value(seconds)
        });
        let nav = core.nav.clone();
        cx.subscribe(
            &notification_seconds,
            move |_, _, event: &SliderEvent, cx| {
                let (value, persist) = match event {
                    SliderEvent::Change(value) => (value.start(), false),
                    SliderEvent::Release(value) => (value.start(), true),
                };
                nav.update(cx, |nav, cx| {
                    nav.set_notification_seconds(value, persist, cx)
                });
            },
        )
        .detach();
        Self {
            core,
            border_color,
            notification_seconds,
        }
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
        let show_background = nav.read(cx).show_background();
        let colors_nav = nav.clone();
        let recency_nav = nav.clone();
        let box_nav = nav.clone();
        let background_nav = nav.clone();
        let preferences = card()
            .child(preference_toggle(
                "show-background",
                "Show background",
                "List background workspaces, which Paseo 0.12 keeps out of its sidebar, and their agents in every view, tagged “background”.",
                show_background,
                move |checked, _, cx| {
                    background_nav.update(cx, |nav, cx| nav.set_show_background(*checked, cx))
                },
            ))
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
        let flash = self.core.nav.read(cx).notification_flash;
        let flash_nav = self.core.nav.clone();
        let (seconds, layout, keep) = {
            let nav = self.core.nav.read(cx);
            (
                nav.notification_seconds,
                nav.notification_layout,
                nav.notification_keep,
            )
        };
        let (layout_nav, keep_nav) = (self.core.nav.clone(), self.core.nav.clone());
        let stay_open = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .child(
                div()
                    .w(px(220.))
                    .child(Slider::new(&self.notification_seconds)),
            )
            .child(
                div()
                    .w(px(90.))
                    .text_color(p.foreground)
                    .child(crate::app::notifications::seconds_label(seconds)),
            );
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
            ))
            .child(preference_row(
                "Stay open",
                "How long an agent's notification stays up, all the way down to until you close it. Hovering one pauses it.",
                stay_open,
            ))
            .child(preference_choice(
                "When several arrive",
                "A compact stack that fans out on hover, a list of each in full that never runs past the window, or only the newest.",
                ButtonGroup::new("notification-layout")
                    .children(crate::app::notifications::Layout::ALL.into_iter().map(|option| {
                        Button::new(SharedString::from(format!("notification-layout-{}", option.as_str())))
                            .label(option.label())
                            .selected(option == layout)
                    }))
                    .on_click(move |indices, _, cx| {
                        if let Some(&layout) = indices
                            .first()
                            .and_then(|ix| crate::app::notifications::Layout::ALL.get(*ix))
                        {
                            layout_nav.update(cx, |nav, cx| nav.set_notification_layout(layout, cx));
                        }
                    }),
            ))
            .child(preference_choice(
                "Keep at most",
                "How many stay up at once; a new one past that closes the oldest.",
                ButtonGroup::new("notification-keep")
                    .children(crate::app::notifications::KEEP_CHOICES.into_iter().map(|count| {
                        Button::new(SharedString::from(format!("notification-keep-{count}")))
                            .label(count.to_string())
                            .selected(count == keep)
                            .disabled(layout == crate::app::notifications::Layout::Newest)
                    }))
                    .on_click(move |indices, _, cx| {
                        if let Some(&count) =
                            indices.first().and_then(|ix| crate::app::notifications::KEEP_CHOICES.get(*ix))
                        {
                            keep_nav.update(cx, |nav, cx| nav.set_notification_keep(count, cx));
                        }
                    }),
            ))
            .child(preference_choice(
                "Flash the card",
                "In Lanes, how long a card opened from a notification blinks its outline, so it's easy to find.",
                ButtonGroup::new("notification-flash")
                    .children(crate::app::nav::NOTIFICATION_FLASH_CHOICES.into_iter().map(
                        |seconds| {
                            Button::new(SharedString::from(format!("notification-flash-{seconds}")))
                                .label(if seconds == 0 {
                                    "Off".to_string()
                                } else {
                                    format!("{seconds} s")
                                })
                                .selected(seconds == flash)
                        },
                    ))
                    .on_click(move |indices, _, cx| {
                        if let Some(&seconds) = indices
                            .first()
                            .and_then(|ix| crate::app::nav::NOTIFICATION_FLASH_CHOICES.get(*ix))
                        {
                            flash_nav.update(cx, |nav, cx| nav.set_notification_flash(seconds, cx));
                        }
                    }),
            ));
        let border = self.core.nav.read(cx).window_border;
        let border_nav = self.core.nav.clone();
        let window_card = card()
            .child(preference_choice(
                "Window border",
                "A solid line around the window's edge, to tell it apart from the windows beside it.",
                ButtonGroup::new("window-border")
                    .children((0..=crate::app::nav::WINDOW_BORDER_MAX).map(|width| {
                        Button::new(SharedString::from(format!("window-border-{width}")))
                            .label(if width == 0 {
                                "Off".to_string()
                            } else {
                                width.to_string()
                            })
                            .selected(width == border)
                    }))
                    .on_click(move |indices, _, cx| {
                        if let Some(&width) = indices.first() {
                            border_nav.update(cx, |nav, cx| nav.set_window_border(width as u8, cx));
                        }
                    }),
            ))
            .child(preference_row(
                "Border color",
                "Pick one, or enter any color as hex.",
                ColorPicker::new(&self.border_color)
                    .featured_colors(
                        BORDER_COLORS
                            .iter()
                            .map(|&color| gpui_kit::gpui::rgb(color).into())
                            .collect(),
                    )
                    .anchor(gpui_kit::gpui::Anchor::TopRight),
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
                .child(div().text_color(p.muted).child("Window"))
                .child(window_card)
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
    host: HostId,
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
        let hosts: Vec<HostId> = self
            .core
            .store
            .read(cx)
            .fleet
            .hosts
            .iter()
            .filter(|h| h.connection.is_online())
            .map(|h| h.id().clone())
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
                multi.then(|| {
                    self.core
                        .store
                        .read(cx)
                        .fleet
                        .host_name(&row.host)
                        .to_string()
                }),
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
