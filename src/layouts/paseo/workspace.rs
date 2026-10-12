//! The selected workspace: header, tab row, and the focused tab's content.

use crate::ui::CachedExt as _;
use std::collections::HashMap;

use fleet::{Bucket, requests};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, Sizable};
use gpui_kit::gpui::{
    AnyElement, AppContext, ClickEvent, Context, CursorStyle, Entity, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;

use super::conversation::{Conversation, ConversationEvent};
use super::draft::Draft;
use super::terminal::TerminalView;
use crate::app::{Closed, Core, FleetEvent, Tab};
use crate::theme::{palette, size};
use crate::ui;
use crate::widgets::agent_rename::{AgentRename, RenameDone};
use gpui_kit::component::tooltip::Tooltip;

pub struct WorkspaceView {
    core: Core,
    conversations: HashMap<(String, String), Entity<Conversation>>,
    drafts: HashMap<(String, String), Entity<Draft>>,
    terminals: HashMap<(String, String), Entity<TerminalView>>,
    /// The active agent tab's title being edited in place.
    renaming: Option<(Entity<AgentRename>, Subscription)>,
}

impl WorkspaceView {
    pub fn new(core: Core, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.nav, |this, nav, cx| {
            // Drop views for tabs that no longer exist.
            let open = nav.read(cx).open_agents();
            this.conversations.retain(|key, _| open.contains(key));
            let terminals = nav.read(cx).open_terminals();
            this.terminals.retain(|key, _| terminals.contains(key));
            cx.notify();
        })
        .detach();
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        Self {
            core,
            conversations: HashMap::new(),
            drafts: HashMap::new(),
            terminals: HashMap::new(),
            renaming: None,
        }
    }

    /// Edits an agent tab's title in place.
    fn start_rename(
        &mut self,
        host: &str,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let editor = cx.new(|cx| AgentRename::new(&core, host, agent, window, cx));
        let done = cx.subscribe(&editor, |this, _, _: &RenameDone, cx| {
            this.renaming = None;
            cx.notify();
        });
        self.renaming = Some((editor, done));
        cx.notify();
    }

    fn conversation(
        &mut self,
        host: &str,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Conversation> {
        let key = (host.to_string(), agent.to_string());
        if let Some(view) = self.conversations.get(&key) {
            return view.clone();
        }
        let core = self.core.clone();
        let view = cx.new(|cx| Conversation::new(core, host, agent, window, cx));
        // A Paseo subagent opened from the track opens in a tab beside its
        // parent's, in its own workspace.
        cx.subscribe(
            &view,
            |this, _, event: &ConversationEvent, cx| match event {
                ConversationEvent::OpenAgent { host, agent } => {
                    let Some(workspace) = this
                        .core
                        .store
                        .read(cx)
                        .fleet
                        .agent(host, agent)
                        .and_then(|record| record.workspace_id.clone())
                    else {
                        return;
                    };
                    let (host, agent) = (host.clone(), agent.clone());
                    this.core.nav.update(cx, |nav, cx| {
                        if nav.selection.as_ref().is_none_or(|current| {
                            current.host != host || current.workspace != workspace
                        }) {
                            nav.select(&host, &workspace, cx);
                        }
                        let key = format!("{host}:{workspace}");
                        nav.focus_tab(&key, Tab::Agent(agent), cx);
                    });
                }
            },
        )
        .detach();
        self.conversations.insert(key, view.clone());
        view
    }

    fn draft(
        &mut self,
        host: &str,
        workspace: &str,
        draft: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Draft> {
        let key = (format!("{host}:{workspace}"), draft.to_string());
        if let Some(view) = self.drafts.get(&key) {
            return view.clone();
        }
        let core = self.core.clone();
        let view = cx.new(|cx| Draft::new(core, host, workspace, draft, window, cx));
        self.drafts.insert(key, view.clone());
        view
    }

    pub fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.core.nav.read(cx).selection.clone() else {
            return;
        };
        let key = selection.key();
        match self.core.nav.read(cx).focused_tab(&key).cloned() {
            Some(Tab::Agent(id)) => {
                let view = self.conversation(&selection.host, &id, window, cx);
                view.update(cx, |view, cx| view.focus_composer(window, cx));
            }
            Some(Tab::Terminal(id)) => {
                let view = self.terminal(&selection.host, &id, window, cx);
                view.update(cx, |view, cx| view.focus(window, cx));
            }
            _ => {}
        }
    }

    fn terminal(
        &mut self,
        host: &str,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        let key = (host.to_string(), id.to_string());
        if let Some(view) = self.terminals.get(&key) {
            return view.clone();
        }
        let core = self.core.clone();
        let view = cx.new(|cx| TerminalView::new(core, host, id, window, cx));
        self.terminals.insert(key, view.clone());
        view
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let Some(selection) = self.core.nav.read(cx).selection.clone() else {
            return empty_workspace(
                "Select a workspace",
                "Pick a workspace from the sidebar, or create one.",
            );
        };
        let store = self.core.store.read(cx);
        let Some(workspace) = store
            .fleet
            .workspace(&selection.host, &selection.workspace)
            .cloned()
        else {
            let host = store.fleet.host(&selection.host);
            let message = match host {
                Some(host) if !host.connection.is_online() => format!(
                    "{} is {}",
                    selection.host,
                    host.connection.label().to_lowercase()
                ),
                Some(host) if !host.directory.hydrated => "Loading workspaces…".to_string(),
                _ => "This workspace is gone".to_string(),
            };
            return empty_workspace("Workspace unavailable", &message);
        };
        let host_online = store
            .fleet
            .host(&selection.host)
            .is_some_and(|h| h.connection.is_online());
        let key = selection.key();
        let nav = self.core.nav.read(cx);
        let tabs = nav.tabs_for(&key).to_vec();
        let focused = nav.focused_tab(&key).cloned();
        // Borrowed, not cloned: a host's directory holds every agent and
        // workspace it has, and this renders on every resize frame.
        let directory = store.fleet.host(&selection.host).map(|h| &h.directory);
        let agent = |id: &str| directory.and_then(|d| d.agents.get(id));

        let project = directory
            .and_then(|d| d.projects.get(&workspace.project_id))
            .map(|p| p.name())
            .unwrap_or_else(|| workspace.project_display_name.clone());
        let branch = workspace.branch().map(str::to_string);
        let agent_title = match &focused {
            Some(Tab::Agent(id)) => agent(id)
                .map(|agent| agent.display_title())
                .unwrap_or_else(|| "this agent".to_string()),
            _ => workspace.name.clone(),
        };
        let tab_labels: Vec<(String, Bucket)> = tabs
            .iter()
            .map(|tab| match tab {
                Tab::Agent(id) => match agent(id) {
                    Some(agent) => (agent.display_title(), agent.bucket()),
                    None => ("Agent".to_string(), Bucket::Done),
                },
                Tab::Draft(_) => ("New Agent".to_string(), Bucket::Done),
                Tab::Terminal(id) => (store.terminal_label(&selection.host, id, cx), Bucket::Done),
            })
            .collect();
        // Extensions' buttons, for this workspace and the agent shown.
        let workspace_actions = self.core.extensions.read(cx).workspace_actions.clone();
        let target = crate::extensions::WorkspaceTarget {
            host: selection.host.clone(),
            workspace: selection.workspace.clone(),
            agent: match &focused {
                Some(Tab::Agent(id)) => Some(id.clone()),
                _ => None,
            },
            agent_title: agent_title.to_string(),
        };

        // Header.
        let header = div()
            .h(px(size::HEADER_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .border_b_1()
            .border_color(p.border)
            .child(div().text_color(p.foreground).child(workspace.name.clone()))
            .child(
                div()
                    .text_size(size::sm())
                    .text_color(p.muted)
                    .child(project),
            )
            .when_some(branch, |d, branch| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .text_size(size::sm())
                        .text_color(p.muted)
                        .child(ui::icon("git-branch", 12., p.muted))
                        .child(branch),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .text_size(size::sm())
                    .text_color(p.extra_muted)
                    .child(ui::icon("server", 12., p.extra_muted))
                    .child(selection.host.clone()),
            )
            .when(!host_online, |d| {
                d.child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.status_warning)
                        .child("offline"),
                )
            })
            .child(div().flex_1())
            .children(
                workspace_actions
                    .into_iter()
                    .enumerate()
                    .map(|(ix, action)| {
                        let target = target.clone();
                        div()
                            .id(("workspace-action", ix))
                            .flex_shrink_0()
                            .px(px(8.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .text_size(size::sm())
                            .text_color(p.muted)
                            .hover(|style| style.bg(p.surface2).text_color(p.foreground))
                            .children(action.icon.map(|icon| ui::icon(icon, 12., p.muted)))
                            .child(action.label.clone())
                            .on_click(move |_: &ClickEvent, window, cx| {
                                (action.run)(&target, window, cx)
                            })
                    }),
            )
            .when(workspace.diff_stat.is_some(), |d| {
                let stat = workspace.diff_stat.clone().unwrap_or_default();
                d.child(
                    div()
                        .flex()
                        .gap(px(4.))
                        .text_size(size::sm())
                        .child(
                            div()
                                .text_color(p.status_success)
                                .child(format!("+{}", stat.additions)),
                        )
                        .child(
                            div()
                                .text_color(p.status_danger)
                                .child(format!("-{}", stat.deletions)),
                        ),
                )
            });

        // Tab row.
        let mut tab_row = div()
            .id("tab-row")
            .h(px(size::HEADER_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(4.))
            .bg(p.surface0)
            .border_b_1()
            .border_color(p.border)
            .overflow_x_scroll();
        for (tab, (label, bucket)) in tabs.iter().zip(tab_labels) {
            let active = focused.as_ref() == Some(tab);
            let tab_id = tab.key();
            let group: SharedString = format!("tab-{tab_id}").into();
            let tab_for_click = tab.clone();
            let tab_for_close = tab.clone();
            let key_click = key.clone();
            let key_close = key.clone();
            let leading: AnyElement = match tab {
                Tab::Draft(_) => ui::icon("square-pen", 14., p.muted).into_any_element(),
                Tab::Terminal(_) => ui::icon("square-terminal", 14., p.muted).into_any_element(),
                Tab::Agent(_) => ui::agent_dot(bucket).into_any_element(),
            };
            tab_row = tab_row.child(
                div()
                    .id(SharedString::from(format!("tab-{tab_id}")))
                    .group(group.clone())
                    .relative()
                    .h(px(size::TAB_HEIGHT))
                    .min_w(px(96.))
                    .max_w(px(200.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .when(active, |d| d.bg(p.surface2))
                    .when(!active, |d| d.hover(|s| s.bg(p.surface1)))
                    .child(leading)
                    .child(match (tab, active) {
                        // The active agent tab's name edits in place.
                        (Tab::Agent(agent), true) => {
                            let host = selection.host.clone();
                            let editor = self
                                .renaming
                                .as_ref()
                                .map(|(editor, _)| editor)
                                .filter(|editor| editor.read(cx).edits(&host, agent))
                                .cloned();
                            match editor {
                                // A tab is as wide as its label; give the edit room.
                                Some(editor) => div().w(px(170.)).child(editor).into_any_element(),
                                None => {
                                    let agent = agent.clone();
                                    div()
                                        .id(SharedString::from(format!("tab-title-{tab_id}")))
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .text_color(p.foreground)
                                        .cursor(CursorStyle::IBeam)
                                        .child(label)
                                        .tooltip(|window, cx| {
                                            Tooltip::new("Rename agent").build(window, cx)
                                        })
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                cx.stop_propagation();
                                                this.start_rename(&host, &agent, window, cx);
                                            },
                                        ))
                                        .into_any_element()
                                }
                            }
                        }
                        _ => div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(if active { p.foreground } else { p.muted })
                            .child(label)
                            .into_any_element(),
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("tab-close-{tab_id}")))
                            .size(px(18.))
                            .rounded(px(4.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .hover(|s| s.bg(p.surface3))
                            .child(ui::icon("x", 12., p.muted))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                close_tab(&this.core, &key_close, &tab_for_close, window, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let tab = tab_for_click.clone();
                        let key = key_click.clone();
                        this.core
                            .nav
                            .update(cx, |nav, cx| nav.focus_tab(&key, tab, cx));
                    }))
                    .context_menu({
                        let core = self.core.clone();
                        let key = key.clone();
                        let tab = tab.clone();
                        let host = selection.host.clone();
                        move |menu, _, cx| tab_menu(menu, &core, &host, &key, &tab, cx)
                    }),
            );
        }
        let key_new = key.clone();
        let core_new = self.core.clone();
        tab_row = tab_row.child(
            Button::new("new-tab")
                .icon(Icon::empty().path("icons/plus.svg"))
                .ghost()
                .xsmall()
                .tooltip("New")
                .dropdown_menu(move |menu, _, _| {
                    let key_new = key_new.clone();
                    let core_new = core_new.clone();
                    menu.item({
                        let nav = core_new.nav.clone();
                        PopupMenuItem::new("New agent").on_click(move |_, _, cx| {
                            let key = key_new.clone();
                            nav.update(cx, |nav, cx| nav.new_draft(&key, cx));
                        })
                    })
                    .item({
                        let core_new = core_new.clone();
                        PopupMenuItem::new("New terminal").on_click(move |_, _, cx| {
                            open_new_terminal(&core_new, cx);
                        })
                    })
                }),
        );

        let body: AnyElement = match focused {
            Some(Tab::Agent(id)) => self
                .conversation(&selection.host, &id, window, cx)
                .cached_fill(),
            Some(Tab::Terminal(id)) => self
                .terminal(&selection.host, &id, window, cx)
                .cached_fill(),
            Some(Tab::Draft(draft)) => self
                .draft(&selection.host, &selection.workspace, &draft, window, cx)
                .cached_fill(),
            None => {
                let key = key.clone();
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.))
                    .text_color(p.muted)
                    .child("No agents in this workspace")
                    .child(
                        Button::new("start-agent")
                            .label("New agent")
                            .primary()
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let key = key.clone();
                                this.core.nav.update(cx, |nav, cx| {
                                    nav.new_draft(&key, cx);
                                });
                            })),
                    )
                    .into_any_element()
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface0)
            .child(header)
            .child(tab_row)
            .child(div().flex_1().min_h(px(0.)).child(body))
            .into_any_element()
    }
}

