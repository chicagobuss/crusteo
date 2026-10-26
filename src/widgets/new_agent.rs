//! New agent: one centered dialog for every way to start an agent (a lane's
//! or a workspace's +, the Paseo view's New agent, Cmd+T). In Skiff's order:
//! where it works (a workspace, a new workspace, or a new project), the tool,
//! model, mode and thinking, its own labels in reef mode, and the first
//! message. The choices made on a host start the next agent there.

use std::rc::Rc;

use fleet::HostId;
use fleet::creation::AgentChoice;
use fleet::labels::LabelId;
use fleet::providers::Provider;
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{Sizable, WindowExt};
use gpui_kit::gpui::{
    AnyElement, App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window,
    div, px, rgb,
};
use gpui_kit::prelude::FluentBuilder;

use crate::app::Core;
use crate::app::store::{LabelChange, ProvidersState};
use crate::layouts::paseo::attachments::{self, AttachmentTray, HasAttachments};
use crate::theme::{self, palette, size};
use crate::ui;
use crate::widgets::add_project::AddProject;
use crate::widgets::workspace_form::{FormChanged, FormPicked, FormSubmit, WorkspaceForm};

gpui_kit::actions!(new_agent, [CreateAgent]);

pub fn bind_keys(cx: &mut App) {
    use gpui_kit::gpui::KeyBinding;
    // Its own, or the dialog's Enter would close it with nothing made.
    // Shift+Enter still breaks the line.
    cx.bind_keys([KeyBinding::new(
        "enter",
        CreateAgent,
        Some("NewAgent > Input"),
    )]);
}

/// Where the agent works, as the dialog first shows it.
#[derive(Debug, Clone)]
pub enum Place {
    /// In this workspace.
    Workspace(HostId, String),
    /// In a new workspace, in the project of this workspace when given.
    NewWorkspace(Option<(HostId, String)>),
    /// In a new workspace of a project added first, on this host if given.
    NewProject(Option<HostId>),
}

/// The labels a new workspace on a host takes.
pub type WorkspaceLabels = Rc<dyn Fn(&HostId, &App) -> Vec<LabelChange>>;

/// How the dialog starts.
#[derive(Clone, Default)]
pub struct Start {
    pub place: Option<Place>,
    /// Workspaces listed first, and whose projects a new workspace offers
    /// first: a lane's.
    pub preferred: Vec<(HostId, String)>,
    /// Reef mode: the labels it starts with, such as a lane's. None starts
    /// from those the workspace's agents share.
    pub labels: Option<Vec<LabelId>>,
    /// Paseo-compatible mode: labels a new workspace takes, so it lands in
    /// the lane it was started from.
    pub workspace_labels: Option<WorkspaceLabels>,
    /// A tool to start with, over the host's last one.
    pub provider: Option<String>,
}

/// What the dialog made.
#[derive(Debug, Clone)]
pub struct Made {
    pub host: HostId,
    pub workspace: String,
    /// None for a new workspace made without a first message.
    pub agent: Option<String>,
}

type OnMade = Rc<dyn Fn(&Made, &mut Window, &mut App)>;

