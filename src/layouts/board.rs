//! Board: every workspace in the fleet as a card, in one column per state,
//! so "what needs me" reads left to right across all hosts.

use fleet::Bucket;
use fleet::sidebar::{Filters, Grouping, SidebarView};
use gpui_kit::component::menu::ContextMenuExt;
use gpui_kit::gpui::{
    AnyView, App, AppContext, ClickEvent, Context, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;

use crate::app::Core;
use crate::theme::{palette, size};
use crate::ui;

pub fn build(core: &Core, _window: &mut Window, cx: &mut App) -> AnyView {
    let core = core.clone();
    cx.new(|cx| Board::new(core, cx)).into()
}

pub struct Board {
    core: Core,
}

impl Board {
    fn new(core: Core, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        Self { core }
    }
}

impl Render for Board {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let view = SidebarView::build(
            &self.core.store.read(cx).fleet,
            Grouping::Status,
            &Filters::default(),
        );
        let pinned = view.pinned.clone();
        let mut columns = div()
            .id("board-columns")
            .size_full()
            .flex()
            .gap(px(12.))
            .p(px(16.))
            .overflow_x_scroll()
            .restrict_scroll_to_axis();
        for bucket in Bucket::GROUP_ORDER {
            let mut rows: Vec<_> = view
                .groups
                .iter()
                .find(|(b, _)| *b == bucket)
                .map(|(_, rows)| rows.clone())
                .unwrap_or_default();
            rows.extend(pinned.iter().filter(|r| r.bucket == bucket).cloned());
            let color = ui::bucket_color(bucket).unwrap_or(p.extra_muted);
            let mut list = div()
                .id(SharedString::from(format!("board-{}", bucket.label())))
                .flex_1()
                .overflow_y_scroll()
                .p(px(8.))
                .flex()
                .flex_col()
                .gap(px(6.));
            for row in &rows {
                let core = self.core.clone();
                let host = row.host.clone();
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
                                .truncate()
                                .text_color(p.foreground)
                                .child(row.title.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(6.))
                                .text_size(size::sm())
                                .text_color(p.muted)
                                .child(div().truncate().child(row.project_name.clone()))
                                .child("·")
                                .child(row.host.clone())
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
            columns = columns.child(
                div()
                    .w(px(280.))
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
                            .p(px(12.))
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
                    .child(list),
            );
        }
        div().size_full().bg(p.surface0).child(columns)
    }
}