fn empty_workspace(title: &str, body: &str) -> AnyElement {
    let p = palette();
    div()
        .size_full()
        .bg(p.surface0)
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .child(div().text_color(p.foreground).child(title.to_string()))
        .child(
            div()
                .text_size(size::sm())
                .text_color(p.muted)
                .child(body.to_string()),
        )
        .into_any_element()
}

/// Closes a tab the way Paseo does: an agent tab archives the agent, and a
/// running agent asks first. Closing a workspace's last agent offers to
/// archive the workspace instead.
pub fn close_tab(
    core: &Core,
    key: &str,
    tab: &Tab,
    window: &mut Window,
    cx: &mut gpui_kit::gpui::App,
) {
    let host = key
        .rsplit_once(':')
        .map(|(h, _)| h.to_string())
        .unwrap_or_default();
    if let Tab::Agent(agent) = tab {
        crate::workspace_archive::close_agent_tab(
            core,
            &host,
            agent,
            crate::workspace_archive::AgentCloseOrigin::Tab(key.to_string()),
            window,
            cx,
        );
        return;
    }
    let terminal = matches!(tab, Tab::Terminal(_));
    let core = core.clone();
    let key = key.to_string();
    let tab = tab.clone();
    let close = move |cx: &mut gpui_kit::gpui::App| {
        let closed = core.nav.update(cx, |nav, cx| nav.close_tab(&key, &tab, cx));
        if let Some(Closed::Terminal(id)) = closed {
            core.store
                .update(cx, |store, cx| store.kill_terminal(&host, &id, cx));
        }
    };
    if terminal {
        ui::confirm(
            window,
            cx,
            "Close terminal?",
            "This stops the shell and closes the PTY.",
            "Close",
            close,
        );
    } else {
        close(cx);
    }
}

