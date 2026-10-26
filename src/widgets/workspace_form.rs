//! Where a new workspace goes and how it is made: a project on its own
//! host, its folder or a new worktree, the branch, and what the worktree
//! starts from (a branch or a pull request). One line under it says what
//! will be made. Lanes' draft and the New workspace screen both use it.

use fleet::HostId;
use std::time::Duration;

use fleet::creation::{
    self, BranchRef, ChangeRequest, Isolation, Plan, Remembered, StartRef, Support,
};
use fleet::requests;
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::gpui::{
    App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;

use crate::app::Core;
use crate::theme::{palette, size};
use crate::ui;

gpui_kit::actions!(workspace_form, [NextRef, PreviousRef, CloseRefs]);

pub fn bind_keys(cx: &mut App) {
    use gpui_kit::gpui::KeyBinding;
    let picker = Some("RefPicker > Input");
    cx.bind_keys([
        KeyBinding::new("down", NextRef, picker),
        KeyBinding::new("up", PreviousRef, picker),
        KeyBinding::new("escape", CloseRefs, picker),
    ]);
}

/// The form changed: the owner redraws, and checks the host's providers.
pub struct FormChanged;

/// The starting-point picker closed: the owner takes focus back, to its
/// prompt, so the keyboard goes on from there.
pub struct FormPicked;

/// What the starting-point picker lists.
struct RefPicker {
    input: Entity<InputState>,
    branches: Vec<BranchRef>,
    changes: Vec<ChangeRequest>,
    /// Why some of the list is missing, such as a signed-out forge CLI.
    note: Option<String>,
    loading: bool,
    highlighted: usize,
    /// Bumped per keystroke; a search answers only if still the newest.
    generation: u64,
    scroll: ScrollHandle,
}

impl RefPicker {
    fn rows(&self, query: &str) -> Vec<StartRef> {
        let mut rows = Vec::new();
        if query.trim().is_empty() {
            rows.push(StartRef::Default);
        }
        rows.extend(self.branches.iter().flat_map(BranchRef::starts));
        rows.extend(self.changes.iter().cloned().map(StartRef::ChangeRequest));
        rows
    }
}

pub struct WorkspaceForm {
    core: Core,
    host: Option<HostId>,
    project: Option<String>,
    /// Projects listed first, as host and project: a lane's own.
    preferred: Vec<(HostId, String)>,
    isolation: Isolation,
    start: StartRef,
    branch: Entity<InputState>,
    /// The first agent's prompt, which names an untyped branch.
    prompt: String,
    seed: u64,
    /// The repository's branches, for naming and for spotting an existing
    /// one; with the host and project they are for.
    branches: Vec<BranchRef>,
    branches_for: Option<(HostId, String)>,
    picker: Option<RefPicker>,
    /// Bumped on every change, for views that cache what they drew.
    pub generation: u64,
    /// Whether [`Self::plan`] last succeeded, to tell owners when it flips.
    ready: bool,
}

/// Enter in the branch box: the owner creates, as Enter in its prompt does.
pub struct FormSubmit;

impl EventEmitter<FormChanged> for WorkspaceForm {}
impl EventEmitter<FormPicked> for WorkspaceForm {}
impl EventEmitter<FormSubmit> for WorkspaceForm {}

impl WorkspaceForm {
    pub fn new(
        core: Core,
        host: Option<HostId>,
        project: Option<String>,
        preferred: Vec<(HostId, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch = cx.new(|cx| InputState::new(window, cx).placeholder("Branch (optional)"));
        cx.subscribe(
            &branch,
            |this: &mut Self, _, event: &InputEvent, cx| match event {
                InputEvent::Change => this.changed(cx),
                InputEvent::PressEnter { .. } => cx.emit(FormSubmit),
                _ => {}
            },
        )
        .detach();
        cx.observe(&core.store, |this, _, cx| {
            // A project added elsewhere, or its host back online.
            if this.branches_for.is_none() {
                this.load_branches(cx);
            }
            // Its host went or came back: an owner's error from before
            // ("… is offline") no longer holds.
            let ready = this.plan(cx).is_ok();
            if ready != this.ready {
                this.ready = ready;
                this.changed(cx);
            }
            cx.notify();
        })
        .detach();
        let seed = crate::app::store::now_millis() as u64;
        let mut form = Self {
            core,
            host: None,
            project: None,
            preferred,
            isolation: Isolation::default(),
            start: StartRef::Default,
            branch,
            prompt: String::new(),
            seed,
            branches: Vec::new(),
            branches_for: None,
            picker: None,
            generation: 0,
            ready: false,
        };
        match (host, project) {
            (Some(host), Some(project)) => form.set_project(&host, &project, cx),
            (host, _) => {
                form.host = host;
                if let Some((host, project)) =
                    form.choices(cx).first().map(|c| (c.0.clone(), c.1.clone()))
                {
                    form.set_project(&host, &project, cx);
                }
            }
        }
        form
    }

    /// The host the workspace goes on: always its project's.
    pub fn host(&self) -> Option<&HostId> {
        self.host.as_ref()
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        cx.emit(FormChanged);
        cx.notify();
    }

    /// Picks `project` on `host`, starting as the last workspace there did.
    pub fn set_project(&mut self, host: &HostId, project: &str, cx: &mut Context<Self>) {
        self.host = Some(host.clone());
        self.project = Some(project.to_string());
        let remembered = (self.core.nav.read(cx).workspace_choices)
            .get(&crate::app::nav::workspace_key(host, project))
            .cloned()
            .unwrap_or_default();
        self.isolation = remembered.isolation;
        self.start = remembered.start();
        self.picker = None;
        self.branches.clear();
        self.branches_for = None;
        self.load_branches(cx);
        self.changed(cx);
    }

    /// The first agent's prompt, as it is typed.
    pub fn set_prompt(&mut self, prompt: &str, cx: &mut Context<Self>) {
        if self.prompt != prompt {
            self.prompt = prompt.to_string();
            if self.isolation == Isolation::Worktree && self.branch.read(cx).value().is_empty() {
                self.changed(cx);
            }
        }
    }

    fn project_record(&self, cx: &App) -> Option<fleet::Project> {
        let store = self.core.store.read(cx);
        let host = store.fleet.host(self.host.as_ref()?)?;
        host.directory
            .projects
            .get(self.project.as_deref()?)
            .cloned()
    }

    fn support(&self, cx: &App) -> Support {
        (self.host.as_ref())
            .and_then(|host| self.core.store.read(cx).fleet.host(host))
            .map(Support::for_host)
            .unwrap_or_default()
    }

    /// Why a worktree cannot be made here, if it cannot.
    fn worktree_blocker(&self, cx: &App) -> Option<String> {
        let project = self.project_record(cx)?;
        if project.project_kind != "git" {
            return Some(format!(
                "Not a git repository. Run git init in {} for worktrees.",
                project.project_root_path
            ));
        }
        if !self.support(cx).worktrees {
            return Some(format!(
                "{} makes no worktrees; update it for them.",
                (self.host.as_ref()).map_or("This host", |host| self
                    .core
                    .store
                    .read(cx)
                    .fleet
                    .host_name(host))
            ));
        }
        None
    }

    fn isolation_now(&self, cx: &App) -> Isolation {
        if self.worktree_blocker(cx).is_some() {
            Isolation::Directory
        } else {
            self.isolation
        }
    }

    /// Projects on online hosts, the preferred ones first, as host,
    /// project id and label.
    fn choices(&self, cx: &App) -> Vec<(HostId, String, String)> {
        let fleet = &self.core.store.read(cx).fleet;
        let label = |host: &HostId, id: &str| {
            let host = fleet.host(host)?;
            let project = host.directory.projects.get(id)?;
            Some(format!("{} · {}", project.name(), host.name()))
        };
        let mut choices: Vec<(HostId, String, String)> = Vec::new();
        for (host, id) in &self.preferred {
            if !choices.iter().any(|(h, p, _)| h == host && p == id)
                && fleet.host(host).is_some_and(|h| h.connection.is_online())
                && let Some(label) = label(host, id)
            {
                choices.push((host.clone(), id.clone(), label));
            }
        }
        let mut others: Vec<(HostId, String, String)> = (fleet.hosts.iter())
            .filter(|host| host.connection.is_online())
            .flat_map(|host| {
                (host.directory.projects.keys()).map(move |id| (host.id().clone(), id.clone()))
            })
            .filter(|(host, id)| !choices.iter().any(|(h, p, _)| h == host && p == id))
            .filter_map(|(host, id)| {
                let label = label(&host, &id)?;
                Some((host, id, label))
            })
            .collect();
        others.sort_by(|a, b| fleet::sidebar::natural(&a.2, &b.2));
        choices.extend(others);
        choices
    }

    fn load_branches(&mut self, cx: &mut Context<Self>) {
        let (Some(host), Some(project)) = (self.host.clone(), self.project.clone()) else {
            return;
        };
        if self.worktree_blocker(cx).is_some() {
            return;
        }
        let Some(root) = self.project_record(cx).map(|p| p.project_root_path) else {
            return;
        };
        self.branches_for = Some((host.clone(), project.clone()));
        let task = (self.core.store.read(cx)).request(
            &host,
            requests::branch_suggestions(&root, "", 200),
            cx,
        );
        cx.spawn(async move |this, cx| {
            let branches = task
                .await
                .and_then(|payload| creation::parse_branches(&payload))
                .unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                if this.branches_for == Some((host, project)) {
                    this.branches = branches;
                    this.changed(cx);
                }
            });
        })
        .detach();
    }

    /// Names a new branch must not take: the repository's branches and the
    /// project's worktrees.
    fn taken(&self, cx: &App) -> Vec<String> {
        let mut taken: Vec<String> = self.branches.iter().map(|b| b.name.clone()).collect();
        if let (Some(host), Some(project)) = (&self.host, &self.project)
            && let Some(record) = self.core.store.read(cx).fleet.host(host)
        {
            for workspace in record.directory.workspaces.values() {
                if &workspace.project_id == project {
                    taken.extend(workspace.worktree_slug.clone());
                    taken.extend(workspace.branch().map(str::to_string));
                }
            }
        }
        taken
    }

    /// What will be made, or why nothing can be yet.
    pub fn plan(&self, cx: &App) -> Result<Plan, String> {
        let host = self.host.clone().ok_or("Pick a project")?;
        let fleet = &self.core.store.read(cx).fleet;
        let host_name = fleet.host_name(&host).to_string();
        if !(fleet.host(&host)).is_some_and(|h| h.connection.is_online()) {
            return Err(format!("{host_name} is offline"));
        }
        let project = self.project_record(cx).ok_or("Pick a project")?;
        let isolation = self.isolation_now(cx);
        let typed = self.branch.read(cx).value().to_string();
        let start = self.start.clone();
        if isolation == Isolation::Worktree
            && !matches!(start, StartRef::ChangeRequest(_))
            && let Some(problem) = creation::branch_problem(&typed)
        {
            return Err(problem.to_string());
        }
        let branch = creation::resolve_branch(&typed, &self.prompt, &self.taken(cx), self.seed);
        Ok(Plan {
            host,
            host_name,
            project_id: project.project_id.clone(),
            project_name: project.name(),
            root: project.project_root_path.clone(),
            isolation,
            branch,
            start,
        })
    }

    /// Remembers how this workspace was made, for the next in its project.
    pub fn remember(&mut self, cx: &mut Context<Self>) {
        let (Some(host), Some(project)) = (self.host.clone(), self.project.clone()) else {
            return;
        };
        let choice = Remembered {
            isolation: self.isolation,
            start: match &self.start {
                StartRef::Branch { .. } => self.start.ref_name(),
                _ => None,
            },
        };
        self.core.nav.update(cx, |nav, cx| {
            nav.remember_workspace(&host, &project, choice, cx)
        });
    }

    fn set_isolation(&mut self, isolation: Isolation, cx: &mut Context<Self>) {
        self.isolation = isolation;
        if isolation == Isolation::Directory {
            self.picker = None;
        }
        self.changed(cx);
    }

    // ---- the starting-point picker ---------------------------------------

    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            self.close_picker(cx);
            return;
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Find a branch or pull request…"));
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => this.search_refs(cx),
            InputEvent::PressEnter { .. } => this.pick_highlighted(cx),
            _ => {}
        })
        .detach();
        input.update(cx, |input, cx| input.focus(window, cx));
        self.picker = Some(RefPicker {
            input,
            branches: self.branches.clone(),
            changes: Vec::new(),
            note: None,
            loading: false,
            highlighted: 0,
            generation: 0,
            scroll: ScrollHandle::new(),
        });
        self.search_refs(cx);
        self.changed(cx);
    }

    fn close_picker(&mut self, cx: &mut Context<Self>) {
        if self.picker.take().is_some() {
            cx.emit(FormPicked);
            self.changed(cx);
        }
    }

    /// Asks for the branches and pull requests matching what is typed, a
    /// moment after typing stops.
    fn search_refs(&mut self, cx: &mut Context<Self>) {
        let (Some(host), Some(root)) = (
            self.host.clone(),
            self.project_record(cx).map(|p| p.project_root_path),
        ) else {
            return;
        };
        let forge = self.support(cx).forge_search;
        let Some(picker) = &mut self.picker else {
            return;
        };
        picker.generation += 1;
        picker.loading = true;
        picker.highlighted = 0;
        let generation = picker.generation;
        let query = picker.input.read(cx).value().to_string();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let tasks = this.update(cx, |this, cx| {
                let current = this.picker.as_ref().map(|p| p.generation);
                (current == Some(generation)).then(|| {
                    let store = this.core.store.read(cx);
                    let branches =
                        store.request(&host, requests::branch_suggestions(&root, &query, 50), cx);
                    let changes = forge.then(|| {
                        store.request(
                            &host,
                            requests::forge_change_requests(&root, &query, 20),
                            cx,
                        )
                    });
                    (branches, changes)
                })
            });
            let Ok(Some((branches, changes))) = tasks else {
                return;
            };
            let branches = branches
                .await
                .and_then(|payload| creation::parse_branches(&payload));
            let changes = match changes {
                Some(task) => task
                    .await
                    .and_then(|payload| creation::parse_change_requests(&payload)),
                None => Ok(Vec::new()),
            };
            let _ = this.update(cx, |this, cx| {
                let Some(picker) = this.picker.as_mut().filter(|p| p.generation == generation)
                else {
                    return;
                };
                picker.loading = false;
                let mut notes = Vec::new();
                match branches {
                    Ok(branches) => picker.branches = branches,
                    Err(error) => {
                        picker.branches.clear();
                        notes.push(if creation::unsupported(&error) {
                            "This host lists no branches; the default branch is used".to_string()
                        } else {
                            error
                        });
                    }
                }
                match changes {
                    Ok(changes) => picker.changes = changes,
                    Err(error) => {
                        picker.changes.clear();
                        if !creation::unsupported(&error) {
                            notes.push(error);
                        }
                    }
                }
                picker.note = (!notes.is_empty()).then(|| notes.join(" · "));
                this.changed(cx);
            });
        })
        .detach();
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        let count = picker.rows(picker.input.read(cx).value().as_ref()).len();
        if count == 0 {
            return;
        }
        picker.highlighted = if forward {
            (picker.highlighted + 1).min(count - 1)
        } else {
            picker.highlighted.saturating_sub(1)
        };
        picker.scroll.scroll_to_item(picker.highlighted);
        self.changed(cx);
    }

    fn pick_highlighted(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = &self.picker else {
            return;
        };
        let rows = picker.rows(picker.input.read(cx).value().as_ref());
        if let Some(start) = rows.get(picker.highlighted).or(rows.first()).cloned() {
            self.pick(start, cx);
        }
    }

    fn pick(&mut self, start: StartRef, cx: &mut Context<Self>) {
        self.start = start;
        self.close_picker(cx);
        self.changed(cx);
    }
}

