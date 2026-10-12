//! Small drawing helpers shared by every layout.
//!
//! Repeating GPUI animations continuously schedule full-window frames. Status
//! visuals must settle between actual data/input changes. Any future animation
//! needs visibility gating and a measured idle CPU budget before enabling it.

use fleet::Bucket;
use fleet::display::ToolIcon;
use gpui_kit::gpui::{Div, Hsla, IntoElement, ParentElement, Rgba, Styled, div, px, svg};

use crate::theme::palette;

/// The cursor over something that can be picked up and dragged. Windows has
/// no open hand (GPUI shows the arrow for it there), so its hand stands in.
pub fn grab_cursor() -> gpui_kit::gpui::CursorStyle {
    if cfg!(target_os = "windows") {
        gpui_kit::gpui::CursorStyle::PointingHand
    } else {
        gpui_kit::gpui::CursorStyle::OpenHand
    }
}

/// A Lucide icon by file name (`"git-branch"`), tinted.
pub fn icon(name: &'static str, size: f32, color: impl Into<Hsla>) -> impl IntoElement {
    svg()
        .path(format!("icons/{name}.svg"))
        .flex_shrink_0()
        .size(px(size))
        .text_color(color.into())
}

/// An icon by a name only known at run time, such as a script's. Names not
/// among the embedded icons draw nothing.
pub fn icon_named(name: &str, size: f32, color: impl Into<Hsla>) -> impl IntoElement {
    svg()
        .path(format!("icons/{name}.svg"))
        .flex_shrink_0()
        .size(px(size))
        .text_color(color.into())
}

pub fn tool_icon_name(icon: ToolIcon) -> &'static str {
    match icon {
        ToolIcon::Terminal => "square-terminal",
        ToolIcon::Eye => "eye",
        ToolIcon::Pencil => "pencil",
        ToolIcon::Search => "search",
        ToolIcon::Bot => "bot",
        ToolIcon::Wrench => "wrench",
        ToolIcon::Brain => "brain",
        ToolIcon::Sparkles => "sparkles",
        ToolIcon::Mic => "mic-vocal",
        ToolIcon::Paseo => "layers",
    }
}

pub fn bucket_color(bucket: Bucket) -> Option<Rgba> {
    let p = palette();
    match bucket {
        Bucket::NeedsInput => Some(p.dot_warning),
        Bucket::Failed => Some(p.dot_danger),
        Bucket::Running => Some(p.dot_running),
        Bucket::Attention => Some(p.dot_success),
        Bucket::Done => None,
    }
}

fn dot(size: f32, color: impl Into<Hsla>) -> Div {
    div().size(px(size)).rounded_full().bg(color.into())
}

