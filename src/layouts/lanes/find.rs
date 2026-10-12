//! Find on the board (Ctrl+F, Cmd+F on macOS). As you type, literal matches
//! from everything the board knows: card titles, workspaces, hosts, tools,
//! and the text of every conversation already loaded, with extensions' find
//! sources adding matches from history crusteo has not loaded. Matching cards are
//! outlined and the rest dimmed. The arrow keys step through the results,
//! pointing the board at each; Enter on one opens its card at the match.
//! Enter with none chosen asks the first source that answers questions,
//! whose excerpts open the same way.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use fleet::lanes::{LaneCard, LaneGrouping, LaneKind};
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::gpui::{
    AnyElement, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;

use super::{Lane, Lanes, Row};
use crate::theme::{palette, size, with_alpha};
use crate::ui;

/// The most results listed; a query that matches more is too broad to step
/// through anyway.
const MAX_HITS: usize = 200;

/// Cards, as host and agent.
type CardKeys = Rc<HashSet<(String, String)>>;

pub(super) struct Find {
    input: Entity<InputState>,
    hits: Vec<Hit>,
    /// The result the board points at; none means Enter asks a source.
    selected: Option<usize>,
    /// The cards that match, as host and agent, for outlining and dimming.
    pub(super) matches: Rc<HashSet<(String, String)>>,
    /// Collapsed lanes whose name or cards match, by key, so their tabs
    /// light up too.
    pub(super) collapsed: Rc<HashSet<String>>,
    /// The cards a label, host or workspace result covers, while the bar
    /// points at it: outlined in place of the matches.
    pub(super) cover: Option<CardKeys>,
    extra: Extra,
    /// A query is typed: the board outlines matches and dims the rest.
    pub(super) active: bool,
    /// Bumped per query, so a late source's reply for an old one is dropped.
    generation: u64,
    /// The last query and every card it matched, so a longer one need only
    /// check those.
    last: Option<(String, CardKeys)>,
    /// Keystrokes so far, and whether the search for the latest is to come.
    typed: u64,
    pending: bool,
    _events: Subscription,
}

#[derive(Clone)]
struct Hit {
    host: String,
    agent: String,
    title: String,
    lane: String,
    snippet: String,
    /// The conversation item the match is in, to scroll to.
    item: Option<String>,
    /// The find source it came from, by name; none for the board's own.
    from: Option<SharedString>,
    /// What the result is: a card, or a collapsed lane, label, host or
    /// workspace that matches by name.
    target: Target,
}

#[derive(Clone, PartialEq)]
enum Target {
    Card,
    /// A collapsed lane, by key.
    Lane(String),
    /// A label, by key.
    Label(String),
    Host(String),
    /// A workspace, as host and id.
    Workspace(String, String),
}

/// The most results of each kind that is not a card.
const MAX_NAMED: usize = 6;

/// What extensions' find sources are doing for the current query.
enum Extra {
    Idle,
    Searching,
    /// Asking the named source.
    Asking(SharedString),
    Answered(String),
    /// The named source failed.
    Failed(SharedString, String),
}

impl Find {
    /// The card the board points at, if any.
    pub(super) fn pointed(&self) -> Option<(&str, &str)> {
        let hit = self.hits.get(self.selected?)?;
        if hit.target != Target::Card {
            return None;
        }
        Some((hit.host.as_str(), hit.agent.as_str()))
    }

    /// The collapsed lane the find bar points at, if any.
    pub(super) fn pointed_lane(&self) -> Option<&str> {
        match &self.hits.get(self.selected?)?.target {
            Target::Lane(key) => Some(key),
            _ => None,
        }
    }
}

/// Where `needle` (lowercase) is in `haystack`, ignoring case, as a byte
/// offset into `haystack` when lowercasing kept its length, else its start.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let lower = haystack.to_lowercase();
    let at = lower.find(needle)?;
    Some(if lower.len() == haystack.len() { at } else { 0 })
}

/// A loaded conversation's messages, lowercased once for find, with what
/// they were made from: its length and its last message's, so a conversation
/// that grew or is streaming is lowercased again.
pub(super) struct Indexed {
    stamp: (usize, usize),
    items: Vec<(String, String)>,
}

/// What a card matched: the line to show, and the conversation item when
/// the match is in one.
type Found = HashMap<(String, String), (String, Option<String>)>;

fn message_text(kind: &fleet::timeline::ItemKind) -> Option<&str> {
    match kind {
        fleet::timeline::ItemKind::User { text, .. }
        | fleet::timeline::ItemKind::Assistant { text, .. } => Some(text),
        _ => None,
    }
}

