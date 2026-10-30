//! One agent's conversation: the stream of rows, permission cards, the
//! working indicator, and the composer.

use fleet::HostId;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use super::prompt_index::{PromptNavigation, magnification};
use fleet::Bucket;
use fleet::display::{self, AgentLink, ToolDisplay};
use fleet::prompt_index::{self, PromptMark};
use fleet::timeline::{ItemKind, TaskActivity, ToolCall, Turn};
use gpui_kit::base::{
    Disableable as _, ElementExt as _, SelectableText, TextSelection, TextSelectionGroupId,
};
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::text::{SelectionFormat, TextView, TextViewState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, DismissEvent, Entity, EventEmitter,
    Focusable, FollowMode, InteractiveElement, IntoElement, ListAlignment, ListOffset, ListState,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, anchored, deferred, div,
    list, px, relative, svg,
};
use gpui_kit::prelude::FluentBuilder;
use serde_json::{Value, json};

use super::attachments;
use super::composer::Composer;
use super::question_form::QuestionForm;
use crate::app::images::Shown;
use crate::app::store::{FleetStore, now_millis, parse_iso};
use crate::app::{AgentSession, Core};
use crate::layouts::columns::edges;
use crate::theme::{palette, size, with_alpha};
use crate::ui;
use fleet::images::ImageFile;
use fleet::prompt::ImageAttachment;
use gpui_kit::gpui::{ImageSource, SharedUri};

