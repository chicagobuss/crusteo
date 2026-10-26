//! Remove project: one host's project, after saying what goes
//! ([`fleet::project_remove`]). While an agent in it is working the dialog
//! only says so. A refusal shows in place; the project leaves the views
//! with the host's push, not from here.

use fleet::HostId;
use fleet::project_remove::{self, Check};
use fleet::requests;
use gpui_kit::component::Sizable;
use gpui_kit::component::WindowExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::gpui::{
    App, AppContext, Context, IntoElement, ParentElement, Render, Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;

use crate::app::Core;
use crate::theme::{palette, size};

/// "Remove project…" for a project on one host, or "Remove from <host>…"
/// per host for an entry merged across hosts; only hosts that can remove
/// projects. `projects` are `(host, projectId)`.
pub fn menu_items(
    mut menu: PopupMenu,
    core: &Core,
    projects: &[(HostId, String)],
    cx: &App,
) -> PopupMenu {
    let fleet = &core.store.read(cx).fleet;
    for (host, project) in projects {
        if !fleet.host(host).is_some_and(project_remove::supported) {
            continue;
        }
        let label = if projects.len() == 1 {
            "Remove project…".to_string()
        } else {
            format!("Remove from {}…", fleet.host_name(host))
        };
        let (core, host, project) = (core.clone(), host.clone(), project.clone());
        menu = menu.item(
            PopupMenuItem::new(label)
                .on_click(move |_, window, cx| open(&core, &host, &project, window, cx)),
        );
    }
    menu
}
/// Opens the confirmation for `project` on `host`.
pub fn open(core: &Core, host: &HostId, project: &str, window: &mut Window, cx: &mut App) {
    let name = (core.store.read(cx).fleet.host(host))
        .and_then(|record| project_remove::check(record, project))
        .map_or_else(|| project.to_string(), |check| check.name().to_string());
    let view = cx.new(|cx| {
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        RemoveProject {
            core: core.clone(),
            host: host.clone(),
            project: project.to_string(),
            removing: false,
            error: None,
        }
    });
    let title = format!("Remove project {name}?");
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title.clone()).w(px(440.)).child(view.clone())
    });
}

pub struct RemoveProject {
    core: Core,
    host: HostId,
    project: String,
    removing: bool,
    error: Option<String>,
}

impl RemoveProject {
    /// The project as its host lists it now.
    fn check(&self, cx: &App) -> Option<Check> {
        let fleet = &self.core.store.read(cx).fleet;
        project_remove::check(fleet.host(&self.host)?, &self.project)
    }

    fn remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Checked again: an agent may have started since the dialog opened.
        if self.removing || !matches!(self.check(cx), Some(Check::Ready { .. })) {
            return;
        }
        self.removing = true;
        self.error = None;
        cx.notify();
        let message = requests::remove_project(&self.project);
        let task = self.core.store.read(cx).write(&self.host, message, cx);
        cx.spawn_in(window, async move |this, cx| {
            let outcome = task
                .await
                .and_then(|reply| project_remove::parse_reply(&reply));
            let _ = this.update_in(cx, |this, window, cx| {
                this.removing = false;
                match outcome {
                    Ok(_) => window.close_dialog(cx),
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

impl Render for RemoveProject {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let check = self.check(cx);
        let ready = matches!(check, Some(Check::Ready { .. }));
        let text = match &check {
            Some(check) => check.message(self.core.store.read(cx).fleet.host_name(&self.host)),
            // The host's push came before its reply.
            None if self.removing => "Removing…".into(),
            None => format!(
                "{} no longer lists this project.",
                self.core.store.read(cx).fleet.host_name(&self.host)
            ),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(div().text_color(p.muted).child(text))
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
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("remove-project-cancel")
                            .label(if ready { "Cancel" } else { "Close" })
                            .small()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .when(ready, |row| {
                        row.child(
                            Button::new("remove-project-ok")
                                .label("Remove project")
                                .danger()
                                .small()
                                .loading(self.removing)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.remove(window, cx)),
                                ),
                        )
                    }),
            )
    }
}
