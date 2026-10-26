//! A bounded, searchable label editor. Changes are applied together so the
//! containing card does not move while the user is still choosing.
//!
//! It edits a workspace's Paseo labels, or, in reef mode, the labels of the
//! agents it is for: Reef's catalog offered, those every one of them has
//! ticked ([`fleet::labels::workspace_default`]).

use crate::{
    app::{Core, store::LabelChange},
    theme::{palette, size},
    ui,
};
use fleet::HostId;
use fleet::directory::label_key;
use gpui_kit::component::{
    Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
use gpui_kit::gpui::{
    App, AppContext, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Pixels, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;
use std::collections::BTreeMap;

gpui_kit::actions!(workspace_labels, [NextLabel, PreviousLabel, AddLabel]);

pub fn bind_keys(cx: &mut App) {
    use gpui_kit::gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("down", NextLabel, Some("WorkspaceLabels > Input")),
        KeyBinding::new("up", PreviousLabel, Some("WorkspaceLabels > Input")),
        // Handled here: the search field passes Enter on, and the popover
        // around the picker takes it as "close".
        KeyBinding::new("enter", AddLabel, Some("WorkspaceLabels > Input")),
    ]);
}

const DEFAULT_LABEL_COLOR: &str = "violet";

#[derive(Clone)]
struct Choice {
    name: String,
    color: String,
    assigned: bool,
    initial: bool,
}

#[derive(Default)]
struct Choices(BTreeMap<String, Choice>);

impl Choices {
    fn load(core: &Core, host: &HostId, workspace: &str, agents: &[String], cx: &App) -> Self {
        let mut choices = Self::default();
        let store = core.store.read(cx);
        if let Some(catalog) = &store.fleet.labels
            && !agents.is_empty()
        {
            for label in &catalog.labels {
                choices.0.entry(label_key(&label.name)).or_insert(Choice {
                    name: label.name.clone(),
                    color: label.color.clone(),
                    assigned: false,
                    initial: false,
                });
            }
            let found = store.fleet.host(host);
            let directory = found.map(|host| &host.directory);
            let workspace = directory.and_then(|d| d.workspaces.get(workspace));
            let agents = (directory.into_iter())
                .flat_map(|d| agents.iter().filter_map(|id| d.agents.get(id)));
            let shared = match workspace {
                Some(workspace) => fleet::labels::workspace_default(agents, workspace, catalog),
                None => {
                    let mut agents = agents;
                    (agents.next()).map_or_else(Vec::new, |agent| {
                        fleet::labels::effective(agent, None, catalog)
                    })
                }
            };
            for id in shared {
                if let Some(label) = catalog.get(&id)
                    && let Some(choice) = choices.0.get_mut(&label_key(&label.name))
                {
                    choice.assigned = true;
                    choice.initial = true;
                }
            }
            return choices;
        }
        // Each host keeps its own labels, but Lanes treats a name as one
        // label across the fleet: offer them all, as this workspace's host
        // names and colours them where it has them. Picking one another host
        // made creates it here.
        let hosts = store
            .fleet
            .host(host)
            .into_iter()
            .chain(store.fleet.hosts.iter().filter(|other| other.id() != host));
        for other in hosts {
            for label in &other.directory.labels {
                choices.0.entry(label_key(&label.name)).or_insert(Choice {
                    name: label.name.clone(),
                    color: label.color.clone(),
                    assigned: false,
                    initial: false,
                });
            }
        }
        if let Some(workspace) = store.fleet.workspace(host, workspace) {
            for name in &workspace.labels {
                let choice = choices.0.entry(label_key(name)).or_insert(Choice {
                    name: name.clone(),
                    color: DEFAULT_LABEL_COLOR.into(),
                    assigned: false,
                    initial: false,
                });
                choice.assigned = true;
                choice.initial = true;
            }
        }
        choices
    }

    fn matching(&self, query: &str) -> Vec<(String, Choice)> {
        let query = label_key(query);
        self.0
            .iter()
            .filter(|(key, _)| key.contains(&query))
            .map(|(key, choice)| (key.clone(), choice.clone()))
            .collect()
    }

    fn add(&mut self, name: &str) -> bool {
        let name = name.trim();
        let key = label_key(name);
        if key.is_empty() {
            return false;
        }
        self.0
            .entry(key)
            .or_insert(Choice {
                name: name.into(),
                color: DEFAULT_LABEL_COLOR.into(),
                assigned: false,
                initial: false,
            })
            .assigned = true;
        true
    }

    fn changes(&self) -> Vec<LabelChange> {
        self.0
            .values()
            .filter(|choice| choice.assigned != choice.initial)
            .map(|choice| LabelChange {
                name: choice.name.clone(),
                color: choice.color.clone(),
                assigned: choice.assigned,
            })
            .collect()
    }
}

pub struct ApplyLabels(pub Vec<LabelChange>);