/// Where a card matches: in its loaded conversation first, which says where
/// to scroll, else in its title, workspace, host, tool or model.
fn card_match(
    card: &fleet::lanes::LaneCard,
    query: &str,
    store: &crate::app::FleetStore,
    index: &mut HashMap<(String, String), Indexed>,
    cx: &gpui_kit::gpui::App,
) -> Option<(String, Option<String>)> {
    let in_conversation = store
        .session(&card.host, &card.agent_id)
        .and_then(|session| {
            let timeline = &session.read(cx).timeline;
            let stamp = (
                timeline.items().count(),
                timeline
                    .items()
                    .last()
                    .and_then(|item| message_text(&item.kind))
                    .map_or(0, str::len),
            );
            let key = (card.host.clone(), card.agent_id.clone());
            let indexed = index.entry(key).or_insert_with(|| Indexed {
                stamp: (usize::MAX, 0),
                items: Vec::new(),
            });
            if indexed.stamp != stamp {
                indexed.items = timeline
                    .items()
                    .filter_map(|item| {
                        Some((item.id.clone(), message_text(&item.kind)?.to_lowercase()))
                    })
                    .collect();
                indexed.stamp = stamp;
            }
            let (id, _) = indexed
                .items
                .iter()
                .find(|(_, lower)| lower.contains(query))?;
            let text = timeline
                .items()
                .find(|item| item.id == *id)
                .and_then(|item| message_text(&item.kind))?;
            Some((
                snippet(text, find_ci(text, query).unwrap_or(0)),
                Some(id.clone()),
            ))
        });
    in_conversation.or_else(|| {
        [
            card.title.as_str(),
            card.workspace_name.as_str(),
            card.host.as_str(),
            card.provider.as_str(),
            card.model.as_deref().unwrap_or_default(),
        ]
        .into_iter()
        .find(|field| find_ci(field, query).is_some())
        .map(|field| (field.to_string(), None))
    })
}
/// A line of context around `at` in `text`.
fn snippet(text: &str, at: usize) -> String {
    let start = text[..at]
        .char_indices()
        .rev()
        .nth(40)
        .map_or(0, |(i, _)| i);
    let end = text[at..]
        .char_indices()
        .nth(100)
        .map_or(text.len(), |(i, _)| at + i);
    let mut line: String = text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if start > 0 {
        line.insert(0, '…');
    }
    if end < text.len() {
        line.push('…');
    }
    line
}

