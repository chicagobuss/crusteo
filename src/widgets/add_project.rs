//! Add project: pick a host, then find a folder on it, by name under the
//! daemon's home or by path, and register it as a project. As in Paseo's
//! Add Project, the daemon searches; crusteo only asks, a moment after
//! typing stops.
//!
//! A folder that is not a git repository becomes a project too, as Paseo
//! makes it: its workspaces open in the folder, and worktrees wait for a
//! `git init` there, which the daemon notices by itself.

use fleet::HostId;
use std::rc::Rc;
use std::time::Duration;

use fleet::creation::{self, Support};
use fleet::requests;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{Sizable, WindowExt};
use gpui_kit::gpui::{
    App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;
use serde_json::json;

use crate::app::Core;
use crate::theme::{palette, size};
use crate::ui;

gpui_kit::actions!(
    add_project,
    [NextFolder, PreviousFolder, CompleteFolder, AddFolder]
);

pub fn bind_keys(cx: &mut App) {
    use gpui_kit::gpui::KeyBinding;
    let input = Some("AddProject > Input");
    cx.bind_keys([
        KeyBinding::new("down", NextFolder, input),
        KeyBinding::new("up", PreviousFolder, input),
        KeyBinding::new("tab", CompleteFolder, input),
        // Its own, or the dialog's Enter would close it before the host
        // answers.
        KeyBinding::new("enter", AddFolder, input),
    ]);
}

/// Called with the host and project id once the project exists.
type OnAdded = Rc<dyn Fn(&HostId, &str, &mut Window, &mut App)>;

/// Opens Add project in a dialog, on `host` if given, else the last host
/// a project was added on, else the first online one.
pub fn open(
    core: &Core,
    host: Option<HostId>,
    window: &mut Window,
    cx: &mut App,
    on_added: impl Fn(&HostId, &str, &mut Window, &mut App) + 'static,
) {
    let on_added: OnAdded = Rc::new(on_added);
    let view = cx.new(|cx| AddProject::new(core.clone(), host, on_added, window, cx));
    {
        // The dialog takes focus when it opens; the search wants it.
        let view = view.clone();
        window.defer(cx, move |window, cx| {
            let input = view.read(cx).input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
        });
    }
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title("Add project").w(px(560.)).child(view.clone())
    });
}

pub struct AddProject {
    core: Core,
    host: Option<HostId>,
    input: Entity<InputState>,
    folders: Vec<String>,
    /// The folder the arrows picked; none means what is typed.
    highlighted: Option<usize>,
    searching: bool,
    /// Why there is nothing to list, such as a host that cannot search.
    note: Option<String>,
    /// Bumped per keystroke; a search answers only if still the newest.
    generation: u64,
    adding: bool,
    error: Option<String>,
    scroll: ScrollHandle,
    on_added: OnAdded,
    /// A step of another dialog (New agent's New project): it leaves that
    /// dialog open, and has no Cancel of its own.
    embedded: bool,
}

impl AddProject {
    /// Add project as a step of another dialog, on `host` if given.
    pub fn embedded(
        core: Core,
        host: Option<HostId>,
        on_added: impl Fn(&HostId, &str, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            embedded: true,
            ..Self::new(core, host, Rc::new(on_added), window, cx)
        }
    }

