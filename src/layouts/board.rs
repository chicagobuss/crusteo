//! Board: every workspace in the fleet as a card, in one column per state,
//! so "what needs me" reads left to right across all hosts.

use fleet::Bucket;
use fleet::sidebar::{Filters, Grouping, SidebarView};
use gpui_kit::component::menu::ContextMenuExt;
use gpui_kit::gpui::{
    AnyView, App, AppContext, ClickEvent, Context, CursorStyle, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;

use crate::app::Core;
use crate::layouts::columns::{self, COLUMN_GAP, COLUMNS_PADDING, ColumnWidth, SideScroll, edges};
use crate::theme::{palette, size};
use crate::ui;

/// A column header's height: fixed, so a plain wheel along the header row
/// can scroll across columns, as in Lanes.
const HEADER_HEIGHT: f32 = 46.;

pub fn build(core: &Core, window: &mut Window, cx: &mut App) -> AnyView {
    let core = core.clone();
    cx.new(|cx| Board::new(core, window, cx)).into()
}

pub struct Board {
    core: Core,
    side: SideScroll,
    width: ColumnWidth,
    /// Each column's own scroll, in [`Bucket::GROUP_ORDER`], for its fades.
    lists: Vec<ScrollHandle>,
}

impl Board {
    fn new(core: Core, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        // Width changes come through the nav.
        cx.observe(&core.nav, |this, nav, cx| {
            if nav.read(cx).layout != "board" {
                this.side.stop_middle(cx);
            }
            cx.notify();
        })
        .detach();
        cx.observe_window_activation(window, |this, window, cx| {
            this.side.set_window_active(window.is_window_active(), cx);
        })
        .detach();
        let width = ColumnWidth::new("board", &core, window, cx);
        Self {
            core,
            side: SideScroll::new(window),
            width,
            lists: Bucket::GROUP_ORDER
                .iter()
                .map(|_| ScrollHandle::new())
                .collect(),
        }
    }
}

impl Render for Board {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let filters = Filters {
            hide_background: !self.core.nav.read(cx).show_background(),
            ..Filters::default()
        };
        let view = SidebarView::build(&self.core.store.read(cx).fleet, Grouping::Status, &filters);
        let pinned = view.pinned.clone();
        let count = Bucket::GROUP_ORDER.len();
        let width = columns::fit_width(
            self.width.chosen(cx),
            self.width.fit(cx),
            count,
            count,
            px(0.),
            columns::visible_width(&self.side, window),
        );
        let mut strip = div()
            .id("board-columns")
            .size_full()
            .flex()
            .gap(px(COLUMN_GAP))
            .p(px(COLUMNS_PADDING))
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.side.handle);
        for (bucket, scroll) in Bucket::GROUP_ORDER.into_iter().zip(self.lists.clone()) {
            let mut rows: Vec<_> = view
                .groups
                .iter()
                .find(|(b, _)| *b == bucket)
                .map(|(_, rows)| rows.clone())
                .unwrap_or_default();
            rows.extend(pinned.iter().filter(|r| r.bucket == bucket).cloned());
            let color = ui::bucket_color(bucket).unwrap_or(p.extra_muted);
            // Cards take the wheel before the column does, so Cmd/Ctrl+wheel
            // over them resizes the columns instead of scrolling this one.
            let mut list = div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                    if this.width.wheel(event, |this| &mut this.width, cx) {
                        cx.stop_propagation();
                    }
                }));
            for row in &rows {
                let core = self.core.clone();
                let host = row.host.clone();
                let host_name = self.core.store.read(cx).fleet.host_name(&host).to_string();
                let workspace = row.workspace_id.clone();
                list = list.child(
                    div()
                        .id(SharedString::from(format!(
                            "card-{}-{}",
                            row.host, row.workspace_id
                        )))
                        .p(px(10.))
                        .rounded(px(8.))
                        .bg(p.surface0)
                        .border_1()
                        .border_color(p.border)
                        .hover(|s| s.bg(p.surface2))
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .text_color(p.foreground)
                                        .child(row.title.clone()),
                                )
                                .when(row.background, |line| line.child(ui::background_tag())),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(6.))
                                .text_size(size::sm())
                                .text_color(p.muted)
                                .child(div().truncate().child(row.project_name.clone()))
                                .child("·")
                                .child(host_name)
                                .when(row.additions + row.deletions > 0, |d| {
                                    d.child(
                                        div()
                                            .text_color(p.status_success)
                                            .child(format!("+{}", row.additions)),
                                    )
                                    .child(
                                        div()
                                            .text_color(p.status_danger)
                                            .child(format!("-{}", row.deletions)),
                                    )
                                }),
                        )
                        .on_click(move |_: &ClickEvent, _, cx| {
                            let (host, workspace) = (host.clone(), workspace.clone());
                            core.nav.update(cx, |nav, cx| {
                                nav.layout = "paseo".into();
                                nav.select(&host, &workspace, cx);
                            });
                        })
                        .context_menu({
                            let core = self.core.clone();
                            let host = row.host.clone();
                            let workspace = row.workspace_id.clone();
                            move |menu, _, cx| {
                                crate::workspace_archive::workspace_menu(
                                    menu, &core, &host, &workspace, cx,
                                )
                            }
                        }),
                );
            }
            let body = div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .child(
                    div()
                        .id(SharedString::from(format!("board-{}", bucket.label())))
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .track_scroll(&scroll)
                        .p(px(8.))
                        .child(list),
                )
                // Fades where the column has more cards to scroll to.
                .child(edges::scroll_fades(&scroll));
            strip = strip.child(
                div()
                    .w(width)
                    .flex_shrink_0()
                    .h_full()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(p.border)
                    .bg(p.surface1)
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(HEADER_HEIGHT))
                            .flex_shrink_0()
                            .px(px(12.))
                            .border_b_1()
                            .border_color(p.border)
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().size(px(8.)).rounded_full().bg(color))
                            .child(div().text_color(p.foreground).child(bucket.label()))
                            .child(
                                div()
                                    .text_color(p.extra_muted)
                                    .child(rows.len().to_string()),
                            ),
                    )
                    .child(body),
            );
        }
        let strip = columns::wheel(
            strip,
            px(COLUMNS_PADDING + HEADER_HEIGHT),
            |this| &mut this.side,
            |this| &mut this.width,
            cx,
        );
        let root = div().id("board-root");
        columns::listen(root, |this| &mut this.side, cx)
            .size_full()
            .relative()
            .bg(p.surface0)
            .when(self.side.scrolling(), |root| {
                root.cursor(CursorStyle::ResizeLeftRight)
            })
            .child(strip)
            .children(self.side.overlays(p.surface0))
            .child(self.width.control(width, None, cx))
            .children(self.side.autoscroll_marker())
    }
}
