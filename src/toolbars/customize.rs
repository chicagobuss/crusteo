//! Customize mode: every bar shows all its items, real but inert, each
//! draggable to any place in any bar, or onto the palette to take it out;
//! the palette holds the items taken out, spacers, which bars show, and
//! Reset and Done.

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement,
    Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder as _;

use super::{Arrangement, BARS, Dragged, LAYOUT, Registry, SPACER, STRIP, Side, SlotRef, TITLE};
use crate::app::Nav;
use crate::theme::{palette, size, with_alpha};
use crate::ui;

/// What follows the pointer while an item is dragged. GPUI draws it where
/// the item was grabbed relative to its corner; `offset` shifts it back so
/// it sits at the pointer however wide the item was.
pub struct DragChip {
    title: SharedString,
    offset: Point<Pixels>,
}

impl DragChip {
    /// A chip titled `title`, for anything dragged by name: toolbar items,
    /// Lanes' lanes.
    pub fn new(title: SharedString, offset: Point<Pixels>) -> Self {
        Self { title, offset }
    }
}

impl Render for DragChip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        div().pl(self.offset.x).pt(self.offset.y).child(
            div()
                .px(px(8.))
                .py(px(3.))
                .rounded(px(6.))
                .bg(p.surface3)
                .border_1()
                .border_color(p.accent)
                .text_size(size::sm())
                .text_color(p.foreground)
                .shadow_md()
                .child(self.title.clone()),
        )
    }
}

fn chip(dragged: &Dragged, offset: Point<Pixels>, cx: &mut App) -> Entity<DragChip> {
    let title = dragged.title.clone();
    cx.new(|_| DragChip { title, offset })
}

/// Applies an edit to the saved arrangement.
fn edit(nav: &Entity<Nav>, cx: &mut App, change: impl FnOnce(&mut Arrangement)) {
    nav.update(cx, |nav, cx| {
        let mut arrangement = nav.toolbars.clone();
        change(&mut arrangement);
        nav.set_toolbars(arrangement, cx);
    });
}

/// What an id is called: its item's title, or the id itself for an item
/// not registered now, such as a script that is not loaded.
fn title_of(id: &str, registry: &Registry) -> SharedString {
    if id == SPACER {
        return "Spacer".into();
    }
    registry
        .get(id)
        .map(|item| item.title())
        .unwrap_or_else(|| id.to_string().into())
}