impl Render for WorkspaceForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let id = cx.entity_id();
        let eid = |name: &str| SharedString::from(format!("nwf-{id}-{name}"));
        let this = cx.entity().downgrade();
        let choices = self.choices(cx);
        // Named even while its host is offline, which the summary says.
        let project_label = (self.project_record(cx).zip(self.host.as_ref()))
            .map(|(project, host)| {
                let host = self.core.store.read(cx).fleet.host_name(host).to_string();
                format!("{} · {host}", project.name())
            })
            .unwrap_or_else(|| "Pick a project".into());
        let blocker = self.worktree_blocker(cx);
        let isolation = self.isolation_now(cx);
        let host_for_add = self.host.clone();
        let core = self.core.clone();
        let projects = Button::new(eid("project"))
            .ghost()
            .xsmall()
            .child(div().text_color(p.muted).child("Project"))
            .child(div().min_w(px(0.)).truncate().child(project_label))
            .child(ui::icon("chevron-down", 12., p.muted))
            .tooltip("The project, and the host it is on")
            .dropdown_menu(move |mut menu, _, _| {
                for (host, project, label) in &choices {
                    let this = this.clone();
                    let (host, project) = (host.clone(), project.clone());
                    menu =
                        menu.item(PopupMenuItem::new(label.clone()).on_click(move |_, _, cx| {
                            let _ =
                                this.update(cx, |this, cx| this.set_project(&host, &project, cx));
                        }));
                }
                if !choices.is_empty() {
                    menu = menu.separator();
                }
                let (this, core, host) = (this.clone(), core.clone(), host_for_add.clone());
                menu.item(
                    PopupMenuItem::new("Add project…").on_click(move |_, window, cx| {
                        let this = this.clone();
                        super::add_project::open(
                            &core,
                            host.clone(),
                            window,
                            cx,
                            move |host, project, _, cx| {
                                let _ =
                                    this.update(cx, |this, cx| this.set_project(host, project, cx));
                            },
                        );
                    }),
                )
                .max_h(px(320.))
            });
        let segment =
            |name: &'static str, label: &'static str, value: Isolation, cx: &mut Context<Self>| {
                Button::new(eid(name))
                    .label(label)
                    .ghost()
                    .xsmall()
                    .selected(isolation == value)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_isolation(value, cx)))
            };
        let folder = segment("folder", "Folder", Isolation::Directory, cx)
            .tooltip("Work in the project's own folder");
        let worktree = segment("worktree", "Worktree", Isolation::Worktree, cx)
            .disabled(blocker.is_some())
            .tooltip(
                blocker
                    .clone()
                    .unwrap_or_else(|| "Work on a branch in a worktree of its own".into()),
            );
        let pr = matches!(self.start, StartRef::ChangeRequest(_));
        let plan = self.plan(cx);
        // An existing branch is checked out as it is, from nothing.
        let existing =
            matches!(&plan, Ok(plan) if matches!(plan.branch, creation::Branch::Existing(_)));
        let start_label = self.start.label();
        let from = Button::new(eid("from"))
            .ghost()
            .xsmall()
            .selected(self.picker.is_some())
            .child(
                div()
                    .text_color(p.muted)
                    .child(if pr { "On" } else { "From" }),
            )
            .child(div().min_w(px(0.)).truncate().child(start_label))
            .child(ui::icon("chevron-down", 12., p.muted))
            .tooltip("What the worktree starts from: a branch, or a pull request")
            .on_click(cx.listener(|this, _, window, cx| this.toggle_picker(window, cx)));
        let summary = match &plan {
            Ok(plan) => div()
                .flex()
                .flex_wrap()
                .text_size(size::sm())
                .text_color(p.muted)
                .children(plan.summary().into_iter().map(|(text, name)| {
                    div().when(name, |d| d.text_color(p.foreground)).child(text)
                })),
            Err(problem) => div()
                .text_size(size::sm())
                .text_color(p.status_warning)
                .child(problem.clone()),
        };
        let picker = self.picker.as_ref().map(|picker| {
            let query = picker.input.read(cx).value().to_string();
            let rows = picker.rows(&query);
            let highlighted = picker.highlighted;
            div()
                .key_context("RefPicker")
                .on_action(cx.listener(|this, _: &NextRef, _, cx| this.step(true, cx)))
                .on_action(cx.listener(|this, _: &PreviousRef, _, cx| this.step(false, cx)))
                .on_action(cx.listener(|this, _: &CloseRefs, _, cx| this.close_picker(cx)))
                .flex()
                .flex_col()
                .gap(px(4.))
                .p(px(4.))
                .rounded(px(6.))
                .border_1()
                .border_color(p.border)
                .bg(p.surface0)
                .child(Input::new(&picker.input).xsmall())
                .child(
                    div()
                        .id(eid("refs"))
                        .max_h(px(200.))
                        .overflow_y_scroll()
                        .track_scroll(&picker.scroll)
                        .flex()
                        .flex_col()
                        .children(rows.into_iter().enumerate().map(|(ix, start)| {
                            let (icon, title, detail) = match &start {
                                StartRef::Default => (
                                    "git-branch",
                                    "Default branch".to_string(),
                                    "as the host resolves it".to_string(),
                                ),
                                StartRef::Branch { remote, .. } => (
                                    "git-branch",
                                    start.label(),
                                    if *remote {
                                        "branch off origin's copy"
                                    } else {
                                        "branch off the local branch"
                                    }
                                    .to_string(),
                                ),
                                StartRef::ChangeRequest(change) => (
                                    "git-pull-request",
                                    format!("#{} {}", change.number, change.title),
                                    format!("check out {}", change.head),
                                ),
                            };
                            div()
                                .id(SharedString::from(format!("nwf-{id}-ref-{ix}")))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .px(px(6.))
                                .py(px(3.))
                                .rounded(px(4.))
                                .text_size(size::sm())
                                .cursor_pointer()
                                .when(ix == highlighted, |d| d.bg(p.surface2))
                                .hover(|d| d.bg(p.surface2))
                                .child(ui::icon(icon, 12., p.muted))
                                .child(div().min_w(px(0.)).truncate().child(title))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .text_color(p.extra_muted)
                                        .child(detail),
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.pick(start.clone(), cx)),
                                )
                        })),
                )
                .when(picker.loading, |d| {
                    d.child(
                        div()
                            .text_size(size::sm())
                            .text_color(p.extra_muted)
                            .child("Searching…"),
                    )
                })
                .when_some(picker.note.clone(), |d, note| {
                    d.child(
                        div()
                            .text_size(size::sm())
                            .text_color(p.status_warning)
                            .child(note),
                    )
                })
                .into_any_element()
        });
        let _ = window;
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .flex_wrap()
                    .child(div().min_w(px(0.)).flex_shrink(1.).child(projects))
                    .child(div().flex().child(folder).child(worktree)),
            )
            .when(isolation == Isolation::Worktree, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .when(!pr, |row| {
                            row.child(
                                div()
                                    .flex_1()
                                    .min_w(px(80.))
                                    .child(Input::new(&self.branch).xsmall()),
                            )
                        })
                        .when(!existing, |row| {
                            row.child(div().min_w(px(0.)).flex_shrink(1.).child(from))
                        }),
                )
            })
            .children(picker)
            .child(summary)
            // Why a worktree is not offered, where it is not.
            .when_some(
                blocker.filter(|_| isolation == Isolation::Directory),
                |d, blocker| {
                    d.child(
                        div()
                            .text_size(size::sm())
                            .text_color(p.extra_muted)
                            .child(blocker),
                    )
                },
            )
    }
}
