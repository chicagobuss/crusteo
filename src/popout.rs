//! A Lanes card popped out into a window of its own, beside crusteo's main
//! one: the agent's conversation and composer, in a window the OS can place,
//! snap side by side with another, or move to another screen.
//!
//! The window draws its own title bar, as the main window does: the agent's
//! state and title (click to rename), its workspace and host, its labels,
//! putting it back in its lane, and its colour, which tints the bar (the
//! colour pinned for the agent, else its first label's lane colour). It
//! reopens where it was last.
//!
//! Nav keeps which agents are out ([`Nav::popouts`]), so their conversations
//! keep streaming and Lanes brings a popped-out card's window forward
//! instead of opening it in place.
//!
//! [`Nav::popouts`]: crate::app::nav::Nav::popouts

use fleet::HostId;
use gpui_kit::component::{
    Icon, Sizable, TitleBar,
    button::{Button, ButtonVariants},
    popover::Popover,
};
use gpui_kit::gpui::{
    Anchor, App, AppContext, Bounds, ClickEvent, Context, Decorations, Entity, FocusHandle,
    Focusable, InteractiveElement, IntoElement, ParentElement, Render, StatefulInteractiveElement,
    Styled, Subscription, Window, WindowBounds, WindowDecorations, WindowOptions, div, point,
    prelude::FluentBuilder, px, size,
};

use crate::app::Core;
use crate::layouts::lanes::{agent_swatch, place_label, swatch_tint};
use crate::layouts::paseo::Conversation;
use crate::theme::palette;
use crate::widgets::agent_rename::{AgentRename, RenameDone};
use crate::widgets::label_picker::{ApplyLabels, LabelPicker};
use crate::widgets::lane_color_picker;

/// The title bar's height, as the main window's.
const BAR_HEIGHT: f32 = 36.;
const MIN_WIDTH: f32 = 420.;
const MIN_HEIGHT: f32 = 360.;

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopoutResult {
    Opened,
    Activated,
    Failed,
}

/// Pops `agent` on `host` out into its own window, or brings its window
/// forward if it already has one.
pub fn pop_out(
    core: &Core,
    host: &HostId,
    agent: &str,
    draft: Option<fleet::prompt::Prompt>,
    cx: &mut App,
) -> PopoutResult {
    let key = (host.clone(), agent.to_string());
    if let Some(window) = core.nav.read(cx).popouts.get(&key).copied()
        && window
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return PopoutResult::Activated;
    }
    let title = (core.agent_title(host, agent, cx)).unwrap_or_else(|| "Agent".to_string());
    let saved = (core.nav.read(cx).popout_bounds)
        .get(&crate::app::nav::agent_key(host, agent))
        .copied();
    let displays: Vec<_> = cx
        .displays()
        .into_iter()
        .map(|display| {
            let b = display.bounds();
            [
                b.origin.x.as_f32(),
                b.origin.y.as_f32(),
                b.size.width.as_f32(),
                b.size.height.as_f32(),
            ]
        })
        .collect();
    let bounds = match saved.and_then(|bounds| fit_bounds(bounds, &displays)) {
        Some([x, y, w, h]) => WindowBounds::Windowed(Bounds::new(
            point(px(x), px(y)),
            size(px(w.max(MIN_WIDTH)), px(h.max(MIN_HEIGHT))),
        )),
        None => WindowBounds::centered(size(px(760.), px(860.)), cx),
    };
    let mut options = WindowOptions {
        window_bounds: Some(bounds),
        window_min_size: Some(size(px(MIN_WIDTH), px(MIN_HEIGHT))),
        window_decorations: Some(WindowDecorations::Client),
        ..TitleBar::window_options()
    };
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some(title.into());
    }
    let (core_for_view, host_for_view, agent_for_view) =
        (core.clone(), host.clone(), agent.to_string());
    let opened = gpui_kit::open_window(options, cx, move |window, cx| {
        cx.new(|cx| {
            Popout::new(
                core_for_view,
                &host_for_view,
                &agent_for_view,
                draft,
                window,
                cx,
            )
        })
    });
    let Ok((window, view)) = opened else {
        return PopoutResult::Failed;
    };
    core.nav
        .update(cx, |nav, cx| nav.set_popout(key.clone(), Some(window), cx));
    // Out of the list when its window closes, however it closes, and where
    // it was remembered.
    let nav = core.nav.clone();
    cx.observe_release(&view, move |popout, cx| {
        popout.stash_draft(cx);
        nav.update(cx, |nav, cx| {
            nav.save();
            if nav.popouts.get(&key) == Some(&window) {
                nav.set_popout(key, None, cx);
            }
        });
    })
    .detach();
    PopoutResult::Opened
}

