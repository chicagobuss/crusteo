//! What the layouts that lay the fleet out in columns share: Lanes, Fleet
//! and Board. Scrolling across the columns, with the wheel over their
//! headers, Shift+wheel, or the middle button, with fades at the edges and
//! a scrollbar on hover; and a column width the user sets with a slider,
//! Fit columns or Cmd/Ctrl+wheel, kept per layout.

pub mod edges;

use std::time::{Duration, Instant};

use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{Icon, Sizable};
use gpui_kit::gpui::{
    Anchor, AnyElement, AppContext, Context, Div, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    ScrollDelta, ScrollHandle, ScrollWheelEvent, Stateful, StatefulInteractiveElement, Styled,
    Window, anchored, deferred, div, point, px,
};

use crate::app::Core;
use crate::app::nav::{COLUMN_WIDTH_MAX, COLUMN_WIDTH_MIN};
use crate::theme::{palette, size};

pub const COLUMN_GAP: f32 = 12.;
pub const COLUMNS_PADDING: f32 = 16.;
/// The narrowest Fit columns makes a column; past that, the board scrolls.
pub const FIT_MIN_WIDTH: f32 = 160.;
/// Cmd/Ctrl+wheel width change per wheel line.
const WHEEL_WIDTH_STEP: f32 = 20.;

const PAN_THRESHOLD: f32 = 4.;
const AUTOSCROLL_DEAD_ZONE: f32 = 12.;
/// Scroll speed in pixels per second, per pixel the pointer is past the dead zone.
const AUTOSCROLL_GAIN: f32 = 8.;
const AUTOSCROLL_TICK: Duration = Duration::from_millis(7);

/// Scrolling across columns with the middle mouse button, the way browsers
/// do it on Windows: hold and drag to pan, or click to autoscroll.
#[derive(Debug, Clone, Copy)]
enum MiddleScroll {
    /// Button held. Turns into a pan once the pointer moves a few pixels.
    Held {
        origin: Point<Pixels>,
        offset: Pixels,
        panning: bool,
    },
    /// Clicked without moving: the board scrolls toward the pointer, faster
    /// the further it is from `origin`. Any click or Escape ends it.
    Auto { origin: Point<Pixels> },
}

/// A board of columns scrolling sideways. Its view holds one, hands
/// [`listen`] a way to reach it, and draws [`Self::overlays`] over the
/// columns.
pub struct SideScroll {
    pub handle: ScrollHandle,
    middle: Option<MiddleScroll>,
    /// Last pointer position over the board, in window coordinates.
    pointer: Point<Pixels>,
    pointer_inside: bool,
    window_active: bool,
}

/// How a view reaches its [`SideScroll`].
pub type SideOf<V> = fn(&mut V) -> &mut SideScroll;

impl SideScroll {
    pub fn new(window: &Window) -> Self {
        Self {
            handle: ScrollHandle::new(),
            middle: None,
            pointer: Point::default(),
            pointer_inside: false,
            window_active: window.is_window_active(),
        }
    }

    pub fn pointer(&self) -> Point<Pixels> {
        self.pointer
    }

    pub fn window_active(&self) -> bool {
        self.window_active
    }