/// A static ring around the running status dot.
pub fn running_ring() -> impl IntoElement {
    let color = palette().dot_running;
    div()
        .size(px(14.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .border(px(1.5))
        .border_color(crate::theme::with_alpha(color, 0.3))
        .child(dot(6., color))
}

/// Put a static running ring before a flexible tool label.
pub fn with_running_ring(element: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_1()
        .min_w(px(0.))
        .items_center()
        .gap(px(6.))
        .child(running_ring())
        .child(element)
}

/// Shared glyphs for workspace status indicators and agent dots.
fn bucket_glyph(bucket: Bucket) -> gpui_kit::gpui::AnyElement {
    let p = palette();
    match bucket {
        Bucket::Running => running_ring().into_any_element(),
        Bucket::NeedsInput => icon("circle-alert", 12., p.dot_warning).into_any_element(),
        Bucket::Done => dot(6., crate::theme::with_alpha(p.extra_muted, 0.3)).into_any_element(),
        other => dot(6., bucket_color(other).unwrap_or(p.extra_muted)).into_any_element(),
    }
}

/// The leading status slot of a workspace row.
pub fn status_indicator(bucket: Bucket, kind: &str) -> impl IntoElement {
    let p = palette();
    let slot = div()
        .w(px(16.))
        .h(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0();
    match bucket {
        Bucket::Failed => {
            let name = match kind {
                "local_checkout" => "monitor",
                "worktree" => "folder-git-2",
                _ => "folder",
            };
            slot.relative()
                .child(icon(name, 14., p.muted))
                .child(
                    div()
                        .absolute()
                        .right(px(0.))
                        .bottom(px(2.))
                        .size(px(8.))
                        .rounded_full()
                        .border_1()
                        .border_color(p.surface0)
                        .bg(p.dot_danger),
                )
                .into_any_element()
        }
        other => slot.child(bucket_glyph(other)).into_any_element(),
    }
}

/// A small coloured dot for an agent row or tab.
pub fn agent_dot(bucket: Bucket) -> impl IntoElement {
    div()
        .size(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .child(bucket_glyph(bucket))
}

/// Project icon fallback: identity-coloured square with a white initial.
pub fn project_badge(name: &str, key: &str) -> impl IntoElement {
    let (fill, _) = crate::theme::identity_for(key);
    let initial = name
        .rsplit('/')
        .next()
        .and_then(|s| s.chars().next())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    div()
        .size(px(16.))
        .rounded(px(4.))
        .bg(gpui_kit::gpui::rgb(fill))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(9.))
        .text_color(gpui_kit::gpui::rgb(0xffffff))
        .child(initial)
}

pub fn host_status_dot(online: &fleet::Connection) -> impl IntoElement {
    let p = palette();
    let color = match online {
        fleet::Connection::Online => p.status_success,
        fleet::Connection::Connecting | fleet::Connection::Idle => p.status_warning,
        _ => p.status_danger,
    };
    dot(8., color)
}

/// Layout for a cached child view: it fills the box its parent gives it.
/// Cached views are laid out from this style, not measured from content.
pub fn fill() -> gpui_kit::gpui::StyleRefinement {
    use gpui_kit::gpui::Styled;
    gpui_kit::gpui::StyleRefinement::default().size_full()
}

/// Hands a virtual list its new items, telling it which changed, so it keeps
/// its scroll position and remeasures only those. A list at its very top
/// stays there, so items arriving above the first one show instead of
/// pushing the view down.
pub fn sync_list<T: PartialEq>(list: &gpui_kit::gpui::ListState, items: &mut Vec<T>, new: Vec<T>) {
    if *items == new {
        return;
    }
    let top = list.logical_scroll_top();
    let at_top = top.item_ix == 0 && top.offset_in_item == px(0.);
    let prefix = items.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = items[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (old_end, new_end) = (items.len() - suffix, new.len() - suffix);
    if old_end == new_end {
        list.remeasure_items(prefix..old_end);
    } else {
        list.splice(prefix..old_end, new_end - prefix);
    }
    if at_top {
        list.scroll_to(gpui_kit::gpui::ListOffset::default());
    }
    *items = new;
}

pub trait CachedExt {
    fn cached_fill(self) -> gpui_kit::gpui::AnyElement;
}

impl<V: gpui_kit::gpui::Render> CachedExt for gpui_kit::gpui::Entity<V> {
    fn cached_fill(self) -> gpui_kit::gpui::AnyElement {
        self.cached(fill()).into_any_element()
    }
}

impl CachedExt for gpui_kit::gpui::AnyView {
    fn cached_fill(self) -> gpui_kit::gpui::AnyElement {
        self.cached(fill()).into_any_element()
    }
}

/// A small modal asking for one line of text. `on_submit` gets the trimmed
/// value when the user confirms with Enter or the button.
pub fn prompt_text(
    window: &mut gpui_kit::gpui::Window,
    cx: &mut gpui_kit::gpui::App,
    title: &str,
    initial: &str,
    confirm: &str,
    on_submit: impl Fn(String, &mut gpui_kit::gpui::App) + 'static,
) {
    use gpui_kit::component::WindowExt;
    use gpui_kit::component::button::{Button, ButtonVariants};
    use gpui_kit::component::input::{Input, InputEvent, InputState};
    use gpui_kit::gpui::AppContext;
    use std::rc::Rc;

    let initial = initial.to_string();
    let input = cx.new(|cx| InputState::new(window, cx).default_value(initial));
    type OnSubmit = Rc<dyn Fn(String, &mut gpui_kit::gpui::App)>;
    let on_submit: OnSubmit = Rc::new(on_submit);
    let submit = {
        let input = input.clone();
        let on_submit = on_submit.clone();
        move |window: &mut gpui_kit::gpui::Window, cx: &mut gpui_kit::gpui::App| {
            let value = input.read(cx).value().trim().to_string();
            window.close_dialog(cx);
            on_submit(value, cx);
        }
    };
    let submit = Rc::new(submit);
    {
        let submit = submit.clone();
        window
            .subscribe(&input, cx, move |_, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    submit(window, cx);
                }
            })
            .detach();
    }
    {
        // The dialog takes focus when it opens; claim it back afterwards.
        let input = input.clone();
        window.defer(cx, move |window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
        });
    }
    let title = title.to_string();
    let confirm = confirm.to_string();
    window.open_dialog(cx, move |dialog, _, _| {
        let submit = submit.clone();
        dialog
            .title(title.clone())
            .w(px(420.))
            .child(Input::new(&input))
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("prompt-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("prompt-ok")
                            .label(confirm.clone())
                            .primary()
                            .on_click(move |_, window, cx| submit(window, cx)),
                    ),
            )
    });
}

