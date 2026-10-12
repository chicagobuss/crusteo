//! One agent's conversation: the stream of rows, permission cards, the
//! working indicator, and the composer.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use super::prompt_index::{PromptNavigation, magnification};
use fleet::Bucket;
use fleet::display::{self, ToolDisplay};
use fleet::prompt_index::{self, PromptMark};
use fleet::timeline::{ItemKind, TaskActivity, ToolCall, Turn};
use gpui_kit::base::Disableable as _;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::text::TextView;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, EventEmitter, FollowMode,
    InteractiveElement, IntoElement, ListAlignment, ListOffset, ListState, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, list, px,
};
use gpui_kit::prelude::FluentBuilder;
use serde_json::{Value, json};

use super::attachments;
use super::composer::Composer;
use crate::app::store::{now_millis, parse_iso};
use crate::app::{AgentSession, Core};
use crate::theme::{palette, size, with_alpha};
use crate::ui;

#[derive(Clone)]
enum Row {
    Item {
        index: usize,
        gap: f32,
    },
    /// After a finished turn: items `start..=end` (start is the user message).
    TurnFooter {
        start: usize,
        end: usize,
    },
    LoadOlder,
    Working,
    Permission(usize),
    Empty,
}

#[derive(Clone)]
struct CachedImages {
    keys: Vec<u64>,
    decoded: Vec<Arc<gpui_kit::gpui::Image>>,
}

pub struct Conversation {
    core: Core,
    pub host: String,
    pub agent_id: String,
    session: Entity<AgentSession>,
    /// None for a provider subagent, which is read-only.
    composer: Option<Entity<Composer>>,
    /// The parent agent and subagent ids, when this shows a provider
    /// subagent rather than an agent.
    subagent: Option<(String, String)>,
    /// A provider subagent opened from the track, shown in place of this
    /// conversation until closed.
    viewing: Option<(String, Entity<Conversation>)>,
    /// The subagents track is unfolded.
    track_open: bool,
    /// What the track last showed, to redraw only when it changes.
    track: Rc<Vec<TrackRow>>,
    track_revision: (u64, u64),
    list: ListState,
    rows: Rc<Vec<Row>>,
    fingerprints: Vec<u64>,
    revision: u64,
    expanded: Rc<std::cell::RefCell<HashSet<String>>>,
    ticking: bool,
    agent_marker: Option<String>,
    navigation: PromptNavigation,
    images: Rc<HashMap<String, Rc<CachedImages>>>,
    presentation: Option<(bool, Option<String>, u64)>,
    /// Where "Archive finished" is.
    archive_finished: ArchiveFinished,
    /// A timeline item to scroll to once it is in the rows.
    reveal: Option<String>,
    /// Fills its width instead of keeping to a column for reading, as in
    /// a Lanes card, whose lane sets how wide it is.
    wide: bool,
}

/// Someone the conversation asks its owner to show.
pub enum ConversationEvent {
    /// A Paseo subagent, opened from the track: a full agent of its own.
    OpenAgent { host: String, agent: String },
}

impl EventEmitter<ConversationEvent> for Conversation {}

/// The subagents bar's "Archive finished": ready, counting up while it
/// works, or offering to retry those that failed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArchiveFinished {
    Idle,
    Archiving { done: usize, total: usize },
    Failed { failed: usize, total: usize },
}

/// One subagent on a conversation's track.
#[derive(Clone, PartialEq)]
struct TrackRow {
    /// A Paseo subagent's agent id, or a provider subagent's id.
    id: String,
    provider_subagent: bool,
    /// Done with its work: an idle or failed Paseo subagent, or a
    /// provider's that completed, failed or was cancelled.
    finished: bool,
    label: String,
    detail: Option<String>,
    bucket: Bucket,
    created_at: String,
}

impl Conversation {
    pub fn new(
        core: Core,
        host: &str,
        agent_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = core
            .store
            .update(cx, |store, cx| store.open_session(host, agent_id, cx));
        let composer = cx.new(|cx| Composer::new(core.clone(), host, agent_id, window, cx));
        Self::build(core, host, agent_id, session, Some(composer), None, cx)
    }

    /// A provider subagent's conversation: read-only, streaming.
    fn new_subagent(
        core: Core,
        host: &str,
        parent: &str,
        subagent: &str,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = core.store.update(cx, |store, cx| {
            store.open_subagent(host, parent, subagent, cx)
        });
        cx.on_release({
            let (store, host, parent, subagent) = (
                core.store.clone(),
                host.to_string(),
                parent.to_string(),
                subagent.to_string(),
            );
            move |_, cx| {
                store.update(cx, |store, _| {
                    store.close_subagent(&host, &parent, &subagent)
                });
            }
        })
        .detach();
        let source = Some((parent.to_string(), subagent.to_string()));
        Self::build(core, host, subagent, session, None, source, cx)
    }