    /// Moves the scroll offset (zero or negative), clamped to the content.
    pub fn set_x<V: 'static>(&mut self, x: Pixels, cx: &mut Context<V>) {
        let current = self.handle.offset();
        let min = -self.handle.max_offset().x;
        let next = if x > px(0.) {
            px(0.)
        } else if x < min {
            min
        } else {
            x
        };
        if next != current.x {
            self.handle.set_offset(point(next, current.y));
            cx.notify();
        }
    }

    /// Positive `dx` reveals later columns.
    pub fn nudge<V: 'static>(&mut self, dx: f32, cx: &mut Context<V>) {
        let x = self.handle.offset().x - px(dx);
        self.set_x(x, cx);
    }

    fn middle_down<V: 'static>(&mut self, position: Point<Pixels>, cx: &mut Context<V>) {
        self.middle = match self.middle {
            Some(MiddleScroll::Auto { .. }) => None,
            _ => Some(MiddleScroll::Held {
                origin: position,
                offset: self.handle.offset().x,
                panning: false,
            }),
        };
        cx.notify();
    }

    fn middle_up<V: 'static>(&mut self, side: SideOf<V>, cx: &mut Context<V>) {
        match self.middle {
            Some(MiddleScroll::Held {
                origin,
                panning: false,
                ..
            }) => {
                self.middle = Some(MiddleScroll::Auto { origin });
                run_autoscroll(side, cx);
            }
            Some(MiddleScroll::Held { .. }) => self.middle = None,
            _ => return,
        }
        cx.notify();
    }

    /// Ends a middle-button pan or autoscroll; true if one was going.
    pub fn stop_middle<V: 'static>(&mut self, cx: &mut Context<V>) -> bool {
        if self.middle.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    /// The pointer is over a fade with columns past it, which shows the
    /// scrollbar.
    fn edge_hover(&self) -> bool {
        self.window_active && self.pointer_inside && edges::hovered(&self.handle, self.pointer)
    }

    fn pointer_moved<V: 'static>(&mut self, position: Point<Pixels>, cx: &mut Context<V>) {
        let was_hover = self.edge_hover();
        self.pointer = position;
        self.pointer_inside = true;
        if self.edge_hover() != was_hover {
            cx.notify();
        }
        let Some(MiddleScroll::Held {
            origin,
            offset,
            panning,
        }) = self.middle
        else {
            return;
        };
        let dx = position.x - origin.x;
        if panning || (dx / px(1.)).abs() > PAN_THRESHOLD {
            self.middle = Some(MiddleScroll::Held {
                origin,
                offset,
                panning: true,
            });
            // The board follows the pointer, like dragging paper.
            self.set_x(offset + dx, cx);
            cx.notify();
        }
    }

    fn pointer_left<V: 'static>(&mut self, cx: &mut Context<V>) {
        let was_hover = self.edge_hover();
        self.pointer_inside = false;
        if was_hover {
            cx.notify();
        }
    }

    /// Follows the window gaining or losing focus; losing it ends a pan.
    pub fn set_window_active<V: 'static>(&mut self, active: bool, cx: &mut Context<V>) {
        self.window_active = active;
        if !active {
            self.stop_middle(cx);
        } else {
            cx.notify();
        }
    }

    /// A mouse wheel turns sideways over the top `header` of the board,
    /// where the column headers are, and anywhere with Shift. A plain wheel
    /// elsewhere is left to the columns. True when it scrolled the board.
    pub fn wheel<V: 'static>(
        &mut self,
        event: &ScrollWheelEvent,
        header: Pixels,
        cx: &mut Context<V>,
    ) -> bool {
        // Headers scroll across columns with a plain wheel, and so do the
        // gaps between them, so a wheel along the header row never falls
        // through a gap. Lower gaps stay inert.
        let board = self.handle.bounds();
        let header_row = event.position.y >= board.top() && event.position.y < board.top() + header;
        if !event.modifiers.shift && !header_row {
            return false;
        }
        // Only a mouse wheel turns sideways; a trackpad's swipe is left to
        // the board, which takes its sideways part alone.
        if !matches!(event.delta, ScrollDelta::Lines(_)) {
            return false;
        }
        let dy = event.delta.pixel_delta(px(16.)).y / px(1.);
        if dy == 0. {
            return false;
        }
        // Wheel down is a negative delta; that should reveal later columns.
        self.nudge(-dy, cx);
        true
    }

    /// Whether the board is panning or autoscrolling, for the cursor.
    pub fn scrolling(&self) -> bool {
        matches!(
            self.middle,
            Some(MiddleScroll::Auto { .. } | MiddleScroll::Held { panning: true, .. })
        )
    }

    /// The fades at the ends with columns past them, and the scrollbar.
    /// Laid over the columns, in a container as large as they are.
    pub fn overlays(&self, background: gpui_kit::gpui::Rgba) -> [AnyElement; 2] {
        // Passive fades show actual overflow. The thumb shows on edge or
        // bottom hover, and the stock scrollbar holds it while in use.
        [
            edges::fades(&self.handle, background).into_any_element(),
            div()
                .absolute()
                .inset_0()
                .child(
                    Scrollbar::horizontal(&self.handle)
                        .mode(if self.edge_hover() {
                            ScrollbarMode::Always
                        } else {
                            ScrollbarMode::Hover
                        })
                        .viewport_from_layout(),
                )
                .into_any_element(),
        ]
    }

    /// The marker where autoscroll began, while it runs.
    pub fn autoscroll_marker(&self) -> Option<AnyElement> {
        let Some(MiddleScroll::Auto { origin }) = self.middle else {
            return None;
        };
        let p = palette();
        Some(
            deferred(
                anchored().position(origin - point(px(14.), px(14.))).child(
                    div()
                        .size(px(28.))
                        .rounded_full()
                        .border_1()
                        .border_color(p.border)
                        .bg(p.surface2)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(p.muted)
                        .child("↔"),
                ),
            )
            .into_any_element(),
        )
    }
}