/// Creates a terminal in the selected workspace and focuses its tab.
pub fn open_new_terminal(core: &Core, cx: &mut gpui_kit::gpui::App) {
    let Some(selection) = core.nav.read(cx).selection.clone() else {
        return;
    };
    let cwd = core
        .store
        .read(cx)
        .fleet
        .workspace(&selection.host, &selection.workspace)
        .map(|workspace| workspace.directory().to_string())
        .filter(|cwd| !cwd.is_empty());
    let Some(cwd) = cwd else {
        core.store.update(cx, |_, cx| {
            cx.emit(FleetEvent::Toast("This workspace has no directory".into()));
        });
        return;
    };
    let host = selection.host.clone();
    let workspace = selection.workspace.clone();
    let key = selection.key();
    let nav = core.nav.clone();
    let store = core.store.clone();
    let task = core.store.update(cx, |store, cx| {
        store.create_terminal(&host, &workspace, &cwd, cx)
    });
    cx.spawn(async move |cx| match task.await {
        Ok(id) => {
            nav.update(cx, |nav, cx| nav.focus_tab(&key, Tab::Terminal(id), cx));
        }
        Err(err) => {
            store.update(cx, |_, cx| cx.emit(FleetEvent::Toast(err)));
        }
    })
    .detach();
}