/// Opens New agent. `on_made` runs once something was made, after the
/// dialog closed: open the agent there.
pub fn open(
    core: &Core,
    start: Start,
    window: &mut Window,
    cx: &mut App,
    on_made: impl Fn(&Made, &mut Window, &mut App) + 'static,
) {
    let on_made: OnMade = Rc::new(on_made);
    let view = cx.new(|cx| NewAgent::new(core.clone(), start, on_made, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("New agent")
            .w(px(580.))
            .margin_top(px(48.))
            // A click beside it must not lose a long first message.
            .overlay_closable(false)
            .child(view.clone())
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Workspace,
    NewWorkspace,
    NewProject,
}

pub struct NewAgent {
    core: Core,
    step: Step,
    /// The workspace step's host and workspace.
    host: Option<HostId>,
    workspace: Option<String>,
    preferred: Vec<(HostId, String)>,
    form: Option<Entity<WorkspaceForm>>,
    project: Option<Entity<AddProject>>,
    /// What was picked here; else the host's last choice, else defaults.
    provider: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    thinking: Option<String>,
    /// Reef mode: the labels picked, or the lane's; None follows the
    /// workspace's.
    labels: Option<Vec<LabelId>>,
    workspace_labels: Option<WorkspaceLabels>,
    input: Entity<TextareaState>,
    attachments: AttachmentTray,
    creating: bool,
    error: Option<String>,
    on_made: OnMade,
    /// Whether the first message took focus yet. The dialog focuses itself
    /// as it is first drawn, after anything asked for when it opened, so
    /// its content takes focus as it is drawn too.
    focused: bool,
    _subscriptions: Vec<Subscription>,
}

impl NewAgent {
    fn new(
        core: Core,
        start: Start,
        on_made: OnMade,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 14)
                .submit_on_enter(true)
                .placeholder("What should the agent do?")
        });
        // What was typed when it last closed without making anything.
        if let Some(text) = core.nav.update(cx, |nav, _| nav.new_agent_stash.take()) {
            input.update(cx, |input, cx| input.set_value(text, window, cx));
        }
        let mut subscriptions = vec![cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { shift: false, .. } => this.create(window, cx),
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    // Kept while typing, so Escape or a lost window keeps it.
                    this.core.nav.update(cx, |nav, _| {
                        nav.new_agent_stash = (!text.trim().is_empty()).then(|| text.clone());
                    });
                    if let Some(form) = &this.form {
                        form.update(cx, |form, cx| form.set_prompt(&text, cx));
                    }
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )];
        subscriptions.push(cx.observe(&core.store, |_, _, cx| cx.notify()));
        let mut this = Self {
            core,
            step: Step::Workspace,
            host: None,
            workspace: None,
            preferred: start.preferred,
            form: None,
            project: None,
            provider: start.provider,
            model: None,
            mode: None,
            thinking: None,
            labels: start.labels,
            workspace_labels: start.workspace_labels,
            input,
            attachments: AttachmentTray::default(),
            creating: false,
            error: None,
            on_made,
            focused: false,
            _subscriptions: subscriptions,
        };
        let place = start.place.unwrap_or_else(|| {
            match this
                .preferred
                .first()
                .cloned()
                .or_else(|| this.any_workspace(cx))
            {
                Some((host, workspace)) => Place::Workspace(host, workspace),
                None => Place::NewWorkspace(None),
            }
        });
        match place {
            Place::Workspace(host, workspace) => {
                this.host = Some(host);
                this.workspace = Some(workspace);
            }
            Place::NewWorkspace(from) => {
                if let Some((host, workspace)) = &from {
                    this.host = Some(host.clone());
                    this.workspace = Some(workspace.clone());
                }
                this.show_new_workspace(from.as_ref(), window, cx);
            }
            Place::NewProject(host) => {
                this.host = host;
                this.show_new_project(window, cx);
            }
        }
        this.load_providers(cx);
        this
    }

    fn focus_first(&self, window: &mut Window, cx: &mut App) {
        match (&self.step, &self.project) {
            (Step::NewProject, Some(project)) => {
                project.update(cx, |project, cx| project.focus(window, cx))
            }
            _ => self.input.update(cx, |input, cx| input.focus(window, cx)),
        }
    }

    /// The first workspace of the first online host, for a dialog opened
    /// from nowhere in particular.
    fn any_workspace(&self, cx: &App) -> Option<(HostId, String)> {
        let fleet = &self.core.store.read(cx).fleet;
        fleet
            .hosts
            .iter()
            .filter(|host| host.connection.is_online())
            .find_map(|host| {
                (host.directory.workspaces.values())
                    .find(|workspace| !workspace.background)
                    .map(|workspace| (host.id().clone(), workspace.id.clone()))
            })
    }

    /// The host the agent will run on.
    fn target_host(&self, cx: &App) -> Option<HostId> {
        match self.step {
            Step::Workspace => self.host.clone(),
            Step::NewWorkspace => self.form.as_ref()?.read(cx).host().cloned(),
            Step::NewProject => self.project.as_ref()?.read(cx).host().cloned(),
        }
    }

    fn load_providers(&self, cx: &mut Context<Self>) {
        if let Some(host) = self.target_host(cx) {
            self.core
                .store
                .update(cx, |store, cx| store.ensure_providers(&host, cx));
        }
    }

    /// Picks reset when the host changes: the new host's last choice applies.
    fn set_host(&mut self, host: HostId, cx: &mut Context<Self>) {
        if self.host.as_ref() == Some(&host) {
            return;
        }
        self.host = Some(host.clone());
        self.workspace = self.workspaces(&host, cx).first().map(|(id, _)| id.clone());
        self.forget_picks();
        self.error = None;
        self.load_providers(cx);
        cx.notify();
    }

    fn forget_picks(&mut self) {
        self.provider = None;
        self.model = None;
        self.mode = None;
        self.thinking = None;
    }

    fn set_step(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        if self.step == step {
            return;
        }
        match step {
            Step::Workspace => self.step = Step::Workspace,
            Step::NewWorkspace => {
                let from = self.host.clone().zip(self.workspace.clone());
                self.show_new_workspace(from.as_ref(), window, cx);
            }
            Step::NewProject => self.show_new_project(window, cx),
        }
        self.error = None;
        self.load_providers(cx);
        self.focus_first(window, cx);
        cx.notify();
    }

    /// The new workspace step, in `from`'s project, else the first preferred
    /// one's. Its form is kept when the step is left and taken up again.
    fn show_new_workspace(
        &mut self,
        from: Option<&(HostId, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step = Step::NewWorkspace;
        if self.form.is_some() {
            return;
        }
        let (preferred, from) = {
            let fleet = &self.core.store.read(cx).fleet;
            let project_of =
                |host: &HostId, id: &str| Some(fleet.workspace(host, id)?.project_id.clone());
            let mut preferred: Vec<(HostId, String)> = Vec::new();
            for (host, workspace) in &self.preferred {
                if let Some(project) = project_of(host, workspace) {
                    let pair = (host.clone(), project);
                    if !preferred.contains(&pair) {
                        preferred.push(pair);
                    }
                }
            }
            let from = from.and_then(|(host, id)| Some((host.clone(), project_of(host, id)?)));
            (preferred, from)
        };
        let (host, project) = from
            .or_else(|| preferred.first().cloned())
            .map_or((None, None), |(host, project)| (Some(host), Some(project)));
        let core = self.core.clone();
        let form = cx.new(|cx| WorkspaceForm::new(core, host, project, preferred, window, cx));
        let text = self.input.read(cx).value().to_string();
        form.update(cx, |form, cx| form.set_prompt(&text, cx));
        self._subscriptions
            .push(cx.subscribe(&form, |this, _, _: &FormChanged, cx| {
                // Its project may be on another host, whose tools then show.
                this.load_providers(cx);
                this.error = None;
                cx.notify();
            }));
        self._subscriptions.push(cx.subscribe_in(
            &form,
            window,
            |this, _, _: &FormPicked, window, cx| {
                this.input.update(cx, |input, cx| input.focus(window, cx));
            },
        ));
        self._subscriptions.push(cx.subscribe_in(
            &form,
            window,
            |this, _, _: &FormSubmit, window, cx| this.create(window, cx),
        ));
        self.form = Some(form);
    }

    /// The new project step: find a folder on a host and add it, then go on
    /// to a new workspace in it.
    fn show_new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step = Step::NewProject;
        if self.project.is_some() {
            return;
        }
        let this = cx.entity().downgrade();
        let (core, host) = (self.core.clone(), self.host.clone());
        let project = cx.new(|cx| {
            AddProject::embedded(
                core,
                host,
                move |host, project, window, cx| {
                    let (host, project) = (host.clone(), project.to_string());
                    let _ =
                        this.update(cx, |this, cx| this.project_added(host, project, window, cx));
                },
                window,
                cx,
            )
        });
        self._subscriptions
            .push(cx.observe(&project, |this, _, cx| {
                this.load_providers(cx);
                cx.notify();
            }));
        self.project = Some(project);
    }

    fn project_added(
        &mut self,
        host: HostId,
        project: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_new_workspace(None, window, cx);
        if let Some(form) = &self.form {
            form.update(cx, |form, cx| form.set_project(&host, &project, cx));
        }
        // It starts over the next time, for another folder.
        self.project = None;
        self.error = None;
        self.load_providers(cx);
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// The host's workspaces, the preferred ones first, then by project and
    /// name: id and how the menu names it.
    fn workspaces(&self, host: &HostId, cx: &App) -> Vec<(String, String)> {
        let fleet = &self.core.store.read(cx).fleet;
        let Some(found) = fleet.host(host) else {
            return Vec::new();
        };
        let mut all: Vec<_> = found.directory.workspaces.values().collect();
        let rank = |id: &str| {
            (self.preferred.iter())
                .position(|(h, w)| h == host && w == id)
                .unwrap_or(usize::MAX)
        };
        all.sort_by(|a, b| {
            (
                rank(&a.id),
                a.project_display_name.to_lowercase(),
                a.name.to_lowercase(),
            )
                .cmp(&(
                    rank(&b.id),
                    b.project_display_name.to_lowercase(),
                    b.name.to_lowercase(),
                ))
        });
        all.into_iter()
            .map(|workspace| {
                let project = &workspace.project_display_name;
                let name = if project.is_empty() || *project == workspace.name {
                    workspace.name.clone()
                } else {
                    format!("{} · {project}", workspace.name)
                };
                (workspace.id.clone(), name)
            })
            .collect()
    }

    fn providers(&self, host: &HostId, cx: &App) -> Vec<Provider> {
        (self.core.store.read(cx).providers.get(host).cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(Provider::ready)
            .collect()
    }

    /// The tool, model, mode and thinking it will be made with: each as
    /// picked here, else as last made on the host, else the defaults; never
    /// one the host does not offer.
    fn resolved(&self, host: &HostId, cx: &App) -> Option<Resolved> {
        let providers = self.providers(host, cx);
        let last = self
            .core
            .nav
            .read(cx)
            .agent_choices
            .get(host.as_str())
            .cloned()
            .unwrap_or_default();
        let picked = self.provider.is_some();
        let provider = (self.provider.as_ref().or(last.provider.as_ref()))
            .and_then(|id| providers.iter().find(|p| &p.id == id))
            .or_else(|| providers.iter().find(|p| p.id == "claude"))
            .or_else(|| providers.first())?
            .clone();
        // The host's last model and mode only go with its last tool.
        let same_tool = last.provider.as_deref() == Some(provider.id.as_str());
        let remembered = |value: &Option<String>| value.clone().filter(|_| same_tool && !picked);
        let model = (self.model.clone().or_else(|| remembered(&last.model)))
            .and_then(|id| provider.models.iter().find(|m| m.id == id))
            .or_else(|| provider.default_model())
            .cloned();
        let mode = (self.mode.clone().or_else(|| remembered(&last.mode)))
            .filter(|id| provider.modes.iter().any(|m| &m.id == id))
            .or_else(|| {
                (provider.default_mode.clone())
                    .filter(|id| provider.modes.iter().any(|m| &m.id == id))
            })
            .or_else(|| provider.modes.first().map(|m| m.id.clone()));
        let thinking = model.as_ref().and_then(|model| {
            (self.thinking.clone().or_else(|| remembered(&last.thinking)))
                .filter(|id| model.thinking.iter().any(|t| &t.id == id))
                .or_else(|| model.default_thinking.clone())
        });
        Some(Resolved {
            provider,
            model,
            mode,
            thinking,
        })
    }

    /// Why there is no tool to make the agent with.
    fn no_provider(&self, host: &HostId, cx: &App) -> String {
        let store = self.core.store.read(cx);
        let name = store.fleet.host_name(host);
        match store.providers_state(host) {
            ProvidersState::Offline => format!("{name} is offline"),
            ProvidersState::Loading => format!("Still loading {name}'s tools"),
            ProvidersState::Failed(error) => format!("{name}'s tools: {error}"),
            ProvidersState::Ready => format!("No tool is ready on {name}"),
        }
    }

    /// Reef mode: the labels the agent starts with, as shown.
    fn shown_labels(&self, cx: &App) -> Option<Vec<LabelId>> {
        let fleet = &self.core.store.read(cx).fleet;
        let catalog = fleet.labels.as_ref()?;
        if let Some(labels) = &self.labels {
            return Some(labels.clone());
        }
        // A new workspace has no agents to share labels from.
        if self.step != Step::Workspace {
            return Some(Vec::new());
        }
        let (host, workspace) = (self.host.as_ref()?, self.workspace.as_ref()?);
        let directory = &fleet.host(host)?.directory;
        let workspace = directory.workspaces.get(workspace)?;
        let agents = (directory.agents.values())
            .filter(|agent| agent.workspace_id.as_deref() == Some(workspace.id.as_str()));
        Some(fleet::labels::workspace_default(agents, workspace, catalog))
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating || self.attachments.pending {
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        let prompt = self.attachments.prompt(text.clone());
        let said = !text.is_empty() || !prompt.images.is_empty();
        let labels = self.shown_labels(cx);
        match self.step {
            Step::NewProject => self.fail("Add the project first, then make the agent in it", cx),
            Step::Workspace => {
                let Some((host, workspace)) = self.host.clone().zip(self.workspace.clone()) else {
                    return self.fail("Pick a workspace, or make a new one", cx);
                };
                if !said {
                    return self.fail("Write what the agent should do", cx);
                }
                let Some(resolved) = self.resolved(&host, cx) else {
                    let error = self.no_provider(&host, cx);
                    return self.fail(error, cx);
                };
                let Some(directory) = (self.core.store.read(cx).fleet)
                    .workspace(&host, &workspace)
                    .map(|workspace| workspace.directory().to_string())
                else {
                    return self.fail("That workspace is gone", cx);
                };
                let config = resolved.config(&directory);
                self.begin(cx);
                let task = self.core.store.update(cx, |store, cx| {
                    store.create_agent(&host, &workspace, config, Some(prompt), labels, cx)
                });
                cx.spawn_in(window, async move |this, cx| {
                    let result = task.await;
                    let _ = this.update_in(cx, |this, window, cx| match result {
                        Ok(agent) => this.made(
                            resolved,
                            Made {
                                host,
                                workspace,
                                agent: Some(agent),
                            },
                            window,
                            cx,
                        ),
                        Err(error) => this.failed(error, cx),
                    });
                })
                .detach();
            }
            Step::NewWorkspace => {
                let Some(form) = self.form.clone() else {
                    return;
                };
                let plan = match form.read(cx).plan(cx) {
                    Ok(plan) => plan,
                    Err(error) => return self.fail(error, cx),
                };
                let host = plan.host.clone();
                let resolved = self.resolved(&host, cx);
                // Without a first message, only the workspace is made.
                let agent = match (&resolved, said) {
                    (_, false) => None,
                    (Some(resolved), true) => Some((resolved.config(&plan.root), prompt)),
                    (None, true) => {
                        let error = self.no_provider(&host, cx);
                        return self.fail(error, cx);
                    }
                };
                let workspace_labels = (self.workspace_labels.as_ref())
                    .map(|labels| labels(&host, cx))
                    .unwrap_or_default();
                self.begin(cx);
                let task = self.core.store.update(cx, |store, cx| {
                    store.create_workspace(&plan, agent, labels, cx)
                });
                cx.spawn_in(window, async move |this, cx| {
                    let created = match task.await {
                        Ok(created) => created,
                        Err(error) => {
                            let _ = this.update(cx, |this, cx| this.failed(error, cx));
                            return;
                        }
                    };
                    let _ = this.update(cx, |_, cx| form.update(cx, |form, cx| form.remember(cx)));
                    // Paseo-compatible mode: in a label lane the workspace
                    // takes the lane's labels, or it lands under Unlabelled.
                    if !workspace_labels.is_empty() {
                        let batch = this.update(cx, |this, cx| {
                            this.core.store.read(cx).set_workspace_labels(
                                &host,
                                &created.workspace,
                                workspace_labels,
                                cx,
                            )
                        });
                        if let Ok(batch) = batch
                            && let Some(error) = batch.await.error
                        {
                            let _ = this.update(cx, |this, cx| {
                                this.core.store.update(cx, |store, cx| {
                                    store.toast(format!("Labelling the new workspace: {error}"), cx)
                                });
                            });
                        }
                    }
                    let _ = this.update_in(cx, |this, window, cx| match created.agent {
                        Ok(agent) => {
                            let made = Made {
                                host: host.clone(),
                                workspace: created.workspace.clone(),
                                agent,
                            };
                            match resolved {
                                Some(resolved) => this.made(resolved, made, window, cx),
                                None => this.close_with(made, window, cx),
                            }
                        }
                        // Made, but its agent did not start: Create again
                        // tries the agent alone, in the workspace now there.
                        Err(error) => {
                            this.step = Step::Workspace;
                            this.host = Some(host.clone());
                            this.workspace = Some(created.workspace.clone());
                            this.failed(
                                format!(
                                    "The workspace is made, but its agent did not start: {error}. Create tries again."
                                ),
                                cx,
                            );
                        }
                    });
                })
                .detach();
            }
        }
    }

    fn fail(&mut self, error: impl Into<String>, cx: &mut Context<Self>) {
        self.error = Some(error.into());
        cx.notify();
    }

    fn begin(&mut self, cx: &mut Context<Self>) {
        self.creating = true;
        self.error = None;
        cx.notify();
    }

    fn failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.creating = false;
        self.error = Some(error);
        cx.notify();
    }

    /// Remembers what it was made with on the host, then closes.
    fn made(
        &mut self,
        resolved: Resolved,
        made: Made,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choice = AgentChoice {
            provider: Some(resolved.provider.id.clone()),
            model: resolved.model.as_ref().map(|m| m.id.clone()),
            mode: resolved.mode.clone(),
            thinking: resolved.thinking.clone(),
        };
        let host = made.host.clone();
        self.core
            .nav
            .update(cx, |nav, cx| nav.set_agent_choice(&host, choice, cx));
        self.close_with(made, window, cx);
    }

    fn close_with(&mut self, made: Made, window: &mut Window, cx: &mut Context<Self>) {
        self.creating = false;
        self.core
            .nav
            .update(cx, |nav, _| nav.new_agent_stash = None);
        window.close_dialog(cx);
        (self.on_made)(&made, window, cx);
    }
}

/// The choices an agent is made with.
#[derive(Debug, Clone)]
struct Resolved {
    provider: Provider,
    model: Option<fleet::providers::Model>,
    mode: Option<String>,
    thinking: Option<String>,
}

impl Resolved {
    fn config(&self, cwd: &str) -> serde_json::Value {
        fleet::creation::agent_config(
            &self.provider,
            self.model.as_ref().map(|m| m.id.as_str()),
            self.thinking.as_deref(),
            self.mode.as_deref(),
            cwd,
        )
    }
}

impl HasAttachments for NewAgent {
    fn attachments(&mut self) -> &mut AttachmentTray {
        &mut self.attachments
    }
    fn attachments_disabled(&self) -> bool {
        self.creating
    }
}

/// A section's name, above its fields.
fn heading(text: &'static str) -> impl IntoElement {
    div()
        .text_size(size::sm())
        .text_color(palette().muted)
        .child(text)
}

/// One choice as a field-like dropdown: what it chooses, in grey, the
/// current choice, and a caret.
fn field(id: impl Into<SharedString>, name: &str, value: impl Into<SharedString>) -> Button {
    let p = palette();
    Button::new(id.into())
        .outline()
        .small()
        .w_full()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .gap(px(6.))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(p.muted)
                        .child(name.to_string()),
                )
                .child(div().min_w(px(0.)).truncate().child(value.into())),
        )
        .child(ui::icon("chevron-down", 12., p.muted))
}