/// Wires a board's root to its [`SideScroll`]: the pointer, for the
/// scrollbar and pans, and the middle button.
pub fn listen<V: 'static>(
    root: Stateful<Div>,
    side: SideOf<V>,
    cx: &mut Context<V>,
) -> Stateful<Div> {
    root.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
        if !*hovered {
            side(this).pointer_left(cx);
        }
    }))
    .on_mouse_down(
        MouseButton::Middle,
        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
            side(this).middle_down(event.position, cx);
            cx.stop_propagation();
        }),
    )
    .on_mouse_up(
        MouseButton::Middle,
        cx.listener(move |this, _: &MouseUpEvent, _, cx| side(this).middle_up(side, cx)),
    )
    .on_mouse_up_out(
        MouseButton::Middle,
        cx.listener(move |this, _: &MouseUpEvent, _, cx| {
            side(this).stop_middle(cx);
        }),
    )
    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
        side(this).pointer_moved(event.position, cx);
    }))
    // A click ends autoscroll without acting on what was under it, as in
    // browsers.
    .capture_any_mouse_down(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
        let side = side(this);
        if event.button != MouseButton::Middle
            && matches!(side.middle, Some(MiddleScroll::Auto { .. }))
            && side.stop_middle(cx)
        {
            cx.stop_propagation();
        }
    }))
}

/// Ticks while in autoscroll, scrolling by how far the pointer is from
/// where the middle button was clicked.
fn run_autoscroll<V: 'static>(side: SideOf<V>, cx: &mut Context<V>) {
    cx.spawn(async move |this, cx| {
        let mut last = Instant::now();
        loop {
            cx.background_executor().timer(AUTOSCROLL_TICK).await;
            let now = Instant::now();
            let elapsed = (now - last).as_secs_f32();
            last = now;
            let running = this
                .update(cx, |this, cx| {
                    let side = side(this);
                    let Some(MiddleScroll::Auto { origin }) = side.middle else {
                        return false;
                    };
                    let distance = (side.pointer.x - origin.x) / px(1.);
                    let past = distance.abs() - AUTOSCROLL_DEAD_ZONE;
                    if past > 0. {
                        // Pointer to the right of the origin reveals later columns.
                        let dx = distance.signum() * past * AUTOSCROLL_GAIN * elapsed;
                        side.nudge(dx, cx);
                    }
                    true
                })
                .unwrap_or(false);
            if !running {
                break;
            }
        }
    })
    .detach();
}

/// A layout's column width: the user's choice, kept per layout, and the
/// control that sets it.
pub struct ColumnWidth {
    layout: &'static str,
    core: Core,
    slider: Entity<SliderState>,
    save_generation: u64,
}

/// How a view reaches its [`ColumnWidth`].
pub type WidthOf<V> = fn(&mut V) -> &mut ColumnWidth;