/// A bar's contents while customizing: both groups with every item, and
/// places to drop at each group's end.
pub fn bar_editor(
    bar: &str,
    arrangement: &Arrangement,
    registry: &Registry,
    nav: &Entity<Nav>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let p = palette();
    let placed = arrangement.bar(bar).cloned().unwrap_or_default();
    let left = group(bar, Side::Left, &placed.left, registry, nav, window, cx);
    let right = group(bar, Side::Right, &placed.right, registry, nav, window, cx);
    // Which bar this is, as the palette's checkboxes call it.
    let name = match bar {
        TITLE => "Title bar".to_string(),
        STRIP => "Under the title bar".to_string(),
        LAYOUT => "Over the layout".to_string(),
        _ => arrangement
            .custom_bars()
            .position(|own| own.id == bar)
            .map_or_else(
                || "Your bar".to_string(),
                |at| format!("Your bar {}", at + 1),
            ),
    };
    // Outlined, so the bar itself reads as the place to drop; a crowded
    // bar scrolls sideways rather than squeezing its items.
    div()
        .id(SharedString::from(format!("customize-bar-{bar}")))
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .my(px(2.))
        .px(px(4.))
        .rounded(px(6.))
        .border_1()
        .border_color(with_alpha(p.accent, 0.45))
        .overflow_x_scroll()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(
            div()
                .flex_shrink_0()
                .px(px(6.))
                .py(px(1.))
                .rounded(px(4.))
                .bg(with_alpha(p.accent, 0.15))
                .text_size(px(11.))
                .text_color(p.muted)
                .child(name),
        )
        .child(left)
        .child(right)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn group(
    bar: &str,
    side: Side,
    ids: &[String],
    registry: &Registry,
    nav: &Entity<Nav>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let p = palette();
    let accent = p.accent;
    let mut group = div()
        .flex()
        .items_center()
        .gap(px(4.))
        .flex_shrink_0()
        .when(side == Side::Left, |group| group.flex_grow(1.));
    for (index, id) in ids.iter().enumerate() {
        let slot = SlotRef {
            bar: bar.to_string(),
            side,
            index,
        };
        let dragged = Dragged {
            id: id.clone(),
            title: title_of(id, registry),
            from: Some(slot),
        };
        let item = registry.get(id);
        let placeholder = |title: SharedString| {
            div()
                .px(px(6.))
                .text_size(size::sm())
                .text_color(p.extra_muted)
                .child(title)
                .into_any_element()
        };
        let content = if id == SPACER {
            div()
                .w(px(super::SPACER_WIDTH))
                .h(px(20.))
                .into_any_element()
        } else {
            match &item {
                // An item that fills its bar has no width of its own here;
                // its name stands in for it.
                Some(item) if item.grows() => div()
                    .min_w(px(160.))
                    .child(placeholder(dragged.title.clone()))
                    .into_any_element(),
                // One with nothing to show now, or not loaded, likewise.
                Some(item) => item
                    .render(window, cx)
                    .unwrap_or_else(|| placeholder(dragged.title.clone())),
                None => placeholder(dragged.title.clone()),
            }
        };
        // Its two halves: dropping on the left one puts what is dragged
        // before it, on the right one after it. Either picks it up. They
        // block what is under them, so in the title bar a drag moves the
        // item, not the window.
        let half = |after: bool| {
            let (nav, bar, dragged) = (nav.clone(), bar.to_string(), dragged.clone());
            let at = index + usize::from(after);
            div()
                .id(SharedString::from(format!(
                    "customize-half-{bar}-{side:?}-{index}-{after}"
                )))
                .flex_1()
                .h_full()
                .occlude()
                .cursor(ui::grab_cursor())
                .drag_over::<Dragged>(move |style, _, _, _| {
                    if after {
                        style.border_r_2().border_color(accent)
                    } else {
                        style.border_l_2().border_color(accent)
                    }
                })
                .on_drop(move |dropped: &Dragged, _, cx| {
                    edit(&nav, cx, |arrangement| {
                        arrangement.place(dropped, &bar, side, Some(at))
                    });
                })
                .on_drag(dragged, |dragged, offset, _, cx| chip(dragged, offset, cx))
        };
        group = group.child(
            div()
                .id(SharedString::from(format!(
                    "customize-{bar}-{side:?}-{index}-{id}"
                )))
                .relative()
                .flex_shrink_0()
                .h(px(28.))
                .px(px(3.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .border_1()
                .border_dashed()
                .border_color(with_alpha(p.extra_muted, 0.7))
                .when(id == SPACER, |spacer| {
                    spacer.bg(with_alpha(p.extra_muted, 0.12))
                })
                .child(content)
                // Over the item, so it can be picked up but not used.
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .child(half(false))
                        .child(half(true)),
                ),
        );
    } // The group's end: dropping here appends.
    let (end_nav, end_bar) = (nav.clone(), bar.to_string());
    group = group.child(
        div()
            .id(SharedString::from(format!("customize-end-{bar}-{side:?}")))
            .occlude()
            .h(px(28.))
            .min_w(px(28.))
            .when(side == Side::Left, |end| end.flex_1())
            .rounded(px(5.))
            .border_1()
            .border_dashed()
            .border_color(with_alpha(p.extra_muted, 0.35))
            .drag_over::<Dragged>(move |style, _, _, _| {
                style.border_color(accent).bg(with_alpha(accent, 0.12))
            })
            .on_drop(move |dragged: &Dragged, _, cx| {
                edit(&end_nav, cx, |arrangement| {
                    arrangement.place(dragged, &end_bar, side, None)
                });
            }),
    );
    group.into_any_element()
}

/// The palette: items taken out, a spacer to add, which bars show, and
/// Reset and Done. Dropping an item on it takes the item out.
pub fn palette_panel(
    arrangement: &Arrangement,
    registry: &Registry,
    nav: &Entity<Nav>,
    on_done: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    let p = palette();
    let accent = p.accent;
    let mut pieces: Vec<Dragged> = vec![Dragged {
        id: SPACER.into(),
        title: "Spacer".into(),
        from: None,
    }];
    pieces.extend(arrangement.removed.iter().map(|id| Dragged {
        id: id.clone(),
        title: title_of(id, registry),
        from: None,
    }));
    // Items registered but neither placed nor taken out show up here too,
    // though arranging places them first.
    let chips = pieces.into_iter().enumerate().map(|(index, dragged)| {
        let title = dragged.title.clone();
        div()
            .id(SharedString::from(format!("customize-palette-{index}")))
            .h(px(26.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(6.))
            .border_1()
            .border_dashed()
            .border_color(with_alpha(p.extra_muted, 0.7))
            .bg(p.surface2)
            .text_size(size::sm())
            .text_color(p.foreground)
            .cursor(ui::grab_cursor())
            .child(title)
            .on_drag(dragged, |dragged, offset, _, cx| chip(dragged, offset, cx))
    });
    let toggles = BARS.iter().filter(|bar| **bar != super::TITLE).map(|bar| {
        let hidden = arrangement.bar(bar).is_some_and(|placed| placed.hidden);
        let label = match *bar {
            STRIP => "Bar under the title bar",
            LAYOUT => "Bar over the layout",
            _ => "Bar",
        };
        let (nav, bar) = (nav.clone(), bar.to_string());
        div()
            .id(SharedString::from(format!("customize-show-{bar}")))
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(size::sm())
            .text_color(p.muted)
            .cursor_pointer()
            .hover(|style| style.text_color(p.foreground))
            .child(ui::icon(
                if hidden { "square" } else { "square-check" },
                14.,
                p.muted,
            ))
            .child(label)
            .on_click(move |_: &ClickEvent, _, cx| {
                edit(&nav, cx, |arrangement| {
                    arrangement.set_hidden(&bar, !hidden)
                });
            })
    });
    // The user's own bars, each removable; removing one takes its items out.
    let own_bars = arrangement
        .custom_bars()
        .enumerate()
        .map(|(index, bar)| {
            let (nav, id) = (nav.clone(), bar.id.clone());
            div()
                .id(SharedString::from(format!("customize-remove-{id}")))
                .flex()
                .items_center()
                .gap(px(4.))
                .text_size(size::sm())
                .text_color(p.muted)
                .cursor_pointer()
                .hover(|style| style.text_color(p.foreground))
                .child(format!("Your bar {}", index + 1))
                .child(ui::icon("x", 12., p.muted))
                .on_click(move |_: &ClickEvent, _, cx| {
                    edit(&nav, cx, |arrangement| arrangement.remove_bar(&id));
                })
        })
        .collect::<Vec<_>>();
    let add_nav = nav.clone();
    let add_bar = Button::new("customize-add-bar")
        .label("+ New bar")
        .ghost()
        .xsmall()
        .tooltip("Add an empty bar under the others")
        .on_click(move |_, _, cx| {
            edit(&add_nav, cx, |arrangement| {
                arrangement.add_bar();
            });
        });
    let (remove_nav, reset_nav) = (nav.clone(), nav.clone());
    div()
        .id("customize-palette")
        .w(px(720.))
        .max_w_full()
        .child(
            div()
                .id("customize-palette-panel")
                .w_full()
                .p(px(14.))
                .rounded(px(10.))
                .border_1()
                .border_color(with_alpha(accent, 0.6))
                .bg(p.surface1)
                .shadow_lg()
                .flex()
                .flex_col()
                .gap(px(10.))
                // Clicks on the panel are its own, not the dimmed board's.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .drag_over::<Dragged>(move |style, _, _, _| style.border_color(accent))
                .on_drop(move |dragged: &Dragged, _, cx| {
                    edit(&remove_nav, cx, |arrangement| arrangement.remove(dragged));
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().text_color(p.foreground).child("Customize toolbars"))
                        .child(
                            div()
                                .flex()
                                .gap(px(6.))
                                .child(
                                    Button::new("customize-reset")
                                        .label("Reset")
                                        .ghost()
                                        .small()
                                        .on_click(move |_, _, cx| {
                                            // An empty arrangement: every item
                                            // goes home again.
                                            reset_nav.update(cx, |nav, cx| {
                                                nav.set_toolbars(Arrangement::default(), cx)
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("customize-done")
                                        .label("Done")
                                        .primary()
                                        .small()
                                        .on_click(move |_, window, cx| on_done(window, cx)),
                                ),
                        ),
                )
                .child(div().text_size(size::sm()).text_color(p.muted).child(
                    "Drag items to any place in any bar. Drag one here to take it \
                             out; drag a spacer in to set items apart.",
                ))
                .child(div().flex().flex_wrap().gap(px(6.)).children(chips))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(16.))
                        .children(toggles)
                        .children(own_bars)
                        .child(add_bar),
                ),
        )
        .into_any_element()
}
