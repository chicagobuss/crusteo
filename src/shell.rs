//! The window: title bar with the layout switcher and fleet summary, the
//! current layout, the command palette, global shortcuts, and toasts.

use crate::ui::CachedExt as _;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use fleet::Bucket;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::{TitleBar, WindowExt};
use gpui_kit::gpui::{
    AbsoluteLength, AnyElement, AnyView, App, AppContext, ClickEvent, Context, Decorations,
    DismissEvent, EdgesRefinement, Entity, FocusHandle, Focusable, Image, ImageFormat,
    InteractiveElement, IntoElement, KeyBinding, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Point, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    WindowBounds, WindowOptions, anchored, deferred, div, img, point, px, rgb, size,
};
use gpui_kit::prelude::FluentBuilder;

use crate::app::{Core, FleetEvent, LayoutEntry, Screen, Tab};
use crate::layouts::paseo::{PaseoLayout, Sidebar};
use crate::theme::{palette, size as sz};
use crate::toolbars;
use crate::ui;

gpui_kit::actions!(
    crusteo,
    [
        ToggleSidebar,
        CommandCenter,
        NewAgentTab,
        NewTerminal,
        CloseTab,
        NextWorkspace,
        PreviousWorkspace,
        NewWorkspace,
        OpenSettings,
        CycleLayout,
        FocusComposer,
        Jump1,
        Jump2,
        Jump3,
        Jump4,
        Jump5,
        Jump6,
        Jump7,
        Jump8,
        Jump9,
        Dismiss,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ShowHelp,
        CustomizeToolbars,
    ]
);

pub fn bind_keys(cx: &mut App) {
    // On Linux a focused terminal has to receive Ctrl+letter. Cmd shortcuts
    // on macOS stay with the app, so those bindings keep an empty context.
    let app = if cfg!(target_os = "macos") {
        None
    } else {
        Some("!Terminal")
    };
    cx.bind_keys([
        KeyBinding::new("secondary-b", ToggleSidebar, app),
        KeyBinding::new("secondary-k", CommandCenter, app),
        KeyBinding::new("secondary-t", NewAgentTab, app),
        KeyBinding::new("secondary-w", CloseTab, app),
        KeyBinding::new("secondary-]", NextWorkspace, app),
        KeyBinding::new("secondary-[", PreviousWorkspace, app),
        KeyBinding::new("secondary-n", NewWorkspace, app),
        KeyBinding::new("secondary-,", OpenSettings, app),
        KeyBinding::new("secondary-shift-l", CycleLayout, None),
        KeyBinding::new("secondary-shift-t", NewTerminal, None),
        KeyBinding::new("secondary-l", FocusComposer, app),
        KeyBinding::new("secondary-1", Jump1, app),
        KeyBinding::new("secondary-2", Jump2, app),
        KeyBinding::new("secondary-3", Jump3, app),
        KeyBinding::new("secondary-4", Jump4, app),
        KeyBinding::new("secondary-5", Jump5, app),
        KeyBinding::new("secondary-6", Jump6, app),
        KeyBinding::new("secondary-7", Jump7, app),
        KeyBinding::new("secondary-8", Jump8, app),
        KeyBinding::new("secondary-9", Jump9, app),
        KeyBinding::new("escape", Dismiss, Some("CommandCenter")),
        // Zoom works everywhere, as in a browser.
        KeyBinding::new("secondary-=", ZoomIn, None),
        KeyBinding::new("secondary-+", ZoomIn, None),
        KeyBinding::new("secondary-shift-=", ZoomIn, None),
        KeyBinding::new("secondary--", ZoomOut, None),
        KeyBinding::new("secondary-0", ZoomReset, None),
        KeyBinding::new("f1", ShowHelp, None),
        // Find on the Lanes board; a focused terminal keeps Ctrl+F.
        KeyBinding::new("secondary-f", crate::layouts::lanes::FindOnBoard, app),
        // Text fields bind it to their own find; in Lanes, the board's wins.
        KeyBinding::new(
            "secondary-f",
            crate::layouts::lanes::FindOnBoard,
            Some("Lanes > Input"),
        ),
        KeyBinding::new("secondary-/", ShowHelp, app),
    ]);
}

/// Extensions' commands on their default keys.
fn bind_commands(commands: &[crate::extensions::Command], cx: &mut App) {
    // As for the shell's own: a focused terminal keeps Ctrl+letter on Linux.
    let app = if cfg!(target_os = "macos") {
        None
    } else {
        Some("!Terminal")
    };
    cx.bind_keys(commands.iter().filter_map(|command| {
        Some(KeyBinding::new(
            command.key?,
            crate::extensions::RunCommand {
                id: command.id.clone(),
            },
            app,
        ))
    }));
}

pub struct Shell {
    core: Core,
    layouts: Vec<LayoutEntry>,
    views: HashMap<&'static str, AnyView>,
    paseo: Option<Entity<PaseoLayout>>,
    palette_input: Entity<InputState>,
    palette_index: usize,
    needs: Entity<crate::needs_you::NeedsYou>,
    focus: FocusHandle,
    /// The zoom just set, shown briefly; the counter lets a newer change
    /// keep it up.
    zoom_badge: Option<(f32, u64)>,
    /// The bars are being arranged: see [`toolbars::customize`].
    customizing: bool,
    /// The menu a right-click on a bar opened, and where.
    bar_menu: Option<(Point<Pixels>, Entity<PopupMenu>, Subscription)>,
    /// The help panel is open: see [`crate::help`].
    help_open: bool,
    /// The color last given the system's window border, and whether the
    /// system took it: see [`color_system_border`].
    system_border: (Option<u32>, bool),
}