impl ColumnWidth {
    pub fn new<V: 'static>(
        layout: &'static str,
        core: &Core,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Self {
        let width = core.nav.read(cx).column_width(layout);
        let slider = cx.new(|_| {
            // Max first: `min` clamps against the current max, which starts at 100.
            SliderState::new()
                .max(COLUMN_WIDTH_MAX)
                .min(COLUMN_WIDTH_MIN)
                .step(10.)
                .default_value(width)
        });
        let nav = core.nav.clone();
        cx.subscribe(&slider, move |_, _, event: &SliderEvent, cx| {
            let (value, persist) = match event {
                SliderEvent::Change(value) => (value.start(), false),
                SliderEvent::Release(value) => (value.start(), true),
            };
            nav.update(cx, |nav, cx| {
                nav.set_column_width(layout, value, persist, cx)
            });
        })
        .detach();
        // Keep the slider in step when the width changes another way.
        cx.observe_in(&core.nav, window, {
            let slider = slider.clone();
            move |_, nav, window, cx| {
                let width = nav.read(cx).column_width(layout);
                slider.update(cx, |slider, cx| {
                    if slider.value().start() != width {
                        slider.set_value(width, window, cx);
                    }
                });
            }
        })
        .detach();
        Self {
            layout,
            core: core.clone(),
            slider,
            save_generation: 0,
        }
    }

    /// The width the user picked, before fitting.
    pub fn chosen(&self, cx: &gpui_kit::gpui::App) -> Pixels {
        px(self.core.nav.read(cx).column_width(self.layout))
    }

    pub fn fit(&self, cx: &gpui_kit::gpui::App) -> bool {
        self.core.nav.read(cx).column_fit(self.layout)
    }

    /// Cmd/Ctrl+wheel makes every column wider or narrower, like zooming.
    /// True when the event was a resize.
    pub fn wheel<V: 'static>(
        &mut self,
        event: &ScrollWheelEvent,
        width: WidthOf<V>,
        cx: &mut Context<V>,
    ) -> bool {
        if !(event.modifiers.control || event.modifiers.platform) {
            return false;
        }
        let dy = event.delta.pixel_delta(px(16.)).y / px(1.);
        if dy == 0. {
            return false;
        }
        let lines = (dy / 16.).clamp(-3., 3.);
        // Save wheel notches now; coalesce trackpad updates into a trailing save.
        let persist = matches!(event.delta, ScrollDelta::Lines(_));
        let layout = self.layout;
        let nav = self.core.nav.clone();
        let next = nav.read(cx).column_width(layout) + lines * WHEEL_WIDTH_STEP;
        nav.update(cx, |nav, cx| {
            nav.set_column_width(layout, next, persist, cx)
        });
        self.save_generation += 1;
        if !persist {
            let generation = self.save_generation;
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    let state = width(this);
                    if state.save_generation == generation {
                        state.core.nav.read(cx).save();
                    }
                });
            })
            .detach();
        }
        true
    }

    /// The width pill in the board's bottom right: what the columns are,
    /// and a popover to change it. `shown` is an ordinary column's width
    /// after fitting; `leading` goes before it in the pill.
    pub fn control(
        &self,
        shown: Pixels,
        leading: Option<AnyElement>,
        cx: &gpui_kit::gpui::App,
    ) -> AnyElement {
        let p = palette();
        let layout = self.layout;
        let nav = self.core.nav.clone();
        let label = if nav.read(cx).column_fit(layout) {
            format!("Fit · {:.0}", shown / px(1.))
        } else {
            format!("Width · {:.0}", nav.read(cx).column_width(layout))
        };
        let slider = self.slider.clone();
        div()
            // This pill overlays cards; its clicks must not reach them.
            .occlude()
            .absolute()
            .right(px(16.))
            .bottom(px(22.))
            .bg(p.surface2)
            .rounded(px(6.))
            .border_1()
            .border_color(p.border)
            .flex()
            .items_center()
            .children(leading)
            .child(
                Popover::new(format!("{layout}-width-popover"))
                    .anchor(Anchor::BottomRight)
                    .trigger(
                        Button::new(format!("{layout}-width-settings"))
                            .small()
                            .ghost()
                            .label(label)
                            .icon(Icon::empty().path("icons/settings-2.svg"))
                            .accessibility_label("Column width and Fit")
                            .tooltip("Column width and Fit · Cmd/Ctrl+wheel also resizes"),
                    )
                    .content(move |_, _, cx| {
                        let fit = nav.read(cx).column_fit(layout);
                        let nav = nav.clone();
                        div()
                            .w(px(240.))
                            .flex()
                            .flex_col()
                            .gap(px(12.))
                            .child(
                                div()
                                    .text_size(size::sm())
                                    .text_color(p.muted)
                                    .child("Column width"),
                            )
                            .child(Slider::new(&slider))
                            .child(
                                Button::new(format!("{layout}-fit"))
                                    .small()
                                    .ghost()
                                    .label("Fit columns")
                                    .selected(fit)
                                    .tooltip("Narrow columns so they all fit in view")
                                    .on_click(move |_, _, cx| {
                                        nav.update(cx, |nav, cx| {
                                            nav.set_column_fit(layout, !fit, cx)
                                        })
                                    }),
                            )
                    }),
            )
            .into_any_element()
    }
}

