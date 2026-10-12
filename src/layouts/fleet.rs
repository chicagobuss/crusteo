//! Fleet: every agent on every host at once, one column per host, most
//! urgent first. Clicking an agent opens it in the Paseo layout.
//!
//! Each column is its own cached view and a virtual list: a fleet change
//! redraws only the columns of the hosts it touched, and a column builds
//! only the cards in view, however many agents a host has.

use fleet::{Agent, Bucket, Connection, DirChange};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::gpui::{
    AnyElement, AnyView, App, AppContext, ClickEvent, Context, DismissEvent, Entity, Focusable,
    InteractiveElement, IntoElement, ListAlignment, ListState, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement,
    StyleRefinement, Styled, Subscription, WeakEntity, Window, anchored, deferred, div, list, px,
};
use gpui_kit::prelude::FluentBuilder;

use crate::app::store::time_ago;
use crate::app::{Core, Tab};
use crate::theme::{palette, size};
use crate::ui;

const COLUMN_WIDTH: f32 = 300.;

pub fn build(core: &Core, _window: &mut Window, cx: &mut App) -> AnyView {
    let core = core.clone();
    cx.new(|cx| FleetView::new(core, cx)).into()
}

pub struct FleetView {
    core: Core,
    /// One column per host, in fleet order.
    columns: Vec<(String, Entity<HostColumn>)>,
    /// Where the fleet's change log was read up to.
    cursor: Option<u64>,
    menu: Option<CardMenu>,
}

/// An open card menu. Cards only carry a right-click listener; the view owns
/// the one menu, as Lanes does.
struct CardMenu {
    position: Point<Pixels>,
    menu: Entity<PopupMenu>,
    _dismiss: Subscription,
}

/// One host's column: its header and a virtual list of its cards.
struct HostColumn {
    core: Core,
    board: WeakEntity<FleetView>,
    host: String,
    header: Header,
    cards: Vec<Card>,
    list: ListState,
}

#[derive(Clone, PartialEq)]
struct Header {
    connection: Connection,
    version: String,
}

/// What one card shows, to tell which cards changed.
#[derive(Clone, PartialEq)]
struct Card {
    id: String,
    title: String,
    workspace: String,
    provider: String,
    updated_at: Option<String>,
    bucket: Bucket,
    /// The card's controls answer the first pending permission.
    permission: Option<String>,
}

impl FleetView {
    fn new(core: Core, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |this, _, cx| this.refresh(cx))
            .detach();
        let mut this = Self {
            core,
            columns: Vec::new(),
            cursor: None,
            menu: None,
        };
        this.refresh(cx);
        this
    }

    /// Brings the columns up to date with the fleet. Only the hosts named in
    /// the change log are re-read, and a column redraws only if what it shows
    /// changed.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let store = self.core.store.read(cx);
        let names: Vec<String> = store
            .fleet
            .hosts
            .iter()
            .map(|host| host.config.name.clone())
            .collect();
        // None: every host. A change with no host (connections, snapshots)
        // could touch any of them.
        let touched: Option<Vec<String>> = self.cursor.and_then(|cursor| {
            let mut hosts = Vec::new();
            for change in store.changes_since(cursor)? {
                if change.host.is_empty() {
                    return None;
                }
                if !matches!(change.change, DirChange::WorkspaceDetail(_))
                    && !hosts.contains(&change.host)
                {
                    hosts.push(change.host.clone());
                }
            }
            Some(hosts)
        });
        self.cursor = Some(store.change_cursor());

        let same_hosts = self.columns.len() == names.len()
            && self.columns.iter().zip(&names).all(|((a, _), b)| a == b);
        if !same_hosts {
            let board = cx.entity().downgrade();
            let old = std::mem::take(&mut self.columns);
            self.columns = names
                .into_iter()
                .map(|name| {
                    let column = old
                        .iter()
                        .find(|(host, _)| *host == name)
                        .map(|(_, column)| column.clone())
                        .unwrap_or_else(|| {
                            let (core, board, host) =
                                (self.core.clone(), board.clone(), name.clone());
                            cx.new(|_| HostColumn {
                                core,
                                board,
                                host,
                                header: Header {
                                    connection: Connection::default(),
                                    version: String::new(),
                                },
                                cards: Vec::new(),
                                list: ListState::new(0, ListAlignment::Top, px(120.)),
                            })
                        });
                    (name, column)
                })
                .collect();
            cx.notify();
        }
        for (host, column) in &self.columns {
            let stale = !same_hosts
                || touched
                    .as_ref()
                    .is_none_or(|touched| touched.contains(host));
            if stale {
                column.update(cx, |column, cx| column.refresh(cx));
            }
        }
    }

    /// Opens the card menu for `host`/`agent` at `position`, replacing any
    /// menu already open.
    fn open_menu(
        &mut self,
        host: &str,
        agent: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let (host, agent) = (host.to_string(), agent.to_string());
        let menu = PopupMenu::build(window, cx, move |menu, _, cx| {
            crate::workspace_archive::agent_menu(menu, &core, &host, &agent, cx)
        });
        let _dismiss = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
            this.menu = None;
            cx.notify();
        });
        self.menu = Some(CardMenu {
            position,
            menu,
            _dismiss,
        });
        cx.notify();
    }

    /// The open card menu, floated above everything at its click position.
    fn menu_layer(&self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let open = self.menu.as_ref()?;
        let focus = open.menu.focus_handle(cx);
        if !focus.contains_focused(window, cx) {
            focus.focus(window, cx);
        }
        Some(
            deferred(
                anchored()
                    .position(open.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(open.menu.clone()),
            )
            .with_priority(gpui_kit::base::POPUP_PRIORITY)
            .into_any_element(),
        )
    }
}

