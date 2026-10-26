//! The slide-out lists behind the title bar's counts: a click on "56 ready
//! to review" opens, at the right under the title bar, every agent
//! in that state, its latest change first, each with what it is about. A
//! click on one opens it as a notification's click does.
//!
//! The list only reads. Building it, scrolling it and reading replies for
//! the rows in view leave "ready to review" alone; opening an agent from
//! it is the one thing that may change that, as opening it anywhere does.
//!
//! The rows are [`fleet::state_list::rows`], rebuilt at most a few times a
//! second while the fleet churns, in a uniform list that draws only the
//! rows in view, however many agents are in the state.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use fleet::state_list::StateRow;
use fleet::{Bucket, HostId};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::gpui::{
    AnyElement, App, ClickEvent, Context, Entity, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Render, StatefulInteractiveElement, Styled, UniformListScrollHandle, Window,
    div, point, px, relative, uniform_list,
};
use gpui_kit::prelude::FluentBuilder as _;

use crate::app::Core;
use crate::app::store::time_ago;
use crate::theme::{palette, size as sz};
use crate::ui;
use crate::ui::CachedExt as _;

/// The panel's width, where the window has room for it.
const WIDTH: f32 = 420.;
/// The title bar's height: the panel slides in under it.
const TOP: f32 = 36.;
/// Every row is this tall, so the list lays out only the rows in view.
const ROW_HEIGHT: f32 = 66.;
/// While the fleet churns, rebuild at most this often, as the counts
/// recount: a list a quarter second behind is not visible.
const REBUILD_INTERVAL: Duration = Duration::from_millis(250);
/// Reply reads in flight at once, across the fleet: enough to fill a
/// screen of rows in a couple of rounds, few enough for any daemon.
const IN_FLIGHT: usize = 6;
/// A reply is read again once the agent has changed since, but no sooner
/// than this after the last read, so a busy agent is not read on every
/// update.
const REREAD_AFTER: Duration = Duration::from_secs(20);
/// A read that failed or found no reply is not tried again sooner than
/// this.
const RETRY: Duration = Duration::from_secs(120);
/// How often the "3m ago"s are redrawn while the list is open.
const TICK: Duration = Duration::from_secs(30);

type Key = (HostId, String);

/// An agent's latest reply as last read, and the change it was read at.
struct Reply {
    line: Option<String>,
    read_for: Option<String>,
    read_at: Instant,
}

/// Whether a row's reply is worth reading: never read, or the agent has
/// changed since and the last read is not recent, or the last read found
/// nothing and is old.
fn needs_read(reply: Option<&Reply>, changed_at: Option<&String>) -> bool {
    let Some(reply) = reply else {
        return true;
    };
    let age = reply.read_at.elapsed();
    if reply.line.is_none() {
        return age >= RETRY;
    }
    reply.read_for.as_ref() != changed_at && age >= REREAD_AFTER
}

/// Where the list should be scrolled to, `top` pixels down, so the row at
/// its top stays there when the rows change; None to leave it. A list at
/// its very top stays at the top, so newly changed agents show.
fn kept_top(top: f32, old: &[StateRow], new: &[StateRow]) -> Option<f32> {
    if top <= 0. {
        return None;
    }
    let ix = (top / ROW_HEIGHT).floor() as usize;
    let row = old.get(ix)?;
    let moved = new
        .iter()
        .position(|other| other.host == row.host && other.agent_id == row.agent_id)?;
    Some(moved as f32 * ROW_HEIGHT + (top - ix as f32 * ROW_HEIGHT))
}

pub struct StateList {
    core: Core,
    /// The state listed, and whether background workspaces are left out.
    shown: Option<(Bucket, bool)>,
    rows: Vec<StateRow>,
    /// The store revision the rows were built from, and when.
    built: Option<(u64, Instant)>,
    rebuild_queued: bool,
    scroll: UniformListScrollHandle,
    replies: HashMap<Key, Reply>,
    reading: HashSet<Key>,
    tick_queued: bool,
}

impl StateList {
    pub fn new(core: Core, cx: &mut Context<Self>) -> Self {
        cx.observe(&core.store, |this, _, cx| this.store_changed(cx))
            .detach();
        cx.observe(&core.nav, |this, _, cx| this.follow_nav(cx))
            .detach();
        Self {
            core,
            shown: None,
            rows: Vec::new(),
            built: None,
            rebuild_queued: false,
            scroll: UniformListScrollHandle::new(),
            replies: HashMap::new(),
            reading: HashSet::new(),
            tick_queued: false,
        }
    }