/// The popped-out window's view: its own title bar over the conversation.
pub struct Popout {
    core: Core,
    host: HostId,
    agent: String,
    conversation: Entity<Conversation>,
    /// The title being edited in place, with its subscription.
    rename: Option<(Entity<AgentRename>, Subscription)>,
    /// The label picker, made when first opened.
    labels: Option<((String, bool), Entity<LabelPicker>)>,
    /// The window title last set, to set it again only when it changes.
    title: String,
    focus: FocusHandle,
    chrome: Chrome,
    draft_returned: bool,
    notice: Option<String>,
}

impl Popout {
    fn new(
        core: Core,
        host: &HostId,
        agent: &str,
        draft: Option<fleet::prompt::Prompt>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        window.set_zoom(core.nav.read(cx).zoom, cx);
        let conversation = cx.new(|cx| Conversation::new(core.clone(), host, agent, window, cx));
        conversation.update(cx, |conversation, cx| {
            if let Some(draft) = draft {
                conversation.restore_draft(draft, window, cx);
            }
            conversation.focus_composer(window, cx)
        });
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            weak.update(cx, |this, cx| this.can_close(cx))
                .unwrap_or(true)
        });
        cx.observe(&core.store, |this, _, cx| this.refresh_chrome(cx))
            .detach();
        cx.observe(&core.nav, |this, _, cx| this.refresh_chrome(cx))
            .detach();
        // Where it is, kept in Nav as it moves (any save writes it) and
        // saved when it closes.
        cx.observe_window_bounds(window, |this, window, cx| {
            // The restore bounds, which open a window exactly where it was;
            // `bounds()` is the client area and drifts on reopening.
            if let WindowBounds::Windowed(b) = window.window_bounds() {
                let bounds = [
                    b.origin.x.as_f32(),
                    b.origin.y.as_f32(),
                    b.size.width.as_f32(),
                    b.size.height.as_f32(),
                ];
                this.core.nav.update(cx, |nav, _| {
                    nav.remember_popout_bounds(&this.host, &this.agent, bounds)
                });
            }
        })
        .detach();
        let chrome = Chrome::read(&core, host, agent, cx);
        Self {
            core,
            host: host.clone(),
            agent: agent.to_string(),
            conversation,
            rename: None,
            labels: None,
            title: String::new(),
            focus: cx.focus_handle(),
            chrome,
            draft_returned: false,
            notice: None,
        }
    }

    /// Closes this window and opens the agent in its lane again, in
    /// crusteo's main window.
    fn put_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_close(cx) {
            return;
        }
        let Some(main) = self
            .core
            .main_window()
            .filter(|main| main.update(cx, |_, _, _| ()).is_ok())
        else {
            self.notice = Some(
                "The main window is closed. Keep this window open to continue this conversation."
                    .into(),
            );
            cx.notify();
            return;
        };
        if !self.core.open_agent(&self.host, &self.agent, cx) {
            self.notice = Some("This agent is no longer available in its lane.".into());
            cx.notify();
            return;
        }
        self.stash_draft(cx);
        self.core.nav.update(cx, |nav, cx| {
            nav.set_popout((self.host.clone(), self.agent.clone()), None, cx)
        });
        let _ = main.update(cx, |_, window, _| window.activate_window());
        window.remove_window();
    }

    fn stash_draft(&mut self, cx: &mut App) {
        if self.draft_returned {
            return;
        }
        let draft = self.conversation.read(cx).draft(cx);
        self.core.stash_draft(&self.host, &self.agent, draft, cx);
        self.draft_returned = true;
    }

    fn can_close(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.conversation.read(cx).preparing_attachment(cx) {
            return true;
        }
        self.notice =
            Some("Wait for the image to finish preparing before closing this window.".into());
        cx.notify();
        false
    }

    fn refresh_chrome(&mut self, cx: &mut Context<Self>) {
        let chrome = Chrome::read(&self.core, &self.host, &self.agent, cx);
        if chrome != self.chrome {
            self.chrome = chrome;
            cx.notify();
        }
    }

    fn zoom(&mut self, step: i32, window: &mut Window, cx: &mut Context<Self>) {
        let zoom = crate::shell::zoom_level(window.zoom(), step);
        let handle = window.window_handle();
        // set_zoom synchronously calls bounds observers. Run after this
        // Popout entity update, so its bounds observer can borrow it safely.
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| window.set_zoom(zoom, cx));
        });
    }

    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let core = self.core.clone();
        let editor = cx.new(|cx| AgentRename::new(&core, &self.host, &self.agent, window, cx));
        let done = cx.subscribe(&editor, |this, _, _: &RenameDone, cx| {
            this.rename = None;
            cx.notify();
        });
        self.rename = Some((editor, done));
        cx.notify();
    }

    /// The label picker for this agent, as its card's in Lanes.
    fn label_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<LabelPicker>> {
        let store = self.core.store.read(cx);
        let workspace = store
            .fleet
            .agent(&self.host, &self.agent)?
            .workspace_id
            .clone()?;
        let reef_mode = store.fleet.labels.is_some();
        let supported =
            (store.fleet.host(&self.host)).is_some_and(|host| host.has("workspaceLabels"));
        if !reef_mode && !supported {
            return None;
        }
        let picker_key = (workspace.clone(), reef_mode);
        if let Some((_, picker)) = self.labels.as_ref().filter(|(key, _)| key == &picker_key) {
            return Some(picker.clone());
        }
        // In reef mode labels are this agent's own; else its workspace's.
        let agents = match reef_mode {
            true => vec![self.agent.clone()],
            false => Vec::new(),
        };
        let core = self.core.clone();
        let host = self.host.clone();
        let picker =
            cx.new(|cx| LabelPicker::new(core, &host, &workspace, agents.clone(), window, cx));
        picker.update(cx, |picker, cx| {
            picker.set_bounds(
                Bounds::new(point(px(0.), px(0.)), size(px(320.), px(420.))),
                cx,
            )
        });
        cx.observe(&picker, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&picker, move |this, _, event: &ApplyLabels, cx| {
            let changes = event.0.clone();
            let (host, agents, workspace) = (this.host.clone(), agents.clone(), workspace.clone());
            let store = this.core.store.clone();
            let task = match agents.is_empty() {
                true => store
                    .read(cx)
                    .set_workspace_labels(&host, &workspace, changes, cx),
                false => store.update(cx, |store, cx| {
                    store.relabel_agents(&host, agents, changes, cx)
                }),
            };
            cx.spawn(async move |_, cx| {
                if let Some(error) = task.await.error {
                    store.update(cx, |store, cx| store.toast(error, cx));
                }
            })
            .detach();
        })
        .detach();
        self.labels = Some((picker_key, picker.clone()));
        Some(picker)
    }
}