    /// The host whose folders it searches.
    pub fn host(&self) -> Option<&HostId> {
        self.host.as_ref()
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn new(
        core: Core,
        host: Option<HostId>,
        on_added: OnAdded,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Folder name, or a path like ~/code/app")
        });
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.highlighted = None;
                this.error = None;
                this.search(cx);
            }
        })
        .detach();
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        let online = |id: &HostId| {
            (core.store.read(cx).fleet.host(id)).is_some_and(|h| h.connection.is_online())
        };
        let host = host
            .filter(|h| online(h))
            .or_else(|| {
                core.nav
                    .read(cx)
                    .add_project_host
                    .clone()
                    .filter(|h| online(h))
            })
            .or_else(|| {
                (core.store.read(cx).fleet.hosts.iter())
                    .find(|h| h.connection.is_online())
                    .map(|h| h.id().clone())
            });
        Self {
            core,
            host,
            input,
            folders: Vec::new(),
            highlighted: None,
            searching: false,
            note: None,
            generation: 0,
            adding: false,
            error: None,
            scroll: ScrollHandle::new(),
            on_added,
            embedded: false,
        }
    }

    fn set_host(&mut self, host: HostId, cx: &mut Context<Self>) {
        self.host = Some(host);
        self.folders.clear();
        self.highlighted = None;
        self.error = None;
        self.search(cx);
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let query = self.input.read(cx).value().trim().to_string();
        let Some(host) = self.host.clone() else {
            return;
        };
        if query.is_empty() {
            self.folders.clear();
            self.searching = false;
            self.note = None;
            cx.notify();
            return;
        }
        self.searching = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let task = this.update(cx, |this, cx| {
                (this.generation == generation).then(|| {
                    (this.core.store.read(cx)).request(
                        &host,
                        requests::directory_suggestions(&query, 30),
                        cx,
                    )
                })
            });
            let Ok(Some(task)) = task else {
                return;
            };
            let result = task
                .await
                .and_then(|payload| creation::parse_directories(&payload));
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.searching = false;
                match result {
                    Ok(folders) => {
                        this.note = folders
                            .is_empty()
                            .then(|| "No folder matches; Enter adds the path as typed".into());
                        this.folders = folders;
                    }
                    Err(error) => {
                        this.folders.clear();
                        this.note = Some(if creation::unsupported(&error) {
                            "This host cannot search its folders; type the full path".into()
                        } else {
                            error
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = self.folders.len();
        if count == 0 {
            return;
        }
        let next = match (self.highlighted, forward) {
            (None, true) => Some(0),
            (None, false) => None,
            (Some(ix), true) => Some((ix + 1).min(count - 1)),
            // Up from the first row goes back to what is typed.
            (Some(0), false) => None,
            (Some(ix), false) => Some(ix - 1),
        };
        self.highlighted = next;
        if let Some(ix) = next {
            self.scroll.scroll_to_item(ix);
        }
        cx.notify();
    }

    /// Tab: the picked (or first) folder into the box, to look inside it.
    fn complete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self
            .highlighted
            .or((!self.folders.is_empty()).then_some(0))
            .and_then(|ix| self.folders.get(ix))
            .cloned()
        else {
            return;
        };
        let text = format!("{}/", folder.trim_end_matches('/'));
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.highlighted = None;
        self.search(cx);
    }

    /// The folder Enter adds: the one picked, else a path as typed, else
    /// the first match of a name.
    fn target(&self, cx: &App) -> Option<String> {
        if let Some(folder) = self.highlighted.and_then(|ix| self.folders.get(ix)) {
            return Some(folder.clone());
        }
        let typed = self.input.read(cx).value().trim().to_string();
        let path_like = typed.starts_with(['/', '~'])
            || typed.chars().nth(1) == Some(':')
            || typed.starts_with('\\');
        if path_like {
            let trimmed = typed.trim_end_matches(['/', '\\']);
            return Some(if trimmed.is_empty() {
                typed
            } else {
                trimmed.to_string()
            });
        }
        self.folders.first().cloned()
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.adding {
            return;
        }
        let Some(host) = self.host.clone() else {
            self.error = Some("No host is online".into());
            cx.notify();
            return;
        };
        let Some(path) = self.target(cx) else {
            self.error = Some("Pick a folder, or type its full path".into());
            cx.notify();
            return;
        };
        let (support, known) = {
            let store = self.core.store.read(cx);
            let record = store.fleet.host(&host);
            let known = record.and_then(|record| {
                (record.directory.projects.values())
                    .find(|project| project.project_root_path.trim_end_matches('/') == path)
                    .map(|project| project.project_id.clone())
            });
            (record.map(Support::for_host).unwrap_or_default(), known)
        };
        if let Some(project) = known {
            // Already one: pick it rather than ask again.
            return self.finish(&host, &project, None, window, cx);
        }
        self.adding = true;
        self.error = None;
        cx.notify();
        let message = if support.add_project {
            requests::add_project(&path)
        } else {
            // Without project.add, a workspace in the folder registers it.
            json!({
                "type": "workspace.create.request",
                "idempotencyKey": crate::app::store::uuid_v4(),
                "source": { "kind": "directory", "path": path },
            })
        };
        let task = self.core.store.read(cx).write(&host, message, cx);
        cx.spawn_in(window, async move |this, cx| {
            let reply = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.adding = false;
                let payload = match reply {
                    Ok(payload) => payload,
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                        return;
                    }
                };
                if let Some(error) = payload["error"].as_str() {
                    this.error = Some(if payload["errorCode"] == "directory_not_found" {
                        let name = this.core.store.read(cx).fleet.host_name(&host).to_string();
                        format!("{name} has no folder {path}")
                    } else {
                        error.to_string()
                    });
                    cx.notify();
                    return;
                }
                let (project, kind) = if support.add_project {
                    let project = &payload["project"];
                    this.core
                        .store
                        .update(cx, |store, cx| store.upsert_project(&host, project, cx));
                    (project["projectId"].as_str(), project["projectKind"].as_str())
                } else {
                    let workspace = &payload["workspace"];
                    this.core
                        .store
                        .update(cx, |store, cx| store.upsert_workspace(&host, workspace, cx));
                    (workspace["projectId"].as_str(), workspace["projectKind"].as_str())
                };
                let Some(project) = project.map(str::to_string) else {
                    this.error = Some("The host did not return the project".into());
                    cx.notify();
                    return;
                };
                let note = match kind {
                    Some("git") | None => None,
                    _ => Some(format!(
                        "{path} is not a git repository: its workspaces open in the folder. Run git init there for worktrees."
                    )),
                };
                let note = note.or_else(|| {
                    (!support.add_project).then(|| {
                        let name = this.core.store.read(cx).fleet.host_name(&host).to_string();
                        format!("{name} cannot add projects, so a workspace was made in {path}")
                    })
                });
                this.finish(&host, &project, note, window, cx);
            });
        })
        .detach();
    }

    fn finish(
        &mut self,
        host: &HostId,
        project: &str,
        note: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.core
            .nav
            .update(cx, |nav, cx| nav.set_add_project_host(host, cx));
        if let Some(note) = note {
            self.core
                .store
                .update(cx, |store, cx| store.toast(note, cx));
        }
        if !self.embedded {
            window.close_dialog(cx);
        }
        (self.on_added)(host, project, window, cx);
    }
}