    /// Opens, switches or closes the list as the nav says.
    fn follow_nav(&mut self, cx: &mut Context<Self>) {
        let nav = self.core.nav.read(cx);
        let wanted = nav.state_list.map(|state| (state, !nav.show_background()));
        if wanted == self.shown {
            return;
        }
        let switched = wanted.map(|(state, _)| state) != self.shown.map(|(state, _)| state);
        self.shown = wanted;
        if switched {
            self.scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(px(0.), px(0.)));
            self.rows.clear();
        }
        self.rebuild(cx);
    }

    /// The fleet changed: rebuild, at most every [`REBUILD_INTERVAL`].
    /// Nothing is done while the list is closed.
    fn store_changed(&mut self, cx: &mut Context<Self>) {
        if self.shown.is_none() || self.rebuild_queued {
            return;
        }
        if self
            .built
            .is_some_and(|(revision, _)| revision == self.core.store.read(cx).revision)
        {
            return;
        }
        let since = self.built.map_or(REBUILD_INTERVAL, |(_, at)| at.elapsed());
        if since >= REBUILD_INTERVAL {
            self.rebuild(cx);
            return;
        }
        self.rebuild_queued = true;
        let wait = REBUILD_INTERVAL - since;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |this, cx| {
                this.rebuild_queued = false;
                this.rebuild(cx);
            });
        })
        .detach();
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some((state, hide)) = self.shown else {
            if !self.rows.is_empty() {
                self.rows.clear();
                cx.notify();
            }
            return;
        };
        let store = self.core.store.read(cx);
        let rows = fleet::state_list::rows(&store.fleet, state, hide);
        self.built = Some((store.revision, Instant::now()));
        if rows == self.rows {
            return;
        }
        let handle = self.scroll.0.borrow().base_handle.clone();
        let offset = handle.offset();
        if let Some(top) = kept_top(-offset.y.as_f32(), &self.rows, &rows) {
            handle.set_offset(point(offset.x, px(-top)));
        }
        self.rows = rows;
        cx.notify();
    }

    /// Reads the latest replies of rows in view that have none yet, or an
    /// old one, a few at a time; each that lands redraws the list, which
    /// starts the next.
    fn read_replies(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        for ix in range {
            if self.reading.len() >= IN_FLIGHT {
                return;
            }
            let Some(row) = self.rows.get(ix) else {
                return;
            };
            // What it waits on or failed with is its line already.
            if row.detail.is_some() {
                continue;
            }
            let key = (row.host.clone(), row.agent_id.clone());
            if self.reading.contains(&key)
                || !needs_read(self.replies.get(&key), row.changed_at.as_ref())
            {
                continue;
            }
            let read_for = row.changed_at.clone();
            self.reading.insert(key.clone());
            // Asked of the store outside this draw: a view that reads the
            // store while drawing is drawn again on its every change.
            cx.spawn(async move |this, cx| {
                let Ok(read) = this.update(cx, |this, cx| {
                    (this.core.store.read(cx)).last_reply(&key.0, &key.1, cx)
                }) else {
                    return;
                };
                let found = read.await;
                let _ = this.update(cx, |this, cx| {
                    this.reading.remove(&key);
                    let line = (found.ok().flatten())
                        .map(|text| fleet::preview::first_line(&text))
                        .filter(|line| !line.is_empty());
                    this.replies.insert(
                        key,
                        Reply {
                            line,
                            read_for,
                            read_at: Instant::now(),
                        },
                    );
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// What a row is about: what it waits on or failed with, else its
    /// latest reply as last read.
    fn about(&self, row: &StateRow) -> Option<String> {
        if let Some(detail) = &row.detail {
            return Some(detail.clone());
        }
        let key = (row.host.clone(), row.agent_id.clone());
        self.replies.get(&key).and_then(|reply| reply.line.clone())
    }

    fn row(&self, ix: usize, state: Bucket, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let Some(row) = self.rows.get(ix) else {
            return div().h(px(ROW_HEIGHT)).into_any_element();
        };
        let about = self.about(row);
        // Not read yet, or being read: its line is on its way.
        let key = (row.host.clone(), row.agent_id.clone());
        let reading = self.reading.contains(&key) || !self.replies.contains_key(&key);
        let (about, about_color) = match about {
            Some(line) => (
                line,
                match state {
                    Bucket::NeedsInput => p.status_warning,
                    Bucket::Failed => p.status_danger,
                    _ => p.foreground,
                },
            ),
            None if reading => ("…".to_string(), p.extra_muted),
            None if state == Bucket::Failed => ("Error".to_string(), p.status_danger),
            None => ("No reply yet".to_string(), p.extra_muted),
        };
        let model = row.model_label();
        let meta = [row.host.as_str(), row.place.as_str(), model.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        let ago = row.changed_at.as_deref().map(time_ago).unwrap_or_default();
        let open = row.workspace_id.clone().map(|workspace| {
            let (host, agent) = (row.host.clone(), row.agent_id.clone());
            cx.listener(move |this, _: &ClickEvent, _, cx| {
                // As a notification's click does: its card in Lanes, else
                // its tab.
                this.core.nav.update(cx, |nav, cx| {
                    nav.set_state_list(None, cx);
                    nav.open_agent(&host, &workspace, &agent, cx);
                });
            })
        });
        // Every line is one line, cut with an ellipsis at the panel's edge,
        // whatever its text: the flex children may shrink to nothing.
        let line = |text: String, color| {
            div()
                .min_w(px(0.))
                .overflow_hidden()
                .h(px(16.))
                .line_height(px(16.))
                .text_size(sz::sm())
                .text_color(color)
                .truncate()
                .child(text)
        };
        div()
            .id(("state-row", ix))
            .w_full()
            .h(px(ROW_HEIGHT))
            .overflow_hidden()
            .px(px(12.))
            .py(px(5.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .border_b_1()
            .border_color(p.border)
            .when_some(open, |el, open| {
                el.cursor_pointer()
                    .hover(|style| style.bg(p.surface2))
                    .on_click(open)
            })
            .child(
                div()
                    .w_full()
                    .h(px(20.))
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(ui::agent_dot(state))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .truncate()
                            .text_color(p.foreground)
                            .child(row.title.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(sz::sm())
                            .text_color(p.extra_muted)
                            .child(ago),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .min_w(px(0.))
                    .child(line(meta, p.muted).flex_1())
                    .when(row.background, |el| el.child(ui::background_tag())),
            )
            .child(line(about, about_color).w_full())
            .into_any_element()
    }

    fn header(&self, state: Bucket) -> AnyElement {
        let p = palette();
        let nav = self.core.nav.clone();
        let show = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            // What a click on the count did before it opened this list.
            nav.update(cx, |nav, cx| {
                nav.set_state_list(None, cx);
                if nav.layout != "lanes" {
                    nav.layout = "lanes".into();
                    nav.save();
                }
                nav.set_state_highlight(Some(state), cx);
                cx.notify();
            });
        };
        let nav = self.core.nav.clone();
        let close = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            nav.update(cx, |nav, cx| nav.set_state_list(None, cx));
        };
        div()
            .h(px(40.))
            .flex_shrink_0()
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(p.border)
            .child(ui::agent_dot(state))
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(p.foreground)
                    .child(state.label()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(sz::sm())
                    .text_color(p.extra_muted)
                    .child(self.rows.len().to_string()),
            )
            .child(
                Button::new("state-list-board")
                    .icon(gpui_kit::component::Icon::empty().path("icons/kanban.svg"))
                    .label("Show on board")
                    .ghost()
                    .xsmall()
                    .tooltip("Single them out on the Lanes board")
                    .on_click(show),
            )
            .child(
                Button::new("state-list-close")
                    .icon(gpui_kit::component::Icon::empty().path("icons/x.svg"))
                    .ghost()
                    .xsmall()
                    .tooltip("Close (Esc)")
                    .on_click(close),
            )
            .into_any_element()
    }

    /// Redraws now and then while open, so "3m ago" keeps up.
    fn keep_ticking(&mut self, cx: &mut Context<Self>) {
        if self.tick_queued || self.shown.is_none() {
            return;
        }
        self.tick_queued = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TICK).await;
            let _ = this.update(cx, |this, cx| {
                this.tick_queued = false;
                if this.shown.is_some() {
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

impl Render for StateList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let Some((state, _)) = self.shown else {
            return div().into_any_element();
        };
        self.keep_ticking(cx);
        let header = self.header(state);
        let body = if self.rows.is_empty() {
            div()
                .flex_1()
                .p(px(16.))
                .text_size(sz::sm())
                .text_color(p.extra_muted)
                .child("None right now.")
                .into_any_element()
        } else {
            let list = uniform_list(
                "state-list-rows",
                self.rows.len(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    this.read_replies(range.clone(), cx);
                    range.map(|ix| this.row(ix, state, cx)).collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll)
            .size_full();
            div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .child(list)
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(Scrollbar::vertical(&self.scroll)),
                )
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }
}

/// The open list over the window, under the title bar: a clear layer that
/// closes it on a click outside, and the panel at the right edge, nearest
/// the counts. It appears at once, as crusteo's other panels do: none of
/// them animate (see `src/ui.rs`).
pub fn layer(list: &Entity<StateList>, core: &Core, cx: &App) -> Option<AnyElement> {
    core.nav.read(cx).state_list?;
    let p = palette();
    let nav = core.nav.clone();
    let outside = div()
        .id("state-list-outside")
        .absolute()
        .top(px(TOP))
        .left_0()
        .right_0()
        .bottom_0()
        .occlude()
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            cx.stop_propagation();
            nav.update(cx, |nav, cx| nav.set_state_list(None, cx));
        });
    let panel = div()
        .id("state-list")
        .absolute()
        .top(px(TOP))
        .bottom_0()
        .right_0()
        .w(px(WIDTH))
        .max_w(relative(0.9))
        .bg(p.surface0)
        .border_l_1()
        .border_color(p.border_accent)
        .shadow_lg()
        .occlude()
        // A click inside is the panel's; the window's "any click closes"
        // must not take it away before the click lands.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(list.clone().cached_fill());
    Some(
        div()
            .absolute()
            .inset_0()
            .child(outside)
            .child(panel)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(agent: &str) -> StateRow {
        StateRow {
            host: HostId::new("h"),
            agent_id: agent.into(),
            workspace_id: None,
            title: agent.into(),
            place: String::new(),
            provider: String::new(),
            model: None,
            changed_at: None,
            background: false,
            detail: None,
        }
    }

    #[test]
    fn the_row_at_the_top_stays_there_as_rows_move() {
        let old: Vec<StateRow> = ["a", "b", "c", "d"].map(row).into();
        let new: Vec<StateRow> = ["x", "y", "a", "b", "c", "d"].map(row).into();
        // Two rows and ten pixels down, at "c": "c" moved down two.
        let top = 2. * ROW_HEIGHT + 10.;
        assert_eq!(kept_top(top, &old, &new), Some(4. * ROW_HEIGHT + 10.));
        // At the very top the list stays at the top, new rows in view.
        assert_eq!(kept_top(0., &old, &new), None);
        // A row that left the state leaves the scroll where it was.
        let gone: Vec<StateRow> = ["a", "b", "d"].map(row).into();
        assert_eq!(kept_top(top, &old, &gone), None);
    }

    #[test]
    fn replies_are_read_once_and_again_only_after_a_change() {
        assert!(needs_read(None, None));
        let changed = Some("2026-10-02T00:00:00Z".to_string());
        let fresh = Reply {
            line: Some("Done.".into()),
            read_for: changed.clone(),
            read_at: Instant::now(),
        };
        assert!(!needs_read(Some(&fresh), changed.as_ref()));
        // Changed since, but read a moment ago: not yet.
        let later = Some("2026-10-03T00:00:00Z".to_string());
        assert!(!needs_read(Some(&fresh), later.as_ref()));
        let old = Reply {
            read_at: Instant::now().checked_sub(REREAD_AFTER).unwrap(),
            ..fresh
        };
        assert!(needs_read(Some(&old), later.as_ref()));
        assert!(!needs_read(Some(&old), changed.as_ref()));
        // Nothing found: tried again only after a while.
        let empty = Reply {
            line: None,
            read_for: changed.clone(),
            read_at: Instant::now(),
        };
        assert!(!needs_read(Some(&empty), later.as_ref()));
    }
}