/// A modal offering two ways to go on besides cancelling: `primary` is the
/// highlighted choice, `secondary` the plain one beside it.
pub fn choose(
    window: &mut gpui_kit::gpui::Window,
    cx: &mut gpui_kit::gpui::App,
    title: &str,
    body: &str,
    (secondary, on_secondary): (
        &str,
        impl Fn(&mut gpui_kit::gpui::Window, &mut gpui_kit::gpui::App) + 'static,
    ),
    (primary, on_primary): (
        &str,
        impl Fn(&mut gpui_kit::gpui::Window, &mut gpui_kit::gpui::App) + 'static,
    ),
) {
    use gpui_kit::component::WindowExt;
    use gpui_kit::component::button::{Button, ButtonVariants};
    use gpui_kit::gpui::{InteractiveElement, StatefulInteractiveElement};
    use std::rc::Rc;

    type Action = Rc<dyn Fn(&mut gpui_kit::gpui::Window, &mut gpui_kit::gpui::App)>;
    let on_secondary: Action = Rc::new(on_secondary);
    let on_primary: Action = Rc::new(on_primary);
    let (title, body) = (title.to_string(), body.to_string());
    let (secondary, primary) = (secondary.to_string(), primary.to_string());
    window.open_dialog(cx, move |dialog, _, _| {
        let (on_secondary, on_primary) = (on_secondary.clone(), on_primary.clone());
        dialog
            .title(title.clone())
            .w(px(440.))
            .child(
                div()
                    .id("choose-body")
                    .max_h(px(280.))
                    .overflow_y_scroll()
                    .text_color(palette().muted)
                    .child(body.clone()),
            )
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("choose-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("choose-secondary")
                            .label(secondary.clone())
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                on_secondary(window, cx);
                            }),
                    )
                    .child(
                        Button::new("choose-primary")
                            .label(primary.clone())
                            .danger()
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                on_primary(window, cx);
                            }),
                    ),
            )
    });
}

/// A yes/no modal for destructive actions.
pub fn confirm(
    window: &mut gpui_kit::gpui::Window,
    cx: &mut gpui_kit::gpui::App,
    title: &str,
    body: &str,
    confirm: &str,
    on_confirm: impl Fn(&mut gpui_kit::gpui::App) + 'static,
) {
    use gpui_kit::component::WindowExt;
    use gpui_kit::component::button::{Button, ButtonVariants};
    use gpui_kit::gpui::{InteractiveElement, StatefulInteractiveElement};
    use std::rc::Rc;

    let on_confirm: Rc<dyn Fn(&mut gpui_kit::gpui::App)> = Rc::new(on_confirm);
    let title = title.to_string();
    let body = body.to_string();
    let confirm = confirm.to_string();
    window.open_dialog(cx, move |dialog, _, _| {
        let on_confirm = on_confirm.clone();
        dialog
            .title(title.clone())
            .w(px(440.))
            .child(
                div()
                    .id("confirm-body")
                    .max_h(px(280.))
                    .overflow_y_scroll()
                    .text_color(palette().muted)
                    .child(body.clone()),
            )
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("confirm-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("confirm-ok")
                            .label(confirm.clone())
                            .danger()
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                on_confirm(cx);
                            }),
                    ),
            )
    });
}