/// Gives the system's own window border `color` (`0xrrggbb`), or its
/// default for none. Only Windows 11 draws such a border and lets it be
/// colored; elsewhere this does nothing and returns false.
#[cfg(windows)]
fn color_system_border(window: &Window, color: Option<u32>) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_COLOR_DEFAULT, DwmSetWindowAttribute,
    };
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return false;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return false;
    };
    // COLORREF is 0x00bbggrr.
    let value: u32 = color.map_or(DWMWA_COLOR_DEFAULT, |rgb| {
        ((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff)
    });
    // Safe: a live window's handle, and a u32 the call only reads.
    let result = unsafe {
        DwmSetWindowAttribute(
            win32.hwnd.get() as _,
            DWMWA_BORDER_COLOR as u32,
            (&value as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    result == 0 && color.is_some()
}

#[cfg(not(windows))]
fn color_system_border(_: &Window, _: Option<u32>) -> bool {
    false
}

/// Spacing at the title bar's left, after the app icon, which drags the
/// window as the empty bar does; and kept clear at its right, so the window
/// can be dragged from that end too.
const TITLE_CLEAR_LEFT: f32 = 10.;
const TITLE_CLEAR_RIGHT: f32 = 20.;

/// The app icon for the title bar, 18px. It is baked at 1x, 2x and 3x,
/// and the window shows the smallest that is not upscaled, so it stays crisp.
fn app_icon(window: &Window) -> impl IntoElement {
    static ICONS: std::sync::OnceLock<[Arc<Image>; 3]> = std::sync::OnceLock::new();
    let icons = ICONS.get_or_init(|| {
        [
            &include_bytes!("../assets/icon/title-18.png")[..],
            &include_bytes!("../assets/icon/title-36.png")[..],
            &include_bytes!("../assets/icon/title-54.png")[..],
        ]
        .map(|png| Arc::new(Image::from_bytes(ImageFormat::Png, png.to_vec())))
    });
    let scale = window.scale_factor();
    let pick = if scale <= 1. {
        0
    } else if scale <= 2. {
        1
    } else {
        2
    };
    img(icons[pick].clone()).size(px(18.)).flex_shrink_0()
}

impl Shell {
    fn new(core: Core, window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.set_zoom(core.nav.read(cx).zoom, cx);
        cx.observe(&core.nav, |_, _, cx| cx.notify()).detach();
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        cx.observe(&core.usage, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(
            &core.store,
            window,
            |this, _, event: &FleetEvent, window, cx| match event {
                FleetEvent::Toast(message) => {
                    // Feedback on something just done: in the app, in the
                    // corner notifications use.
                    let corner = this.core.nav.read(cx).notification_corner;
                    let note =
                        gpui_kit::component::notification::Notification::from(message.clone())
                            .placement(corner.anchor());
                    window.push_notification(note, cx)
                }
                FleetEvent::Attention {
                    host,
                    agent_id,
                    workspace_id,
                    reason,
                    title,
                    body,
                } => this.notify_attention(
                    host,
                    agent_id,
                    workspace_id.as_deref(),
                    reason,
                    title,
                    body,
                    window,
                    cx,
                ),
                FleetEvent::Changed => {}
            },
        )
        .detach();
        let palette_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search workspaces and agents…"));
        cx.subscribe_in(
            &palette_input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.palette_index = 0;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.run_palette(window, cx),
                _ => {}
            },
        )
        .detach();
        let needs = cx.new(|cx| crate::needs_you::NeedsYou::new(core.clone(), cx));
        cx.observe(&needs, |_, _, cx| cx.notify()).detach();
        let layouts: Vec<(&'static str, &'static str)> = crate::layouts::registry()
            .iter()
            .map(|entry| (entry.id, entry.title))
            .collect();
        let mut items = toolbars::builtin::items(&core, layouts);
        let host = crate::extensions::Host {
            core: core.clone(),
            needs: needs.clone(),
        };
        let mut contributed = crate::extensions::Contributed::default();
        let mut usage_sources = Vec::new();
        for extension in crate::extensions::take_pending(cx) {
            let parts = extension.contribute(&host, window, cx);
            items.extend(parts.toolbar_items);
            contributed.commands.extend(parts.commands);
            contributed.panels.extend(parts.panel);
            contributed.find.extend(parts.find);
            contributed
                .workspace_actions
                .extend(parts.workspace_actions);
            contributed.needs.extend(parts.needs);
            usage_sources.extend(parts.usage);
        }
        core.usage
            .update(cx, |usage, cx| usage.start(usage_sources, cx));
        bind_commands(&contributed.commands, cx);
        needs.update(cx, |needs, cx| {
            needs.set_sources(contributed.needs.clone(), cx)
        });
        crate::app_menu::set_menus(&contributed.commands, cx);
        core.extensions.update(cx, |extensions, cx| {
            *extensions = contributed;
            cx.notify();
        });
        core.toolbars.update(cx, |registry, cx| {
            for item in items {
                registry.register(item);
            }
            cx.notify();
        });
        cx.observe(&core.toolbars, |_, _, cx| cx.notify()).detach();
        Self {
            core,
            layouts: crate::layouts::registry(),
            views: HashMap::new(),
            paseo: None,
            palette_input,
            palette_index: 0,
            needs,
            focus: cx.focus_handle(),
            zoom_badge: None,
            customizing: false,
            bar_menu: None,
            help_open: false,
            system_border: (None, false),
        }
    }

    /// The current layout's view, and whether to draw it cached.
    fn current_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) -> (AnyView, bool) {
        let id = self.core.nav.read(cx).layout.clone();
        let entry = self
            .layouts
            .iter()
            .find(|entry| entry.id == id)
            .unwrap_or(&self.layouts[0]);
        let (key, cached) = (entry.id, entry.cached);
        if let Some(view) = self.views.get(key) {
            return (view.clone(), cached);
        }
        let view = if key == "paseo" {
            let core = self.core.clone();
            let layout = cx.new(|cx| PaseoLayout::new(core, window, cx));
            self.paseo = Some(layout.clone());
            AnyView::from(layout)
        } else {
            (entry.build)(&self.core, window, cx)
        };
        self.views.insert(key, view.clone());
        (view, cached)
    }

    fn set_layout(&mut self, id: &str, cx: &mut Context<Self>) {
        let id = id.to_string();
        self.core.nav.update(cx, |nav, cx| {
            nav.set_layout(&id, cx);
        });
    }

    fn sidebar(&self, cx: &App) -> Option<Entity<Sidebar>> {
        self.paseo.as_ref().map(|p| p.read(cx).sidebar.clone())
    }

    fn jump(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(sidebar) = self.sidebar(cx) else {
            return;
        };
        let rows = sidebar.read(cx).visible_rows(cx);
        if let Some((host, workspace)) = rows.get(index).cloned() {
            self.core.nav.update(cx, |nav, cx| {
                nav.layout = "paseo".into();
                nav.select(&host, &workspace, cx);
            });
        }
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(sidebar) = self.sidebar(cx) else {
            return;
        };
        let rows = sidebar.read(cx).visible_rows(cx);
        if rows.is_empty() {
            return;
        }
        let current = self.core.nav.read(cx).selection.clone();
        let position = current.and_then(|s| {
            rows.iter()
                .position(|(h, w)| *h == s.host && *w == s.workspace)
        });
        let next = match position {
            Some(i) => (i as isize + delta).rem_euclid(rows.len() as isize) as usize,
            None if delta > 0 => 0,
            None => rows.len() - 1,
        };
        let (host, workspace) = rows[next].clone();
        self.core
            .nav
            .update(cx, |nav, cx| nav.select(&host, &workspace, cx));
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = !self.core.nav.read(cx).command_open;
        self.core.nav.update(cx, |nav, cx| {
            nav.command_open = open;
            cx.notify();
        });
        if open {
            self.palette_index = 0;
            self.palette_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
        }
    }

    /// Workspaces and agents across the fleet that match the query.
    fn palette_results(&self, cx: &App) -> Vec<PaletteItem> {
        let query = self.palette_input.read(cx).value().to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        let matches = |text: &str| {
            let text = text.to_lowercase();
            terms.iter().all(|t| text.contains(t))
        };
        let store = self.core.store.read(cx);
        let multi = store.fleet.hosts.len() > 1;
        let mut out = Vec::new();
        for host in &store.fleet.hosts {
            for workspace in host.directory.workspaces.values() {
                let project = host
                    .directory
                    .projects
                    .get(&workspace.project_id)
                    .map(|p| p.name())
                    .unwrap_or_default();
                let subtitle = [
                    multi.then(|| host.config.name.clone()),
                    Some(project),
                    workspace.branch().map(str::to_string),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                if matches(&format!("{} {}", workspace.name, subtitle)) {
                    out.push(PaletteItem {
                        action: None,
                        kind: "Workspace".into(),
                        title: workspace.name.clone(),
                        subtitle,
                        host: host.config.name.clone(),
                        workspace: workspace.id.clone(),
                        agent: None,
                        bucket: host.directory.effective_bucket(workspace),
                    });
                }
            }
            for agent in host.directory.agents.values() {
                let Some(workspace) = agent.workspace_id.clone() else {
                    continue;
                };
                let ws_name = host
                    .directory
                    .workspaces
                    .get(&workspace)
                    .map(|w| w.name.clone())
                    .unwrap_or_default();
                let subtitle = [
                    multi.then(|| host.config.name.clone()),
                    Some(ws_name),
                    Some(agent.provider_id().to_string()),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                if matches(&format!(
                    "{} {} {}",
                    agent.display_title(),
                    subtitle,
                    agent.cwd
                )) {
                    out.push(PaletteItem {
                        action: None,
                        kind: "Agent".into(),
                        title: agent.display_title(),
                        subtitle,
                        host: host.config.name.clone(),
                        workspace,
                        agent: Some(agent.id.clone()),
                        bucket: agent.bucket(),
                    });
                }
            }
        }
        // What extensions' "Needs you" sources are waiting on.
        for (ix, source) in self.needs.read(cx).sources().iter().enumerate() {
            for item in source.items(cx) {
                if matches(&format!("{} {} {}", item.title, item.detail, item.id)) {
                    out.push(PaletteItem {
                        action: Some(PaletteAction::Need(ix, item.id)),
                        kind: source.kind(),
                        title: item.title,
                        subtitle: item.detail,
                        host: String::new(),
                        workspace: String::new(),
                        agent: None,
                        bucket: Bucket::Done,
                    });
                }
            }
        }
        out.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .reverse()
                .then(a.bucket.cmp(&b.bucket))
                .then(fleet::sidebar::natural(&a.title, &b.title))
        });
        // Actions first, like Paseo's command center.
        let selection_host = self
            .core
            .nav
            .read(cx)
            .selection
            .as_ref()
            .map(|s| s.host.clone());
        // Extensions' commands, by id and name.
        let commands: Vec<(SharedString, SharedString)> = (self.core.extensions.read(cx).commands)
            .iter()
            .map(|command| (command.id.clone(), command.name.clone()))
            .collect();
        let mut actions: Vec<(&str, PaletteAction)> = vec![
            (
                "New workspace",
                PaletteAction::Screen(Screen::NewWorkspace {
                    host: selection_host,
                    project: None,
                }),
            ),
            ("History", PaletteAction::Screen(Screen::History)),
            ("Hosts", PaletteAction::Screen(Screen::Hosts)),
            ("Settings", PaletteAction::Screen(Screen::Settings)),
            ("Toggle sidebar", PaletteAction::ToggleSidebar),
            ("About Crusteo", PaletteAction::About),
            (
                "Toggle grouping (project / status)",
                PaletteAction::ToggleGrouping,
            ),
        ];
        for (id, name) in &commands {
            actions.push((name.as_ref(), PaletteAction::Command(id.clone())));
        }
        for layout in &self.layouts {
            actions.push((layout.title, PaletteAction::Layout(layout.id)));
        }
        let mut action_rows: Vec<PaletteItem> = actions
            .into_iter()
            .filter(|(title, _)| !terms.is_empty() && matches(title))
            .map(|(title, action)| PaletteItem {
                kind: "Action".into(),
                title: match &action {
                    PaletteAction::Layout(_) => format!("Switch to {title} layout"),
                    _ => title.to_string(),
                },
                subtitle: String::new(),
                host: String::new(),
                workspace: String::new(),
                agent: None,
                bucket: Bucket::Done,
                action: Some(action),
            })
            .collect();
        action_rows.extend(out);
        let mut out = action_rows;
        out.truncate(60);
        out
    }

    fn run_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let results = self.palette_results(cx);
        let Some(item) = results.get(self.palette_index).cloned() else {
            return;
        };
        self.choose(item, window, cx);
    }

    fn choose(&mut self, item: PaletteItem, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(PaletteAction::Need(ix, id)) = item.action.clone() {
            self.core.nav.update(cx, |nav, cx| {
                nav.command_open = false;
                cx.notify();
            });
            if let Some(source) = self.needs.read(cx).sources().get(ix).cloned() {
                source.open(&id, window, cx);
            }
            return;
        }
        if let Some(PaletteAction::Command(id)) = item.action.clone() {
            self.core.nav.update(cx, |nav, cx| {
                nav.command_open = false;
                cx.notify();
            });
            self.run_command(&id, window, cx);
            return;
        }
        if let Some(PaletteAction::About) = item.action {
            self.core.nav.update(cx, |nav, cx| {
                nav.command_open = false;
                cx.notify();
            });
            crate::app_menu::about(&self.core, window, cx);
            return;
        }
        if let Some(action) = item.action {
            self.core.nav.update(cx, |nav, cx| {
                nav.command_open = false;
                match action {
                    PaletteAction::Screen(screen) => {
                        nav.layout = "paseo".into();
                        nav.show(screen, cx);
                    }
                    PaletteAction::Layout(id) => nav.layout = id.to_string(),
                    PaletteAction::ToggleSidebar => nav.sidebar_open = !nav.sidebar_open,
                    PaletteAction::ToggleGrouping => {
                        nav.grouping = match nav.grouping {
                            fleet::sidebar::Grouping::Project => fleet::sidebar::Grouping::Status,
                            fleet::sidebar::Grouping::Status => fleet::sidebar::Grouping::Project,
                        }
                    }
                    PaletteAction::Need(..) | PaletteAction::Command(_) | PaletteAction::About => {}
                }
                nav.save();
                cx.notify();
            });
            return;
        }
        self.core.nav.update(cx, |nav, cx| {
            nav.command_open = false;
            nav.layout = "paseo".into();
            nav.select(&item.host, &item.workspace, cx);
            if let Some(agent) = item.agent {
                nav.focus_tab(
                    &format!("{}:{}", item.host, item.workspace),
                    Tab::Agent(agent),
                    cx,
                );
            }
        });
    }

    fn render_palette(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let results = self.palette_results(cx);
        if self.palette_index >= results.len() {
            self.palette_index = results.len().saturating_sub(1);
        }
        let selected = self.palette_index;
        let mut list = div()
            .id("palette-results")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        let mut last_kind = SharedString::default();
        for (i, item) in results.into_iter().enumerate() {
            if item.kind != last_kind {
                last_kind = item.kind.clone();
                list = list.child(
                    div()
                        .px(px(16.))
                        .pt(px(10.))
                        .pb(px(6.))
                        .text_size(sz::sm())
                        .text_color(p.muted)
                        .child(format!("{}s", item.kind)),
                );
            }
            let chosen = item.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("palette-{i}")))
                    .px(px(16.))
                    .py(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .when(i == selected, |d| d.bg(p.surface1))
                    .hover(|s| s.bg(p.surface1))
                    .child(match &item.action {
                        Some(PaletteAction::Need(ix, _)) => {
                            let icon = (self.needs.read(cx).sources().get(*ix))
                                .map_or("list-checks", |source| source.icon());
                            ui::icon(icon, 16., p.muted).into_any_element()
                        }
                        Some(_) => ui::icon("activity", 16., p.muted).into_any_element(),
                        None if item.agent.is_some() => {
                            ui::agent_dot(item.bucket).into_any_element()
                        }
                        None => ui::icon("folder", 16., p.muted).into_any_element(),
                    })
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
                                    .child(item.title.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(sz::sm())
                                    .text_color(p.muted)
                                    .child(item.subtitle.clone()),
                            ),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.choose(chosen.clone(), window, cx)
                    })),
            );
        }
        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::gpui::rgba(0x00000080))
            .flex()
            .justify_center()
            .pt(px(48.))
            .on_mouse_down(
                gpui_kit::gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.core.nav.update(cx, |nav, cx| {
                        nav.command_open = false;
                        cx.notify();
                    });
                }),
            )
            .child(
                div()
                    .key_context("CommandCenter")
                    .on_action(cx.listener(|this, _: &Dismiss, _, cx| {
                        this.core.nav.update(cx, |nav, cx| {
                            nav.command_open = false;
                            cx.notify();
                        });
                    }))
                    .on_key_down(cx.listener(
                        |this, event: &gpui_kit::gpui::KeyDownEvent, _, cx| {
                            match event.keystroke.key.as_str() {
                                "down" => {
                                    this.palette_index += 1;
                                    cx.notify();
                                }
                                "up" => {
                                    this.palette_index = this.palette_index.saturating_sub(1);
                                    cx.notify();
                                }
                                _ => {}
                            }
                        },
                    ))
                    .on_mouse_down(gpui_kit::gpui::MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation()
                    })
                    .w(px(640.))
                    .h(px(560.))
                    .max_h(gpui_kit::gpui::relative(0.8))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(p.border)
                    .bg(p.surface0)
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .px(px(16.))
                            .py(px(12.))
                            .border_b_1()
                            .border_color(p.border)
                            .child(Input::new(&self.palette_input).appearance(false)),
                    )
                    .child(list),
            )
            .into_any_element()
    }

    /// Places items the arrangement has not seen yet where they ask to go,
    /// and saves that.
    fn place_new_items(&mut self, cx: &mut Context<Self>) {
        let mut homes: Vec<toolbars::Home> = self
            .core
            .toolbars
            .read(cx)
            .items()
            .map(|item| toolbars::Home::of(item.as_ref()))
            .collect();
        homes.sort_by_key(|home| home.order);
        let mut arrangement = self.core.nav.read(cx).toolbars.clone();
        if arrangement.place_new(&homes) {
            self.core
                .nav
                .update(cx, |nav, cx| nav.set_toolbars(arrangement, cx));
        }
    }

    /// A bar's left and right groups for the layout on screen.
    fn bar_slots(
        &self,
        bar: &str,
        layout: &str,
        cx: &App,
    ) -> Option<(Vec<toolbars::Slot>, Vec<toolbars::Slot>)> {
        toolbars::bar_items(
            bar,
            layout,
            &self.core.nav.read(cx).toolbars,
            self.core.toolbars.read(cx),
            cx,
        )
    }

    /// A bar's contents: its items, or while customizing, its editor.
    fn bar_body(
        &mut self,
        bar: &str,
        layout: &str,
        occlude: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.customizing {
            let arrangement = self.core.nav.read(cx).toolbars.clone();
            let registry = self.core.toolbars.read(cx).clone();
            let nav = self.core.nav.clone();
            return Some(toolbars::customize::bar_editor(
                bar,
                &arrangement,
                &registry,
                &nav,
                window,
                cx,
            ));
        }
        let (left, right) = self.bar_slots(bar, layout, cx)?;
        Some(toolbars::bar_contents(left, right, occlude, window, cx))
    }

    /// Right-click on a bar offers to customize the bars.
    fn bar_menu_listener(
        &self,
        bar: &str,
        cx: &mut Context<Self>,
    ) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        let bar = bar.to_string();
        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
            this.open_bar_menu(bar.clone(), event.position, window, cx);
            cx.stop_propagation();
        })
    }

    fn open_bar_menu(
        &mut self,
        bar: String,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shell = cx.entity().downgrade();
        let nav = self.core.nav.clone();
        let customizing = self.customizing;
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            let customize = {
                let shell = shell.clone();
                PopupMenuItem::new(if customizing {
                    "Done customizing"
                } else {
                    "Customize toolbars…"
                })
                .on_click(move |_, _, cx| {
                    let _ = shell.update(cx, |shell, cx| shell.set_customizing(!customizing, cx));
                })
            };
            let menu = menu.item(customize);
            if bar == toolbars::TITLE {
                return menu;
            }
            let (nav, bar) = (nav.clone(), bar.clone());
            let custom = toolbars::is_custom(&bar);
            let label = if custom {
                "Remove this bar"
            } else {
                "Hide this bar"
            };
            menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                nav.update(cx, |nav, cx| {
                    let mut arrangement = nav.toolbars.clone();
                    if custom {
                        arrangement.remove_bar(&bar);
                    } else {
                        arrangement.set_hidden(&bar, true);
                    }
                    nav.set_toolbars(arrangement, cx);
                });
            }))
        });
        let _dismiss =
            cx.subscribe_in(&menu, window, |this, menu, _: &DismissEvent, window, cx| {
                this.bar_menu = None;
                // The menu had focus; give it back so shortcuts keep working.
                let menu_focused = menu.focus_handle(cx).contains_focused(window, cx);
                if menu_focused || window.focused(cx).is_none() {
                    this.focus.focus(window, cx);
                }
                cx.notify();
            });
        self.bar_menu = Some((position, menu, _dismiss));
        cx.notify();
    }

    /// A solid line around the window's edge, as wide and in the color
    /// chosen in Settings, to tell the window apart from those beside it.
    /// Drawn over everything but popups; with no hitbox, it takes no clicks
    /// and leaves resizing at the edge alone.
    fn window_edge(&mut self, window: &mut Window, cx: &App) -> Option<AnyElement> {
        let nav = self.core.nav.read(cx);
        let color = (nav.window_border > 0)
            .then(|| {
                crate::app::nav::parse_hex(&nav.window_border_color)
                    .or_else(|| crate::app::nav::parse_hex(crate::app::nav::WINDOW_BORDER_COLOR))
            })
            .flatten();
        let border = nav.window_border as f32;
        // The system's own line takes the color too, where it can.
        if self.system_border.0 != color {
            self.system_border = (color, color_system_border(window, color));
        }
        let color = color?;
        // Windows 11 draws a 1px line round a window that is not maximized:
        // outside crusteo's content at the sides and bottom, but over its
        // top row, as the content runs up to the window's top edge. Colored,
        // it is the outer pixel of the border: crusteo draws one less at the
        // sides and bottom, and at the top as many as asked, the first
        // hidden under it.
        let system_line = self.system_border.1 && !window.is_maximized();
        let sides = if system_line { border - 1. } else { border };
        let width = |w: f32| Some(AbsoluteLength::from(px(w.max(0.))));
        let mut edge = div().absolute().inset_0().border_color(rgb(color));
        edge.style().border_widths = EdgesRefinement {
            top: width(border),
            right: width(sides),
            bottom: width(sides),
            left: width(sides),
        };
        Some(edge.into_any_element())
    }

    fn set_customizing(&mut self, customizing: bool, cx: &mut Context<Self>) {
        self.customizing = customizing;
        self.bar_menu = None;
        cx.notify();
    }

    /// While customizing: everything under the bars dimmed, and the palette
    /// centred right under them, where the eye already is. A click on the
    /// dimmed part is Done.
    fn customize_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.customizing {
            return None;
        }
        let arrangement = self.core.nav.read(cx).toolbars.clone();
        let registry = self.core.toolbars.read(cx).clone();
        let shell = cx.entity().downgrade();
        let panel = toolbars::customize::palette_panel(
            &arrangement,
            &registry,
            &self.core.nav,
            move |_, cx| {
                let _ = shell.update(cx, |shell, cx| shell.set_customizing(false, cx));
            },
        );
        Some(
            div()
                .id("customize-dim")
                .absolute()
                .inset_0()
                .bg(gpui_kit::gpui::rgba(0x00000099))
                .flex()
                .justify_center()
                .items_start()
                .pt(px(14.))
                .px(px(16.))
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.set_customizing(false, cx)),
                )
                .child(panel)
                .into_any_element(),
        )
    }

    /// The open bar menu, over everything, where it was opened.
    fn bar_menu_layer(&self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let (position, menu, _) = self.bar_menu.as_ref()?;
        let focus = menu.focus_handle(cx);
        if !focus.contains_focused(window, cx) {
            focus.focus(window, cx);
        }
        Some(
            deferred(
                anchored()
                    .position(*position)
                    .snap_to_window_with_margin(px(8.))
                    .child(menu.clone()),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }

    /// The title bar: the app icon, after the window buttons on macOS; then
    /// its items between a strip kept clear at the left and one at the
    /// right, so the window can always be dragged from both ends.
    fn title_bar(
        &mut self,
        layout: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let body = self.bar_body(toolbars::TITLE, layout, true, window, cx);
        let menu = self.bar_menu_listener(toolbars::TITLE, cx);
        // The bar's contents lie over the title bar in a box that ends
        // where the window controls begin, so however much the bar holds it
        // cannot push them off the edge: the title bar's own container does
        // not shrink below what is inside it. Empty space in the box still
        // drags the window; only items block that.
        let (inset, controls) = if cfg!(target_os = "macos") {
            (80., 0.)
        } else {
            (12., 3. * gpui_kit::component::TITLE_BAR_HEIGHT.as_f32())
        };
        div()
            .relative()
            .flex_shrink_0()
            .h(px(36.))
            .child(
                TitleBar::new()
                    .h(px(36.))
                    .bg(p.sidebar)
                    .border_color(p.border),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(inset))
                    .right(px(controls))
                    .flex()
                    .items_center()
                    .on_mouse_down(MouseButton::Right, menu)
                    .child(app_icon(window))
                    .child(div().w(px(TITLE_CLEAR_LEFT)).flex_shrink_0())
                    .children(body)
                    .child(div().w(px(TITLE_CLEAR_RIGHT)).flex_shrink_0()),
            )
            .into_any_element()
    }

    /// A bar under the title bar, or nothing when none of its items apply.
    /// While customizing every bar shows, hidden ones dimmed, so items can
    /// go anywhere.
    fn plain_bar(
        &mut self,
        bar: &str,
        layout: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = palette();
        let body = self.bar_body(bar, layout, false, window, cx)?;
        let hidden = self
            .core
            .nav
            .read(cx)
            .toolbars
            .bar(bar)
            .is_some_and(|placed| placed.hidden);
        let (height, padding, background) = match bar {
            toolbars::STRIP => (36., 8., p.sidebar),
            _ => (40., 16., p.surface0),
        };
        let menu = self.bar_menu_listener(bar, cx);
        Some(
            div()
                .h(px(height))
                .flex_shrink_0()
                .min_w(px(0.))
                .px(px(padding))
                .flex()
                .items_center()
                .bg(background)
                .border_b_1()
                .border_color(p.border)
                .when(hidden, |bar| bar.opacity(0.5))
                .on_mouse_down(MouseButton::Right, menu)
                .child(body)
                .into_any_element(),
        )
    }
    /// A fleet-wide notice when an agent elsewhere finishes or needs you.
    /// Skipped for the conversation already on screen.
    #[allow(clippy::too_many_arguments)]
    fn notify_attention(
        &mut self,
        host: &str,
        agent_id: &str,
        workspace_id: Option<&str>,
        reason: &str,
        title: &str,
        body: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let nav = self.core.nav.read(cx);
        let viewing = window.is_window_active()
            && nav.layout == "paseo"
            && nav.screen == Screen::Workspace
            && nav.selection.as_ref().is_some_and(|s| {
                s.host == host
                    && nav.focused_tab(&s.key()) == Some(&Tab::Agent(agent_id.to_string()))
            });
        if viewing {
            return;
        }
        let agent_title = self
            .core
            .store
            .read(cx)
            .fleet
            .agent(host, agent_id)
            .map(|a| a.display_title())
            .unwrap_or_default();
        let message = format!(
            "{host} · {agent_title}\n{}",
            body.lines().next().unwrap_or_default()
        );
        let (corner, route) = (nav.notification_corner, nav.notification_route);
        let mut note = gpui_kit::component::notification::Notification::new()
            .title(title.to_string())
            .message(message)
            .placement(corner.anchor())
            .delivery(route.delivery());
        note = match reason {
            "permission" => {
                note.with_type(gpui_kit::component::notification::NotificationType::Warning)
            }
            "error" => note.with_type(gpui_kit::component::notification::NotificationType::Error),
            _ => note.with_type(gpui_kit::component::notification::NotificationType::Success),
        };
        if let Some(workspace) = workspace_id {
            let nav = self.core.nav.clone();
            let host = host.to_string();
            let workspace = workspace.to_string();
            let agent = agent_id.to_string();
            // In Lanes, go to the agent's card; anywhere else, open its tab.
            note = note.on_click(move |_, _, cx| {
                nav.update(cx, |nav, cx| {
                    if nav.layout == "lanes" {
                        nav.reveal_in_lanes(&host, &agent, cx);
                        return;
                    }
                    nav.layout = "paseo".into();
                    nav.select(&host, &workspace, cx);
                    nav.focus_tab(
                        &format!("{host}:{workspace}"),
                        Tab::Agent(agent.clone()),
                        cx,
                    );
                });
            });
        }
        window.push_notification(note, cx);
    }

    /// Runs an extension's command. A panel it opens takes focus.
    fn run_command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(run) =
            (self.core.extensions.read(cx).command(id)).map(|command| command.run.clone())
        else {
            return;
        };
        let before = self.core.nav.read(cx).panel.clone();
        run(window, cx);
        let after = self.core.nav.read(cx).panel.clone();
        if after != before
            && let Some(panel) =
                after.and_then(|id| self.core.extensions.read(cx).panel(&id).cloned())
        {
            cx.on_next_frame(window, move |_, window, cx| panel.focus(window, cx));
        }
    }

    /// Zooms the whole window a browser step in or out, or back to 100% for
    /// a zero step, and remembers it.
    fn zoom_step(&mut self, step: i32, window: &mut Window, cx: &mut Context<Self>) {
        const STEPS: [f32; 13] = [
            0.5, 0.67, 0.75, 0.8, 0.9, 1., 1.1, 1.25, 1.5, 1.75, 2., 2.5, 3.,
        ];
        let current = window.zoom();
        let at = STEPS
            .iter()
            .position(|zoom| *zoom >= current - 0.001)
            .unwrap_or(STEPS.len() - 1);
        let zoom = match step {
            0 => 1.,
            1 => STEPS[(at + 1).min(STEPS.len() - 1)],
            _ => STEPS[at.saturating_sub(1)],
        };
        window.set_zoom(zoom, cx);
        self.core.nav.update(cx, |nav, cx| nav.set_zoom(zoom, cx));
        // A badge, not a toast: each change replaces it at once, and it goes
        // soon after the last one.
        let shown = self.zoom_badge.map_or(0, |(_, shown)| shown) + 1;
        self.zoom_badge = Some((zoom, shown));
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(900))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.zoom_badge.is_some_and(|(_, current)| current == shown) {
                    this.zoom_badge = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// The zoom level, centered near the top while it changes.
    fn zoom_badge(&self, window: &Window) -> Option<AnyElement> {
        let (zoom, _) = self.zoom_badge?;
        let p = palette();
        let width = px(72.);
        let at = point((window.viewport_size().width - width) / 2., px(56.));
        Some(
            deferred(
                anchored().position(at).child(
                    div()
                        .w(width)
                        .py(px(6.))
                        .rounded(px(8.))
                        .bg(p.surface3)
                        .border_1()
                        .border_color(p.border)
                        .shadow_md()
                        .flex()
                        .justify_center()
                        .text_color(p.foreground)
                        .child(format!("{:.0}%", zoom * 100.)),
                ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.core.nav.update(cx, |nav, cx| {
            nav.sidebar_open = !nav.sidebar_open;
            nav.save();
            cx.notify();
        });
    }

    fn new_agent_tab(&mut self, _: &NewAgentTab, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.core.nav.read(cx).selection.clone() else {
            return;
        };
        self.core.nav.update(cx, |nav, cx| {
            nav.new_draft(&selection.key(), cx);
        });
    }

    fn new_terminal(&mut self, _: &NewTerminal, _: &mut Window, cx: &mut Context<Self>) {
        crate::layouts::paseo::open_new_terminal(&self.core, cx);
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.core.nav.read(cx).selection.clone() else {
            return;
        };
        let key = selection.key();
        let Some(tab) = self.core.nav.read(cx).focused_tab(&key).cloned() else {
            return;
        };
        crate::layouts::paseo::close_tab(&self.core, &key, &tab, window, cx);
    }
}

#[derive(Clone, PartialEq)]
enum PaletteAction {
    Screen(Screen),
    Layout(&'static str),
    ToggleSidebar,
    ToggleGrouping,
    /// An item of the "Needs you" source at this index, by id.
    Need(usize, String),
    /// An extension's command, by id.
    Command(SharedString),
    /// The version and each host's daemon version, as macOS's About does.
    About,
}

#[derive(Clone)]
struct PaletteItem {
    action: Option<PaletteAction>,
    kind: SharedString,
    title: String,
    subtitle: String,
    host: String,
    workspace: String,
    agent: Option<String>,
    bucket: Bucket,
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let (layout, layout_cached) = self.current_layout(window, cx);
        let palette_open = self.core.nav.read(cx).command_open;
        // What "Needs you" sources draw over the window, such as details.
        let sources = self.needs.read(cx).sources().to_vec();
        let overlays: Vec<AnyElement> = sources
            .iter()
            .filter_map(|source| source.overlay(window, cx))
            .collect();
        let open_panel = self.core.nav.read(cx).panel.clone();
        let panel = open_panel
            .as_deref()
            .and_then(|id| self.core.extensions.read(cx).panel(id).cloned());
        // A panel closed with focus still inside it, which is no longer
        // drawn: actions and keys sent there go nowhere, so the window takes
        // focus back.
        let panels = self.core.extensions.read(cx).panels.clone();
        if panels.iter().any(|other| {
            Some(other.id()) != open_panel.as_deref() && other.contains_focus(window, cx)
        }) {
            self.focus.focus(window, cx);
        }
        self.place_new_items(cx);
        let layout_id = self.core.nav.read(cx).layout.clone();
        let title_bar = self.title_bar(&layout_id, window, cx);
        let strip = self.plain_bar(toolbars::STRIP, &layout_id, window, cx);
        let layout_bar = self.plain_bar(toolbars::LAYOUT, &layout_id, window, cx);
        // The user's own bars, under the built-in ones.
        let own_bars: Vec<String> = (self.core.nav.read(cx).toolbars.custom_bars())
            .map(|bar| bar.id.clone())
            .collect();
        let own_bars: Vec<AnyElement> = own_bars
            .iter()
            .filter_map(|bar| self.plain_bar(bar, &layout_id, window, cx))
            .collect();
        let body = div()
            .id("crusteo-root")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(p.surface0)
            .text_color(p.foreground)
            .font_family(".SystemUIFont")
            .text_size(sz::base())
            // Escape leaves customizing before anything else sees it.
            .capture_key_down(
                cx.listener(|this, event: &gpui_kit::gpui::KeyDownEvent, _, cx| {
                    // A state singled out from the counts ends at any key.
                    if this.core.nav.read(cx).state_highlight.is_some() {
                        this.core
                            .nav
                            .update(cx, |nav, cx| nav.set_state_highlight(None, cx));
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            return;
                        }
                    }
                    if event.keystroke.key != "escape" {
                        return;
                    }
                    if this.help_open {
                        this.help_open = false;
                        cx.notify();
                        cx.stop_propagation();
                    } else if this.customizing {
                        this.set_customizing(false, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            // ...and at any click, which still does what it would have.
            .on_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                if this.core.nav.read(cx).state_highlight.is_some() {
                    this.core
                        .nav
                        .update(cx, |nav, cx| nav.set_state_highlight(None, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| this.zoom_step(1, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| this.zoom_step(-1, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, window, cx| this.zoom_step(0, window, cx)))
            // Lanes takes it when focus is in the board; otherwise it comes
            // here and goes to Lanes when Lanes is on screen.
            .on_action(
                cx.listener(|this, _: &crate::layouts::lanes::FindOnBoard, window, cx| {
                    if this.core.nav.read(cx).layout == "lanes"
                        && let Some(view) = this.views.get("lanes")
                    {
                        crate::layouts::lanes::open_find(view, window, cx);
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &crate::app_menu::About, window, cx| {
                crate::app_menu::about(&this.core, window, cx)
            }))
            .on_action(|_: &crate::app_menu::Minimize, window, _| window.minimize_window())
            .on_action(|_: &crate::app_menu::ZoomWindow, window, _| window.zoom_window())
            .on_action(cx.listener(|this, _: &ShowHelp, _, cx| {
                this.help_open = !this.help_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CustomizeToolbars, _, cx| {
                this.set_customizing(true, cx);
            }))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(
                cx.listener(|this, _: &CommandCenter, window, cx| this.open_palette(window, cx)),
            )
            .on_action(cx.listener(Self::new_agent_tab))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(|this, _: &NextWorkspace, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousWorkspace, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &NewWorkspace, _, cx| {
                let host = this
                    .core
                    .nav
                    .read(cx)
                    .selection
                    .as_ref()
                    .map(|s| s.host.clone());
                this.core.nav.update(cx, |nav, cx| {
                    nav.layout = "paseo".into();
                    nav.show(
                        Screen::NewWorkspace {
                            host,
                            project: None,
                        },
                        cx,
                    );
                });
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.core.nav.update(cx, |nav, cx| {
                    nav.layout = "paseo".into();
                    nav.show(Screen::Settings, cx);
                });
            }))
            .on_action(
                cx.listener(|this, action: &crate::extensions::RunCommand, window, cx| {
                    this.run_command(&action.id, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &CycleLayout, _, cx| {
                let current = this.core.nav.read(cx).layout.clone();
                let index = this
                    .layouts
                    .iter()
                    .position(|l| l.id == current)
                    .unwrap_or(0);
                let next = this.layouts[(index + 1) % this.layouts.len()].id;
                this.set_layout(next, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusComposer, window, cx| {
                if let Some(paseo) = this.paseo.clone() {
                    let workspace = paseo.read(cx).workspace().clone();
                    workspace.update(cx, |workspace, cx| workspace.focus_active(window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &Jump1, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &Jump2, _, cx| this.jump(1, cx)))
            .on_action(cx.listener(|this, _: &Jump3, _, cx| this.jump(2, cx)))
            .on_action(cx.listener(|this, _: &Jump4, _, cx| this.jump(3, cx)))
            .on_action(cx.listener(|this, _: &Jump5, _, cx| this.jump(4, cx)))
            .on_action(cx.listener(|this, _: &Jump6, _, cx| this.jump(5, cx)))
            .on_action(cx.listener(|this, _: &Jump7, _, cx| this.jump(6, cx)))
            .on_action(cx.listener(|this, _: &Jump8, _, cx| this.jump(7, cx)))
            .on_action(cx.listener(|this, _: &Jump9, _, cx| this.jump(8, cx)))
            .child(title_bar)
            .children(strip)
            .children(layout_bar)
            .children(own_bars)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_row()
                    .child(div().flex_1().min_w(px(0.)).h_full().map(|slot| {
                        if layout_cached {
                            slot.child(layout.cached_fill())
                        } else {
                            slot.child(layout)
                        }
                    }))
                    .when_some(panel, |row, panel| {
                        row.child(
                            div()
                                .w(px(420.))
                                .h_full()
                                .flex_shrink_0()
                                .border_l_1()
                                .border_color(p.border)
                                .overflow_hidden()
                                .child(panel.view().cached_fill()),
                        )
                    })
                    .relative()
                    .children(self.customize_layer(cx)),
            )
            .children(overlays)
            .when(palette_open, |d| d.child(self.render_palette(cx)))
            .when(self.help_open, |d| {
                let (close, customize) = (cx.entity().downgrade(), cx.entity().downgrade());
                d.child(crate::help::panel(crate::help::Actions {
                    close: Rc::new(move |_, cx| {
                        let _ = close.update(cx, |shell, cx| {
                            shell.help_open = false;
                            cx.notify();
                        });
                    }),
                    customize: Rc::new(move |_, cx| {
                        let _ = customize.update(cx, |shell, cx| {
                            shell.help_open = false;
                            shell.set_customizing(true, cx);
                        });
                    }),
                    extra: (self.core.extensions.read(cx).commands)
                        .iter()
                        .filter_map(|command| {
                            Some((
                                crate::help::key_parts(command.key?),
                                command.name.to_string(),
                            ))
                        })
                        .collect(),
                }))
            })
            .children(self.bar_menu_layer(window, cx))
            .children(self.zoom_badge(window))
            .children(self.window_edge(window, cx));

        if matches!(window.window_decorations(), Decorations::Client { .. }) {
            gpui_kit::component::window_border()
                .child(body)
                .into_any_element()
        } else {
            body.into_any_element()
        }
    }
}

pub fn open(cx: &mut App) {
    // Frameless like the Paseo desktop app: the title bar is ours and
    // carries the layout switcher; TitleBar draws window controls where the
    // platform needs them.
    let mut options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1280.), px(820.)), cx)),
        window_decorations: Some(gpui_kit::gpui::WindowDecorations::Client),
        window_min_size: Some(size(px(720.), px(480.))),
        ..TitleBar::window_options()
    };
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some("Crusteo".into());
    }
    gpui_kit::open_window(options, cx, |window, cx| {
        let core = Core::new(cx);
        let shell = cx.new(|cx| Shell::new(core, window, cx));
        shell.update(cx, |shell, cx| window.focus(&shell.focus, cx));
        shell
    })
    .expect("open window");
}