/// Two fields side by side, each half the row.
fn pair(left: impl IntoElement, right: Option<impl IntoElement>) -> impl IntoElement {
    div()
        .flex()
        .gap(px(8.))
        .child(div().flex_1().min_w(px(0.)).child(left))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .children(right.map(IntoElement::into_any_element)),
        )
}

impl NewAgent {
    fn where_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let step = self.step;
        let segment = |id: &'static str,
                       label: &'static str,
                       value: Step,
                       cx: &mut Context<Self>| {
            Button::new(id)
                .label(label)
                .ghost()
                .small()
                .flex_1()
                .selected(step == value)
                .on_click(cx.listener(move |this, _, window, cx| this.set_step(value, window, cx)))
        };
        let switch = div()
            .flex()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(8.))
            .border_1()
            .border_color(p.border)
            .child(segment(
                "new-agent-in-workspace",
                "Workspace",
                Step::Workspace,
                cx,
            ))
            .child(segment(
                "new-agent-new-workspace",
                "New workspace",
                Step::NewWorkspace,
                cx,
            ))
            .child(segment(
                "new-agent-new-project",
                "New project",
                Step::NewProject,
                cx,
            ));
        let body = match step {
            Step::Workspace => self.workspace_fields(cx).into_any_element(),
            Step::NewWorkspace => match &self.form {
                Some(form) => div().child(form.clone()).into_any_element(),
                None => div().into_any_element(),
            },
            Step::NewProject => match &self.project {
                Some(project) => div().child(project.clone()).into_any_element(),
                None => div().into_any_element(),
            },
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(heading("Where"))
            .child(switch)
            .child(body)
            .into_any_element()
    }

    fn workspace_fields(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let this = cx.entity().downgrade();
        let store = self.core.store.read(cx);
        let hosts: Vec<(HostId, String, bool)> = (store.fleet.hosts.iter())
            .map(|host| {
                (
                    host.id().clone(),
                    host.name().to_string(),
                    host.connection.is_online(),
                )
            })
            .collect();
        let host_label = (self.host.as_ref())
            .map(|host| store.fleet.host_name(host).to_string())
            .unwrap_or_else(|| "Pick a host".into());
        let workspaces = (self.host.as_ref())
            .map(|host| self.workspaces(host, cx))
            .unwrap_or_default();
        let workspace_label = (self.workspace.as_ref())
            .and_then(|id| workspaces.iter().find(|(w, _)| w == id))
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| "Pick a workspace".into());
        let directory = (self.host.as_ref().zip(self.workspace.as_ref()))
            .and_then(|(host, id)| store.fleet.workspace(host, id))
            .map(|workspace| workspace.directory().to_string());
        let host_menu = {
            let this = this.clone();
            field("new-agent-host", "Host", host_label).dropdown_menu(move |mut menu, _, _| {
                for (id, name, online) in &hosts {
                    let (this, id) = (this.clone(), id.clone());
                    let label = if *online {
                        name.clone()
                    } else {
                        format!("{name} (offline)")
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        let id = id.clone();
                        let _ = this.update(cx, |this, cx| this.set_host(id, cx));
                    }));
                }
                menu.scrollable(true).max_h(px(320.))
            })
        };
        let empty = workspaces.is_empty();
        let workspace_menu = field("new-agent-workspace", "Workspace", workspace_label)
            .when(empty, |b| b.disabled(true))
            .dropdown_menu(move |mut menu, _, _| {
                for (id, name) in &workspaces {
                    let (this, id) = (this.clone(), id.clone());
                    menu = menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                        let id = id.clone();
                        let _ = this.update(cx, |this, cx| {
                            this.workspace = Some(id);
                            this.error = None;
                            cx.notify();
                        });
                    }));
                }
                menu.scrollable(true).max_h(px(360.))
            });
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(pair(host_menu, Some(workspace_menu)))
            .child(
                div()
                    .text_size(size::sm())
                    .text_color(p.extra_muted)
                    .truncate()
                    .child(match (empty, directory) {
                        (true, _) => "No workspaces on this host: make a new one".to_string(),
                        (false, Some(directory)) => directory,
                        (false, None) => String::new(),
                    }),
            )
            .into_any_element()
    }

    fn agent_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let this = cx.entity().downgrade();
        let host = self.target_host(cx);
        let resolved = host.as_ref().and_then(|host| self.resolved(host, cx));
        let providers = host
            .as_ref()
            .map(|host| self.providers(host, cx))
            .unwrap_or_default();
        let Some(resolved) = resolved else {
            let note = match &host {
                Some(host) => self.no_provider(host, cx),
                None => "Pick where it works first".into(),
            };
            return div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(heading("Agent"))
                .child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.extra_muted)
                        .child(note),
                )
                .into_any_element();
        };
        let pick = |this: &WeakEntity<Self>, set: fn(&mut Self, String), id: String| {
            let this = this.clone();
            move |_: &gpui_kit::gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                let id = id.clone();
                let _ = this.update(cx, |this, cx| {
                    set(this, id);
                    this.error = None;
                    cx.notify();
                });
            }
        };
        let tool = {
            let (this, current) = (this.clone(), resolved.provider.id.clone());
            field("new-agent-tool", "Tool", resolved.provider.label.clone()).dropdown_menu(
                move |mut menu, _, _| {
                    for provider in &providers {
                        let set: fn(&mut Self, String) = |this, id| {
                            this.forget_picks();
                            this.provider = Some(id);
                        };
                        menu = menu.item(
                            PopupMenuItem::new(provider.label.clone())
                                .checked(provider.id == current)
                                .on_click(pick(&this, set, provider.id.clone())),
                        );
                    }
                    menu
                },
            )
        };
        let models = resolved.provider.models.clone();
        let model_id = resolved.model.as_ref().map(|m| m.id.clone());
        let model = {
            let this = this.clone();
            let label = (resolved.model.as_ref())
                .map(|m| m.label.clone())
                .unwrap_or_else(|| "Default".into());
            field("new-agent-model", "Model", label).dropdown_menu(move |mut menu, _, _| {
                for model in &models {
                    let set: fn(&mut Self, String) = |this, id| {
                        this.model = Some(id);
                        this.thinking = None;
                    };
                    menu = menu.item(
                        PopupMenuItem::new(model.label.clone())
                            .checked(model_id.as_ref() == Some(&model.id))
                            .on_click(pick(&this, set, model.id.clone())),
                    );
                }
                menu.scrollable(true).max_h(px(360.))
            })
        };
        let modes = resolved.provider.modes.clone();
        let mode = (!modes.is_empty()).then(|| {
            let this = this.clone();
            let current = resolved.mode.clone();
            let label = (modes.iter())
                .find(|m| Some(&m.id) == current.as_ref())
                .map(|m| m.label.clone())
                .unwrap_or_default();
            field("new-agent-mode", "Mode", label).dropdown_menu(move |mut menu, _, _| {
                for mode in &modes {
                    let set: fn(&mut Self, String) = |this, id| this.mode = Some(id);
                    menu = menu.item(
                        PopupMenuItem::new(mode.label.clone())
                            .checked(current.as_ref() == Some(&mode.id))
                            .on_click(pick(&this, set, mode.id.clone())),
                    );
                }
                menu
            })
        });
        let thinkings = (resolved.model.as_ref())
            .map(|m| m.thinking.clone())
            .unwrap_or_default();
        let thinking = (!thinkings.is_empty()).then(|| {
            let current = resolved.thinking.clone();
            let label = (thinkings.iter())
                .find(|t| Some(&t.id) == current.as_ref())
                .map(|t| t.label.clone())
                .unwrap_or_default();
            field("new-agent-thinking", "Thinking", label).dropdown_menu(move |mut menu, _, _| {
                for choice in &thinkings {
                    let set: fn(&mut Self, String) = |this, id| this.thinking = Some(id);
                    menu = menu.item(
                        PopupMenuItem::new(choice.label.clone())
                            .checked(current.as_ref() == Some(&choice.id))
                            .on_click(pick(&this, set, choice.id.clone())),
                    );
                }
                menu
            })
        });
        let second = match (mode, thinking) {
            (Some(mode), thinking) => Some(pair(mode, thinking).into_any_element()),
            (None, Some(thinking)) => Some(pair(thinking, None::<Button>).into_any_element()),
            (None, None) => None,
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(heading("Agent"))
            .child(pair(tool, Some(model)))
            .children(second)
            .into_any_element()
    }

    /// Reef mode only: the agent's own labels, to take off or put on.
    fn labels_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let shown = self.shown_labels(cx)?;
        let p = palette();
        let this = cx.entity().downgrade();
        let nav = self.core.nav.read(cx);
        let store = self.core.store.read(cx);
        let catalog = store.fleet.labels.as_ref()?;
        let color = |id: &LabelId, named: &str| {
            let index = crate::layouts::lanes::colors::index(
                id.as_str(),
                Some(named),
                &nav.lane_color_choices,
            );
            rgb(theme::LANE_COLORS[index].1)
        };
        let chips: Vec<_> = shown
            .iter()
            .filter_map(|id| {
                let label = catalog.get(id)?;
                let hue = color(id, &label.color);
                let this = this.clone();
                let id = id.clone();
                Some(
                    div()
                        .id(SharedString::from(format!("new-agent-label-{id}")))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .px(px(8.))
                        .py(px(2.))
                        .rounded(px(999.))
                        .bg(crate::theme::with_alpha(hue, 0.22))
                        .text_size(size::sm())
                        .text_color(p.foreground)
                        .child(div().size(px(7.)).rounded(px(4.)).bg(hue))
                        .child(label.name.clone())
                        .child(
                            div()
                                .id(SharedString::from(format!("new-agent-unlabel-{id}")))
                                .cursor_pointer()
                                .child(ui::icon("x", 11., p.muted))
                                .on_click(move |_, _, cx| {
                                    let id = id.clone();
                                    let _ = this.update(cx, |this, cx| {
                                        let mut labels = this.shown_labels(cx).unwrap_or_default();
                                        labels.retain(|l| *l != id);
                                        this.labels = Some(labels);
                                        cx.notify();
                                    });
                                }),
                        ),
                )
            })
            .collect();
        let mut others: Vec<(LabelId, String)> = (catalog.labels.iter())
            .filter(|label| !shown.contains(&label.id))
            .map(|label| (label.id.clone(), label.name.clone()))
            .collect();
        others.sort_by_key(|(_, name)| name.to_lowercase());
        let add = (!others.is_empty()).then(|| {
            Button::new("new-agent-add-label")
                .ghost()
                .xsmall()
                .child(ui::icon("plus", 12., p.muted))
                .child("Add")
                .dropdown_menu(move |mut menu, _, _| {
                    for (id, name) in &others {
                        let (this, id) = (this.clone(), id.clone());
                        menu = menu.item(PopupMenuItem::new(name.clone()).on_click(
                            move |_, _, cx| {
                                let id = id.clone();
                                let _ = this.update(cx, |this, cx| {
                                    let mut labels = this.shown_labels(cx).unwrap_or_default();
                                    if !labels.contains(&id) {
                                        labels.push(id);
                                    }
                                    this.labels = Some(labels);
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu.scrollable(true).max_h(px(320.))
                })
        });
        let note = match (&self.labels, self.step) {
            (Some(_), _) => "This agent's own",
            (None, Step::Workspace) => "Those this workspace's agents share",
            (None, _) => "This agent's own",
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(heading("Labels"))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(6.))
                        .children(chips)
                        .when(shown.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(size::sm())
                                    .text_color(p.extra_muted)
                                    .child("No labels"),
                            )
                        })
                        .children(add)
                        .child(
                            div()
                                .text_size(size::sm())
                                .text_color(p.extra_muted)
                                .child(note),
                        ),
                )
                .into_any_element(),
        )
    }

    fn message_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let optional = self.step == Step::NewWorkspace;
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(heading(if optional {
                "First message (none makes only the workspace)"
            } else {
                "First message"
            }))
            .child(
                div()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(p.border)
                    .bg(p.surface0)
                    .p(px(8.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .when(!self.attachments.images.is_empty(), |d| {
                        d.child(attachments::strip(
                            &self.attachments.images,
                            cx.entity().downgrade(),
                        ))
                    })
                    .when(self.attachments.pending, |d| {
                        d.child(div().text_color(p.muted).child("Preparing image…"))
                    })
                    .child(
                        Textarea::new(&self.input)
                            .appearance(false)
                            .text_size(size::content())
                            .on_paste(attachments::paste_handler(cx.entity().downgrade())),
                    ),
            )
            .into_any_element()
    }
}

impl Render for NewAgent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            self.focused = true;
            self.focus_first(window, cx);
        }
        let p = palette();
        let error = self
            .attachments
            .error
            .clone()
            .or_else(|| self.error.clone());
        let label = match (self.step, self.input.read(cx).value().trim().is_empty()) {
            (Step::NewWorkspace, true) => "Make workspace",
            _ => "Create",
        };
        let tall = window.viewport_size().height * 0.72;
        let labels = self.labels_section(cx);
        div()
            .key_context("NewAgent")
            .on_action(cx.listener(|this, _: &CreateAgent, window, cx| this.create(window, cx)))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .id("new-agent-body")
                    .max_h(tall)
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .pr(px(4.))
                    .child(self.where_section(cx))
                    .child(self.agent_section(cx))
                    .children(labels)
                    .child(self.message_section(cx)),
            )
            .when_some(error, |d, error| {
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
                            .child("Enter creates · Shift+Enter for a new line · Esc closes"),
                    )
                    .child(
                        Button::new("new-agent-cancel")
                            .label("Cancel")
                            .ghost()
                            .small()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("new-agent-create")
                            .label(label)
                            .primary()
                            .small()
                            .loading(self.creating)
                            .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                    ),
            )
    }
}
