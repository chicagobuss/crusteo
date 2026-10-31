//! The slide-in pane: the lists behind the title bar's counts and its
//! History button, and Settings. It slides in under the title bar from the
//! edge nearest what opened it, the right for a key, and lies over the
//! layout, which stays as it was underneath: closing it changes nothing
//! else. A click outside it, or Escape, closes it.
//!
//! The slide plays once as it opens and then stops asking for frames, so
//! it is not one of the repeating animations `src/ui.rs` warns of; with
//! reduced motion it is not played at all.

use std::time::Duration;

use gpui_kit::gpui::{
    Animation, AnimationExt as _, AnyElement, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, SharedString, Styled, Window, div, ease_out_quint, px, relative,
};
use gpui_kit::prelude::FluentBuilder as _;

use crate::app::Core;
use crate::theme::palette;
use crate::toolbars::Side;

/// The title bar's height: the pane slides in under it.
pub const TOP: f32 = 36.;
/// How long the pane takes to slide in.
const SLIDE: Duration = Duration::from_millis(160);

/// The window's edge nearest `x`.
pub fn side_at(x: Pixels, window: &Window) -> Side {
    if x < window.viewport_size().width / 2. {
        Side::Left
    } else {
        Side::Right
    }
}

/// `content` in a pane `width` wide at the `side` edge, over a clear layer
/// that closes it on a click outside. `key` names what it shows, so a
/// switch to other content of the same kind, such as another state's
/// list, does not slide it in again.
pub fn layer(key: &str, side: Side, width: f32, content: AnyElement, core: &Core) -> AnyElement {
    let p = palette();
    let nav = core.nav.clone();
    let outside = div()
        .id("pane-outside")
        .absolute()
        .top(px(TOP))
        .left_0()
        .right_0()
        .bottom_0()
        .occlude()
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            cx.stop_propagation();
            nav.update(cx, |nav, cx| nav.set_pane(None, cx));
        });
    let panel = div()
        .id("pane")
        .absolute()
        .top(px(TOP))
        .bottom_0()
        .w(px(width))
        .max_w(relative(0.9))
        .bg(p.surface0)
        .border_color(p.border_accent)
        .map(|panel| match side {
            Side::Left => panel.left_0().border_r_1(),
            Side::Right => panel.right_0().border_l_1(),
        })
        .shadow_lg()
        .occlude()
        // A click inside is the pane's; the window's "any click closes"
        // must not take it away before the click lands.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(content);
    // Played once, as the module says, so not what the lint guards against.
    #[allow(clippy::disallowed_methods)]
    let panel = panel.with_animation(
        SharedString::from(format!("pane-{key}-{side:?}")),
        Animation::new(SLIDE).with_easing(ease_out_quint()),
        move |panel, shown| {
            let off = px(-width * (1. - shown));
            match side {
                Side::Left => panel.left(off),
                Side::Right => panel.right(off),
            }
        },
    );
    div()
        .absolute()
        .inset_0()
        .child(outside)
        .child(panel)
        .into_any_element()
}