/// An ordinary column's width: `chosen`, or with `fit`, narrower so that
/// `ordinary` such columns and `fixed` worth of other columns, `columns` in
/// all, fit in `visible` (never below [`FIT_MIN_WIDTH`]; past that, scroll).
pub fn fit_width(
    chosen: Pixels,
    fit: bool,
    ordinary: usize,
    columns: usize,
    fixed: Pixels,
    visible: Pixels,
) -> Pixels {
    if !fit || ordinary == 0 {
        return chosen;
    }
    let gaps = px(COLUMN_GAP) * columns.saturating_sub(1) as f32;
    let room = visible - px(COLUMNS_PADDING * 2.) - gaps - fixed;
    let fit = room / ordinary as f32;
    if fit > chosen {
        chosen
    } else if fit < px(FIT_MIN_WIDTH) {
        px(FIT_MIN_WIDTH)
    } else {
        fit
    }
}

/// The width the board shows its columns in: its scroll area's, once laid
/// out, else the window's.
pub fn visible_width(side: &SideScroll, window: &Window) -> Pixels {
    match side.handle.bounds().size.width {
        width if width > px(0.) => width,
        _ => window.viewport_size().width,
    }
}

/// Lets `root` (a board's columns) take the wheel for width and sideways
/// scrolling: Cmd/Ctrl+wheel resizes, and the wheel turns sideways over the
/// top `header` of the board or with Shift.
pub fn wheel<V: 'static>(
    columns: Stateful<Div>,
    header: Pixels,
    side: SideOf<V>,
    width: WidthOf<V>,
    cx: &mut Context<V>,
) -> Stateful<Div> {
    columns.on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
        if width(this).wheel(event, width, cx) || side(this).wheel(event, header, cx) {
            cx.stop_propagation();
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_narrows_columns_to_the_view_but_never_past_the_floor() {
        let chosen = px(300.);
        // Room enough: the chosen width.
        assert_eq!(fit_width(chosen, true, 2, 2, px(0.), px(2000.)), chosen);
        // Not fitting: the chosen width however little room.
        assert_eq!(fit_width(chosen, false, 5, 5, px(0.), px(500.)), chosen);
        // 5 columns in 1000: 1000 - 32 padding - 48 gaps = 920 / 5.
        assert_eq!(fit_width(chosen, true, 5, 5, px(0.), px(1000.)), px(184.));
        // Wider columns take their share first.
        assert_eq!(
            fit_width(chosen, true, 4, 5, px(400.), px(1000.)),
            px(FIT_MIN_WIDTH)
        );
        assert_eq!(fit_width(chosen, true, 0, 1, px(400.), px(100.)), chosen);
    }
}