impl Lanes {
    pub(super) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = &self.find {
            let input = find.input.clone();
            input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find on the board…"));
        let _events =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => this.find_changed(window, cx),
                    InputEvent::PressEnter { .. } => this.find_enter(window, cx),
                    _ => {}
                },
            );
        input.update(cx, |input, cx| input.focus(window, cx));
        self.find = Some(Find {
            input,
            hits: Vec::new(),
            selected: None,
            matches: Rc::new(HashSet::new()),
            collapsed: Rc::new(HashSet::new()),
            cover: None,
            extra: Extra::Idle,
            active: false,
            generation: 0,
            last: None,
            typed: 0,
            pending: false,
            _events,
        });
        cx.notify();
    }

    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.take().is_some() {
            // The index copies every searched conversation; don't hold it
            // while the bar is closed.
            self.find_index.borrow_mut().clear();
            self.focus.focus(window, cx);
            // The collapsed-lane tabs live in a toolbar.
            self.core.toolbars.update(cx, |_, cx| cx.notify());
            cx.notify();
        }
    }

    fn find_query(&self, cx: &Context<Self>) -> String {
        self.find
            .as_ref()
            .map(|find| find.input.read(cx).value().trim().to_lowercase())
            .unwrap_or_default()
    }

    /// The query changed. The search runs a frame later, so what was typed
    /// shows first, with "Searching…" if it takes a moment; quick typing
    /// skips the searches in between.
    fn find_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else {
            return;
        };
        find.typed += 1;
        find.pending = true;
        let typed = find.typed;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(20))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(find) = &mut this.find else {
                    return;
                };
                if find.typed != typed {
                    return;
                }
                find.pending = false;
                this.run_find(window, cx);
            });
        })
        .detach();
    }

    /// Lists the literal matches, and find sources' ones once typing
    /// pauses.
    fn run_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.find_query(cx);
        let found = if query.is_empty() {
            Found::new()
        } else {
            self.match_cards(&query, cx)
        };
        let mut hits = self.local_hits(&found, cx);
        let collapsed = if query.is_empty() {
            HashSet::new()
        } else {
            self.collapsed_matches(&query, &found, cx)
        };
        let last =
            (!query.is_empty()).then(|| (query.clone(), Rc::new(found.into_keys().collect())));
        // Matching collapsed lanes come first: on the board they are only
        // tabs, easy to miss.
        let lane_hits: Vec<Hit> = self
            .current_lanes(cx)
            .iter()
            .filter(|lane| collapsed.contains(&lane.key))
            .map(|lane| Hit {
                host: String::new(),
                agent: String::new(),
                title: lane.title.clone(),
                lane: "collapsed lane".into(),
                snippet: match lane.cards.len() {
                    1 => "1 card · Enter restores the lane".into(),
                    n => format!("{n} cards · Enter restores the lane"),
                },
                item: None,
                from: None,
                target: Target::Lane(lane.key.clone()),
            })
            .collect();
        // Then labels, hosts and workspaces whose names match.
        let mut named = lane_hits;
        if !query.is_empty() {
            named.extend(self.named_hits(&query, cx));
        }
        hits.splice(0..0, named);
        let Some(find) = &mut self.find else {
            return;
        };
        find.generation += 1;
        find.selected = None;
        find.active = !query.is_empty();
        find.extra = Extra::Idle;
        find.cover = None;
        find.matches = Rc::new(
            hits.iter()
                .filter(|h| h.target == Target::Card)
                .map(|h| (h.host.clone(), h.agent.clone()))
                .collect(),
        );
        find.hits = hits;
        find.collapsed = Rc::new(collapsed);
        find.last = last;
        self.core.toolbars.update(cx, |_, cx| cx.notify());
        cx.notify();
        // Without find sources the literal matches are the whole answer.
        let sources = self.core.extensions.read(cx).find.clone();
        let Some(find) = &mut self.find else {
            return;
        };
        if query.chars().count() < 3 || sources.is_empty() {
            return;
        }
        let generation = find.generation;
        find.extra = Extra::Searching;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let current = this
                .update(cx, |this, _| {
                    this.find.as_ref().map(|find| find.generation) == Some(generation)
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            let found = cx
                .background_spawn({
                    let query = query.clone();
                    async move {
                        // A source that fails leaves the others' matches.
                        sources
                            .iter()
                            .flat_map(|source| {
                                let name = source.name();
                                (source.search(&query).unwrap_or_default())
                                    .into_iter()
                                    .map(move |excerpt| (name.clone(), excerpt))
                            })
                            .collect::<Vec<_>>()
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.add_excerpts(generation, found, None, cx);
            });
        })
        .detach();
    }

    /// Find sources' excerpts, placed on the cards of the agents they come
    /// from; those not on the board are left out. With an answer, the
    /// result of asking.
    fn add_excerpts(
        &mut self,
        generation: u64,
        excerpts: Vec<(SharedString, crate::extensions::Excerpt)>,
        answer: Option<(SharedString, Result<String, String>)>,
        cx: &mut Context<Self>,
    ) {
        let lanes = self.arranged(&self.visible_lanes(cx), cx);
        let Some(find) = &mut self.find else {
            return;
        };
        if find.generation != generation {
            return;
        }
        for (name, excerpt) in excerpts {
            let agent = excerpt.agent.as_str();
            let place = lanes.iter().find_map(|lane| {
                let card = lane.cards.iter().find(|card| card.agent_id == agent)?;
                Some((lane, card))
            });
            let Some((lane, card)) = place else {
                continue;
            };
            if find
                .hits
                .iter()
                .any(|hit| hit.host == card.host && hit.agent == card.agent_id)
            {
                continue;
            }
            let text = excerpt.text.trim();
            find.hits.push(Hit {
                host: card.host.clone(),
                agent: card.agent_id.clone(),
                title: card.title.clone(),
                lane: lane.title.clone(),
                snippet: snippet(text, 0),
                item: None,
                from: Some(name),
                target: Target::Card,
            });
        }
        find.hits.truncate(MAX_HITS);
        find.matches = Rc::new(
            find.hits
                .iter()
                .map(|h| (h.host.clone(), h.agent.clone()))
                .collect(),
        );
        find.extra = match answer {
            Some((_, Ok(text))) => Extra::Answered(text),
            Some((name, Err(error))) => Extra::Failed(name, error),
            None => Extra::Idle,
        };
        cx.notify();
    }

    /// Every card on the board, collapsed lanes too, that matches, each
    /// checked once. A query that only adds to the last one checks just the
    /// cards that matched that.
    fn match_cards(&self, query: &str, cx: &Context<Self>) -> Found {
        let narrow = self.find.as_ref().and_then(|find| {
            let (last, found) = find.last.as_ref()?;
            query.contains(last.as_str()).then(|| found.clone())
        });
        let store = self.core.store.read(cx);
        let mut index = self.find_index.borrow_mut();
        let mut found = Found::new();
        let mut checked: HashSet<(&str, &str)> = HashSet::new();
        let lanes = self.current_lanes(cx);
        for card in lanes.iter().flat_map(|lane| lane.cards.iter()) {
            if !checked.insert((card.host.as_str(), card.agent_id.as_str())) {
                continue;
            }
            let key = (card.host.clone(), card.agent_id.clone());
            if narrow.as_ref().is_some_and(|narrow| !narrow.contains(&key)) {
                continue;
            }
            if let Some(matched) = card_match(card, query, store, &mut index, cx) {
                found.insert(key, matched);
            }
        }
        // Agents that left the board take their copied text with them.
        index.retain(|(host, agent), _| checked.contains(&(host.as_str(), agent.as_str())));
        found
    }

    /// The collapsed lanes whose name, or any of whose cards, match.
    fn collapsed_matches(&self, query: &str, found: &Found, cx: &Context<Self>) -> HashSet<String> {
        self.current_lanes(cx)
            .iter()
            .filter(|lane| lane.collapsed)
            .filter(|lane| {
                find_ci(&lane.title, query).is_some()
                    || lane
                        .cards
                        .iter()
                        .any(|card| found.contains_key(&(card.host.clone(), card.agent_id.clone())))
            })
            .map(|lane| lane.key.clone())
            .collect()
    }

    /// The matching cards on the board, one result each, first lane first.
    fn local_hits(&self, found: &Found, cx: &Context<Self>) -> Vec<Hit> {
        let lanes = self.arranged(&self.visible_lanes(cx), cx);
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        let mut hits = Vec::new();
        for lane in lanes.iter() {
            for card in &lane.cards {
                if hits.len() >= MAX_HITS {
                    return hits;
                }
                if !seen.insert((card.host.as_str(), card.agent_id.as_str())) {
                    continue;
                }
                let key = (card.host.clone(), card.agent_id.clone());
                if let Some((snippet, item)) = found.get(&key) {
                    hits.push(Hit {
                        host: card.host.clone(),
                        agent: card.agent_id.clone(),
                        title: card.title.clone(),
                        lane: lane.title.clone(),
                        snippet: snippet.clone(),
                        item: item.clone(),
                        from: None,
                        target: Target::Card,
                    });
                }
            }
        }
        hits
    }
    /// Enter: open the chosen result, or with none chosen, ask the first
    /// find source that answers questions.
    fn find_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(find) = &self.find else {
            return;
        };
        if let Some(hit) = find.selected.and_then(|at| find.hits.get(at)).cloned() {
            self.open_hit(&hit, window, cx);
            return;
        }
        let query = find.input.read(cx).value().trim().to_string();
        let asker = self.core.extensions.read(cx).find.first().cloned();
        if query.is_empty() || matches!(find.extra, Extra::Asking(_)) {
            return;
        }
        let Some(asker) = asker else {
            return;
        };
        let Some(find) = &mut self.find else {
            return;
        };
        find.generation += 1;
        let generation = find.generation;
        let name = asker.name();
        find.extra = Extra::Asking(name.clone());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let answer = cx.background_spawn(async move { asker.ask(&query) }).await;
            let _ = this.update(cx, |this, cx| match answer {
                // It does not answer questions after all.
                None => {
                    if let Some(find) = &mut this.find {
                        find.extra = Extra::Idle;
                    }
                    cx.notify();
                }
                Some(Ok(answer)) => {
                    let excerpts = (answer.excerpts.into_iter())
                        .map(|excerpt| (name.clone(), excerpt))
                        .collect();
                    this.add_excerpts(generation, excerpts, Some((name, Ok(answer.text))), cx)
                }
                Some(Err(error)) => {
                    this.add_excerpts(generation, Vec::new(), Some((name, Err(error))), cx)
                }
            });
        })
        .detach();
    }

    /// Points the board at a result: its lane in view, its card in view in
    /// the lane and focused, without opening it.
    fn point_at(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(hit) = self
            .find
            .as_ref()
            .and_then(|find| find.hits.get(at))
            .cloned()
        else {
            return;
        };
        if let Some(find) = &mut self.find {
            find.selected = Some(at);
        }
        // A label, host or workspace: the cards it covers outlined.
        if !matches!(hit.target, Target::Card | Target::Lane(_)) {
            self.point_at_named(&hit.target, cx);
            return;
        }
        if let Some(find) = &mut self.find {
            find.cover = None;
        }
        // A collapsed lane: its tab scrolled into view and singled out.
        if let Target::Lane(key) = &hit.target {
            let tab = self
                .current_lanes(cx)
                .iter()
                .filter(|lane| lane.collapsed)
                .position(|lane| lane.key == *key);
            if let Some(tab) = tab {
                self.collapsed_scroll.scroll_to_item(tab);
            }
            self.core.toolbars.update(cx, |_, cx| cx.notify());
            cx.notify();
            return;
        }
        self.core.toolbars.update(cx, |_, cx| cx.notify());
        let lanes = self.arranged(&self.visible_lanes(cx), cx);
        let Some(lane) = lanes.iter().find(|lane| {
            lane.cards
                .iter()
                .any(|card| card.host == hit.host && card.agent_id == hit.agent)
        }) else {
            cx.notify();
            return;
        };
        self.focused_lane = Some(lane.key.clone());
        self.focused_agent = Some((hit.host.clone(), hit.agent.clone()));
        self.focused_row = None;
        self.reveal_column = Some(lane.key.clone());
        let box_single = self.core.nav.read(cx).box_single_workspaces;
        let rows = self.rows(lane, box_single, cx);
        let row = rows.iter().position(|row| {
            matches!(row, Row::Card { card, .. }
                if lane.cards[*card].host == hit.host && lane.cards[*card].agent_id == hit.agent)
        });
        if let Some(row) = row
            && let Some(list) = self.lists.get(&lane.key)
        {
            list.state.scroll_to_reveal_item(row);
        }
        cx.notify();
    }

    /// Opens a result's card and scrolls its conversation to the match,
    /// keeping the find bar and its keyboard focus for the next one.
    fn open_hit(&mut self, hit: &Hit, window: &mut Window, cx: &mut Context<Self>) {
        // A collapsed lane is restored, and the results found again so its
        // cards are among them.
        match &hit.target {
            Target::Card | Target::Lane(_) => {}
            other => {
                let other = other.clone();
                self.open_named(&other, window, cx);
                return;
            }
        }
        if let Target::Lane(key) = hit.target.clone() {
            self.restore_lane(&key, window, cx);
            self.find_changed(window, cx);
            if let Some(find) = &self.find {
                let input = find.input.clone();
                input.update(cx, |input, cx| input.focus(window, cx));
            }
            return;
        }
        self.reveal_agent(&hit.host, &hit.agent, window, cx);
        self.focus_composer = None;
        if let Some(item) = hit.item.clone() {
            let view = self.conversation(&hit.host, &hit.agent, window, cx);
            view.update(cx, |view, cx| view.reveal_item(item, cx));
        }
        if let Some(find) = &self.find {
            let input = find.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    fn step_find(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(find) = &self.find else {
            return;
        };
        if find.hits.is_empty() {
            return;
        }
        let last = find.hits.len() as isize - 1;
        let next = match find.selected {
            None if delta > 0 => 0,
            None => last,
            Some(at) => (at as isize + delta).clamp(0, last),
        };
        self.point_at(next as usize, cx);
    }

    /// The find bar, over the top of the board.
    pub(super) fn find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let find = self.find.as_ref()?;
        let p = palette();
        let count = find
            .hits
            .iter()
            .filter(|hit| hit.target == Target::Card)
            .count();
        let query_empty = find.input.read(cx).value().trim().is_empty();
        let collapsed = match find.collapsed.len() {
            0 => String::new(),
            1 => " · 1 collapsed lane matches".to_string(),
            n => format!(" · {n} collapsed lanes match"),
        };
        let asker = (self.core.extensions.read(cx).find.first()).map(|source| source.name());
        let (asks, asks_unchosen) = match &asker {
            Some(name) => (
                format!(" · Enter asks {name}"),
                format!(" · Enter with none chosen asks {name}"),
            ),
            None => (String::new(), String::new()),
        };
        let summary = match (&find.extra, count) {
            _ if query_empty => "Type to find on the board".to_string(),
            _ if find.pending => "Searching…".to_string(),
            (Extra::Asking(name), _) => format!("Asking {name}…"),
            (_, 0) => format!("No matches on the board{asks}"),
            (_, 1) => format!("1 card · ↑↓ steps · Enter opens{asks_unchosen}"),
            (_, n) => format!("{n} cards · ↑↓ steps · Enter opens{asks_unchosen}"),
        };
        let summary = if query_empty {
            summary
        } else {
            format!("{summary}{collapsed}")
        };
        let searching = matches!(find.extra, Extra::Searching);
        let answer = match &find.extra {
            Extra::Answered(text) => Some((text.clone(), p.foreground)),
            Extra::Failed(name, error) => Some((format!("{name}: {error}"), p.status_danger)),
            _ => None,
        };
        let hits = find.hits.clone();
        let rows: Vec<_> = hits
            .iter()
            .enumerate()
            .map(|(at, hit)| {
                let chosen = find.selected == Some(at);
                let open = hit.clone();
                div()
                    .id(SharedString::from(format!("find-hit-{at}")))
                    .px(px(10.))
                    .py(px(5.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(chosen, |row| row.bg(with_alpha(p.accent, 0.18)))
                    .hover(|row| row.bg(p.surface2))
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(size::sm())
                            .child(
                                div()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(p.foreground)
                                    .child(hit.title.clone()),
                            )
                            .child(div().flex_shrink_0().text_color(p.extra_muted).child(
                                if hit.target == Target::Card {
                                    format!("{} · {}", hit.lane, hit.host)
                                } else {
                                    hit.lane.clone()
                                },
                            ))
                            .children(
                                target_icon(&hit.target)
                                    .map(|icon| ui::icon(icon, 11., p.extra_muted)),
                            )
                            .children(filter_button(&hit.target, at, cx))
                            .when(hit.from.is_some(), |row| {
                                row.child(ui::icon("sparkles", 11., p.extra_muted))
                            }),
                    )
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(px(11.))
                            .text_color(p.muted)
                            .child(hit.snippet.clone()),
                    )
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered && this.find.as_ref().and_then(|f| f.selected) != Some(at) {
                            this.point_at(at, cx);
                        }
                    }))
                    // A click opens it and is done with finding; Enter keeps the
                    // bar for stepping on to the next.
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.point_at(at, cx);
                        this.open_hit(&open, window, cx);
                        this.close_find(window, cx);
                    }))
            })
            .collect();
        Some(
            div()
                .absolute()
                .top(px(8.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("lanes-find")
                        // What is under the bar takes no hover or tooltips.
                        .occlude()
                        .w(px(620.))
                        .max_w(gpui_kit::gpui::relative(0.9))
                        .p(px(8.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(with_alpha(p.accent, 0.6))
                        .bg(p.surface1)
                        .shadow_lg()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        // The arrows and Escape are the bar's, not the board's.
                        .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            match event.keystroke.key.as_str() {
                                "down" => this.step_find(1, cx),
                                "up" => this.step_find(-1, cx),
                                "escape" => this.close_find(window, cx),
                                _ => return,
                            }
                            cx.stop_propagation();
                        }))
                        // Other keys are left alone: a key press marked handled
                        // never becomes typed text on Windows, and the board's
                        // own keys act only while the board itself has focus.
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(ui::icon("search", 14., p.muted))
                                .child(
                                    div()
                                        .flex_1()
                                        .child(Input::new(&find.input).appearance(false)),
                                )
                                .when(searching, |row| {
                                    row.child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(p.extra_muted)
                                            .child("Searching…"),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .px(px(4.))
                                .text_size(px(11.))
                                .text_color(p.extra_muted)
                                .child(summary),
                        )
                        .when_some(answer, |bar, (text, color)| {
                            bar.child(
                                div()
                                    .id("lanes-find-answer")
                                    .max_h(px(160.))
                                    .overflow_y_scroll()
                                    .px(px(8.))
                                    .py(px(6.))
                                    .rounded(px(6.))
                                    .bg(p.surface0)
                                    .text_size(size::sm())
                                    .text_color(color)
                                    .child(text),
                            )
                        })
                        .when(!find.hits.is_empty(), |bar| {
                            bar.child(
                                div()
                                    .id("lanes-find-hits")
                                    .max_h(px(360.))
                                    .overflow_y_scroll()
                                    .flex()
                                    .flex_col()
                                    .children(rows),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    /// A row of the board as find shows it: a matching card outlined, the
    /// one pointed at strongly, and everything else dimmed.
    pub(super) fn find_row(&self, lane: &Lane, row: &Row, element: AnyElement) -> AnyElement {
        let Some(find) = &self.find else {
            return element;
        };
        if !find.active {
            return element;
        }
        let p = palette();
        let card = match row {
            Row::Card { card, .. } => Some(&lane.cards[*card]),
            _ => None,
        };
        // A label, host or workspace pointed at outlines what it covers.
        let lit = find.cover.as_ref().unwrap_or(&find.matches);
        let Some(card) =
            card.filter(|card| lit.contains(&(card.host.clone(), card.agent_id.clone())))
        else {
            return div().opacity(0.3).child(element).into_any_element();
        };
        let pointed = find.pointed() == Some((card.host.as_str(), card.agent_id.as_str()));
        div()
            .relative()
            .child(element)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(6.))
                    .border_color(p.accent)
                    .map(|outline| {
                        if pointed {
                            outline.border_2()
                        } else {
                            outline.border_1()
                        }
                    }),
            )
            .into_any_element()
    }
}

/// The icon a result shows for its kind; cards show none.
fn target_icon(target: &Target) -> Option<&'static str> {
    match target {
        Target::Card => None,
        Target::Lane(_) => Some("fold-horizontal"),
        Target::Label(_) => Some("tag"),
        Target::Host(_) => Some("server"),
        Target::Workspace(..) => Some("folder"),
    }
}

/// "Filter" on a label or host result: shows only what it covers.
fn filter_button(target: &Target, at: usize, cx: &mut Context<Lanes>) -> Option<AnyElement> {
    if !matches!(target, Target::Label(_) | Target::Host(_)) {
        return None;
    }
    let target = target.clone();
    Some(
        Button::new(SharedString::from(format!("find-filter-{at}")))
            .label("Filter")
            .ghost()
            .xsmall()
            .tooltip("Show only what this covers; the × in the filter bar clears it")
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.filter_to(&target, window, cx);
            }))
            .into_any_element(),
    )
}

