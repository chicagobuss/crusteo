//! Slim strip under the title bar: pending Paseo permissions, and what
//! extensions' [`NeedsSource`]s say needs a person.

use std::rc::Rc;

use gpui_kit::gpui::{
    App, ClickEvent, Context, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};
use gpui_kit::prelude::FluentBuilder;

use crate::app::Core;
use crate::extensions::NeedsSource;
use crate::theme::{palette, size as sz};
use crate::ui;
use crate::widgets::agent_controls::permission_buttons;

/// Permission chips past this many collapse into one "+N more" chip. Each
/// chip carries real buttons, and a strip one line tall never shows more.
const MAX_CHIPS: usize = 12;

pub struct NeedsYou {
    core: Core,
    /// Extensions' sources of items, set once the window has asked them.
    sources: Vec<Rc<dyn NeedsSource>>,
    collapsed: bool,
    /// Agents with a pending permission, sorted, and a hash of what their
    /// chips show. Most store changes leave both alone and skip the render.
    pending: Vec<(String, String)>,
    pending_key: u64,
}

impl NeedsYou {
    pub fn new(core: Core, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |this, _, cx| {
            if this.refresh_pending(cx) {
                cx.notify();
            }
        })
        .detach();
        let mut this = Self {
            core,
            sources: Vec::new(),
            collapsed: false,
            pending: Vec::new(),
            pending_key: 0,
        };
        this.refresh_pending(cx);
        this
    }

    /// Recollect the pending permissions; true when the chips would change.
    pub fn set_sources(&mut self, sources: Vec<Rc<dyn NeedsSource>>, cx: &mut Context<Self>) {
        self.sources = sources;
        cx.notify();
    }

    pub fn sources(&self) -> &[Rc<dyn NeedsSource>] {
        &self.sources
    }

    fn refresh_pending(&mut self, cx: &App) -> bool {
        use std::hash::{Hash, Hasher};
        let store = self.core.store.read(cx);
        let mut pending: Vec<(String, String)> = store
            .fleet
            .agents()
            .filter(|(host, agent)| {
                host.connection.is_online() && !agent.pending_permissions.is_empty()
            })
            .map(|(host, agent)| (host.name().to_string(), agent.id.clone()))
            .collect();
        pending.sort();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        pending.len().hash(&mut hasher);
        for (host, id) in pending.iter().take(MAX_CHIPS) {
            host.hash(&mut hasher);
            id.hash(&mut hasher);
            if let Some(agent) = store.fleet.agent(host, id) {
                agent.display_title().hash(&mut hasher);
                if let Some(request) = agent.pending_permissions.first() {
                    request.to_string().hash(&mut hasher);
                }
            }
        }
        let key = hasher.finish();
        self.pending = pending;
        let changed = key != self.pending_key;
        self.pending_key = key;
        changed
    }
}

fn permission_title(agent: &fleet::Agent) -> String {
    agent
        .pending_permissions
        .first()
        .and_then(|request| request["title"].as_str().or(request["name"].as_str()))
        .unwrap_or("Permission")
        .to_string()
}

impl Render for NeedsYou {
    fn render(
        &mut self,
        _window: &mut gpui_kit::gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = palette();
        let store = self.core.store.read(cx);
        let pending = &self.pending;
        let extra: Vec<(Rc<dyn NeedsSource>, crate::extensions::NeedsItem)> = (self.sources.iter())
            .flat_map(|source| {
                (source.items(cx).into_iter()).map(move |item| (source.clone(), item))
            })
            .collect();
        let statuses: Vec<SharedString> = (self.sources.iter())
            .filter_map(|source| source.status(cx))
            .collect();
        let count = pending.len() + extra.len();

        let mut items = div()
            .id("needs-you-items")
            .flex_1()
            .min_w(px(0.))
            .overflow_x_scroll()
            .flex()
            .items_center()
            .gap(px(6.));
        if pending.is_empty() && extra.is_empty() {
            items = items.child(
                div()
                    .text_size(sz::sm())
                    .text_color(p.extra_muted)
                    .child("Nothing waiting"),
            );
        }
        for (host_name, agent_id) in pending.iter().take(MAX_CHIPS) {
            let Some(agent) = store.fleet.agent(host_name, agent_id) else {
                continue;
            };
            let Some(buttons) = permission_buttons(&self.core, host_name, agent, "ny") else {
                continue;
            };
            let label = format!("{host_name} · {}", agent.display_title());
            let title = permission_title(agent);
            items = items.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(6.))
                    .bg(p.surface1)
                    .border_1()
                    .border_color(p.border)
                    .child(ui::icon("circle-alert", 12., p.dot_warning))
                    .child(
                        div()
                            .max_w(px(220.))
                            .truncate()
                            .text_size(sz::sm())
                            .text_color(p.foreground)
                            .child(label),
                    )
                    .child(
                        div()
                            .max_w(px(180.))
                            .truncate()
                            .text_size(sz::sm())
                            .text_color(p.status_warning)
                            .child(title),
                    )
                    .child(buttons),
            );
        }
        if pending.len() > MAX_CHIPS {
            items = items.child(
                div()
                    .flex_shrink_0()
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(6.))
                    .bg(p.surface1)
                    .border_1()
                    .border_color(p.border)
                    .text_size(sz::sm())
                    .text_color(p.muted)
                    .child(format!("+{} more", pending.len() - MAX_CHIPS)),
            );
        }
        for (ix, (source, item)) in extra.into_iter().enumerate() {
            let id = item.id.clone();
            items = items.child(
                div()
                    .id(("needs-item", ix))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(2.))
                    .max_w(px(360.))
                    .rounded(px(6.))
                    .bg(if item.selected {
                        p.surface3
                    } else {
                        p.surface1
                    })
                    .border_1()
                    .border_color(p.border)
                    .hover(|style| style.bg(p.surface2))
                    .child(ui::icon(source.icon(), 12., p.muted))
                    .child(
                        div()
                            .flex_shrink_0()
                            .max_w(px(140.))
                            .truncate()
                            .text_size(sz::sm())
                            .text_color(p.foreground)
                            .child(item.title),
                    )
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(sz::sm())
                            .text_color(p.muted)
                            .child(item.detail),
                    )
                    .on_click(move |_: &ClickEvent, window, cx| source.open(&id, window, cx)),
            );
        }

        let collapsed = self.collapsed;

        // The bar it sits in draws the frame.
        div()
            .size_full()
            .min_w(px(0.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .id("needs-you-toggle")
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(4.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .hover(|style| style.bg(p.sidebar_hover))
                    .child(ui::icon(
                        if collapsed {
                            "chevron-right"
                        } else {
                            "chevron-down"
                        },
                        14.,
                        p.muted,
                    ))
                    .child(
                        div()
                            .text_size(sz::sm())
                            .text_color(p.foreground)
                            .child("Needs you"),
                    )
                    .child(
                        div()
                            .text_size(sz::sm())
                            .text_color(p.extra_muted)
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.collapsed = !this.collapsed;
                        cx.notify();
                    })),
            )
            .when(!collapsed, |row| row.child(items))
            .when(collapsed, |row| row.child(div().flex_1()))
            .children(statuses.into_iter().map(|status| {
                div()
                    .flex_shrink_0()
                    .text_size(sz::sm())
                    .text_color(p.extra_muted)
                    .child(status)
            }))
    }
}