pub struct LabelPicker {
    core: Core,
    host: HostId,
    workspace: String,
    /// In reef mode, the agents whose own labels it edits; none edits the
    /// workspace's.
    agents: Vec<String>,
    input: Entity<InputState>,
    bounds: Bounds<Pixels>,
    choices: Choices,
    highlighted: Option<usize>,
    open: bool,
    scroll: ScrollHandle,
}
impl EventEmitter<ApplyLabels> for LabelPicker {}
impl Focusable for LabelPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl LabelPicker {
    pub fn new(
        core: Core,
        host: &HostId,
        workspace: &str,
        agents: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find or create a label…"));
        // Enter is the AddLabel action, not the field's PressEnter.
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.highlighted = None;
                cx.notify();
            }
        })
        .detach();
        Self {
            choices: Choices::load(&core, host, workspace, &agents, cx),
            core,
            host: host.clone(),
            workspace: workspace.into(),
            agents,
            input,
            bounds: Bounds::default(),
            highlighted: None,
            open: false,
            scroll: ScrollHandle::new(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn set_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.open == open {
            return;
        }
        self.open = open;
        if open {
            self.choices = Choices::load(&self.core, &self.host, &self.workspace, &self.agents, cx);
            self.highlighted = None;
            self.input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
        }
        cx.notify();
    }

    /// The popover host supplies its card bounds during prepaint; changed
    /// constraints redraw an open editor once, including on window resize.
    pub fn set_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if self.bounds != bounds {
            self.bounds = bounds;
            if self.open {
                cx.notify();
            }
        }
    }

    fn add_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().to_string();
        let rows = self.choices.matching(&query);
        if let Some(index) = self.highlighted {
            if let Some((key, _)) = rows.get(index)
                && let Some(choice) = self.choices.0.get_mut(key)
            {
                choice.assigned = !choice.assigned;
            }
        } else if !self.choices.add(&query) {
            return;
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.highlighted = None;
        cx.notify();
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = self
            .choices
            .matching(self.input.read(cx).value().as_ref())
            .len();
        if count == 0 {
            return;
        }
        let index = if forward {
            self.highlighted.map_or(0, |i| (i + 1).min(count - 1))
        } else {
            self.highlighted.unwrap_or(1).saturating_sub(1)
        };
        self.highlighted = Some(index);
        self.scroll.scroll_to_item(index);
        cx.notify();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let changes = self.choices.changes();
        self.set_open(false, window, cx);
        if !changes.is_empty() {
            cx.emit(ApplyLabels(changes));
        }
    }
}

impl Render for LabelPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let query = self.input.read(cx).value().to_string();
        let rows = self.choices.matching(&query);
        let new_name = query.trim().to_string();
        let can_create =
            !new_name.is_empty() && !self.choices.0.contains_key(&label_key(&new_name));
        let reef_mode = self.core.store.read(cx).fleet.labels.is_some() && !self.agents.is_empty();
        let title = match (reef_mode, self.agents.len()) {
            (false, _) => "Workspace labels",
            (true, 1) => "This agent's labels",
            (true, _) => "These agents' labels",
        };
        let bounds = self.bounds;
        let width = (bounds.size.width - px(4.)).max(px(1.)).min(px(320.));
        let height = (bounds.size.height - px(4.))
            .max(px(1.))
            .min(px(360.))
            .min(window.viewport_size().height - px(24.));
        div()
            .id("label-editor")
            .w(width)
            .max_h(height)
            .min_h(px(0.))
            .p(px(8.))
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .flex()
            .flex_col()
            .gap(px(6.))
            .key_context("WorkspaceLabels")
            .on_action(cx.listener(|this, _: &NextLabel, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousLabel, _, cx| this.step(false, cx)))
            .on_action(cx.listener(|this, _: &AddLabel, window, cx| this.add_query(window, cx)))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(size::sm())
                    .text_color(p.muted)
                    .child(title),
            )
            .child(div().flex_shrink_0().child(Input::new(&self.input).small()))
            .when(can_create, |d| {
                d.child(
                    Button::new("create-label")
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .child(format!("Add “{new_name}”")),
                        )
                        .w_full()
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.highlighted = None;
                            this.add_query(window, cx);
                        })),
                )
            })
            .child(
                div()
                    .id("label-options")
                    .h(px((rows.len() as f32 * 30.).min(240.)))
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .restrict_scroll_to_axis()
                    .children(rows.into_iter().enumerate().map(|(index, (key, choice))| {
                        let assigned = choice.assigned;
                        div()
                            .id(SharedString::from(format!("label-{key}")))
                            .min_h(px(30.))
                            .py(px(4.))
                            .text_size(size::sm())
                            .line_height(px(18.))
                            .flex_shrink_0()
                            .px(px(6.))
                            .rounded(px(4.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .when(self.highlighted == Some(index), |d| d.bg(p.surface2))
                            .hover(|d| d.bg(p.surface2))
                            .child(div().flex_1().min_w(px(0.)).child(choice.name))
                            .child(
                                div()
                                    .w(px(14.))
                                    .flex_shrink_0()
                                    .when(assigned, |d| d.child(ui::icon("check", 14., p.accent))),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(choice) = this.choices.0.get_mut(&key) {
                                    choice.assigned = !choice.assigned;
                                }
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .gap(px(6.))
                    .child(
                        Button::new("cancel-labels")
                            .label("Cancel")
                            .ghost()
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.set_open(false, window, cx)),
                            ),
                    )
                    .child(
                        Button::new("apply-labels")
                            .label("Apply")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn label_search_creation_and_toggle_diff_keep_existing_colors() {
        let mut choices = Choices::default();
        choices.0.insert(
            "devtools".into(),
            Choice {
                name: "Devtools".into(),
                color: "teal".into(),
                assigned: true,
                initial: true,
            },
        );
        assert_eq!(choices.matching(" TOOL ").len(), 1);
        choices.add("  DEVTOOLS  ");
        assert_eq!(choices.0.len(), 1);
        assert!(choices.changes().is_empty());
        choices.0.get_mut("devtools").unwrap().assigned = false;
        assert!(choices.add("New label"));
        assert!(!choices.add("   "));
        let changes = choices.changes();
        assert_eq!(
            changes[0],
            LabelChange {
                name: "Devtools".into(),
                color: "teal".into(),
                assigned: false
            }
        );
        assert_eq!(
            changes[1],
            LabelChange {
                name: "New label".into(),
                color: DEFAULT_LABEL_COLOR.into(),
                assigned: true
            }
        );
    }
}