/// Right-click on a tab: what Paseo's tab context menu offers for agents.
fn tab_menu(
    menu: PopupMenu,
    core: &Core,
    host: &str,
    key: &str,
    tab: &Tab,
    cx: &mut gpui_kit::gpui::App,
) -> PopupMenu {
    let mut menu = menu;
    if let Tab::Agent(agent_id) = tab {
        let title = core
            .store
            .read(cx)
            .fleet
            .agent(host, agent_id)
            .map(|a| a.display_title())
            .unwrap_or_default();
        {
            let core = core.clone();
            let host = host.to_string();
            let agent_id = agent_id.clone();
            menu = menu.item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                let core = core.clone();
                let host = host.clone();
                let agent_id = agent_id.clone();
                ui::prompt_text(
                    window,
                    cx,
                    "Rename agent",
                    &title,
                    "Rename",
                    move |value, cx| {
                        if value.is_empty() {
                            return;
                        }
                        let message = requests::rename_agent(&agent_id, &value);
                        core.store
                            .update(cx, |store, cx| store.act(&host, message, cx));
                    },
                );
            }));
        }
        {
            let agent_id = agent_id.clone();
            menu = menu.item(
                PopupMenuItem::new("Copy agent id").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(gpui_kit::gpui::ClipboardItem::new_string(
                        agent_id.clone(),
                    ));
                }),
            );
        }
        {
            let core = core.clone();
            let host = host.to_string();
            let agent_id = agent_id.clone();
            menu = menu.item(
                PopupMenuItem::new("Reload agent").on_click(move |_, _, cx| {
                    let message =
                        serde_json::json!({"type": "refresh_agent_request", "agentId": agent_id});
                    core.store
                        .update(cx, |store, cx| store.act(&host, message, cx));
                }),
            );
        }
        menu = menu.separator();
    }
    let core = core.clone();
    let key = key.to_string();
    let tab = tab.clone();
    menu.item(
        PopupMenuItem::new(match tab {
            Tab::Agent(_) => "Close (archives agent)",
            Tab::Terminal(_) => "Close terminal",
            Tab::Draft(_) => "Close",
        })
        .on_click(move |_, window, cx| close_tab(&core, &key, &tab, window, cx)),
    )
}
