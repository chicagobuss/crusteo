//! Passive overflow fades; no hitboxes or animation/frame loop.
//! Geometric hover deliberately includes overlays; it only reveals the scrollbar.
use gpui_kit::gpui::{
    Bounds, IntoElement, ListState, Pixels, Point, Rgba, ScrollHandle, Styled, Window, canvas,
    fill, linear_color_stop, linear_gradient, point, px, size,
};

pub const FADE_WIDTH: f32 = 40.;
/// How deep a lane's top and bottom shadows are.
const FADE_HEIGHT: f32 = 36.;
/// Laid over the board's side fades, so they read as an edge against dark
/// cards and not only as content dimming into the background.
const SHADOW: Rgba = Rgba {
    r: 0.,
    g: 0.,
    b: 0.,
    a: 0.45,
};
/// A lane's top and bottom are a shadow alone, and a deeper one: fading
/// into the lane's colour reads as the card simply ending wherever a lane's
/// cards are tinted close to it.
const LANE_SHADOW: Rgba = Rgba {
    r: 0.,
    g: 0.,
    b: 0.,
    a: 0.7,
};

#[derive(Clone, Copy)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// One edge's fade, `depth` deep inside `bounds`: the `surface` the content
/// fades into, if any, and `shadow` over it at the edge.
fn paint_fade(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    edge: Edge,
    depth: Pixels,
    surface: Option<Rgba>,
    shadow: Rgba,
) {
    // Gradient angles run as CSS's do: 90 is toward the right.
    let (rect, angle) = match edge {
        Edge::Left => (
            Bounds::new(bounds.origin, size(depth, bounds.size.height)),
            90.,
        ),
        Edge::Right => (
            Bounds::new(
                point(bounds.right() - depth, bounds.top()),
                size(depth, bounds.size.height),
            ),
            270.,
        ),
        Edge::Top => (
            Bounds::new(bounds.origin, size(bounds.size.width, depth)),
            180.,
        ),
        Edge::Bottom => (
            Bounds::new(
                point(bounds.left(), bounds.bottom() - depth),
                size(bounds.size.width, depth),
            ),
            0.,
        ),
    };
    for color in surface.into_iter().chain([shadow]) {
        window.paint_quad(fill(
            rect,
            linear_gradient(
                angle,
                linear_color_stop(color, 0.),
                linear_color_stop(color, 1.).opacity(0.),
            ),
        ));
    }
}

pub fn overflow(offset: f32, maximum: f32) -> (bool, bool) {
    // Ignore subpixel rounding at either end.
    (offset < -0.5, maximum + offset > 0.5)
}

fn width(bounds: Bounds<Pixels>) -> Pixels {
    px(FADE_WIDTH).min(bounds.size.width / 2.)
}

pub fn hovered(handle: &ScrollHandle, pointer: Point<Pixels>) -> bool {
    hovered_in(
        handle.bounds(),
        handle.offset().x / px(1.),
        handle.max_offset().x / px(1.),
        pointer,
    )
}

fn hovered_in(bounds: Bounds<Pixels>, offset: f32, maximum: f32, pointer: Point<Pixels>) -> bool {
    let (left, right) = overflow(offset, maximum);
    bounds.contains(&pointer)
        && ((left && pointer.x <= bounds.left() + width(bounds))
            || (right && pointer.x >= bounds.right() - width(bounds)))
}

pub fn fades(handle: &ScrollHandle, background: Rgba) -> impl IntoElement {
    let handle = handle.clone();
    canvas(
        move |_, _, _| overflow(handle.offset().x / px(1.), handle.max_offset().x / px(1.)),
        move |bounds, (left, right), window, _| {
            let w = width(bounds);
            if left {
                paint_fade(window, bounds, Edge::Left, w, Some(background), SHADOW);
            }
            if right {
                paint_fade(window, bounds, Edge::Right, w, Some(background), SHADOW);
            }
        },
    )
    .absolute()
    .size_full()
}

/// Whether a lane's list has cards above and below what it shows: scrolled
/// past its first row, and its last row not wholly in view.
fn list_overflow(state: &ListState) -> (bool, bool) {
    let count = state.item_count();
    if count == 0 {
        return (false, false);
    }
    let viewport = state.viewport_bounds();
    let top = state.logical_scroll_top();
    let above = top.item_ix > 0 || top.offset_in_item > px(0.5);
    // A last row not laid out lies past the bottom.
    let below = state
        .bounds_for_item(count - 1)
        .is_none_or(|last| last.bottom() > viewport.bottom() + px(0.5));
    (above, below)
}

/// Fades at the top and bottom of a lane's list where it has more cards to
/// scroll to. Goes after the list, so it reads the list's fresh layout.
pub fn list_fades(state: ListState) -> impl IntoElement {
    canvas(
        {
            let state = state.clone();
            move |_, _, _| list_overflow(&state)
        },
        move |_, (above, below), window, _| {
            let viewport = state.viewport_bounds();
            let depth = px(FADE_HEIGHT).min(viewport.size.height / 2.);
            if above {
                paint_fade(window, viewport, Edge::Top, depth, None, LANE_SHADOW);
            }
            if below {
                paint_fade(window, viewport, Edge::Bottom, depth, None, LANE_SHADOW);
            }
        },
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_available_directions_fade() {
        assert_eq!(overflow(0., 0.), (false, false));
        assert_eq!(overflow(0., 100.), (false, true));
        assert_eq!(overflow(-50., 100.), (true, true));
        assert_eq!(overflow(-100., 100.), (true, false));
        assert_eq!(overflow(-0.1, 0.2), (false, false));
    }
    #[test]
    fn hover_matches_the_painted_fade_even_on_a_narrow_board() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(20.), px(100.)));
        assert!(hovered_in(bounds, -100., 100., point(px(5.), px(50.))));
        assert!(!hovered_in(bounds, -100., 100., point(px(15.), px(50.))));
        assert!(!hovered_in(bounds, 0., 100., point(px(5.), px(50.))));
        assert!(hovered_in(bounds, 0., 100., point(px(15.), px(50.))));
        assert!(!hovered_in(bounds, -50., 100., point(px(5.), px(110.))));
        assert!(!hovered_in(bounds, 0., 0., point(px(5.), px(50.))));
    }
}