    fn build(
        core: Core,
        host: &str,
        agent_id: &str,
        session: Entity<AgentSession>,
        composer: Option<Entity<Composer>>,
        subagent: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&session, |this, _, cx| this.refresh(cx))
            .detach();
        cx.observe(&core.store, |this, store, cx| {
            // Only this agent's snapshot and its subagents matter here; the
            // store changes on every push from every host.
            let marker = store
                .read(cx)
                .fleet
                .agent(&this.host, &this.agent_id)
                .map(|a| {
                    format!(
                        "{:?}{}{}",
                        a.updated_at,
                        a.status,
                        a.pending_permissions.len()
                    )
                });
            let revision = (store.read(cx).revision, store.read(cx).subagents_revision);
            let track = if revision != this.track_revision {
                this.track_revision = revision;
                Rc::new(this.track_rows(cx))
            } else {
                this.track.clone()
            };
            if marker != this.agent_marker || track != this.track {
                this.agent_marker = marker;
                this.track = track;
                cx.notify();
            }
        })
        .detach();
        let list = ListState::new(0, ListAlignment::Bottom, px(800.));
        list.set_follow_mode(FollowMode::Tail);
        let mut this = Self {
            core,
            host: host.to_string(),
            agent_id: agent_id.to_string(),
            session,
            composer,
            subagent,
            viewing: None,
            track_open: false,
            track: Rc::new(Vec::new()),
            track_revision: (u64::MAX, u64::MAX),
            list,
            rows: Rc::new(Vec::new()),
            fingerprints: Vec::new(),
            revision: u64::MAX,
            expanded: Rc::new(std::cell::RefCell::new(HashSet::new())),
            ticking: false,
            agent_marker: None,
            navigation: PromptNavigation::default(),
            images: Rc::new(HashMap::new()),
            presentation: None,
            wide: false,
            archive_finished: ArchiveFinished::Idle,
            reveal: None,
        };
        let weak = cx.entity().downgrade();
        this.list.set_scroll_handler(move |event, _, cx| {
            let _ = weak.update(cx, |this, cx| {
                let row = (!event.is_following_tail).then_some(event.visible_range.start);
                if this.navigation.set_reading_row(row) {
                    cx.notify();
                }
            });
        });
        this.track = Rc::new(this.track_rows(cx));
        this.track_revision = (
            this.core.store.read(cx).revision,
            this.core.store.read(cx).subagents_revision,
        );
        this.refresh(cx);
        this
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        if let Some(composer) = &self.composer {
            composer.update(cx, |composer, cx| composer.focus(window, cx));
        }
    }

    pub fn composer_is_blank(&self, cx: &App) -> bool {
        self.composer
            .as_ref()
            .is_none_or(|composer| composer.read(cx).is_blank(cx))
    }

    pub fn composer_is_focused(&self, window: &Window, cx: &App) -> bool {
        self.composer
            .as_ref()
            .is_some_and(|composer| composer.read(cx).is_focused(window, cx))
    }

    /// Loads the page before what is shown.
    fn load_older(&mut self, cx: &mut Context<Self>) {
        let host = self.host.clone();
        match self.subagent.clone() {
            Some((parent, subagent)) => self.core.store.update(cx, |store, cx| {
                store.fetch_older_subagent(&host, &parent, &subagent, cx)
            }),
            None => {
                let agent = self.agent_id.clone();
                self.core
                    .store
                    .update(cx, |store, cx| store.fetch_older(&host, &agent, cx));
            }
        }
    }

    /// The subagents this conversation's agent or subagent started, oldest
    /// first: Paseo subagents, which are agents of their own, and the
    /// provider's, which live inside it.
    fn track_rows(&self, cx: &App) -> Vec<TrackRow> {
        let store = self.core.store.read(cx);
        let mut rows = Vec::new();
        let (parent, within) = match &self.subagent {
            Some((parent, subagent)) => (parent.as_str(), Some(subagent.as_str())),
            None => {
                if let Some(host) = store.fleet.host(&self.host) {
                    rows.extend(
                        host.directory
                            .agents
                            .values()
                            .filter(|agent| {
                                agent.parent_agent_id.as_deref() == Some(self.agent_id.as_str())
                                    && agent.archived_at.is_none()
                            })
                            .map(|agent| TrackRow {
                                id: agent.id.clone(),
                                provider_subagent: false,
                                finished: matches!(agent.status.as_str(), "idle" | "error"),
                                label: agent.display_title(),
                                detail: None,
                                // Paseo shows a subagent's own state, not
                                // whether it asks for review.
                                bucket: match agent.bucket() {
                                    Bucket::Attention => Bucket::Done,
                                    bucket => bucket,
                                },
                                created_at: agent.created_at.clone().unwrap_or_default(),
                            }),
                    );
                }
                (self.agent_id.as_str(), None)
            }
        };
        rows.extend(
            store
                .provider_subagents(&self.host, parent)
                .iter()
                .filter(|sub| sub.parent_subagent_id.as_deref() == within)
                .filter(|sub| !store.provider_subagent_hidden(&self.host, parent, &sub.id))
                .map(|sub| TrackRow {
                    id: sub.id.clone(),
                    provider_subagent: true,
                    finished: !sub.running(),
                    label: sub.label(),
                    detail: sub.detail(),
                    bucket: sub.bucket(),
                    created_at: sub.created_at.clone(),
                }),
        );
        rows.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
        rows
    }

    /// Lets the conversation and its composer fill their width rather than
    /// keep to a reading column.
    pub fn set_wide(&mut self, wide: bool, cx: &mut Context<Self>) {
        if self.wide == wide {
            return;
        }
        self.wide = wide;
        if let Some(composer) = &self.composer {
            composer.update(cx, |composer, cx| composer.set_wide(wide, cx));
        }
        if let Some((_, view)) = &self.viewing {
            view.update(cx, |view, cx| view.set_wide(wide, cx));
        }
        cx.notify();
    }

    /// Opens a subagent from the track: a Paseo subagent in its owner's
    /// view, a provider subagent here, read-only.
    fn open_track_row(&mut self, row: &TrackRow, cx: &mut Context<Self>) {
        if !row.provider_subagent {
            cx.emit(ConversationEvent::OpenAgent {
                host: self.host.clone(),
                agent: row.id.clone(),
            });
            return;
        }
        let parent = match &self.subagent {
            Some((parent, _)) => parent.clone(),
            None => self.agent_id.clone(),
        };
        let (core, host, id) = (self.core.clone(), self.host.clone(), row.id.clone());
        let wide = self.wide;
        let view = cx.new(|cx| {
            let mut view = Conversation::new_subagent(core, &host, &parent, &id, cx);
            view.wide = wide;
            view
        });
        // A nested subagent asks this conversation to open Paseo agents.
        cx.subscribe(&view, |_, _, event: &ConversationEvent, cx| match event {
            ConversationEvent::OpenAgent { host, agent } => cx.emit(ConversationEvent::OpenAgent {
                host: host.clone(),
                agent: agent.clone(),
            }),
        })
        .detach();
        self.viewing = Some((row.label.clone(), view));
        cx.notify();
    }

    /// "Archive finished" on the subagents bar, as Paseo's track has it:
    /// one click archives the Paseo subagents that are idle or failed and
    /// takes the provider's finished ones off the track, since they are not
    /// Paseo agents. Those working or waiting on the user stay. It counts
    /// up while it works and offers a retry for any that failed.
    fn archive_finished_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let finished: Vec<TrackRow> = self
            .track
            .iter()
            .filter(|row| row.finished)
            .cloned()
            .collect();
        let label = match self.archive_finished {
            ArchiveFinished::Archiving { done, total } => format!("Archiving {done}/{total}"),
            ArchiveFinished::Failed { failed, total } => format!("Retry ({failed}/{total})"),
            ArchiveFinished::Idle if finished.is_empty() => return None,
            ArchiveFinished::Idle => "Archive finished".to_string(),
        };
        let busy = matches!(self.archive_finished, ArchiveFinished::Archiving { .. });
        Some(
            Button::new(SharedString::from(format!(
                "track-archive-{}",
                self.agent_id
            )))
            .icon(gpui_kit::component::Icon::empty().path("icons/archive.svg"))
            .label(label)
            .ghost()
            .xsmall()
            .disabled(busy)
            .tooltip("Archive finished subagents")
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                // Not a click on the bar, which folds the list.
                cx.stop_propagation();
                this.archive_finished_subagents(finished.clone(), cx);
            }))
            .into_any_element(),
        )
    }

    fn archive_finished_subagents(&mut self, rows: Vec<TrackRow>, cx: &mut Context<Self>) {
        if rows.is_empty() || matches!(self.archive_finished, ArchiveFinished::Archiving { .. }) {
            return;
        }
        let parent = match &self.subagent {
            Some((parent, _)) => parent.clone(),
            None => self.agent_id.clone(),
        };
        let (provider, paseo): (Vec<TrackRow>, Vec<TrackRow>) =
            rows.into_iter().partition(|row| row.provider_subagent);
        let total = provider.len() + paseo.len();
        let host = self.host.clone();
        // The provider's are only taken off the track.
        let hidden: Vec<String> = provider.into_iter().map(|row| row.id).collect();
        self.core.store.update(cx, |store, cx| {
            store.hide_provider_subagents(&host, &parent, &hidden);
            cx.notify();
        });
        self.archive_finished = ArchiveFinished::Archiving {
            done: hidden.len(),
            total,
        };
        cx.notify();
        let store = self.core.store.clone();
        cx.spawn(async move |this, cx| {
            let mut failed = 0;
            for (at, row) in paseo.iter().enumerate() {
                let task = store.update(cx, |store, cx| {
                    store.write(&host, fleet::requests::archive_agent(&row.id), cx)
                });
                let ok = match task.await {
                    Ok(payload) => !payload["error"].is_string(),
                    Err(_) => false,
                };
                if !ok {
                    failed += 1;
                }
                let done = hidden.len() + at + 1;
                let _ = this.update(cx, |this, cx| {
                    this.archive_finished = ArchiveFinished::Archiving { done, total };
                    cx.notify();
                });
            }
            let _ = this.update(cx, |this, cx| {
                this.archive_finished = if failed > 0 {
                    ArchiveFinished::Failed {
                        failed,
                        total: failed,
                    }
                } else {
                    ArchiveFinished::Idle
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// The bar above the composer that lists this conversation's subagents,
    /// summed up as Paseo does: what is not done, else how many.
    fn track_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.track.is_empty() {
            return None;
        }
        let p = palette();
        let count = |bucket: Bucket| self.track.iter().filter(|row| row.bucket == bucket).count();
        let mut parts = Vec::new();
        for (bucket, one, many) in [
            (Bucket::NeedsInput, "needs input", "need input"),
            (Bucket::Failed, "failed", "failed"),
            (Bucket::Attention, "ready to review", "ready to review"),
            (Bucket::Running, "working", "working"),
        ] {
            match count(bucket) {
                0 => {}
                1 => parts.push((bucket, format!("1 {one}"))),
                n => parts.push((bucket, format!("{n} {many}"))),
            }
        }
        let total = match self.track.len() {
            1 => "1 subagent".to_string(),
            n => format!("{n} subagents"),
        };
        let id = |part: &str| SharedString::from(format!("track-{part}-{}", self.agent_id));
        let header = div()
            .id(id("toggle"))
            .h(px(28.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|style| style.bg(p.surface1))
            .text_size(size::sm())
            .text_color(p.muted)
            .child(ui::icon(
                if self.track_open {
                    "chevron-down"
                } else {
                    "chevron-right"
                },
                12.,
                p.extra_muted,
            ))
            .child(ui::icon("bot", 13., p.muted))
            .child(total)
            .children(parts.into_iter().map(|(bucket, text)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(ui::agent_dot(bucket))
                    .child(text)
            }))
            .child(div().flex_1())
            .children(self.archive_finished_button(cx))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.track_open = !this.track_open;
                cx.notify();
            }));
        let rows = self.track_open.then(|| {
            let mut list = div()
                .id(id("rows"))
                .max_h(px(220.))
                .overflow_y_scroll()
                .flex()
                .flex_col();
            for row in self.track.iter() {
                let open = row.clone();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("track-row-{}", row.id)))
                        .h(px(26.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|style| style.bg(p.surface1))
                        .text_size(size::sm())
                        .child(ui::agent_dot(row.bucket))
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(p.foreground)
                                .child(row.label.clone()),
                        )
                        .children(row.detail.clone().map(|detail| {
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(p.extra_muted)
                                .child(detail)
                        }))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.open_track_row(&open, cx);
                        })),
                );
            }
            list
        });
        Some(
            div()
                .w_full()
                .flex()
                .justify_center()
                .pl(px(size::CONVERSATION_PADDING_LEFT))
                .pr(px(size::CONVERSATION_PADDING_RIGHT))
                .pt(px(4.))
                .child(
                    div()
                        .w_full()
                        .when(!self.wide, |track| {
                            track.max_w(px(size::COMPOSER_MAX_WIDTH))
                        })
                        .flex()
                        .flex_col()
                        .child(header)
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    /// A provider subagent opened from the track, with the way back.
    fn subagent_view(
        &self,
        label: &str,
        view: &Entity<Conversation>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface0)
            .child(
                div()
                    .h(px(32.))
                    .flex_shrink_0()
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(p.border)
                    .text_size(size::sm())
                    .child(
                        Button::new(SharedString::from(format!("track-back-{}", self.agent_id)))
                            .icon(gpui_kit::component::Icon::empty().path("icons/arrow-left.svg"))
                            .ghost()
                            .xsmall()
                            .tooltip("Back to the conversation")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.viewing = None;
                                cx.notify();
                            })),
                    )
                    .child(ui::icon("bot", 13., p.muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(p.foreground)
                            .child(label.to_string()),
                    )
                    .child(
                        div()
                            .text_color(p.extra_muted)
                            .child("Subagent · read-only"),
                    ),
            )
            .child(div().flex_1().min_h(px(0.)).child(view.clone()))
            .into_any_element()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let presentation = (
            session.loading,
            session.error.clone(),
            session.prompt_index.revision(),
        );
        let presentation_changed = self.presentation.as_ref() != Some(&presentation);
        self.presentation = Some(presentation);
        let timeline = &session.timeline;
        if timeline.revision == self.revision {
            self.scroll_pending(cx);
            if presentation_changed {
                cx.notify();
            }
            return;
        }
        self.revision = timeline.revision;

        let items: Vec<_> = timeline.items().collect();
        let mut rows = Vec::with_capacity(items.len() + 4);
        let mut prints = Vec::with_capacity(items.len() + 4);
        if timeline.has_older {
            rows.push(Row::LoadOlder);
            prints.push(1);
        }
        let toolish = |kind: &ItemKind| {
            matches!(
                kind,
                ItemKind::Tool(_) | ItemKind::Thought { .. } | ItemKind::Todo { .. }
            )
        };
        let mut turn_start: Option<usize> = None;
        for (index, item) in items.iter().enumerate() {
            if matches!(item.kind, ItemKind::User { .. }) {
                if let Some(start) = turn_start.take()
                    && index > start + 1
                {
                    rows.push(Row::TurnFooter {
                        start,
                        end: index - 1,
                    });
                    prints.push(hash_str(&format!("footer-{start}-{}", index - 1)));
                }
                turn_start = Some(index);
            }
            if is_hidden(&item.kind) {
                continue;
            }
            let next_toolish = items.get(index + 1).is_some_and(|next| toolish(&next.kind));
            let gap = if toolish(&item.kind) && next_toolish {
                4.
            } else {
                16.
            };
            rows.push(Row::Item { index, gap });
            prints.push(fingerprint(item, self.expanded.borrow().contains(&item.id)));
        }
        if let Some(start) = turn_start
            && !timeline.is_running()
            && items.len() > start + 1
        {
            let end = items.len() - 1;
            rows.push(Row::TurnFooter { start, end });
            prints.push(hash_str(&format!("footer-{start}-{end}")));
        }
        if items.is_empty() && !session.loading {
            rows.push(Row::Empty);
            prints.push(2);
        }
        if timeline.is_running() {
            rows.push(Row::Working);
            prints.push(3);
        }
        for (i, permission) in timeline.permissions.iter().enumerate() {
            rows.push(Row::Permission(i));
            prints.push(hash_str(&permission.id));
        }

        // Splice from the first row that changed so the list re-measures
        // only what moved and keeps the scroll position.
        let first_change = self
            .fingerprints
            .iter()
            .zip(prints.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(self.fingerprints.len().min(prints.len()));
        if first_change < self.fingerprints.len() || prints.len() != self.fingerprints.len() {
            self.list.splice(
                first_change..self.fingerprints.len(),
                prints.len() - first_change,
            );
        }
        self.fingerprints = prints;
        self.navigation.rows.clear();
        self.navigation.loaded.clear();
        let cache = Rc::make_mut(&mut self.images);
        cache.retain(|id, _| items.iter().any(|item| &item.id == id));
        for (row, entry) in rows.iter().enumerate() {
            if let Row::Item { index, .. } = entry
                && let ItemKind::User {
                    text, images, seq, ..
                } = &items[*index].kind
            {
                if let Some(seq) = seq {
                    self.navigation.rows.insert(*seq, row);
                    self.navigation.loaded.push(PromptMark {
                        seq: *seq,
                        preview: prompt_index::preview(text, !images.is_empty()),
                    });
                }
                let keys: Vec<_> = images.iter().map(|image| image.content_key()).collect();
                if !cache
                    .get(&items[*index].id)
                    .is_some_and(|entry| entry.keys == keys)
                {
                    let id = items[*index].id.clone();
                    let wires = images.clone();
                    cache.insert(
                        id.clone(),
                        Rc::new(CachedImages {
                            keys: keys.clone(),
                            decoded: Vec::new(),
                        }),
                    );
                    if !wires.is_empty() {
                        cx.spawn(async move |this, cx| {
                            let decoded = cx.background_executor().spawn(async move {
                                wires.iter().filter_map(attachments::decode).collect()
                            }).await;
                            let _ = this.update(cx, |this, cx| {
                                let cache = Rc::make_mut(&mut this.images);
                                if cache.get(&id).is_some_and(|entry| entry.keys == keys) {
                                    cache.insert(id.clone(), Rc::new(CachedImages { keys, decoded }));
                                    let timeline = &this.session.read(cx).timeline;
                                    let item = timeline.items().position(|item| item.id == id);
                                    if let Some(row) = this.rows.iter().position(|row| matches!(row, Row::Item { index, .. } if Some(*index) == item)) {
                                        this.list.splice(row..row+1, 1);
                                    }
                                    cx.notify();
                                }
                            });
                        }).detach();
                    }
                }
            }
        }
        self.rows = Rc::new(rows);
        let running = timeline.is_running();
        self.scroll_pending(cx);
        self.try_reveal(cx);

        if running && !self.ticking {
            self.ticking = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let running = this
                        .update(cx, |this, cx| {
                            let running = this.session.read(cx).timeline.is_running();
                            if running {
                                cx.notify();
                            } else {
                                this.ticking = false;
                            }
                            running
                        })
                        .unwrap_or(false);
                    if !running {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.notify();
    }
    /// Scrolls to a timeline item, now or once its rows are built, as when
    /// find opens a card at a match.
    pub fn reveal_item(&mut self, id: String, cx: &mut Context<Self>) {
        self.reveal = Some(id);
        self.try_reveal(cx);
        cx.notify();
    }

    fn try_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.reveal.clone() else {
            return;
        };
        let index = self
            .session
            .read(cx)
            .timeline
            .items()
            .position(|item| item.id == id);
        let row = index.and_then(|index| {
            self.rows
                .iter()
                .position(|row| matches!(row, Row::Item { index: at, .. } if *at == index))
        });
        if let Some(row) = row {
            self.list.pause_following_tail();
            self.list.scroll_to(ListOffset {
                item_ix: row,
                offset_in_item: px(0.),
            });
            self.reveal = None;
        }
    }

    fn scroll_pending(&mut self, cx: &mut Context<Self>) {
        let Some(seq) = self.navigation.pending else {
            return;
        };
        if matches!(
            self.session.read(cx).prompt_index.jump(),
            fleet::prompt_index::JumpState::Failed(_)
        ) {
            self.navigation.pending = None;
            return;
        }
        if let Some(&row) = self.navigation.rows.get(&seq) {
            self.list.pause_following_tail();
            self.list.scroll_to(ListOffset {
                item_ix: row,
                offset_in_item: px(0.),
            });
            self.navigation.reading_row = Some(row);
            self.navigation.pending = None;
            cx.notify();
        } else if matches!(
            self.session.read(cx).prompt_index.jump(),
            fleet::prompt_index::JumpState::Idle
        ) {
            self.navigation.pending = None;
        }
    }

    fn prompt_index(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let session = self.session.read(cx);
        let prompts = if session.prompt_index.marks().is_empty() {
            &self.navigation.loaded
        } else {
            session.prompt_index.marks()
        };
        let prompt_rows = &self.navigation.rows;
        let active = prompts
            .iter()
            .enumerate()
            .filter_map(|(i, mark)| {
                prompt_rows
                    .get(&mark.seq)
                    .filter(|row| {
                        self.navigation
                            .reading_row
                            .is_none_or(|reading| **row <= reading)
                    })
                    .map(|_| i)
            })
            .next_back();
        let hovered = self
            .navigation
            .hovered
            .and_then(|seq| prompts.iter().position(|mark| mark.seq == seq));
        div()
            .absolute()
            .left(px(0.))
            .top(px(0.))
            .h_full()
            .w(px(size::PROMPT_INDEX_WIDTH))
            .flex()
            .items_center()
            .child(
                div()
                    .id("prompt-index")
                    .w(px(size::PROMPT_INDEX_WIDTH))
                    .max_h(px(400.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(prompts.iter().enumerate().map(|(i, mark)| {
                        let scale = hovered.map(|h| magnification(i.abs_diff(h))).unwrap_or(0.);
                        let width = if active == Some(i) { 18. } else { 10. };
                        let target = mark.clone();
                        let seq = mark.seq;
                        let preview = mark.preview.clone();
                        div()
                            .id(SharedString::from(format!("prompt-tick-{}", mark.seq)))
                            .w(px(size::PROMPT_INDEX_WIDTH))
                            .h(px(10.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .tooltip(move |window, cx| {
                                Tooltip::new(preview.clone())
                                    .max_w(px(260.))
                                    .build(window, cx)
                            })
                            .tooltip_show_delay(Duration::from_millis(100))
                            .on_hover(cx.listener(move |this, hovering, _, cx| {
                                let hovered = if *hovering { Some(seq) } else { None };
                                if this.navigation.hovered != hovered {
                                    this.navigation.hovered = hovered;
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.navigation.pending = Some(target.seq);
                                this.list.pause_following_tail();
                                this.core.store.update(cx, |store, cx| {
                                    store.load_prompt(&this.host, &this.agent_id, target.seq, cx)
                                });
                                this.scroll_pending(cx);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w(px(width + scale * (size::PROMPT_TICK_MAX_WIDTH - width)))
                                    .h(px(2. + scale * 2.))
                                    .rounded(px(2.))
                                    .bg(if active == Some(i) || hovered == Some(i) {
                                        p.foreground
                                    } else {
                                        p.extra_muted
                                    }),
                            )
                    })),
            )
            .into_any_element()
    }
}

fn is_hidden(kind: &ItemKind) -> bool {
    match kind {
        ItemKind::Tool(tool) => {
            let name = tool.name.as_str();
            name == "ExitPlanMode" || (name == "plan_approval" && tool.running())
        }
        _ => false,
    }
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn fingerprint(item: &fleet::timeline::Item, expanded: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    item.id.hash(&mut h);
    expanded.hash(&mut h);
    match &item.kind {
        ItemKind::User {
            text,
            images,
            message_id,
            ..
        } => {
            text.len().hash(&mut h);
            images.len().hash(&mut h);
            message_id.is_some().hash(&mut h);
        }
        ItemKind::Assistant { text, .. } => text.len().hash(&mut h),
        ItemKind::Thought { text, loading } => {
            text.len().hash(&mut h);
            loading.hash(&mut h);
        }
        ItemKind::Tool(tool) => {
            tool.status.hash(&mut h);
            tool.detail_type().hash(&mut h);
            for key in [
                "output",
                "content",
                "command",
                "filePath",
                "newString",
                "unifiedDiff",
                "text",
            ] {
                tool.detail[key].as_str().map(str::len).hash(&mut h);
            }
            tool.error.is_some().hash(&mut h);
        }
        ItemKind::Todo { items, .. } => items.len().hash(&mut h),
        ItemKind::Notification { message, .. } => message.len().hash(&mut h),
        ItemKind::Compaction { loading, .. } => loading.hash(&mut h),
    }
    h.finish()
}

impl Render for Conversation {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((label, view)) = self.viewing.clone() {
            return self.subagent_view(&label, &view, cx);
        }
        let p = palette();
        let rows = self.rows.clone();
        let wide = self.wide;
        let session = self.session.clone();
        let core = self.core.clone();
        let expanded = self.expanded.clone();
        let this = cx.entity();
        let cwd = core
            .store
            .read(cx)
            .fleet
            .agent(&self.host, &self.agent_id)
            .map(|a| a.cwd.clone())
            .unwrap_or_default();

        let stream = list(self.list.clone(), move |ix, _window, cx| {
            let Some(row) = rows.get(ix).cloned() else {
                return div().into_any_element();
            };
            let content = render_row(&row, &session, &core, &expanded, &this, &cwd, cx);
            div()
                .w_full()
                .flex()
                .justify_center()
                .pl(px(size::CONVERSATION_PADDING_LEFT))
                .pr(px(size::CONVERSATION_PADDING_RIGHT))
                .child(
                    div()
                        .w_full()
                        .when(!wide, |row| row.max_w(px(size::COMPOSER_MAX_WIDTH)))
                        .child(content),
                )
                .into_any_element()
        })
        .size_full();

        let session = self.session.read(cx);
        let error = session.display_error();
        let loading = session.loading;
        let show_index = session
            .prompt_index
            .marks()
            .len()
            .max(self.navigation.loaded.len())
            >= 2;
        let jump_loading = session.prompt_index.loading();
        let index = (!loading && show_index).then(|| self.prompt_index(cx));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface0)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .pt(px(12.))
                    .when(loading, |d| {
                        d.child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(p.muted)
                                .child("Loading conversation…"),
                        )
                    })
                    .when(!loading, |d| d.child(stream))
                    .when_some(index, |d, index| d.child(index))
                    .when(jump_loading, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(4.))
                                .left(px(size::CONVERSATION_PADDING_LEFT + 8.))
                                .text_color(p.muted)
                                .child("Loading prompt…"),
                        )
                    }),
            )
            .when_some(error, |d, error| {
                d.child(
                    div()
                        .pl(px(size::CONVERSATION_PADDING_LEFT))
                        .pr(px(size::CONVERSATION_PADDING_RIGHT))
                        .py(px(6.))
                        .text_size(size::sm())
                        .text_color(p.status_danger)
                        .child(error),
                )
            })
            .children(self.track_bar(cx))
            .children(self.composer.clone())
            .into_any_element()
    }
}

fn render_row(
    row: &Row,
    session: &Entity<AgentSession>,
    core: &Core,
    expanded: &Rc<std::cell::RefCell<HashSet<String>>>,
    view: &Entity<Conversation>,
    cwd: &str,
    cx: &mut App,
) -> AnyElement {
    let p = palette();
    let session_ref = session.read(cx);
    let timeline = &session_ref.timeline;
    match row {
        Row::LoadOlder => {
            let view = view.clone();
            div()
                .id(SharedString::from(format!(
                    "load-older-{}",
                    session_ref.agent_id
                )))
                .py(px(8.))
                .flex()
                .justify_center()
                .text_size(size::sm())
                .text_color(p.muted)
                .hover(|s| s.text_color(p.foreground))
                .child("Load earlier messages")
                .on_click(move |_, _, cx| {
                    view.update(cx, |view, cx| view.load_older(cx));
                })
                .into_any_element()
        }
        Row::Empty => div()
            .py(px(48.))
            .flex()
            .justify_center()
            .text_color(p.muted)
            .child("No messages yet")
            .into_any_element(),
        Row::Working => {
            let elapsed = match &timeline.turn {
                Turn::Open {
                    started_at: Some(start),
                    ..
                } => parse_iso(start)
                    .map(|t| display::short_duration((now_millis() - t) / 1000))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            div()
                .pb(px(16.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(size::sm())
                .text_color(p.muted)
                .child(ui::running_ring())
                .child(format!("Working {elapsed}"))
                .into_any_element()
        }
        Row::TurnFooter { start, end } => {
            let items: Vec<_> = timeline
                .items()
                .skip(*start)
                .take(end - start + 1)
                .collect();
            let first = items.first().and_then(|i| parse_iso(&i.timestamp));
            let last = items
                .last()
                .map(|i| i.timestamp.clone())
                .unwrap_or_default();
            let duration = match (first, parse_iso(&last)) {
                (Some(a), Some(b)) if b >= a => display::short_duration((b - a) / 1000),
                _ => String::new(),
            };
            let text: String = items
                .iter()
                .filter_map(|item| match &item.kind {
                    ItemKind::Assistant { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            let ago = crate::app::store::time_ago(&last);
            div()
                .mt(px(-8.))
                .pb(px(16.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(size::sm())
                .text_color(p.extra_muted)
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "copy-turn-{}-{start}",
                            session_ref.agent_id
                        )))
                        .p(px(2.))
                        .rounded(px(4.))
                        .hover(|s| s.bg(p.surface1))
                        .child(ui::icon("copy", 12., p.extra_muted))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui_kit::gpui::ClipboardItem::new_string(
                                text.clone(),
                            ));
                        }),
                )
                .child(ago)
                .when(!duration.is_empty(), |d| d.child("·").child(duration))
                .into_any_element()
        }
        Row::Permission(i) => {
            let Some(permission) = timeline.permissions.get(*i) else {
                return div().into_any_element();
            };
            permission_card(
                &permission.request,
                &session_ref.host,
                &session_ref.agent_id,
                &permission.id,
                core,
                cwd,
            )
        }
        Row::Item { index, gap } => {
            let Some(item) = timeline.items().nth(*index) else {
                return div().into_any_element();
            };
            let pending = matches!(&item.kind, ItemKind::User { client_message_id: Some(id), .. } if timeline.pending_sends.contains(id));
            let images = view.read(cx).images.clone();
            let body = match &item.kind {
                ItemKind::User {
                    text,
                    images: wires,
                    ..
                } => div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            // Narrower than its text when the view is: GPUI
                            // text will not wrap below its unwrapped width
                            // unless allowed to shrink.
                            .min_w(px(0.))
                            .max_w(px(640.))
                            .px(px(16.))
                            .py(px(12.))
                            .bg(p.surface3)
                            .rounded(px(16.))
                            .rounded_tr(px(2.))
                            .text_size(size::content())
                            .text_color(p.foreground)
                            .when(pending, |d| d.opacity(0.6))
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .children(
                                images
                                    .get(&item.id)
                                    .into_iter()
                                    .flat_map(|entry| entry.decoded.iter())
                                    .map(|image| {
                                        gpui_kit::gpui::img(image.clone())
                                            .w(px(180.))
                                            .h(px(140.))
                                            .into_any_element()
                                    }),
                            )
                            .when(text.is_empty() && !wires.is_empty(), |d| {
                                d.child("Image attachment")
                            })
                            .child(text.clone())
                            // When it was sent, tucked into the corner.
                            .child(
                                div()
                                    .mt(px(-4.))
                                    .flex()
                                    .justify_end()
                                    .text_size(size::sm())
                                    .text_color(p.muted)
                                    .child(if pending {
                                        "Sending…".to_string()
                                    } else {
                                        crate::app::store::time_ago(&item.timestamp)
                                    }),
                            ),
                    )
                    .into_any_element(),
                ItemKind::Assistant { text, .. } => div()
                    .w_full()
                    .text_size(size::content())
                    .text_color(p.foreground)
                    .child(
                        TextView::markdown(SharedString::from(item.id.clone()), text.clone())
                            .selectable(true),
                    )
                    .into_any_element(),
                ItemKind::Thought { text, loading } => {
                    let open = expanded.borrow().contains(&item.id);
                    let header = tool_header(
                        &item.id,
                        "brain",
                        "Thinking",
                        None,
                        *loading,
                        false,
                        !text.trim().is_empty(),
                        open,
                        expanded,
                        view,
                    );
                    let mut col = div().flex().flex_col().child(header);
                    if open {
                        col = col.child(
                            div()
                                .ml(px(22.))
                                .mt(px(4.))
                                .text_size(size::sm())
                                .text_color(p.muted)
                                .child(text.clone()),
                        );
                    }
                    col.into_any_element()
                }
                ItemKind::Tool(tool) if tool.detail_type() == "plan" => plan_card(tool, &item.id),
                ItemKind::Tool(tool) => tool_row(tool, &item.id, cwd, expanded, view),
                ItemKind::Todo {
                    items, activity, ..
                } => {
                    let (icon_name, text) = match activity {
                        TaskActivity::Created(n) => ("square-check", format!("Created {n} tasks")),
                        TaskActivity::Added(t) => ("plus", format!("Added {t}")),
                        TaskActivity::Started(t) => ("circle-dot", format!("Started {t}")),
                        TaskActivity::Completed(t) => ("check", format!("Completed {t}")),
                    };
                    let open = expanded.borrow().contains(&item.id);
                    let header = tool_header(
                        &item.id, icon_name, &text, None, false, false, true, open, expanded, view,
                    );
                    let mut col = div().flex().flex_col().child(header);
                    if open {
                        col = col.child(
                            div()
                                .ml(px(22.))
                                .mt(px(4.))
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .children(items.iter().map(|task| {
                                    let done = task.status == "completed";
                                    let active = task.status == "in_progress";
                                    div()
                                        .flex()
                                        .gap(px(6.))
                                        .text_size(size::sm())
                                        .text_color(if done { p.extra_muted } else { p.foreground })
                                        .child(ui::icon(
                                            if done {
                                                "circle-check"
                                            } else if active {
                                                "circle-dot"
                                            } else {
                                                "circle"
                                            },
                                            12.,
                                            if active { p.dot_running } else { p.muted },
                                        ))
                                        .child(task.text.clone())
                                })),
                        );
                    }
                    col.into_any_element()
                }
                ItemKind::Notification { level, message } => {
                    let (icon_name, color) = match level.as_str() {
                        "error" => ("circle-alert", p.status_danger),
                        "warning" => ("triangle-alert", p.status_warning),
                        _ => ("info", p.muted),
                    };
                    div()
                        .flex()
                        .gap(px(8.))
                        .items_start()
                        .text_size(size::sm())
                        .text_color(color)
                        .child(div().flex_shrink_0().child(ui::icon(icon_name, 16., color)))
                        // Allowed to shrink, so it wraps to the view rather
                        // than running past it.
                        .child(div().flex_1().min_w(px(0.)).child(message.clone()))
                        .into_any_element()
                }
                ItemKind::Compaction {
                    loading,
                    trigger,
                    pre_tokens,
                } => {
                    let label = if *loading {
                        "Compacting...".to_string()
                    } else if let Some(tokens) = pre_tokens {
                        format!("Context compacted ({}K tokens)", tokens / 1000)
                    } else if trigger.as_deref() == Some("auto") {
                        "Context automatically compacted".into()
                    } else if trigger.as_deref() == Some("manual") {
                        "Context manually compacted".into()
                    } else {
                        "Context compacted".into()
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(div().flex_1().h(px(1.)).bg(p.border))
                        .child(div().text_size(size::sm()).text_color(p.muted).child(label))
                        .child(div().flex_1().h(px(1.)).bg(p.border))
                        .into_any_element()
                }
            };
            div().pb(px(*gap)).child(body).into_any_element()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn tool_header(
    id: &str,
    icon_name: &'static str,
    name: &str,
    summary: Option<String>,
    running: bool,
    failed: bool,
    expandable: bool,
    open: bool,
    expanded: &Rc<std::cell::RefCell<HashSet<String>>>,
    view: &Entity<Conversation>,
) -> AnyElement {
    let p = palette();
    let group: SharedString = format!("tool-{id}").into();
    let leading = if failed {
        ui::icon("triangle-alert", 12., p.destructive).into_any_element()
    } else {
        ui::icon(icon_name, 12., p.muted).into_any_element()
    };
    let label = div()
        .flex()
        .items_center()
        .gap(px(6.))
        .min_w(px(0.))
        .flex_1()
        .child(
            div()
                .flex_shrink_0()
                .text_color(p.muted)
                .child(name.to_string()),
        )
        .when_some(summary, |d, summary| {
            d.child(
                div()
                    .min_w(px(0.))
                    .truncate()
                    .text_color(with_alpha(p.muted, 0.8))
                    .child(summary.lines().next().unwrap_or_default().to_string()),
            )
        });
    let label: AnyElement = if running {
        ui::with_running_ring(label).into_any_element()
    } else {
        label.into_any_element()
    };
    let expanded = expanded.clone();
    let view = view.clone();
    let id_owned = id.to_string();
    div()
        .id(SharedString::from(format!("tool-row-{id}")))
        .group(group.clone())
        .flex()
        .items_center()
        .gap(px(0.))
        .text_size(size::sm())
        .rounded(px(6.))
        .when(expandable, |d| {
            d.cursor_pointer().hover(|s| s.bg(p.surface1))
        })
        .child(
            div()
                .w(px(22.))
                .h(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .relative()
                .child(
                    div()
                        .id(SharedString::from(format!("{group}-lead")))
                        .when(expandable, |d| {
                            d.group_hover(group.clone(), |s| s.invisible())
                        })
                        .child(leading),
                )
                .when(expandable, |d| {
                    d.child(
                        div()
                            .id(SharedString::from(format!("{group}-chevron")))
                            .absolute()
                            .invisible()
                            .when(open, |d| d.visible())
                            .group_hover(group.clone(), |s| s.visible())
                            .child(ui::icon(
                                if open {
                                    "chevron-down"
                                } else {
                                    "chevron-right"
                                },
                                12.,
                                p.muted,
                            )),
                    )
                }),
        )
        .child(label)
        .when(expandable, |d| {
            d.on_click(move |_: &ClickEvent, _, cx| {
                {
                    let mut set = expanded.borrow_mut();
                    if !set.remove(&id_owned) {
                        set.insert(id_owned.clone());
                    }
                }
                view.update(cx, |this, cx| {
                    this.revision = u64::MAX;
                    this.refresh(cx);
                });
            })
        })
        .into_any_element()
}

fn tool_row(
    tool: &ToolCall,
    id: &str,
    cwd: &str,
    expanded: &Rc<std::cell::RefCell<HashSet<String>>>,
    view: &Entity<Conversation>,
) -> AnyElement {
    let p = palette();
    let ToolDisplay {
        name,
        summary,
        icon,
    } = display::tool_display(tool, cwd);
    let expandable = display::has_details(tool);
    let open = expandable && expanded.borrow().contains(id);
    let header = tool_header(
        id,
        ui::tool_icon_name(icon),
        &name,
        summary,
        tool.running(),
        tool.failed(),
        expandable,
        open,
        expanded,
        view,
    );
    let mut col = div().flex().flex_col().child(header);
    if open {
        col = col.child(detail_block(tool, id));
    }
    let _ = p;
    col.into_any_element()
}

fn detail_block(tool: &ToolCall, id: &str) -> AnyElement {
    let p = palette();
    let sections = display::detail_sections(tool);
    let is_edit = tool.detail_type() == "edit";
    let mut block = div()
        .id(SharedString::from(format!("tool-detail-{id}")))
        .mt(px(4.))
        .ml(px(22.))
        .max_h(px(400.))
        .overflow_y_scroll()
        .rounded(px(6.))
        .border_1()
        .border_color(p.border)
        .bg(p.surface1)
        .flex()
        .flex_col();
    if sections.is_empty() {
        return block
            .p(px(12.))
            .text_size(size::sm())
            .italic()
            .text_color(p.muted)
            .child("No additional details available")
            .into_any_element();
    }
    for (title, body) in sections {
        let is_error = title == Some("Error");
        let mut section = div().flex().flex_col().p(px(12.)).gap(px(6.));
        if let Some(title) = title {
            section = section.child(
                div()
                    .text_size(size::sm())
                    .text_color(if is_error { p.destructive } else { p.muted })
                    .child(title),
            );
        }
        let code = div()
            .font_family(crate::theme::mono_font())
            .text_size(size::code())
            .line_height(px(18.))
            .text_color(p.foreground)
            .flex()
            .flex_col();
        let code = if is_edit && !is_error {
            code.children(body.lines().map(|line| {
                let (bg, fg) = if line.starts_with('+') {
                    (Some(gpui_kit::gpui::rgba(0x2ea04326)), p.status_success)
                } else if line.starts_with('-') {
                    (Some(gpui_kit::gpui::rgba(0xf851491a)), p.status_danger)
                } else if line.starts_with("@@") {
                    (None, p.muted)
                } else {
                    (None, p.foreground)
                };
                div()
                    .px(px(4.))
                    .when_some(bg, |d, bg| d.bg(bg))
                    .text_color(fg)
                    .child(if line.is_empty() {
                        " ".to_string()
                    } else {
                        line.to_string()
                    })
            }))
        } else {
            code.child(body)
        };
        section = section.child(code);
        block = block.child(section);
    }
    block.into_any_element()
}

fn plan_card(tool: &ToolCall, id: &str) -> AnyElement {
    let p = palette();
    let title = match tool.metadata.get("approved").and_then(Value::as_bool) {
        Some(true) => "Approved plan",
        Some(false) => "Rejected plan",
        None if tool.status == "canceled" => "Canceled plan",
        None => "Plan",
    };
    let text = tool.detail["text"].as_str().unwrap_or_default().to_string();
    div()
        .rounded(px(8.))
        .border_1()
        .border_color(p.border_accent)
        .bg(p.surface1)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_color(p.foreground)
                .child(ui::icon("brain", 14., p.muted))
                .child(title),
        )
        .child(div().text_size(size::content()).child(
            TextView::markdown(SharedString::from(format!("plan-{id}")), text).selectable(true),
        ))
        .into_any_element()
}

fn permission_card(
    request: &Value,
    host: &str,
    agent_id: &str,
    permission_id: &str,
    core: &Core,
    cwd: &str,
) -> AnyElement {
    let p = palette();
    let kind = request["kind"].as_str().unwrap_or("tool");
    let title = request["title"]
        .as_str()
        .or(request["name"].as_str())
        .unwrap_or("Permission Required")
        .to_string();
    let description = request["description"].as_str().map(str::to_string);

    let detail_tool = ToolCall {
        identity: permission_id.to_string(),
        call_id: permission_id.to_string(),
        name: request["name"].as_str().unwrap_or_default().to_string(),
        status: "completed".into(),
        error: None,
        detail: if request["detail"].is_object() {
            request["detail"].clone()
        } else {
            json!({"type": "unknown", "input": request["input"].clone(), "output": null})
        },
        metadata: Default::default(),
    };
    let plan_text = request["metadata"]["planText"]
        .as_str()
        .or(request["input"]["plan"].as_str())
        .map(str::to_string);

    let mut buttons: Vec<(String, String, bool, String)> = Vec::new(); // (id, label, allow, variant)
    if let Some(actions) = request["actions"].as_array().filter(|a| !a.is_empty()) {
        for action in actions {
            buttons.push((
                action["id"].as_str().unwrap_or_default().to_string(),
                action["label"].as_str().unwrap_or("OK").to_string(),
                action["behavior"] == "allow",
                action["variant"]
                    .as_str()
                    .unwrap_or("secondary")
                    .to_string(),
            ));
        }
    } else {
        buttons.push(("reject".into(), "Deny".into(), false, "secondary".into()));
        buttons.push((
            "accept".into(),
            if kind == "plan" {
                "Implement".into()
            } else {
                "Accept".into()
            },
            true,
            "primary".into(),
        ));
    }

    let mut card = div()
        .mb(px(16.))
        .rounded(px(8.))
        .border_1()
        .border_color(p.status_warning)
        .bg(p.surface1)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .flex()
                .items_start()
                .gap(px(8.))
                .text_color(p.foreground)
                // The icon sits on the first line of a title that wraps.
                .child(div().h(px(20.)).flex().items_center().child(ui::icon(
                    "circle-alert",
                    14.,
                    p.dot_warning,
                )))
                .child(div().flex_1().min_w(px(0.)).child(title)),
        )
        .when_some(description, |d, text| {
            d.child(div().text_size(size::sm()).text_color(p.muted).child(text))
        });

    if let Some(plan) = plan_text.filter(|_| kind == "plan") {
        card = card.child(div().text_size(size::content()).child(TextView::markdown(
            SharedString::from(format!("perm-plan-{permission_id}")),
            plan,
        )));
    } else if display::has_details(&detail_tool) {
        let summary = display::tool_display(&detail_tool, cwd).summary;
        if let Some(summary) = summary {
            card = card.child(
                div()
                    .font_family(crate::theme::mono_font())
                    .text_size(size::code())
                    .text_color(p.foreground)
                    .child(summary),
            );
        }
        card = card.child(detail_block(&detail_tool, &format!("perm-{permission_id}")));
    }

    let mut row = div().flex().gap(px(8.)).justify_end();
    for (action_id, label, allow, variant) in buttons {
        let core = core.clone();
        let host = host.to_string();
        let agent = agent_id.to_string();
        let permission = permission_id.to_string();
        let mut button = Button::new(SharedString::from(format!(
            "perm-{permission_id}-{action_id}"
        )))
        .label(label)
        .small();
        button = match variant.as_str() {
            "primary" => button.primary(),
            "danger" => button.danger(),
            _ => button,
        };
        row = row.child(button.on_click(move |_, _, cx| {
            let response = if allow {
                json!({"behavior": "allow", "selectedActionId": action_id})
            } else {
                json!({"behavior": "deny", "selectedActionId": action_id, "message": "Denied by user"})
            };
            core.store.update(cx, |store, cx| {
                store.answer_permission(&host, &agent, &permission, response, cx)
            });
        }));
    }
    card.child(
        div()
            .text_size(size::sm())
            .text_color(p.muted)
            .child("How would you like to proceed?"),
    )
    .child(row)
    .into_any_element()
}
