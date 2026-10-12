//! Passive overflow fades; no hitboxes or animation/frame loop.
//! Geometric hover deliberately includes overlays; it only reveals the scrollbar.
use gpui_kit::gpui::{
    Bounds, IntoElement, Pixels, Point, Rgba, ScrollHandle, Styled, canvas, fill,
    linear_color_stop, linear_gradient, point, px, size,
};

pub const FADE_WIDTH: f32 = 32.;

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
            let solid = linear_color_stop(background, 0.);
            let clear = linear_color_stop(background, 1.).opacity(0.);
            if left {
                window.paint_quad(fill(
                    Bounds::new(bounds.origin, size(w, bounds.size.height)),
                    linear_gradient(90., solid, clear),
                ));
            }
            if right {
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.right() - w, bounds.top()),
                        size(w, bounds.size.height),
                    ),
                    linear_gradient(270., solid, clear),
                ));
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