impl Render for FleetView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let columns = div()
            .id("fleet-columns")
            .size_full()
            .flex()
            .gap(px(12.))
            .p(px(16.))
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .children(self.columns.iter().map(|(_, column)| {
                // A cached view is laid out from this style, not its content.
                AnyView::from(column.clone())
                    .cached(
                        StyleRefinement::default()
                            .w(px(COLUMN_WIDTH))
                            .h_full()
                            .flex_shrink_0(),
                    )
                    .relocatable(true)
            }));
        div()
            .size_full()
            .bg(p.surface0)
            .child(columns)
            .children(self.menu_layer(window, cx))
    }
}

impl HostColumn {
    /// Re-reads this host's header and cards; redraws if either changed.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let store = self.core.store.read(cx);
        let Some(host) = store.fleet.host(&self.host) else {
            return;
        };
        let header = Header {
            connection: host.connection.clone(),
            version: host
                .server
                .as_ref()
                .map(|s| format!("v{}", s.version))
                .unwrap_or_default(),
        };
        let cards = cards(host);
        if header == self.header && cards == self.cards {
            return;
        }
        self.header = header;
        ui::sync_list(&self.list, &mut self.cards, cards);
        cx.notify();
    }

    fn card(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let Some(card) = self.cards.get(ix) else {
            return div().into_any_element();
        };
        let last = ix + 1 == self.cards.len();
        let controls = self
            .core
            .store
            .read(cx)
            .fleet
            .agent(&self.host, &card.id)
            .and_then(|agent| {
                crate::widgets::agent_controls::controls(&self.core, &self.host, agent, "fleet")
            });
        let open = {
            let id = card.id.clone();
            cx.listener(move |this, _: &ClickEvent, _, cx| {
                open_agent(&this.core, &this.host, &id, cx)
            })
        };
        let menu = {
            let id = card.id.clone();
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                let host = this.host.clone();
                let _ = this.board.update(cx, |board, cx| {
                    board.open_menu(&host, &id, event.position, window, cx)
                });
                cx.stop_propagation();
            })
        };
        // A list lays each card out on its own and pads only its ends, so
        // the gap and the side padding are the card's.
        div()
            .w_full()
            .px(px(8.))
            .when(!last, |d| d.pb(px(6.)))
            .child(
                div()
                    .id(SharedString::from(format!("fleet-agent-{}", card.id)))
                    .p(px(10.))
                    .rounded(px(8.))
                    .bg(p.surface0)
                    .border_1()
                    .border_color(match card.bucket {
                        Bucket::NeedsInput => p.dot_warning,
                        Bucket::Failed => p.dot_danger,
                        _ => p.border,
                    })
                    .hover(|s| s.bg(p.surface2))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(ui::agent_dot(card.bucket))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(p.foreground)
                                    .child(card.title.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .text_size(size::sm())
                            .text_color(p.muted)
                            .child(div().truncate().child(card.workspace.clone()))
                            .child("·")
                            .child(card.provider.clone())
                            .when_some(card.updated_at.as_ref(), |d, at| {
                                d.child("·").child(time_ago(at))
                            }),
                    )
                    .children(controls)
                    .on_click(open)
                    .on_mouse_down(MouseButton::Right, menu),
            )
            .into_any_element()
    }
}