#[derive(Clone)]
enum Row {
    Item {
        index: usize,
        gap: f32,
        /// The first item of the item's turn, whose text a selection
        /// started in it stays within.
        turn: usize,
        /// For the last item of a step, the item the step began with, whose
        /// time it shows.
        stamp: Option<usize>,
    },
    /// After a finished turn: items `start..=end`.
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

/// The images an item has its daemon keep, read when its row is drawn: a
/// prompt's kept files, or the files a reply's Markdown images name.
struct RowImages {
    files: Vec<ImageFile>,
    /// For a reply, each Markdown image's destination and its file in `files`.
    sources: Vec<(String, usize)>,
}

/// Each item's [`RowImages`] by item id, with the text length it was found
/// in, so a reply still streaming is looked at again.
type RemoteImages = Rc<std::cell::RefCell<HashMap<String, (usize, Rc<RowImages>)>>>;

pub struct Conversation {
    core: Core,
    pub host: HostId,
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
    /// Whether the list was following its end when last scrolled, to
    /// redraw the arrow back to it when that changes.
    following: bool,
    rows: Rc<Vec<Row>>,
    fingerprints: Vec<u64>,
    revision: u64,
    expanded: Rc<std::cell::RefCell<HashSet<String>>>,
    ticking: bool,
    agent_marker: Option<String>,
    navigation: PromptNavigation,
    images: Rc<HashMap<String, Rc<CachedImages>>>,
    /// Images its daemon keeps, for the items drawn so far.
    remote_images: RemoteImages,
    /// The store's image revision last seen, to redraw rows when one lands.
    images_revision: u64,
    presentation: Option<(bool, Option<String>, u64)>,
    /// Where "Archive finished" is.
    archive_finished: ArchiveFinished,
    /// A timeline item to scroll to once it is in the rows.
    reveal: Option<String>,
    /// Fills its width instead of keeping to a column for reading, as in
    /// a Lanes card, whose lane sets how wide it is.
    wide: bool,
    /// Each assistant reply's text, held here rather than by its view so a
    /// selection can be copied as Markdown as well as plain text.
    replies: Replies,
    /// What a right-click on selected text opens: where, and the menu.
    copy_menu: Option<(Point<Pixels>, Entity<PopupMenu>, Subscription)>,
    /// A form for each pending question, by permission id.
    question_forms: QuestionForms,
}

/// Pending questions' forms by permission id.
type QuestionForms = Rc<std::cell::RefCell<HashMap<String, Entity<QuestionForm>>>>;

/// Assistant replies' text states by timeline item id, with the text each
/// was last given.
type Replies = Rc<std::cell::RefCell<HashMap<String, (Entity<TextViewState>, String)>>>;

/// The text state for the reply `id`, made on first sight and kept to its
/// latest `text` as it streams in.
fn reply_state(replies: &Replies, id: &str, text: &str, cx: &mut App) -> Entity<TextViewState> {
    let mut replies = replies.borrow_mut();
    if let Some((state, shown)) = replies.get_mut(id) {
        if shown != text {
            *shown = text.to_string();
            state.update(cx, |state, cx| state.set_text(text, cx));
        }
        return state.clone();
    }
    let state = cx.new(|cx| TextViewState::markdown(text, cx));
    replies.insert(id.to_string(), (state.clone(), text.to_string()));
    state
}

/// Someone the conversation asks its owner to show.
pub enum ConversationEvent {
    /// A Paseo subagent, opened from the track: a full agent of its own.
    OpenAgent { host: HostId, agent: String },
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
        host: &HostId,
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
        host: &HostId,
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
                host.clone(),
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
        host: &HostId,
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
            let images = store.read(cx).images.revision;
            if images != this.images_revision {
                this.images_revision = images;
                this.remeasure_image_rows(cx);
            }
        })
        .detach();
        let list = ListState::new(0, ListAlignment::Bottom, px(800.));
        list.set_follow_mode(FollowMode::Tail);
        let mut this = Self {
            core,
            host: host.clone(),
            agent_id: agent_id.to_string(),
            session,
            composer,
            subagent,
            viewing: None,
            track_open: false,
            track: Rc::new(Vec::new()),
            track_revision: (u64::MAX, u64::MAX),
            list,
            following: true,
            rows: Rc::new(Vec::new()),
            fingerprints: Vec::new(),
            revision: u64::MAX,
            expanded: Rc::new(std::cell::RefCell::new(HashSet::new())),
            ticking: false,
            agent_marker: None,
            navigation: PromptNavigation::default(),
            images: Rc::new(HashMap::new()),
            remote_images: Rc::default(),
            images_revision: 0,
            presentation: None,
            wide: false,
            archive_finished: ArchiveFinished::Idle,
            reveal: None,
            replies: Rc::default(),
            copy_menu: None,
            question_forms: Rc::default(),
        };
        let weak = cx.entity().downgrade();
        this.list.set_scroll_handler(move |event, _, cx| {
            let _ = weak.update(cx, |this, cx| {
                let row = (!event.is_following_tail).then_some(event.visible_range.start);
                let changed = event.is_following_tail != this.following;
                this.following = event.is_following_tail;
                if this.navigation.set_reading_row(row) || changed {
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

    /// Measures again the replies showing images their daemon keeps, once
    /// the store has read one: an image is not its placeholder's size. A
    /// prompt's thumbnails are, so its row only needs drawing again.
    fn remeasure_image_rows(&mut self, cx: &mut Context<Self>) {
        let remote = self.remote_images.borrow();
        if remote.values().all(|(_, images)| images.files.is_empty()) {
            return;
        }
        if remote
            .values()
            .any(|(_, images)| !images.sources.is_empty())
        {
            let ids: Vec<&str> = (self.session.read(cx).timeline.items())
                .map(|item| item.id.as_str())
                .collect();
            for (row, entry) in self.rows.iter().enumerate() {
                if let Row::Item { index, .. } = entry
                    && (ids.get(*index).and_then(|id| remote.get(*id)))
                        .is_some_and(|(_, images)| !images.sources.is_empty())
                {
                    self.list.splice(row..row + 1, 1);
                }
            }
        }
        drop(remote);
        cx.notify();
    }

    /// A round arrow over the bottom of the conversation that scrolls to its
    /// end and follows it again.
    fn jump_to_latest(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        div()
            .absolute()
            .bottom(px(12.))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .id(SharedString::from(format!("jump-latest-{}", self.agent_id)))
                    .size(px(28.))
                    .rounded_full()
                    .border_1()
                    // Over the fade and whatever text is under it.
                    .border_color(p.muted)
                    .bg(p.surface2)
                    .shadow_md()
                    .hover(|s| s.border_color(p.foreground))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(ui::icon("arrow-down", 14., p.foreground))
                    .tooltip(|window, cx| Tooltip::new("Jump to latest").build(window, cx))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.list.set_follow_mode(FollowMode::Tail);
                        this.following = true;
                        this.navigation.set_reading_row(None);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        if let Some(composer) = &self.composer {
            composer.update(cx, |composer, cx| composer.focus(window, cx));
        }
    }

    pub fn draft(&self, cx: &App) -> Option<fleet::prompt::Prompt> {
        self.composer
            .as_ref()
            .map(|composer| composer.read(cx).draft(cx))
    }

    pub fn preparing_attachment(&self, cx: &App) -> bool {
        self.composer
            .as_ref()
            .is_some_and(|composer| composer.read(cx).preparing_attachment())
    }

    pub fn restore_draft(&self, draft: fleet::prompt::Prompt, window: &mut Window, cx: &mut App) {
        if let Some(composer) = &self.composer {
            composer.update(cx, |composer, cx| composer.restore_draft(draft, window, cx));
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
        // Each finished turn ends in a footer, including the turns an agent
        // runs on its own between messages.
        let finished = fleet::timeline::finished_turns(&items, timeline.is_running());
        // Each step shows when it began at the right of its last row, but
        // for a turn's last, whose footer shows the time.
        let stamps: HashMap<usize, usize> = fleet::timeline::steps(&items)
            .into_iter()
            .filter(|(_, end)| !finished.iter().any(|(_, last)| last == end))
            .map(|(start, end)| (end, start))
            .collect();
        let mut turns = finished.into_iter().peekable();
        let turn_starts = fleet::timeline::turn_starts(&items);
        for (index, item) in items.iter().enumerate() {
            if !is_hidden(&item.kind) {
                let next_toolish = items.get(index + 1).is_some_and(|next| toolish(&next.kind));
                // A turn's footer sits in its last item's gap.
                let ends_turn = turns.peek().is_some_and(|(_, end)| *end == index);
                let gap = if toolish(&item.kind) && next_toolish && !ends_turn {
                    4.
                } else {
                    16.
                };
                let turn = turn_starts[index];
                let stamp = stamps.get(&index).copied();
                rows.push(Row::Item {
                    index,
                    gap,
                    turn,
                    stamp,
                });
                let print = fingerprint(item, self.expanded.borrow().contains(&item.id));
                prints.push(print ^ stamp.map_or(0, |start| hash_str(&format!("stamp-{start}"))));
            }
            if let Some((start, end)) = turns.next_if(|(_, end)| *end == index) {
                rows.push(Row::TurnFooter { start, end });
                prints.push(hash_str(&format!("footer-{start}-{end}")));
            }
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
        let ids: HashSet<&str> = items.iter().map(|item| item.id.as_str()).collect();
        (self.replies.borrow_mut()).retain(|id, _| ids.contains(id.as_str()));
        (self.remote_images.borrow_mut()).retain(|id, _| ids.contains(id.as_str()));
        let cache = Rc::make_mut(&mut self.images);
        cache.retain(|id, _| ids.contains(id.as_str()));
        for (row, entry) in rows.iter().enumerate() {
            if let Row::Item { index, .. } = entry
                && let ItemKind::User {
                    text,
                    images,
                    image_files,
                    seq,
                    ..
                } = &items[*index].kind
            {
                if let Some(seq) = seq {
                    self.navigation.rows.insert(*seq, row);
                    self.navigation.loaded.push(PromptMark {
                        seq: *seq,
                        preview: prompt_index::preview(
                            text,
                            !images.is_empty() || !image_files.is_empty(),
                        ),
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
    /// On a right-click over selected text, opens a menu to copy it, as
    /// shown or as the Markdown it was written in; over a `link` in a reply,
    /// to open it or copy its address. The text is taken now, so the menu
    /// copies what was selected whatever happens to the selection.
    fn open_copy_menu(
        &mut self,
        position: Point<Pixels>,
        link: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let plain = (TextSelection::has_selection(window, cx))
            .then(|| TextSelection::selected_text(window, cx))
            .filter(|text| !text.trim().is_empty());
        if plain.is_none() && link.is_none() {
            return;
        }
        let selected = plain.map(|plain| (plain, self.selected_markdown(window, cx)));
        let store = self.core.store.clone();
        let menu = PopupMenu::build(window, cx, move |mut menu, _, _| {
            let copy = |label: &'static str, text: String, done: &'static str| {
                let store = store.clone();
                PopupMenuItem::new(label).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(gpui_kit::gpui::ClipboardItem::new_string(text.clone()));
                    store.update(cx, |store, cx| store.toast(done.to_string(), cx));
                })
            };
            if let Some(url) = &link {
                let open = url.clone();
                menu = menu
                    .item(
                        PopupMenuItem::new("Open link")
                            .on_click(move |_, _, cx| cx.open_url(&open)),
                    )
                    .item(copy(
                        "Copy link address",
                        url.to_string(),
                        "Copied link address",
                    ));
            }
            if let Some((plain, markdown)) = &selected {
                if link.is_some() {
                    menu = menu.separator();
                }
                menu = menu.item(copy("Copy", plain.clone(), "Copied")).item(copy(
                    "Copy as Markdown",
                    markdown.clone(),
                    "Copied as Markdown",
                ));
            }
            menu
        });
        let dismiss = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
            this.copy_menu = None;
            cx.notify();
        });
        self.copy_menu = Some((position, menu, dismiss));
        cx.stop_propagation();
        cx.notify();
    }

    /// The selection with each reply in it written as its Markdown source:
    /// the replies copy Markdown just for the moment it is read.
    fn selected_markdown(&self, window: &mut Window, cx: &mut Context<Self>) -> String {
        let states: Vec<Entity<TextViewState>> = (self.replies.borrow().values())
            .map(|(state, _)| state.clone())
            .collect();
        for state in &states {
            state.update(cx, |state, cx| {
                state.set_selection_format(SelectionFormat::Source, cx)
            });
        }
        let text = TextSelection::selected_text(window, cx);
        for state in &states {
            state.update(cx, |state, cx| {
                state.set_selection_format(SelectionFormat::Plain, cx)
            });
        }
        text
    }

    /// Keeps a form for each pending question, made here because its text
    /// boxes need the window, and drops those the daemon resolved. A
    /// request whose questions don't parse gets none and shows as a
    /// permission.
    fn sync_question_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending: Vec<(String, Value)> = (self.session.read(cx).timeline.permissions.iter())
            .filter(|permission| fleet::questions::is_question(&permission.request))
            .map(|permission| (permission.id.clone(), permission.request.clone()))
            .collect();
        let mut forms = self.question_forms.borrow_mut();
        forms.retain(|id, _| pending.iter().any(|(pending, _)| pending == id));
        for (id, request) in pending {
            if forms.contains_key(&id) {
                continue;
            }
            let Some(questions) = fleet::questions::parse(&request["input"]) else {
                continue;
            };
            let (core, host, agent) = (self.core.clone(), self.host.clone(), self.agent_id.clone());
            let form = cx.new(|cx| {
                QuestionForm::new(core, &host, &agent, &id, request, questions, window, cx)
            });
            forms.insert(id, form);
        }
    }

    fn copy_menu_layer(&self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let (position, menu, _) = self.copy_menu.as_ref()?;
        let focus = menu.focus_handle(cx);
        if !focus.contains_focused(window, cx) {
            focus.focus(window, cx);
        }
        Some(
            deferred(
                anchored()
                    .position(*position)
                    .snap_to_window_with_margin(px(8.))
                    .child(menu.clone()),
            )
            .with_priority(gpui_kit::base::POPUP_PRIORITY)
            .into_any_element(),
        )
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
        // Centred in the space it has, up to 400px, never past its edges:
        // a short conversation, such as an open Lanes card's, scrolls it.
        div()
            .absolute()
            .left(px(0.))
            .top(px(0.))
            .h_full()
            .py(px(8.))
            .w(px(size::PROMPT_INDEX_WIDTH))
            .flex()
            .items_center()
            .child(
                div()
                    .id("prompt-index")
                    .w(px(size::PROMPT_INDEX_WIDTH))
                    .max_h(gpui_kit::gpui::relative(1.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("prompt-index-ticks")
                            .w(px(size::PROMPT_INDEX_WIDTH))
                            .max_h(px(400.))
                            .min_h(px(0.))
                            .flex_shrink(1.)
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(prompts.iter().enumerate().map(|(i, mark)| {
                                let scale =
                                    hovered.map(|h| magnification(i.abs_diff(h))).unwrap_or(0.);
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
                                            store.load_prompt(
                                                &this.host,
                                                &this.agent_id,
                                                target.seq,
                                                cx,
                                            )
                                        });
                                        this.scroll_pending(cx);
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .w(px(width
                                                + scale * (size::PROMPT_TICK_MAX_WIDTH - width)))
                                            .h(px(2. + scale * 2.))
                                            .rounded(px(2.))
                                            .bg(if active == Some(i) || hovered == Some(i) {
                                                p.foreground
                                            } else {
                                                p.extra_muted
                                            }),
                                    )
                            })),
                    ),
            )
            .into_any_element()
    }
}

fn is_hidden(kind: &ItemKind) -> bool {
    fleet::display::hidden_in_stream(kind)
}

/// The images `row`'s item has its daemon keep, found the first time the
/// row is drawn and again when the item changes. `None` when it has none.
fn row_images(
    row: &Row,
    session: &Entity<AgentSession>,
    cwd: &str,
    memo: &RemoteImages,
    cx: &App,
) -> Option<Rc<RowImages>> {
    let Row::Item { index, .. } = row else {
        return None;
    };
    let item = session.read(cx).timeline.items().nth(*index)?;
    let stamp = match &item.kind {
        ItemKind::User {
            images,
            image_files,
            ..
        } => image_files.len() * 2 + usize::from(!images.is_empty()),
        ItemKind::Assistant { text, .. } => text.len(),
        _ => return None,
    };
    if let Some((seen, found)) = memo.borrow().get(&item.id)
        && *seen == stamp
    {
        return (!found.files.is_empty()).then(|| found.clone());
    }
    let found = Rc::new(match &item.kind {
        // Sent from here, the bytes are already in hand.
        ItemKind::User {
            images,
            image_files,
            ..
        } if images.is_empty() => RowImages {
            files: image_files.clone(),
            sources: Vec::new(),
        },
        ItemKind::Assistant { text, .. } => {
            let cwd = Some(cwd).filter(|cwd| !cwd.is_empty());
            let (files, sources) = fleet::images::markdown_files(text, cwd);
            RowImages { files, sources }
        }
        _ => RowImages {
            files: Vec::new(),
            sources: Vec::new(),
        },
    });
    memo.borrow_mut()
        .insert(item.id.clone(), (stamp, found.clone()));
    (!found.files.is_empty()).then_some(found)
}

/// A prompt's images as thumbnails: those sent from here, else those its
/// daemon keeps, as they arrive. A click opens one full size.
fn prompt_images(
    item_id: &str,
    decoded: Option<&[Arc<gpui_kit::gpui::Image>]>,
    remote: Option<&(Rc<RowImages>, Vec<Shown>)>,
    host: &HostId,
    core: &Core,
) -> Vec<AnyElement> {
    if let Some(decoded) = decoded.filter(|decoded| !decoded.is_empty()) {
        return (decoded.iter().enumerate())
            .map(|(i, image)| {
                let copy = image.clone();
                thumbnail(
                    format!("prompt-image-{item_id}-{i}"),
                    Shown::Ready(image.clone()),
                    move |cx| {
                        crate::app::images::open_copy(
                            &format!("prompt-image-{:016x}", copy.id),
                            copy.format.mime_type(),
                            copy.bytes.clone(),
                            cx,
                        )
                    },
                )
            })
            .collect();
    }
    let Some((images, shown)) = remote else {
        return Vec::new();
    };
    (images.files.iter().zip(shown).enumerate())
        .map(|(i, (file, shown))| {
            let (core, host, file) = (core.clone(), host.clone(), file.clone());
            thumbnail(
                format!("prompt-image-{item_id}-{i}"),
                shown.clone(),
                move |cx| open_kept_image(&core.store, &host, &file, cx),
            )
        })
        .collect()
}

/// One prompt image, the size of the composer's own once sent, or a box
/// that size while it loads or when its host cannot send it.
fn thumbnail(id: String, shown: Shown, open: impl Fn(&mut App) + 'static) -> AnyElement {
    let p = palette();
    let (width, height) = (px(180.), px(140.));
    let frame = div()
        .id(SharedString::from(id))
        .flex_shrink_0()
        .w(width)
        .h(height);
    let placeholder = |frame: gpui_kit::gpui::Stateful<gpui_kit::gpui::Div>, label| {
        frame
            .rounded(px(6.))
            .border_1()
            .border_color(p.border)
            .flex()
            .items_center()
            .justify_center()
            .text_size(size::sm())
            .text_color(p.muted)
            .child(label)
            .into_any_element()
    };
    match shown {
        Shown::Ready(image) => frame
            .cursor_pointer()
            .tooltip(|window, cx| Tooltip::new("Open image").build(window, cx))
            .on_click(move |_, _, cx| open(cx))
            .child(fitted(gpui_kit::gpui::img(image), width, height))
            .into_any_element(),
        Shown::Loading => placeholder(frame, "Loading image…"),
        Shown::Unavailable => placeholder(
            frame.tooltip(|window, cx| {
                Tooltip::new("Its host could not send this image").build(window, cx)
            }),
            "Image unavailable",
        ),
    }
}

/// An image drawn whole inside a `width` by `height` box. The box's own
/// aspect ratio is given: GPUI otherwise takes the image's, and Taffy's
/// final layout then makes the box at least as tall as the width over it,
/// whatever its height, so a tall image drew down over the rows below.
pub(crate) fn fitted(
    image: gpui_kit::gpui::Img,
    width: gpui_kit::gpui::Pixels,
    height: gpui_kit::gpui::Pixels,
) -> gpui_kit::gpui::Img {
    image.w(width).h(height).aspect_ratio(width / height)
}

/// Opens an image a daemon keeps at full size. It is read again: what the
/// cache holds may be scaled down.
fn open_kept_image(store: &Entity<FleetStore>, host: &HostId, file: &ImageFile, cx: &mut App) {
    let read = store.read(cx).read_image(host, file, cx);
    let name = file.file_name().to_string();
    cx.spawn(async move |cx| {
        if let Ok(image) = read.await {
            cx.update(|cx| crate::app::images::open_read(&name, &image, cx));
        }
    })
    .detach();
}

/// What a reply's Markdown images show: a file its daemon keeps once read,
/// a placeholder while it loads or when it cannot be, and any other image
/// as the renderer would show it.
fn reply_image_source(
    images: &RowImages,
    shown: &[Shown],
) -> impl Fn(&SharedUri) -> ImageSource + Send + Sync + 'static {
    let sources: HashMap<String, Arc<gpui_kit::gpui::Image>> = (images.sources.iter())
        .filter_map(|(source, file)| {
            let image = match shown.get(*file)? {
                Shown::Ready(image) => image.clone(),
                Shown::Loading => crate::app::images::placeholder(true),
                Shown::Unavailable => crate::app::images::placeholder(false),
            };
            Some((source.clone(), image))
        })
        .collect();
    move |uri| {
        let url: &str = uri;
        match sources.get(url) {
            Some(image) => ImageSource::Image(image.clone()),
            None => default_image_source(uri),
        }
    }
}

/// An image as the renderer shows it with no resolver: a `data:` URL's own
/// bytes, anything else loaded from its URI.
fn default_image_source(uri: &SharedUri) -> ImageSource {
    let url: &str = uri;
    (url.strip_prefix("data:"))
        .and_then(|rest| rest.split_once(";base64,"))
        .and_then(|(mime, data)| attachments::decode(&ImageAttachment::new(data, mime)))
        .map(ImageSource::Image)
        .unwrap_or_else(|| uri.clone().into())
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
            image_files,
            message_id,
            ..
        } => {
            text.len().hash(&mut h);
            images.len().hash(&mut h);
            image_files.len().hash(&mut h);
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((label, view)) = self.viewing.clone() {
            return self.subagent_view(&label, &view, cx);
        }
        let p = palette();
        self.sync_question_forms(window, cx);
        let rows = self.rows.clone();
        let replies = self.replies.clone();
        let forms = self.question_forms.clone();
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

        let agent_id = self.agent_id.clone();
        let remote_images = self.remote_images.clone();
        let host = self.host.clone();
        let stream = list(self.list.clone(), move |ix, _window, cx| {
            let Some(row) = rows.get(ix).cloned() else {
                return div().into_any_element();
            };
            // Read now that the row is on screen; never for rows scrolled past.
            let remote = row_images(&row, &session, &cwd, &remote_images, cx).map(|images| {
                let shown = (core.store)
                    .update(cx, |store, cx| store.show_images(&host, &images.files, cx));
                (images, shown)
            });
            let content = render_row(
                &row, &session, &core, &expanded, &replies, &forms, &this, &cwd, remote, cx,
            );
            // A selection stays within the turn it started in, and within
            // this conversation: a drag never runs on into the next turn or
            // across other cards.
            let group = match &row {
                Row::Item { turn, .. } => format!("{agent_id}:turn:{turn}"),
                _ => format!("{agent_id}:row:{ix}"),
            };
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
                .text_selection_group(TextSelectionGroupId::from_raw(hash_str(&group)))
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
        // Scrolled up from the end: an arrow back down to it.
        let latest =
            (!loading && !self.list.is_following_tail() && edges::list_overflow(&self.list).1)
                .then(|| self.jump_to_latest(cx));

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
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.open_copy_menu(event.position, None, window, cx);
                        }),
                    )
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
                    .when(!loading, |d| {
                        // Fades where there is more above or below; after the
                        // list, so they read its fresh layout.
                        d.child(stream).child(edges::list_fades(self.list.clone()))
                    })
                    .children(latest)
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
            .children(self.copy_menu_layer(window, cx))
            .into_any_element()
    }
}

/// A small copy icon that copies `text`. It brightens under the pointer,
/// so it reads as something to click.
fn copy_button(id: String, text: String) -> impl IntoElement {
    let p = palette();
    div()
        .id(SharedString::from(id))
        .group("copy-button")
        .p(px(2.))
        .rounded(px(4.))
        .cursor_pointer()
        .hover(|s| s.bg(p.surface1))
        .child(
            svg()
                .path("icons/copy.svg")
                .flex_shrink_0()
                .size(px(12.))
                .text_color(p.extra_muted)
                .group_hover("copy-button", |s| s.text_color(p.foreground)),
        )
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(gpui_kit::gpui::ClipboardItem::new_string(text.clone()));
        })
}