impl Render for AddProject {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let this = cx.entity().downgrade();
        let store = self.core.store.read(cx);
        let online: Vec<(HostId, String)> = (store.fleet.hosts.iter())
            .filter(|h| h.connection.is_online())
            .map(|h| {
                let version = (h.server.as_ref())
                    .map(|s| s.version.clone())
                    .filter(|v| !v.is_empty())
                    .map(|v| format!(" · v{v}"))
                    .unwrap_or_default();
                (h.id().clone(), format!("{}{version}", h.name()))
            })
            .collect();
        let roots: Vec<String> = (self.host.as_ref())
            .and_then(|host| store.fleet.host(host))
            .map(|h| {
                (h.directory.projects.values())
                    .map(|project| project.project_root_path.trim_end_matches('/').to_string())
                    .collect()
            })
            .unwrap_or_default();
        let host_menu = {
            let online = online.clone();
            Button::new("add-project-host")
                .ghost()
                .small()
                .child(div().text_color(p.muted).child("Host"))
                .child(match &self.host {
                    Some(host) => store.fleet.host_name(host).to_string(),
                    None => "No host online".into(),
                })
                .child(ui::icon("chevron-down", 12., p.muted))
                .dropdown_menu(move |mut menu, _, _| {
                    for (name, label) in &online {
                        let (this, name) = (this.clone(), name.clone());
                        menu = menu.item(PopupMenuItem::new(label.clone()).on_click(
                            move |_, _, cx| {
                                let name = name.clone();
                                let _ = this.update(cx, |this, cx| this.set_host(name, cx));
                            },
                        ));
                    }
                    menu.max_h(px(320.))
                })
        };
        let highlighted = self.highlighted;
        let rows = self.folders.iter().enumerate().map(|(ix, folder)| {
            let (parent, name) = match folder.trim_end_matches('/').rsplit_once('/') {
                Some((parent, name)) => (format!("{parent}/"), name.to_string()),
                None => (String::new(), folder.clone()),
            };
            let known = roots
                .iter()
                .any(|root| root == folder.trim_end_matches('/'));
            let folder = folder.clone();
            div()
                .id(SharedString::from(format!("add-project-row-{ix}")))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .py(px(4.))
                .rounded(px(4.))
                .cursor_pointer()
                .when(highlighted == Some(ix), |d| d.bg(p.surface2))
                .hover(|d| d.bg(p.surface2))
                .child(ui::icon("folder", 14., p.muted))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .truncate()
                        .child(div().text_color(p.muted).child(parent))
                        .child(div().text_color(p.foreground).child(name)),
                )
                .when(known, |d| {
                    d.child(
                        div()
                            .text_size(size::sm())
                            .text_color(p.extra_muted)
                            .child("project"),
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.highlighted = this.folders.iter().position(|f| *f == folder);
                    this.add(window, cx);
                }))
        });
        div()
            .key_context("AddProject")
            .on_action(cx.listener(|this, _: &NextFolder, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousFolder, _, cx| this.step(false, cx)))
            .on_action(cx.listener(|this, _: &CompleteFolder, window, cx| {
                this.complete(window, cx)
            }))
            .on_action(cx.listener(|this, _: &AddFolder, window, cx| this.add(window, cx)))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(host_menu)
                    .child(
                        div()
                            .text_size(size::sm())
                            .text_color(p.extra_muted)
                            .child("The folder is on this host"),
                    ),
            )
            .child(Input::new(&self.input))
            .child(
                div()
                    .id("add-project-folders")
                    .h(px(if self.embedded { 160. } else { 220. }))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .flex()
                    .flex_col()
                    .children(rows)
                    .when(self.folders.is_empty(), |d| {
                        d.child(
                            div()
                                .p(px(8.))
                                .text_size(size::sm())
                                .text_color(p.extra_muted)
                                .child(if self.searching {
                                    "Searching…".to_string()
                                } else {
                                    self.note.clone().unwrap_or_else(|| {
                                        "Type a folder's name to search the host's home, or a path to browse it".into()
                                    })
                                }),
                        )
                    }),
            )
            .when_some(self.error.clone(), |d, error| {
                d.child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.status_danger)
                        .child(error),
                )
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
                            .child("↑↓ pick · Tab looks inside · Enter adds"),
                    )
                    .when(!self.embedded, |row| {
                        row.child(
                            Button::new("add-project-cancel")
                                .label("Cancel")
                                .ghost()
                                .small()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                    })
                    .child(
                        Button::new("add-project-add")
                            .label("Add project")
                            .when(!self.embedded, |b| b.primary())
                            .when(self.embedded, |b| b.outline())
                            .small()
                            .loading(self.adding)
                            .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                    ),
            )
    }
}