impl Render for HostColumn {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let mut counts = [0usize; 5];
        for (slot, bucket) in counts.iter_mut().zip(Bucket::GROUP_ORDER) {
            *slot = self
                .cards
                .iter()
                .filter(|card| card.bucket == bucket)
                .count();
        }
        let status = match &self.header.connection {
            Connection::Online => format!("{} agents", self.cards.len()),
            Connection::Offline(reason) | Connection::Refused(reason) => reason.clone(),
            other => other.label().to_string(),
        };
        let cards = list(
            self.list.clone(),
            cx.processor(|this, ix: usize, _window, cx| this.card(ix, cx)),
        )
        .flex_1()
        .py(px(8.));
        div()
            .size_full()
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
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(ui::host_status_dot(&self.header.connection))
                            .child(div().text_color(p.foreground).child(self.host.clone()))
                            .child(
                                div()
                                    .text_size(size::sm())
                                    .text_color(p.extra_muted)
                                    .child(self.header.version.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(size::sm())
                            .text_color(p.muted)
                            .truncate()
                            .child(status),
                    )
                    .child(
                        div().flex().gap(px(10.)).children(
                            Bucket::GROUP_ORDER
                                .iter()
                                .zip(counts.iter())
                                .filter(|(_, n)| **n > 0)
                                .map(|(bucket, n)| {
                                    let color = ui::bucket_color(*bucket).unwrap_or(p.extra_muted);
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(4.))
                                        .text_size(size::sm())
                                        .text_color(color)
                                        .child(div().size(px(6.)).rounded_full().bg(color))
                                        .child(format!("{n} {}", bucket.label().to_lowercase()))
                                }),
                        ),
                    ),
            )
            .child(cards)
    }
}

/// Opens an agent in the Paseo layout, in its workspace.
fn open_agent(core: &Core, host: &str, agent_id: &str, cx: &mut App) {
    let Some(workspace) = core
        .store
        .read(cx)
        .fleet
        .agent(host, agent_id)
        .and_then(|agent| agent.workspace_id.clone())
    else {
        return;
    };
    let (host, agent_id) = (host.to_string(), agent_id.to_string());
    core.nav.update(cx, |nav, cx| {
        nav.layout = "paseo".into();
        nav.select(&host, &workspace, cx);
        nav.focus_tab(&format!("{host}:{workspace}"), Tab::Agent(agent_id), cx);
    });
}

/// A host's cards: most urgent first, then most recently updated, then by
/// id, so the order never depends on how the directory stores them.
fn cards(host: &fleet::Host) -> Vec<Card> {
    let mut agents: Vec<&Agent> = host.directory.agents.values().collect();
    agents.sort_by(|a, b| {
        a.bucket()
            .cmp(&b.bucket())
            .then(b.updated_at.cmp(&a.updated_at))
            .then(a.id.cmp(&b.id))
    });
    agents
        .into_iter()
        .map(|agent| Card {
            id: agent.id.clone(),
            title: agent.display_title(),
            workspace: agent
                .workspace_id
                .as_ref()
                .and_then(|id| host.directory.workspaces.get(id))
                .map(|w| w.name.clone())
                .unwrap_or_default(),
            provider: agent.provider_id().to_string(),
            updated_at: agent.updated_at.clone(),
            bucket: agent.bucket(),
            permission: agent
                .pending_permissions
                .first()
                .map(|request| request["id"].to_string()),
        })
        .collect()
}