impl Render for Popout {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let chrome = &self.chrome;
        let title = chrome.title.clone().unwrap_or_else(|| "Agent".into());
        let bucket = chrome.bucket;
        if title != self.title {
            window.set_window_title(&title);
            self.title = title.clone();
        }
        let place = (
            chrome
                .workspace
                .as_ref()
                .map(|(_, workspace)| workspace.clone()),
            chrome.host.clone(),
        );
        let swatch = chrome.swatch;
        let pinned = chrome.pinned;
        let bar_bg = swatch.map_or(p.sidebar, |index| swatch_tint(p.sidebar, index));

        let editing = self.rename.is_some();
        let title_element = match &self.rename {
            Some((editor, _)) => editor.clone().into_any_element(),
            None => div()
                .id("popout-title")
                .min_w(px(0.))
                .truncate()
                .px(px(4.))
                .mx(px(-4.))
                .rounded(px(4.))
                .text_color(p.foreground)
                .hover(move |style| style.bg(p.surface2))
                .child(title)
                .tooltip(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new("Rename agent").build(window, cx)
                })
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.start_rename(window, cx);
                }))
                .into_any_element(),
        };
        let labels = self.label_picker(window, cx).map(|picker| {
            let open = picker.read(cx).is_open();
            let opened = picker.clone();
            let focus = gpui_kit::gpui::Focusable::focus_handle(picker.read(cx), cx);
            Popover::new("popout-labels")
                .anchor(Anchor::TopRight)
                .open(open)
                .p(px(0.))
                .track_focus(&focus)
                .trigger(
                    Button::new("popout-labels-button")
                        .icon(Icon::empty().path("icons/tag.svg"))
                        .ghost()
                        .xsmall()
                        .tooltip("Labels"),
                )
                .on_open_change(move |open, window, cx| {
                    opened.update(cx, |picker, cx| picker.set_open(*open, window, cx));
                })
                .content(move |_, _, _| picker.clone())
                .into_any_element()
        });
        let back = Button::new("popout-put-back")
            .icon(Icon::empty().path("icons/square-arrow-down-left.svg"))
            .ghost()
            .xsmall()
            .tooltip("Put it back in its lane")
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.put_back(window, cx)));
        let color = {
            let (nav, host, agent) = (self.core.nav.clone(), self.host.clone(), self.agent.clone());
            lane_color_picker::picker_titled(
                "popout",
                "Window color",
                swatch.unwrap_or(0),
                px(260.),
                move |index, _, cx| {
                    nav.update(cx, |nav, cx| {
                        nav.set_agent_color(&host, &agent, Some(index), cx)
                    });
                },
            )
        };
        // Back to its label's colour, once one is pinned.
        let unpin = pinned.map(|_| {
            let (nav, host, agent) = (self.core.nav.clone(), self.host.clone(), self.agent.clone());
            Button::new("popout-color-reset")
                .icon(Icon::empty().path("icons/refresh-cw.svg"))
                .ghost()
                .xsmall()
                .tooltip("Use its label's colour")
                .on_click(move |_, _, cx| {
                    nav.update(cx, |nav, cx| nav.set_agent_color(&host, &agent, None, cx));
                })
        });
        // Clear of the window buttons on the right, as the main window's bar.
        let (inset, controls) = if cfg!(target_os = "macos") {
            (80., 0.)
        } else {
            (12., 3. * gpui_kit::component::TITLE_BAR_HEIGHT.as_f32())
        };
        let bar = div()
            .relative()
            .flex_shrink_0()
            .h(px(BAR_HEIGHT))
            .child(
                TitleBar::new()
                    .h(px(BAR_HEIGHT))
                    .bg(bar_bg)
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
                    .gap(px(6.))
                    .children(bucket.map(crate::ui::agent_dot))
                    .child(
                        div()
                            .min_w(px(0.))
                            .flex()
                            // While renaming, the editor takes the bar's room.
                            .when(editing, |slot| slot.flex_1())
                            .child(title_element),
                    )
                    .child(place_label(
                        &self.core,
                        &self.host,
                        &self.agent,
                        place,
                        px(260.),
                        cx,
                    ))
                    // Empty: this part of the bar drags the window (not
                    // while renaming, when the editor takes the room).
                    .when(!editing, |bar| bar.child(div().flex_1().min_w(px(12.))))
                    .children(labels)
                    .child(back)
                    .children(unpin)
                    .child(color),
            );
        let body =
            div()
                .track_focus(&self.focus)
                .key_context("Popout")
                .on_action(cx.listener(|this, _: &ClosePopout, window, cx| {
                    if this.can_close(cx) {
                        window.remove_window();
                    }
                }))
                .on_action(cx.listener(|this, _: &CancelRename, window, cx| {
                    if this.rename.take().is_some() {
                        window.focus(&this.focus, cx);
                        cx.notify();
                    } else {
                        cx.propagate();
                    }
                }))
                .on_action(cx.listener(|this, _: &crate::shell::ZoomIn, window, cx| {
                    this.zoom(1, window, cx)
                }))
                .on_action(cx.listener(|this, _: &crate::shell::ZoomOut, window, cx| {
                    this.zoom(-1, window, cx)
                }))
                .on_action(
                    cx.listener(|this, _: &crate::shell::ZoomReset, window, cx| {
                        this.zoom(0, window, cx)
                    }),
                )
                .size_full()
                .flex()
                .flex_col()
                .bg(p.surface0)
                .child(bar)
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .child(self.conversation.clone()),
                );
        if matches!(window.window_decorations(), Decorations::Client { .. }) {
            gpui_kit::component::window_border()
                .child(body)
                .into_any_element()
        } else {
            body.into_any_element()
        }
    }
}