impl Lanes {
    /// Labels, hosts and workspaces whose names match, a few of each.
    fn named_hits(&self, query: &str, cx: &Context<Self>) -> Vec<Hit> {
        let store = self.core.store.read(cx);
        let fleet = &store.fleet;
        let lanes = self.current_lanes(cx);
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        let cards: Vec<&LaneCard> = lanes
            .iter()
            .flat_map(|lane| lane.cards.iter())
            .filter(|card| seen.insert((card.host.as_str(), card.agent_id.as_str())))
            .collect();
        let mut hits = Vec::new();

        let mut labels: BTreeMap<String, String> = BTreeMap::new();
        for host in &fleet.hosts {
            for def in &host.directory.labels {
                labels
                    .entry(fleet::directory::label_key(&def.name))
                    .or_insert_with(|| def.name.clone());
            }
        }
        for (key, name) in labels
            .into_iter()
            .filter(|(_, name)| find_ci(name, query).is_some())
            .take(MAX_NAMED)
        {
            let count = cards
                .iter()
                .filter(|card| card_labels(fleet, card).contains(&key))
                .count();
            let action = if self.label_lane(&key, cx).is_some() {
                "Enter goes to its lane"
            } else {
                "Enter filters the board"
            };
            hits.push(named_hit(
                name,
                "label",
                format!("{} · {action}", cards_text(count)),
                Target::Label(key),
            ));
        }

        for host in fleet
            .hosts
            .iter()
            .filter(|host| find_ci(host.name(), query).is_some())
            .take(MAX_NAMED)
        {
            let count = cards.iter().filter(|card| card.host == host.name()).count();
            let state = if host.connection.is_online() {
                "online"
            } else {
                "offline"
            };
            hits.push(named_hit(
                host.name().to_string(),
                "host",
                format!("{state} · {} · Enter filters the board", cards_text(count)),
                Target::Host(host.name().to_string()),
            ));
        }

        let mut workspaces: Vec<(&str, &str, &str)> = Vec::new();
        for ws in lanes.iter().flat_map(|lane| lane.workspaces.iter()) {
            let entry = (ws.host.as_str(), ws.workspace_id.as_str(), ws.name.as_str());
            if !workspaces.contains(&entry) && find_ci(&ws.name, query).is_some() {
                workspaces.push(entry);
            }
        }
        for (host, id, name) in workspaces.into_iter().take(MAX_NAMED) {
            let count = cards
                .iter()
                .filter(|card| card.host == host && card.workspace_id.as_deref() == Some(id))
                .count();
            hits.push(named_hit(
                name.to_string(),
                &format!("workspace · {host}"),
                format!("{} · Enter goes to it", cards_text(count)),
                Target::Workspace(host.to_string(), id.to_string()),
            ));
        }
        hits
    }