/// What a click on a link in a reply does: a right-click or a long press
/// opens the menu to open it or copy its address, others open it as a
/// text view does by itself.
fn reply_links(
    view: WeakEntity<Conversation>,
    kept: Option<(Entity<FleetStore>, HostId, HashMap<String, ImageFile>)>,
) -> impl Fn(&SharedString, &ClickEvent, &mut Window, &mut App) + Send + Sync + 'static {
    move |url, event, window, cx| {
        let (menu, open) = match event {
            ClickEvent::Mouse(click) => (
                click.up.button == MouseButton::Right,
                matches!(click.up.button, MouseButton::Left | MouseButton::Middle),
            ),
            ClickEvent::Keyboard(_) => (false, true),
            ClickEvent::Touch(click) => (click.long_press, !click.long_press),
        };
        if menu {
            let position = event.position();
            let _ = view.update(cx, |view, cx| {
                view.open_copy_menu(position, Some(url.clone()), window, cx)
            });
        } else if open {
            open_reply_link(url, kept.as_ref(), cx);
        }
    }
}

/// Opens a reply's link or image (an image without a link passes its own
/// address): a file its daemon keeps is read from there at full size, a
/// `data:` image is opened as a copy, anything else as a URL.
fn open_reply_link(
    url: &SharedString,
    kept: Option<&(Entity<FleetStore>, HostId, HashMap<String, ImageFile>)>,
    cx: &mut App,
) {
    if let Some((store, host, files)) = kept
        && let Some(file) = files.get(url.as_ref())
    {
        return open_kept_image(store, host, file, cx);
    }
    let embedded = (url.strip_prefix("data:"))
        .and_then(|rest| rest.split_once(";base64,"))
        .map(|(mime, data)| ImageAttachment::new(data, mime));
    match embedded {
        Some(image) => crate::app::images::open_read("reply-image", &image, cx),
        None => cx.open_url(url),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_row(
    row: &Row,
    session: &Entity<AgentSession>,
    core: &Core,
    expanded: &Rc<std::cell::RefCell<HashSet<String>>>,
    replies: &Replies,
    forms: &QuestionForms,
    view: &Entity<Conversation>,
    cwd: &str,
    remote: Option<(Rc<RowImages>, Vec<Shown>)>,
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
            // A turn of one message, as an agent's own often is, has no
            // length to show.
            let footer = fleet::timeline::turn_footer(&items, 0, items.len().saturating_sub(1));
            let duration = footer
                .duration_secs
                .map(display::short_duration)
                .unwrap_or_default();
            let last = footer.ended_at;
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
                .child(copy_button(
                    format!("copy-turn-{}-{start}", session_ref.agent_id),
                    text,
                ))
                .child(ago)
                .when(!duration.is_empty(), |d| d.child("·").child(duration))
                .into_any_element()
        }
        Row::Permission(i) => {
            let Some(permission) = timeline.permissions.get(*i) else {
                return div().into_any_element();
            };
            // A question the agent asks is a form to answer, not a yes or no.
            if let Some(form) = forms.borrow().get(&permission.id) {
                return form.clone().into_any_element();
            }
            permission_card(
                &permission.request,
                &session_ref.host,
                &session_ref.agent_id,
                &permission.id,
                core,
                cwd,
            )
        }
        Row::Item {
            index, gap, stamp, ..
        } => {
            let Some(item) = timeline.items().nth(*index) else {
                return div().into_any_element();
            };
            let pending = matches!(&item.kind, ItemKind::User { client_message_id: Some(id), .. } if timeline.pending_sends.contains(id));
            // When the step began, shown at the right of its last line, so
            // it costs no height.
            let ago = (stamp.and_then(|start| timeline.items().nth(start)))
                .map(|first| crate::app::store::time_ago(&first.timestamp))
                .filter(|ago| !ago.is_empty());
            let images = view.read(cx).images.clone();
            let body = match &item.kind {
                ItemKind::User {
                    text,
                    images: wires,
                    image_files,
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
                            .children(prompt_images(
                                &item.id,
                                images.get(&item.id).map(|entry| entry.decoded.as_slice()),
                                remote.as_ref(),
                                &session_ref.host,
                                core,
                            ))
                            .when(
                                text.is_empty() && !(wires.is_empty() && image_files.is_empty()),
                                |d| d.child("Image attachment"),
                            )
                            // Selectable like a reply, within its turn.
                            .when(!text.is_empty(), |d| {
                                d.child(SelectableText::new(
                                    SharedString::from(format!("prompt-{}", item.id)),
                                    text.clone(),
                                ))
                            })
                            // When it was sent, tucked into the corner, then
                            // a copy of the whole prompt.
                            .child(
                                div()
                                    .mt(px(-4.))
                                    .flex()
                                    .justify_end()
                                    .items_center()
                                    .gap(px(6.))
                                    .text_size(size::sm())
                                    .text_color(p.muted)
                                    .child(if pending {
                                        "Sending…".to_string()
                                    } else {
                                        crate::app::store::time_ago(&item.timestamp)
                                    })
                                    .when(!text.is_empty(), |d| {
                                        d.child(copy_button(
                                            format!("copy-prompt-{}", item.id),
                                            text.clone(),
                                        ))
                                    }),
                            ),
                    )
                    .into_any_element(),
                ItemKind::Assistant { text, .. } => {
                    // Owned, so the session is no longer borrowed when the
                    // reply's state is brought up to date.
                    let (id, text) = (item.id.clone(), text.clone());
                    let host = session_ref.host.clone();
                    let state = reply_state(replies, &id, &text, cx);
                    // Images saved on the daemon's host are read from there,
                    // through Base's view: the component's has no resolver.
                    let kept = remote.as_ref().map(|(images, _)| {
                        (images.sources.iter())
                            .map(|(source, file)| (source.clone(), images.files[*file].clone()))
                            .collect()
                    });
                    let links = reply_links(
                        view.downgrade(),
                        kept.map(|kept| (core.store.clone(), host.clone(), kept)),
                    );
                    let view = match remote {
                        Some((images, shown)) => gpui_kit::base::TextView::new(&state)
                            .selectable(true)
                            .image_source(reply_image_source(&images, &shown))
                            .on_link_click(links)
                            .into_any_element(),
                        None => TextView::new(&state)
                            .selectable(true)
                            .on_link_click(links)
                            .into_any_element(),
                    };
                    div()
                        .w_full()
                        .text_size(size::content())
                        .text_color(p.foreground)
                        .child(view)
                        .into_any_element()
                }
                ItemKind::Thought { text, loading } => {
                    let open = expanded.borrow().contains(&item.id);
                    let header = tool_header(
                        &item.id,
                        "brain",
                        "Thinking",
                        None,
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
                ItemKind::Tool(tool) => {
                    let host = &session_ref.host;
                    tool_row(tool, &item.id, cwd, expanded, view, host, core, cx)
                }
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
                        &item.id, icon_name, &text, None, None, false, false, true, open, expanded,
                        view,
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
            let body = match ago {
                Some(ago) => div()
                    .flex()
                    .items_end()
                    .gap(px(8.))
                    .child(div().flex_1().min_w(px(0.)).child(body))
                    .child(
                        div()
                            .flex_shrink_0()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .text_size(size::sm())
                            .text_color(p.extra_muted)
                            .child(ago),
                    )
                    .into_any_element(),
                None => body,
            };
            div().pb(px(*gap)).child(body).into_any_element()
        }
    }
}

/// An agent a row names, as a link that opens it; plain text when `host`,
/// the conversation's own, does not know the agent.
fn agent_link(id: &str, link: &AgentLink, host: &HostId, core: &Core, cx: &App) -> AnyElement {
    let p = palette();
    let known = core.agent_title(host, &link.id, cx);
    let openable = known.is_some();
    let label = div()
        .id(SharedString::from(format!("agent-link-{id}")))
        .min_w(px(0.))
        // A long title leaves room for what the message says.
        .max_w(relative(0.4))
        .flex_shrink_0()
        .truncate()
        .text_color(with_alpha(p.foreground, 0.8))
        .child(link.label(known));
    if !openable {
        return label.into_any_element();
    }
    let core = core.clone();
    let (host, agent) = (host.clone(), link.id.clone());
    label
        .cursor_pointer()
        .hover(|s| s.text_color(p.foreground).underline())
        .on_click(move |_, _, cx| {
            // Opening the agent, not unfolding the row.
            cx.stop_propagation();
            core.open_agent(&host, &agent, cx);
        })
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn tool_header(
    id: &str,
    icon_name: &'static str,
    name: &str,
    agent: Option<(AnyElement, bool)>,
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
    let name = div()
        .flex_shrink_0()
        .text_color(p.muted)
        .child(name.to_string());
    // "From planner" or "worker finished".
    let (name, agent) = match agent {
        Some((link, true)) => (link, Some(name.into_any_element())),
        Some((link, false)) => (name.into_any_element(), Some(link)),
        None => (name.into_any_element(), None),
    };
    let label = div()
        .flex()
        .items_center()
        .gap(px(6.))
        .min_w(px(0.))
        .flex_1()
        .child(name)
        .children(agent)
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

/// A tool call's one-line row, unfolding to its detail. A message from
/// another agent, or a notice about one, names the agent on the same line
/// ("From planner · …", "worker finished · …"), so it costs no height.
#[allow(clippy::too_many_arguments)]
fn tool_row(
    tool: &ToolCall,
    id: &str,
    cwd: &str,
    expanded: &Rc<std::cell::RefCell<HashSet<String>>>,
    view: &Entity<Conversation>,
    host: &HostId,
    core: &Core,
    cx: &App,
) -> AnyElement {
    let p = palette();
    let ToolDisplay {
        name,
        summary,
        icon,
        agent,
    } = display::tool_display(tool, cwd);
    let agent = agent.map(|link| (agent_link(id, &link, host, core, cx), link.lead));
    let expandable = display::has_details(tool);
    let open = expandable && expanded.borrow().contains(id);
    let header = tool_header(
        id,
        ui::tool_icon_name(icon),
        &name,
        agent,
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
    host: &HostId,
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
        agent_message: None,
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
        let host = host.clone();
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