gpui_kit::actions!(popout, [ClosePopout, CancelRename]);

impl Focusable for Popout {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Only changes that affect the window chrome invalidate its subtree.
#[derive(PartialEq)]
struct Chrome {
    title: Option<String>,
    bucket: Option<fleet::Bucket>,
    workspace: Option<(String, String)>,
    folder: Option<(String, Option<String>)>,
    host: String,
    swatch: Option<usize>,
    pinned: Option<usize>,
    reef_mode: bool,
    workspace_labels: bool,
}
impl Chrome {
    fn read(core: &Core, host: &HostId, agent: &str, cx: &App) -> Self {
        let fleet = &core.store.read(cx).fleet;
        let record = fleet.agent(host, agent);
        Self {
            title: record.map(|agent| agent.display_title()),
            bucket: record.map(|agent| agent.bucket()),
            workspace: record
                .and_then(|agent| agent.workspace_id.as_ref())
                .and_then(|id| {
                    fleet.workspace(host, id).map(|workspace| {
                        (
                            id.clone(),
                            workspace
                                .title
                                .clone()
                                .unwrap_or_else(|| workspace.name.clone()),
                        )
                    })
                }),
            folder: crate::layouts::lanes::agent_folder(core, host, agent, cx),
            host: fleet.host_name(host).to_string(),
            swatch: agent_swatch(core, host, agent, cx),
            pinned: core.nav.read(cx).agent_color(host, agent),
            reef_mode: fleet.labels.is_some(),
            workspace_labels: fleet
                .host(host)
                .is_some_and(|host| host.has("workspaceLabels")),
        }
    }
}

/// Restore only finite, visible rectangles, keeping the whole title bar reachable.
fn fit_bounds(saved: [f32; 4], displays: &[[f32; 4]]) -> Option<[f32; 4]> {
    let [x, y, w, h] = saved;
    if !saved.iter().all(|value| value.is_finite()) || w <= 0. || h <= 0. {
        return None;
    }
    let &[dx, dy, dw, dh] = displays
        .iter()
        .filter(|[dx, dy, dw, dh]| x < dx + dw && x + w > *dx && y < dy + dh && y + h > *dy)
        .max_by(|a, b| {
            let area = |d: &[f32; 4]| {
                ((x + w).min(d[0] + d[2]) - x.max(d[0])) * ((y + h).min(d[1] + d[3]) - y.max(d[1]))
            };
            area(a).total_cmp(&area(b))
        })?;
    let w = w.max(MIN_WIDTH).min(dw.max(MIN_WIDTH));
    let h = h.max(MIN_HEIGHT).min(dh.max(MIN_HEIGHT));
    Some([
        x.clamp(dx, (dx + dw - w).max(dx)),
        y.clamp(dy, (dy + dh - h).max(dy)),
        w,
        h,
    ])
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui_kit::gpui::KeyBinding::new("secondary-w", ClosePopout, Some("Popout")),
        gpui_kit::gpui::KeyBinding::new("escape", CancelRename, Some("Popout")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::fit_bounds;
    #[test]
    fn bounds_restore_clamps_visible_windows_and_rejects_missing_displays_or_invalid_values() {
        let display = [[0., 25., 1440., 875.]];
        assert_eq!(fit_bounds([2000., 100., 760., 860.], &display), None);
        assert_eq!(fit_bounds([f32::NAN, 100., 760., 860.], &display), None);
        assert_eq!(fit_bounds([10., 100., -1., 860.], &display), None);
        assert_eq!(
            fit_bounds([1300., 800., 760., 1000.], &display),
            Some([680., 25., 760., 875.])
        );
        assert_eq!(
            fit_bounds([10., 100., 760., 700.], &display),
            Some([10., 100., 760., 700.])
        );
    }
}
