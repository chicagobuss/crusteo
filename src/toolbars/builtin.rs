//! The items the shell itself puts in its bars: the sidebar toggle, the
//! layout switcher, attention counts and hosts. Others come from layouts
//! and extensions.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use fleet::Bucket;
use gpui_kit::gpui::{
    AnyElement, App, ClickEvent, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder as _;

use super::{Side, TITLE, ToolbarItem};
use crate::app::{Core, Screen};
use crate::theme::{palette, size as sz};
use crate::ui;

/// The shell's items, given what they read.
pub fn items(core: &Core, layouts: Vec<(&'static str, &'static str)>) -> Vec<Rc<dyn ToolbarItem>> {
    vec![
        Rc::new(SidebarToggle { core: core.clone() }),
        Rc::new(LayoutSwitcher {
            core: core.clone(),
            layouts,
        }),
        Rc::new(AttentionCounts {
            core: core.clone(),
            counts: RefCell::new(None),
            recount_queued: Rc::new(Cell::new(false)),
        }),
        Rc::new(Help),
        Rc::new(Hosts { core: core.clone() }),
    ]
}

/// The app's menu: settings, the help panel, customizing the bars and
/// About, which Windows and Linux have no menu bar for. Its id is the help
/// button's it replaced, so it keeps that button's place in a saved bar.
struct Help;

impl ToolbarItem for Help {
    fn id(&self) -> &str {
        "help"
    }
    fn title(&self) -> SharedString {
        "Menu".into()
    }
    fn home(&self) -> (&'static str, Side) {
        (TITLE, Side::Right)
    }
    // Before hosts, which stay last.
    fn order(&self) -> i32 {
        90
    }
    fn render(&self, _: &mut Window, _: &mut App) -> Option<AnyElement> {
        use crate::shell::{CustomizeToolbars, DismissNotifications, OpenSettings, ShowHelp};
        use gpui_kit::component::Sizable as _;
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::menu::DropdownMenu as _;
        Some(
            Button::new("app-menu")
                .icon(gpui_kit::component::Icon::empty().path("icons/menu.svg"))
                .ghost()
                .small()
                .tooltip("Menu")
                .dropdown_menu(|menu, _, _| {
                    menu.menu("Settings…", Box::new(OpenSettings))
                        .menu("Keyboard shortcuts", Box::new(ShowHelp))
                        .menu("Customize toolbars…", Box::new(CustomizeToolbars))
                        .menu("Dismiss all notifications", Box::new(DismissNotifications))
                        .separator()
                        .menu("About Crusteo", Box::new(crate::app_menu::About))
                })
                .into_any_element(),
        )
    }
}

/// Shows and hides the Paseo sidebar; only Paseo has one.
struct SidebarToggle {
    core: Core,
}

impl ToolbarItem for SidebarToggle {
    fn id(&self) -> &str {
        "sidebar-toggle"
    }
    fn title(&self) -> SharedString {
        "Sidebar toggle".into()
    }
    fn home(&self) -> (&'static str, Side) {
        (TITLE, Side::Left)
    }
    fn shows_in(&self, layout: &str, _: &App) -> bool {
        layout == "paseo"
    }
    fn render(&self, _: &mut Window, _: &mut App) -> Option<AnyElement> {
        let p = palette();
        let nav = self.core.nav.clone();
        Some(
            div()
                .id("toggle-sidebar")
                .size(px(26.))
                .rounded(px(6.))
                .flex()
                .items_center()
                .justify_center()
                .hover(|s| s.bg(p.sidebar_hover))
                .child(ui::icon("panel-left", 16., p.extra_muted))
                .on_click(move |_: &ClickEvent, _, cx| {
                    nav.update(cx, |nav, cx| {
                        nav.sidebar_open = !nav.sidebar_open;
                        nav.save();
                        cx.notify();
                    });
                })
                .into_any_element(),
        )
    }
}

/// Paseo, Fleet, Board, Lanes.
struct LayoutSwitcher {
    core: Core,
    layouts: Vec<(&'static str, &'static str)>,
}

impl ToolbarItem for LayoutSwitcher {
    fn id(&self) -> &str {
        "layout-switcher"
    }
    fn title(&self) -> SharedString {
        "Layout switcher".into()
    }
    fn home(&self) -> (&'static str, Side) {
        (TITLE, Side::Left)
    }
    fn render(&self, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let p = palette();
        let current = self.core.nav.read(cx).layout.clone();
        let mut switcher = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(6.))
            .bg(p.surface1);
        for &(id, title) in &self.layouts {
            let active = current == id;
            let nav = self.core.nav.clone();
            switcher = switcher.child(
                div()
                    .id(SharedString::from(format!("layout-{id}")))
                    .px(px(10.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .text_size(sz::sm())
                    .text_color(if active { p.foreground } else { p.muted })
                    .when(active, |d| d.bg(p.surface3))
                    .hover(|s| s.text_color(p.foreground))
                    .child(title)
                    .on_click(move |_: &ClickEvent, _, cx| {
                        nav.update(cx, |nav, cx| {
                            nav.set_layout(id, cx);
                        });
                    }),
            );
        }
        Some(switcher.into_any_element())
    }
}

/// Counting walks every agent. While the fleet churns, recount at most this
/// often: a count a quarter second behind is not visible.
const RECOUNT_INTERVAL: Duration = Duration::from_millis(250);

type BucketCounts = [(Bucket, usize); 5];

/// "2 ready to review · 1 working": the fleet's agents by state, without
/// those done.
struct AttentionCounts {
    core: Core,
    /// The counts, with the store revision and time they were taken at.
    counts: RefCell<Option<(u64, Instant, BucketCounts)>>,
    /// A redraw is queued for when the counts may be recounted.
    recount_queued: Rc<Cell<bool>>,
}

impl AttentionCounts {
    fn counts(&self, cx: &mut App) -> BucketCounts {
        let revision = self.core.store.read(cx).revision;
        let cached = *self.counts.borrow();
        match cached {
            Some((at, _, counts)) if at == revision => counts,
            Some((_, taken, counts)) if taken.elapsed() < RECOUNT_INTERVAL => {
                if !self.recount_queued.replace(true) {
                    let wait = RECOUNT_INTERVAL.saturating_sub(taken.elapsed());
                    let (queued, toolbars) =
                        (self.recount_queued.clone(), self.core.toolbars.clone());
                    cx.spawn(async move |cx| {
                        cx.background_executor().timer(wait).await;
                        queued.set(false);
                        // The shell redraws its bars when the registry says so.
                        toolbars.update(cx, |_, cx| cx.notify());
                    })
                    .detach();
                }
                counts
            }
            _ => {
                let counts = self.core.store.read(cx).fleet.agent_bucket_counts();
                *self.counts.borrow_mut() = Some((revision, Instant::now(), counts));
                counts
            }
        }
    }
}

impl ToolbarItem for AttentionCounts {
    fn id(&self) -> &str {
        "attention-counts"
    }
    fn title(&self) -> SharedString {
        "Agents by state".into()
    }
    fn home(&self) -> (&'static str, Side) {
        (TITLE, Side::Right)
    }
    fn render(&self, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let p = palette();
        let counts = self.counts(cx);
        let singled = self.core.nav.read(cx).state_highlight;
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .children(
                    counts
                        .iter()
                        .filter(|(bucket, n)| *n > 0 && *bucket != Bucket::Done)
                        .map(|(bucket, n)| {
                            let bucket = *bucket;
                            let color = ui::bucket_color(bucket).unwrap_or(p.extra_muted);
                            let nav = self.core.nav.clone();
                            let on = singled == Some(bucket);
                            div()
                                .id(SharedString::from(format!("count-{}", bucket.as_str())))
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .px(px(6.))
                                .py(px(2.))
                                .rounded(px(6.))
                                .text_size(sz::sm())
                                .text_color(if on { p.foreground } else { p.muted })
                                .when(on, |chip| chip.bg(p.surface3))
                                .cursor_pointer()
                                .hover(|chip| chip.bg(p.sidebar_hover).text_color(p.foreground))
                                .tooltip(move |window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(state_help(bucket))
                                        .build(window, cx)
                                })
                                .child(div().size(px(6.)).rounded_full().bg(color))
                                .child(format!("{n} {}", bucket.label().to_lowercase()))
                                // On mouse down, so the click that ends a
                                // highlight elsewhere does not end this one.
                                .on_mouse_down(
                                    gpui_kit::gpui::MouseButton::Left,
                                    move |_, _, cx| {
                                        cx.stop_propagation();
                                        nav.update(cx, |nav, cx| {
                                            if on {
                                                nav.set_state_highlight(None, cx);
                                                return;
                                            }
                                            if nav.layout != "lanes" {
                                                nav.layout = "lanes".into();
                                                nav.save();
                                            }
                                            nav.set_state_highlight(Some(bucket), cx);
                                            cx.notify();
                                        });
                                    },
                                )
                        }),
                )
                .into_any_element(),
        )
    }
}

