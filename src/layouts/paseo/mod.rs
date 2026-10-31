//! The reference layout: Paseo's sidebar and workspace screen, driven by the
//! shared fleet and navigation. It owns views, not state.

pub(crate) mod attachments;
mod composer;
mod conversation;
mod prompt_index;
mod question_form;
mod screens;
mod sidebar;
mod terminal;
mod workspace;

pub use composer::bind_keys;
pub use conversation::{Conversation, ConversationEvent};
pub use screens::{SETTINGS_WIDTH, SettingsPane};
pub use sidebar::{Sidebar, add_project};
pub use terminal::TerminalView;
pub use terminal::bind_keys as bind_terminal_keys;
pub use workspace::{close_tab, open_new_agent, open_new_terminal};

use crate::ui::CachedExt as _;
use gpui_kit::gpui::{
    AnyElement, AnyView, App, AppContext, Context, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseMoveEvent, ParentElement, Render, Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;

use crate::app::{Core, Screen};
use crate::theme::{palette, size};
use screens::{HistoryScreen, HostsScreen, NewWorkspaceScreen};
use workspace::WorkspaceView;

pub fn build(core: &Core, window: &mut Window, cx: &mut App) -> AnyView {
    let core = core.clone();
    cx.new(|cx| PaseoLayout::new(core, window, cx)).into()
}

pub struct PaseoLayout {
    core: Core,
    pub sidebar: Entity<Sidebar>,
    workspace: Entity<WorkspaceView>,
    hosts: Option<Entity<HostsScreen>>,
    new_workspace: Option<Entity<NewWorkspaceScreen>>,
    history: Option<Entity<HistoryScreen>>,
    last_screen: Screen,
    resizing: bool,
}

impl PaseoLayout {
    pub fn new(core: Core, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| Sidebar::new(core.clone(), cx));
        let workspace = cx.new(|cx| WorkspaceView::new(core.clone(), cx));
        cx.observe(&core.nav, |_, _, cx| cx.notify()).detach();
        Self {
            core,
            sidebar,
            workspace,
            hosts: None,
            new_workspace: None,
            history: None,
            last_screen: Screen::Workspace,
            resizing: false,
        }
    }

    pub fn workspace(&self) -> &Entity<WorkspaceView> {
        &self.workspace
    }
}

impl Render for PaseoLayout {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let nav = self.core.nav.read(cx);
        let open = nav.sidebar_open;
        let width = nav.sidebar_width;
        let screen = nav.screen.clone();

        let content: AnyElement = match screen {
            Screen::Workspace => self.workspace.clone().cached_fill(),
            Screen::Hosts => {
                let core = self.core.clone();
                self.hosts
                    .get_or_insert_with(|| cx.new(|cx| HostsScreen::new(core, window, cx)))
                    .clone()
                    .cached_fill()
            }
            Screen::NewWorkspace { .. } => {
                let core = self.core.clone();
                self.new_workspace
                    .get_or_insert_with(|| cx.new(|cx| NewWorkspaceScreen::new(core, window, cx)))
                    .clone()
                    .cached_fill()
            }
            Screen::History => {
                let core = self.core.clone();
                let created = self.history.is_none();
                let view = self
                    .history
                    .get_or_insert_with(|| cx.new(|cx| HistoryScreen::new(core, window, cx)))
                    .clone();
                if !created && self.last_screen != Screen::History {
                    view.update(cx, |view, cx| view.reload(cx));
                }
                view.cached_fill()
            }
        };

        self.last_screen = screen.clone();
        div()
            .size_full()
            .flex()
            .bg(p.surface0)
            .text_color(p.foreground)
            .text_size(size::base())
            .when(self.resizing, |d| {
                d.on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                    let x = f32::from(event.position.x);
                    let width = x.clamp(size::SIDEBAR_MIN, size::SIDEBAR_MAX);
                    this.core.nav.update(cx, |nav, cx| {
                        nav.sidebar_width = width;
                        cx.notify();
                    });
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.resizing = false;
                        this.core.nav.update(cx, |nav, _| nav.save());
                        cx.notify();
                    }),
                )
            })
            .when(open, |d| {
                d.child(
                    div()
                        .relative()
                        .w(px(width))
                        .h_full()
                        .flex_shrink_0()
                        .child(self.sidebar.clone().cached_fill())
                        .child(
                            div()
                                .id("sidebar-resize")
                                .absolute()
                                .top_0()
                                .right(px(-5.))
                                .w(px(10.))
                                .h_full()
                                .cursor_col_resize()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.resizing = true;
                                        cx.notify();
                                    }),
                                ),
                        ),
                )
            })
            .child(div().flex_1().min_w(px(0.)).h_full().child(content))
    }
}