    /// The label's own lane, when the board groups by label.
    fn label_lane(&self, key: &str, cx: &Context<Self>) -> Option<Rc<Lane>> {
        if self.core.nav.read(cx).lane_grouping != LaneGrouping::Label {
            return None;
        }
        self.current_lanes(cx)
            .iter()
            .find(|lane| lane.kind == LaneKind::Label && lane.label_keys == [key])
            .cloned()
    }

    /// The cards a label, host or workspace covers.
    fn covered(&self, target: &Target, cx: &Context<Self>) -> CardKeys {
        let store = self.core.store.read(cx);
        let fleet = &store.fleet;
        let covers = |card: &LaneCard| match target {
            Target::Label(key) => card_labels(fleet, card).contains(key),
            Target::Host(host) => card.host == *host,
            Target::Workspace(host, id) => {
                card.host == *host && card.workspace_id.as_deref() == Some(id.as_str())
            }
            Target::Card | Target::Lane(_) => false,
        };
        Rc::new(
            self.current_lanes(cx)
                .iter()
                .flat_map(|lane| lane.cards.iter())
                .filter(|card| covers(card))
                .map(|card| (card.host.clone(), card.agent_id.clone()))
                .collect(),
        )
    }

    /// Points at a label, host or workspace: what it covers outlined, and
    /// its lane or box brought into view where it has one.
    fn point_at_named(&mut self, target: &Target, cx: &mut Context<Self>) {
        let cover = self.covered(target, cx);
        if let Some(find) = &mut self.find {
            find.cover = Some(cover);
        }
        match target {
            Target::Label(key) => {
                if let Some(lane) = self.label_lane(key, cx).filter(|lane| !lane.collapsed) {
                    self.focused_lane = Some(lane.key.clone());
                    self.reveal_column = Some(lane.key.clone());
                }
            }
            Target::Workspace(host, id) => self.reveal_workspace(host, id, cx),
            _ => {}
        }
        self.core.toolbars.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// Scrolls the board to a workspace's box, in the first lane showing it.
    fn reveal_workspace(&mut self, host: &str, id: &str, cx: &mut Context<Self>) {
        let lanes = self.arranged(&self.visible_lanes(cx), cx);
        let Some(lane) = lanes.iter().find(|lane| {
            lane.workspaces
                .iter()
                .any(|ws| ws.host == host && ws.workspace_id == id)
        }) else {
            return;
        };
        self.focused_lane = Some(lane.key.clone());
        self.reveal_column = Some(lane.key.clone());
        let in_workspace = |card: usize| {
            lane.cards[card].host == host && lane.cards[card].workspace_id.as_deref() == Some(id)
        };
        let box_single = self.core.nav.read(cx).box_single_workspaces;
        let rows = self.rows(lane, box_single, cx);
        let row = rows.iter().position(|row| match row {
            Row::Header { card, .. } | Row::Card { card, .. } => in_workspace(*card),
            Row::Empty { workspace, .. } => {
                let ws = &lane.workspaces[*workspace];
                ws.host == host && ws.workspace_id == id
            }
            _ => false,
        });
        if let Some(row) = row
            && let Some(list) = self.lists.get(&lane.key)
        {
            list.state.scroll_to_reveal_item(row);
        }
    }

    /// Enter on a label, host or workspace: a label goes to its lane, or
    /// filters the board when it has none; a host filters the board; a
    /// workspace goes to its box, restoring its lane if collapsed.
    fn open_named(&mut self, target: &Target, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            Target::Label(key) => match self.label_lane(key, cx) {
                Some(lane) => {
                    if lane.collapsed {
                        self.restore_lane(&lane.key, window, cx);
                    }
                    self.focused_lane = Some(lane.key.clone());
                    self.reveal_column = Some(lane.key.clone());
                }
                None => return self.filter_to(target, window, cx),
            },
            Target::Host(_) => return self.filter_to(target, window, cx),
            Target::Workspace(host, id) => {
                let hidden_in = self
                    .current_lanes(cx)
                    .iter()
                    .filter(|lane| {
                        lane.workspaces
                            .iter()
                            .any(|ws| ws.host == *host && ws.workspace_id == *id)
                    })
                    .map(|lane| (lane.key.clone(), lane.collapsed))
                    .collect::<Vec<_>>();
                if hidden_in.iter().all(|(_, collapsed)| *collapsed)
                    && let Some((key, _)) = hidden_in.first()
                {
                    self.restore_lane(key, window, cx);
                }
                self.reveal_workspace(host, id, cx);
            }
            Target::Card | Target::Lane(_) => {}
        }
        if let Some(find) = &self.find {
            let input = find.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    /// Filters the board to a label or host, in place of any filter of that
    /// kind, then finds again on what is left.
    fn filter_to(&mut self, target: &Target, window: &mut Window, cx: &mut Context<Self>) {
        self.core.nav.update(cx, |nav, cx| {
            let mut filter = nav.lane_filter.clone();
            match target {
                Target::Label(key) => filter.labels = [key.clone()].into(),
                Target::Host(host) => filter.hosts = [host.clone()].into(),
                _ => return,
            }
            nav.set_lane_filter(filter, cx);
        });
        self.find_changed(window, cx);
        if let Some(find) = &self.find {
            let input = find.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
        }
    }
}

/// A card's workspace labels, as label keys.
fn card_labels(fleet: &fleet::Fleet, card: &LaneCard) -> Vec<String> {
    card.workspace_id
        .as_deref()
        .and_then(|id| fleet.workspace(&card.host, id))
        .map(|ws| {
            ws.labels
                .iter()
                .map(|l| fleet::directory::label_key(l))
                .collect()
        })
        .unwrap_or_default()
}

fn cards_text(count: usize) -> String {
    match count {
        1 => "1 card".into(),
        n => format!("{n} cards"),
    }
}

fn named_hit(title: String, kind: &str, snippet: String, target: Target) -> Hit {
    Hit {
        host: String::new(),
        agent: String::new(),
        title,
        lane: kind.to_string(),
        snippet,
        item: None,
        from: None,
        target,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_frame_the_match_on_one_line() {
        let text = "first line\nthe needle is here\nlast";
        let at = find_ci(text, "needle").unwrap();
        assert_eq!(snippet(text, at), "first line the needle is here last");
        let long = format!("{}needle{}", "a ".repeat(60), "b ".repeat(80));
        let at = find_ci(&long, "needle").unwrap();
        let line = snippet(&long, at);
        assert!(line.starts_with('…') && line.ends_with('…') && line.contains("needle"));
        assert_eq!(find_ci("Hello World", "world"), Some(6));
        assert_eq!(find_ci("Hello", "bye"), None);
    }
}