/// What a state means, for its count's tooltip.
fn state_help(state: Bucket) -> &'static str {
    match state {
        Bucket::NeedsInput => {
            "Waiting on you: a permission to grant or a question to answer. \
             Click to single them out on the board."
        }
        Bucket::Failed => {
            "Their last turn ended in an error. Click to single them out on the board."
        }
        Bucket::Attention => {
            "Finished a turn you have not looked at yet. Click to single them out on the board."
        }
        Bucket::Running => "Working on a turn right now. Click to single them out on the board.",
        Bucket::Done => "Done, and already seen.",
    }
}

/// How many hosts are online; opens the hosts screen.
struct Hosts {
    core: Core,
}

impl ToolbarItem for Hosts {
    fn id(&self) -> &str {
        "hosts"
    }
    fn title(&self) -> SharedString {
        "Hosts online".into()
    }
    fn home(&self) -> (&'static str, Side) {
        (TITLE, Side::Right)
    }
    // Last in the title bar, next to the window controls.
    fn order(&self) -> i32 {
        100
    }
    fn render(&self, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let p = palette();
        let store = self.core.store.read(cx);
        let total = store.fleet.hosts.len();
        let online = store
            .fleet
            .hosts
            .iter()
            .filter(|h| h.connection.is_online())
            .count();
        let nav = self.core.nav.clone();
        Some(
            div()
                .id("hosts-summary")
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(sz::sm())
                .text_color(p.muted)
                .child(ui::icon(
                    "server",
                    12.,
                    if online == total {
                        p.status_success
                    } else {
                        p.status_warning
                    },
                ))
                .child(format!("{online}/{total}"))
                .on_click(move |_: &ClickEvent, _, cx| {
                    nav.update(cx, |nav, cx| {
                        nav.set_layout("paseo", cx);
                        nav.show(Screen::Hosts, cx);
                    });
                })
                .into_any_element(),
        )
    }
}
