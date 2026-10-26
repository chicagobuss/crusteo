//! Label color choices; the caller owns persistence.
use crate::theme::{LANE_COLORS, palette, size};
use gpui_kit::base::Selectable as _;
use gpui_kit::component::{
    Sizable,
    button::{Button, ButtonVariants},
    popover::Popover,
};
use gpui_kit::gpui::{
    Anchor, App, InteractiveElement, ParentElement, Pixels, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px, rgb,
};

pub const MAX_WIDTH: f32 = 248.;

pub fn picker(
    label: &str,
    current: usize,
    width: Pixels,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Popover {
    picker_titled(label, "Label color", current, width, on_pick)
}

/// [`picker`] headed `heading`, for what the colour is of.
pub fn picker_titled(
    label: &str,
    heading: &'static str,
    current: usize,
    width: Pixels,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Popover {
    let on_pick = std::rc::Rc::new(on_pick);
    Popover::new(SharedString::from(format!("lane-color-{label}")))
        .anchor(Anchor::TopRight)
        .p(px(0.))
        .trigger(
            Button::new(SharedString::from(format!("lane-color-trigger-{label}")))
                .ghost()
                .xsmall()
                .tooltip(format!("Choose {}", heading.to_lowercase()))
                .accessibility_label(format!("Choose {}", heading.to_lowercase()))
                .child(
                    div()
                        .size(px(12.))
                        .rounded_full()
                        .bg(rgb(LANE_COLORS[current].1)),
                ),
        )
        .content(move |_, _, cx| {
            let popup = cx.entity().downgrade();
            div()
                .id("lane-color-options")
                .w((width - px(16.)).clamp(px(120.), px(MAX_WIDTH)))
                .max_h(px(360.))
                .overflow_y_scroll()
                .restrict_scroll_to_axis()
                .p(px(8.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(size::sm())
                        .text_color(palette().muted)
                        .child(heading),
                )
                .child(div().flex().flex_wrap().gap(px(4.)).children(
                    LANE_COLORS.iter().enumerate().map(|(index, (name, hex))| {
                        let on_pick = on_pick.clone();
                        let popup = popup.clone();
                        Button::new(SharedString::from(format!("lane-swatch-{index}")))
                            .ghost()
                            .xsmall()
                            .w(px(26.))
                            .h(px(26.))
                            .selected(index == current)
                            .tooltip(*name)
                            .accessibility_label(*name)
                            .child(div().size(px(16.)).rounded(px(3.)).bg(rgb(*hex)))
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                on_pick(index, window, cx);
                                if let Some(popup) = popup.upgrade() {
                                    popup.update(cx, |popup, cx| popup.dismiss(window, cx));
                                }
                            })
                    }),
                ))
        })
}
