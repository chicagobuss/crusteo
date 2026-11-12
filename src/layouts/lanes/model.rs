//! The board's lane list: lanes as the store has them, grouped, filtered and
//! arranged, and the bookkeeping that keeps expansions, focus and open
//! conversations in step with them.

use fleet::HostId;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use fleet::lanes::{
    LabelLaneMode, Lane, LaneCard, LaneGrouping, LaneModel, LaneState, Placement, group_workspaces,
    next_visible_lane, visible_focus_lane,
};
use gpui_kit::gpui::{App, Context, Window};

use crate::app::nav::LaneFilter;

use super::*;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct LaneModelKey {
    /// Grouping, label mode, and whether background workspaces are left out.
    pub(super) membership: (LaneGrouping, LabelLaneMode, bool),
    pub(super) order: Vec<String>,
    pub(super) collapsed: BTreeSet<String>,
    pub(super) bands: BTreeSet<String>,
    pub(super) colors: BTreeMap<String, String>,
}

#[derive(PartialEq, Eq)]
pub(super) struct LaneModelKeyRef<'a> {
    pub(super) membership: (LaneGrouping, LabelLaneMode, bool),
    pub(super) order: &'a [String],
    pub(super) collapsed: &'a BTreeSet<String>,
    pub(super) bands: &'a BTreeSet<String>,
    pub(super) colors: &'a BTreeMap<String, String>,
}

impl LaneModelKey {
    pub(super) fn as_ref(&self) -> LaneModelKeyRef<'_> {
        LaneModelKeyRef {
            membership: self.membership,
            order: &self.order,
            collapsed: &self.collapsed,
            bands: &self.bands,
            colors: &self.colors,
        }
    }
    pub(super) fn matches_nav(&self, nav: &crate::app::Nav) -> bool {
        self.as_ref() == lane_model_key_ref(nav)
    }
}

fn lane_model_key_ref(nav: &crate::app::Nav) -> LaneModelKeyRef<'_> {
    LaneModelKeyRef {
        membership: (
            nav.lane_grouping,
            nav.lane_label_mode,
            !nav.show_background(),
        ),
        order: &nav.lane_order,
        collapsed: &nav.lane_collapsed,
        bands: &nav.lane_bands,
        colors: &nav.lane_color_choices,
    }
}

pub(super) fn lane_model_key(nav: &crate::app::Nav) -> LaneModelKey {
    let key = lane_model_key_ref(nav);
    LaneModelKey {
        membership: key.membership,
        order: key.order.to_vec(),
        collapsed: key.collapsed.clone(),
        bands: key.bands.clone(),
        colors: key.colors.clone(),
    }
}

/// A cached snapshot for board geometry and its item indices.
/// `lanes` with only what `filter` lets through: cards whose host, tool
/// and workspace labels match, and workspaces likewise; a lane left with
/// neither goes. `labels` gives what a card (host, workspace, agent) or a
/// workspace's box (no agent) matches by, as [`fleet::lanes::filter_keys`]
/// has it. Lanes the filter leaves whole keep their identity.
fn filter_lanes(
    lanes: &[Rc<Lane>],
    filter: &LaneFilter,
    labels: impl Fn(&HostId, Option<&str>, Option<&str>) -> Vec<String>,
) -> Vec<Rc<Lane>> {
    if filter.is_empty() {
        return lanes.to_vec();
    }
    let host_ok = |host: &HostId| filter.hosts.is_empty() || filter.hosts.contains(host);
    let labels_ok = |host: &HostId, workspace: Option<&str>, agent: Option<&str>| {
        filter.labels.is_empty()
            || labels(host, workspace, agent)
                .iter()
                .any(|label| filter.labels.contains(label))
    };
    let card_ok = |card: &LaneCard| {
        let tool = card.provider.split('/').next().unwrap_or(&card.provider);
        host_ok(&card.host)
            && (filter.tools.is_empty() || filter.tools.contains(tool))
            && (filter.states.is_empty() || filter.states.contains(card.bucket.as_str()))
            && labels_ok(
                &card.host,
                card.workspace_id.as_deref(),
                Some(&card.agent_id),
            )
    };
    lanes
        .iter()
        .filter_map(|lane| {
            let cards: Vec<LaneCard> = lane.cards.iter().filter(|c| card_ok(c)).cloned().collect();
            // With a tool or state chosen, a workspace shows only for its
            // matching agents.
            let workspaces: Vec<_> = lane
                .workspaces
                .iter()
                .filter(|ws| {
                    host_ok(&ws.host)
                        && labels_ok(&ws.host, Some(&ws.workspace_id), None)
                        && ((filter.tools.is_empty() && filter.states.is_empty())
                            || cards.iter().any(|card| {
                                card.host == ws.host
                                    && card.workspace_id.as_deref() == Some(&ws.workspace_id)
                            }))
                })
                .cloned()
                .collect();
            if cards.is_empty() && workspaces.is_empty() {
                return None;
            }
            if cards.len() == lane.cards.len() && workspaces.len() == lane.workspaces.len() {
                return Some(lane.clone());
            }
            let mut kept = (**lane).clone();
            kept.hosts.retain(|host| {
                host_ok(host)
                    && (cards.iter().any(|card| card.host == *host)
                        || workspaces.iter().any(|ws| ws.host == *host))
            });
            kept.cards = cards;
            kept.workspaces = workspaces;
            Some(Rc::new(kept))
        })
        .collect()
}

fn displayed_lanes(lanes: &[Rc<Lane>]) -> LaneSet {
    Rc::new(
        lanes
            .iter()
            .filter(|lane| !lane.collapsed)
            .cloned()
            .collect(),
    )
}

impl Lanes {
    /// The board's lanes, current with the store. The model is patched from
    /// the store's change log since it was last read, so an agent update
    /// costs a card move rather than a rebuild of every lane.
    pub(super) fn lanes(&self, cx: &App) -> LaneSet {
        let store = self.core.store.read(cx);
        let cursor = store.change_cursor();
        let nav = self.core.nav.read(cx);
        let mut slot = self.lanes_cache.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|cache| !cache.nav_key.matches_nav(nav))
        {
            *slot = None;
        }
        if let Some(cache) = slot.as_mut() {
            if cache.cursor == cursor {
                return cache.lanes.clone();
            }
            match store.changes_since(cache.cursor) {
                Some(changes) => cache.model.apply(
                    &store.fleet,
                    changes.map(|change| (&change.host, &change.change)),
                ),
                None => {
                    cache.model =
                        LaneModel::build(&store.fleet, cache.model.grouping(), cache.model.state())
                }
            }
            cache.cursor = cursor;
        } else {
            let nav = self.core.nav.read(cx);
            let state = LaneState {
                label_mode: nav.lane_label_mode,
                order: nav.lane_order.clone(),
                collapsed: nav.lane_collapsed.clone(),
                hide_background: !nav.show_background(),
                bands: nav.lane_bands.clone(),
            };
            *slot = Some(LaneCache {
                model: LaneModel::build(&store.fleet, nav.lane_grouping, &state),
                cursor,
                nav_key: lane_model_key(nav),
                lanes: Rc::new(Vec::new()),
                visible: Rc::new(Vec::new()),
                visible_filter: None,
                decor: HashMap::new(),
            });
        }
        let cache = slot.as_mut().expect("just filled");
        let lanes: LaneSet = Rc::new(cache.model.lanes().to_vec());
        if lanes.len() == cache.lanes.len()
            && lanes
                .iter()
                .zip(cache.lanes.iter())
                .all(|(a, b)| Rc::ptr_eq(a, b))
        {
            return cache.lanes.clone();
        }
        // Display data only for lanes that changed; the rest keep theirs.
        let colors = &self.core.nav.read(cx).lane_color_choices;
        let old: HashMap<&str, &Rc<Lane>> = cache
            .lanes
            .iter()
            .map(|lane| (lane.key.as_str(), lane))
            .collect();
        let mut decor = HashMap::with_capacity(lanes.len());
        for lane in lanes.iter() {
            let kept = old
                .get(lane.key.as_str())
                .filter(|previous| Rc::ptr_eq(previous, lane))
                .and_then(|_| cache.decor.get(&lane.key));
            decor.insert(
                lane.key.clone(),
                kept.copied().unwrap_or_else(|| lane_decor(lane, colors)),
            );
        }
        cache.decor = decor;
        cache.visible_filter = None;
        cache.lanes = lanes.clone();
        self.focus_check_needed.set(true);
        lanes
    }

    /// The lanes user actions act on. The same as [`Self::lanes`] now that
    /// nothing throttles it.
    pub(super) fn current_lanes(&self, cx: &App) -> LaneSet {
        self.lanes(cx)
    }

    /// Displayed columns are cached separately; hidden lanes keep their model,
    /// saved order, draft and list state without occupying board scroll space.
    pub(super) fn visible_lanes(&self, cx: &App) -> LaneSet {
        self.lanes(cx);
        let filter = &self.core.nav.read(cx).lane_filter;
        let mut slot = self.lanes_cache.borrow_mut();
        let cache = slot.as_mut().expect("lanes initialized");
        // Made again only when the lanes or the filter change.
        if cache.visible_filter.as_ref() != Some(filter) {
            let fleet = &self.core.store.read(cx).fleet;
            let labels = |host: &HostId, workspace: Option<&str>, agent: Option<&str>| {
                fleet::lanes::filter_keys(fleet, host, workspace, agent)
            };
            let shown = filter_lanes(&cache.lanes, filter, labels);
            cache.visible = displayed_lanes(&shown);
            cache.visible_filter = Some(filter.clone());
        }
        cache.visible.clone()
    }

    /// The selected workspace, as host and workspace id: the focused empty
    /// workspace, or the focused card's. Its box is outlined in the focused
    /// lane and tinted in the others.
    pub(super) fn selected_workspace(&self, cx: &App) -> Option<(HostId, String)> {
        match &self.focused_row {
            Some(RowFocus::Workspace(host, workspace)) => {
                return Some((host.clone(), workspace.clone()));
            }
            Some(RowFocus::Fold) => return None,
            Some(RowFocus::Terminal(host, id)) => {
                let workspace = self.core.store.read(cx).terminal_workspace(host, id)?;
                return Some((host.clone(), workspace));
            }
            Some(RowFocus::Subagents(..)) | None => {}
        }
        let (host, agent) = match &self.focused_row {
            Some(RowFocus::Subagents(host, agent)) => (host, agent),
            _ => self
                .focused_agent
                .as_ref()
                .map(|(host, agent)| (host, agent))?,
        };
        let workspace = self
            .core
            .store
            .read(cx)
            .fleet
            .agent(host, agent)?
            .workspace_id
            .clone()?;
        Some((host.clone(), workspace))
    }

    pub(super) fn row_generation(&self, lane: &Lane, cx: &App) -> (u64, u64) {
        use std::hash::{Hash, Hasher};
        let mut folds = std::collections::hash_map::DefaultHasher::new();
        for card in &lane.cards {
            self.subagents_open
                .get(&card.host)
                .is_some_and(|ids| ids.contains(&card.agent_id))
                .hash(&mut folds);
        }
        let mut terminals = std::collections::hash_map::DefaultHasher::new();
        let store = self.core.store.read(cx);
        for workspace in &lane.workspaces {
            store
                .workspace_terminals_revision(&workspace.host, &workspace.workspace_id)
                .hash(&mut terminals);
        }
        (folds.finish(), terminals.finish())
    }

    /// A grouped lane's rows, with this board's folds. See [`lane_rows`].
    pub(super) fn rows(&self, lane: &Lane, box_single: bool, cx: &App) -> Vec<Row> {
        let store = self.core.store.read(cx);
        lane_rows(
            lane,
            box_single,
            self.empty_open.contains(&lane.key),
            &|card: &LaneCard| {
                self.subagents_open
                    .get(&card.host)
                    .is_some_and(|ids| ids.contains(&card.agent_id))
            },
            &|host, workspace| store.workspace_terminal_ids(host, workspace).count(),
        )
    }

    pub(super) fn toggle_subagents(
        &mut self,
        lane: &str,
        host: &HostId,
        agent: &str,
        cx: &mut Context<Self>,
    ) {
        let ids = self.subagents_open.entry(host.clone()).or_default();
        if !ids.remove(agent) {
            ids.insert(agent.to_string());
        }
        self.focus_row(
            lane,
            RowFocus::Subagents(host.clone(), agent.to_string()),
            cx,
        );
    }

    /// Lanes in the order they are shown and navigated, each workspace's
    /// cards together. See [`group_workspaces`].
    pub(super) fn arranged(&self, lanes: &LaneSet, cx: &App) -> LaneSet {
        let nav = self.core.nav.read(cx);
        let mut cache = self.arranged_cache.borrow_mut();
        // Pins and card order changed: every lane is grouped again.
        if self.arranged_revision.replace(nav.ordering_revision) != nav.ordering_revision {
            cache.clear();
        }
        let placement = Placement {
            pinned: &nav.pinned_workspaces,
            order: &nav.card_order,
        };
        let current = self.lanes_cache.borrow();
        if let Some(current) = current.as_ref() {
            let keys: HashSet<_> = current.lanes.iter().map(|lane| &lane.key).collect();
            cache.retain(|key, _| keys.contains(key));
        }
        let mut rearranged = false;
        let out: Vec<Rc<Lane>> = lanes
            .iter()
            .map(|lane| {
                if let Some((source, grouped)) = cache.get(&lane.key)
                    && Rc::ptr_eq(source, lane)
                {
                    rearranged |= !Rc::ptr_eq(grouped, lane);
                    return grouped.clone();
                }
                let grouped =
                    group_workspaces(lane, &placement).map_or_else(|| lane.clone(), Rc::new);
                rearranged |= !Rc::ptr_eq(&grouped, lane);
                cache.insert(lane.key.clone(), (lane.clone(), grouped.clone()));
                grouped
            })
            .collect();
        if rearranged {
            Rc::new(out)
        } else {
            lanes.clone()
        }
    }

    pub(super) fn reveal_lane(&mut self, key: &str, cx: &mut Context<Self>) {
        // A lane in a collapsed band shows with its band.
        let band = (self.current_lanes(cx).iter())
            .find(|lane| lane.key == key)
            .and_then(|lane| lane.band.as_ref().filter(|band| band.collapsed))
            .map(|band| fleet::lanes::band_collapse_key(&band.key));
        self.core.nav.update(cx, |nav, cx| {
            nav.set_lane_collapsed(key, false, cx);
            if let Some(band) = &band {
                nav.set_lane_collapsed(band, false, cx);
            }
        });
        let lanes = self.visible_lanes(cx);
        if let Some(index) = lanes.iter().position(|lane| lane.key == key) {
            self.side.handle.scroll_to_item(index);
        }
        cx.notify();
    }

    pub(super) fn restore_lane(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.reveal_lane(key, cx);
        let lanes = self.visible_lanes(cx);
        if let Some(lane) = lanes.iter().find(|lane| lane.key == key) {
            self.focused_lane = Some(key.to_string());
            self.focused_agent = self.expanded.get(key).cloned().or_else(|| {
                lane.cards
                    .first()
                    .map(|card| (card.host.clone(), card.agent_id.clone()))
            });
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn hide_lane(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let lanes = self.current_lanes(cx);
        if let Some(index) = lanes
            .iter()
            .position(|lane| lane.key == key && !lane.collapsed)
        {
            let next = next_visible_lane(&lanes, index);
            self.core
                .nav
                .update(cx, |nav, cx| nav.set_lane_collapsed(key, true, cx));
            if self.focused_lane.as_deref() == Some(key) {
                self.focused_lane = next;
                self.focused_agent = None;
                self.move_column(0, cx);
            }
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn prune_expanded(&mut self, cx: &mut Context<Self>) {
        self.reconcile_expansions(None, cx);
    }

    pub(super) fn reconcile_expansions(
        &mut self,
        requested: Option<(&(HostId, String), &str)>,
        cx: &mut Context<Self>,
    ) {
        if self.expanded.is_empty() && self.pending.is_none() {
            self.prune_label_pickers(cx);
            self.prune_conversations(cx);
            self.sync_pins(cx);
            return;
        }
        let lanes = self.current_lanes(cx);
        if let Some((lane, host, agent)) = self.pending.clone()
            && let Some(index) = placement::destination(&lanes, &lane, &host, &agent, &[], false)
        {
            let found = &lanes[index];
            if found.key != lane {
                self.expanded.remove(&lane);
                self.expanded
                    .insert(found.key.clone(), (host.clone(), agent.clone()));
            }
            self.pending = None;
        }
        let pending = self.pending.clone();
        let job = self.relabel_pending.as_ref();
        let protected = self.protected_agents();
        // Compute the whole new map before replacing it: sequential moves lose
        // an expansion when two workspaces exchange lanes.
        let relocated = placement::relocate(
            &self.expanded,
            &lanes,
            &protected,
            self.focused_agent.as_ref(),
            requested,
        );
        self.expanded = relocated;
        self.expanded.retain(|lane, (host, agent)| {
            lanes.iter().any(|item| {
                &item.key == lane
                    && item
                        .cards
                        .iter()
                        .any(|card| &card.host == host && &card.agent_id == agent)
            }) || pending
                .as_ref()
                .is_some_and(|(key, pending_host, pending_agent)| {
                    key == lane && pending_host == host && pending_agent == agent
                })
                || job.is_some_and(|job| &job.host == host && job.agent.as_ref() == Some(agent))
        });
        self.prune_conversations(cx);
        self.prune_label_pickers(cx);
        self.sync_pins(cx);
    }

    pub(super) fn view_membership(&self, cx: &App) -> placement::ViewMembership {
        placement::view_membership(&self.core.store.read(cx).fleet, self.focused_agent.as_ref())
    }

    fn protected_agents(&self) -> Vec<(HostId, String)> {
        self.relabel_pending
            .as_ref()
            .and_then(|job| Some((job.host.clone(), job.agent.clone()?)))
            .into_iter()
            .chain(
                self.pending
                    .as_ref()
                    .map(|(_, host, agent)| (host.clone(), agent.clone())),
            )
            .collect()
    }

    pub(super) fn reconcile_view(&mut self, follow: bool, cx: &mut Context<Self>) {
        self.reconcile_view_at(follow, None, cx);
    }

    pub(super) fn reconcile_view_at(
        &mut self,
        follow: bool,
        preferred: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        // Batch relabeling owns its final relocation; do not follow intermediate
        // label pushes for that focused conversation.
        let protected_focus = self
            .focused_agent
            .as_ref()
            .is_some_and(|agent| self.protected_agents().contains(agent));
        if self.focused_lane.is_none() || protected_focus {
            self.ensure_visible_focus(cx);
            self.view_membership = Some(self.view_membership(cx));
            return;
        }
        let lanes = self.current_lanes(cx);
        let outcome = placement::reconcile(
            &lanes,
            placement::ViewInputs {
                source: self.focused_lane.as_deref(),
                preferred,
                focused: self.focused_agent.as_ref(),
                follow,
            },
        );
        self.focused_lane = outcome.focused_lane;
        self.focused_agent = outcome.focused_agent;
        self.ensure_visible_focus(cx);
        if let Some(index) = outcome.scroll_to {
            // Reactive reconciliation must respect explicitly hidden lanes.
            // Only restore actions and explicit relabel follow reveal them.
            let visible = self.visible_lanes(cx);
            if self.focused_lane.as_deref() == Some(lanes[index].key.as_str())
                && let Some(index) = visible.iter().position(|lane| lane.key == lanes[index].key)
            {
                self.side.handle.scroll_to_item(index);
            }
        }
        self.view_membership = Some(self.view_membership(cx));
    }

    /// Reconciliation may move a conversation to a hidden lane without opening
    /// it. Keep its view state there, but keyboard focus stays on the board.
    fn ensure_visible_focus(&mut self, cx: &mut Context<Self>) {
        let lanes = self.current_lanes(cx);
        let resolved = visible_focus_lane(&lanes, self.focused_lane.as_deref());
        if resolved != self.focused_lane {
            self.focused_lane = resolved;
            self.focused_agent = None;
            self.move_column(0, cx);
        }
    }

    pub(super) fn prune_label_pickers(&mut self, cx: &App) {
        self.label_pickers
            .retain(|(lane, host, agent, workspace), _| {
                self.expanded
                    .get(lane)
                    .is_some_and(|pair| pair == &(host.clone(), agent.clone()))
                    && self
                        .core
                        .store
                        .read(cx)
                        .fleet
                        .agent(host, agent)
                        .is_some_and(|agent| agent.workspace_id.as_ref() == Some(workspace))
                    || self.relabel_pending.as_ref().is_some_and(|job| {
                        &job.source == lane
                            && &job.host == host
                            && job.agent.as_ref() == Some(agent)
                            && &job.workspace == workspace
                    })
            });
    }

    fn prune_conversations(&mut self, cx: &App) {
        let keep: HashSet<(HostId, String)> = self.expanded.values().cloned().collect();
        let released: Vec<_> = self
            .conversations
            .keys()
            .filter(|key| !keep.contains(*key))
            .cloned()
            .collect();
        for key in released {
            self.release_conversation(&key, cx);
        }
    }

    /// Unsent text and attachments outlive their expansion. Every close,
    /// replacement, and directory-driven relocation uses the same rule.
    pub(super) fn release_conversation(&mut self, key: &(HostId, String), cx: &App) {
        if self.expanded.values().any(|expanded| expanded == key) {
            return;
        }
        if self
            .conversations
            .get(key)
            .is_some_and(|view| view.read(cx).composer_is_blank(cx))
        {
            self.conversations.remove(key);
        }
    }

    pub(super) fn sync_pins(&self, cx: &mut Context<Self>) {
        let pins: BTreeSet<_> = self
            .expanded
            .values()
            .cloned()
            .chain(
                self.conversations
                    .iter()
                    .filter(|(_, view)| !view.read(cx).composer_is_blank(cx))
                    .map(|(key, _)| key.clone()),
            )
            // Keep drafts for a temporarily missing host without repeatedly
            // subscribing to a deleted or archived agent.
            .filter(|(host, agent)| {
                self.core
                    .store
                    .read(cx)
                    .fleet
                    .agent(host, agent)
                    .is_some_and(|agent| agent.archived_at.is_none())
            })
            .collect();
        self.core
            .nav
            .update(cx, |nav, cx| nav.set_pinned_sessions(pins, cx));
        let terminals: BTreeSet<_> = self.expanded_terminals.values().cloned().collect();
        self.core
            .nav
            .update(cx, |nav, cx| nav.set_pinned_terminals(terminals, cx));
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    use fleet::lanes::{BACKGROUND, LaneKind, LaneWorkspace, UNLABELLED};

    fn card(host: &str, provider: &str, workspace: &str) -> LaneCard {
        LaneCard {
            host: HostId::new(host),
            agent_id: format!("{host}-{provider}-{workspace}"),
            workspace_id: Some(workspace.into()),
            workspace_name: workspace.into(),
            title: String::new(),
            provider: provider.into(),
            model: None,
            updated_at: None,
            bucket: fleet::Bucket::Done,
            attention_reason: None,
            parent: None,
        }
    }

    fn lane(key: &str, cards: Vec<LaneCard>, workspaces: &[(&str, &str)]) -> Rc<Lane> {
        Rc::new(Lane {
            key: key.into(),
            kind: LaneKind::Label,
            title: key.into(),
            color: None,
            label_keys: vec![key.into()],
            collapsed: false,
            band: None,
            hosts: vec![HostId::new("mac"), HostId::new("box")],
            cards,
            workspaces: workspaces
                .iter()
                .map(|(host, id)| LaneWorkspace {
                    host: HostId::new(*host),
                    workspace_id: (*id).into(),
                    name: (*id).into(),
                    background: false,
                })
                .collect(),
        })
    }

    #[test]
    fn filters_keep_matching_cards_and_drop_emptied_lanes() {
        let lanes = vec![
            lane(
                "a",
                vec![card("mac", "claude/opus", "w1"), card("box", "codex", "w2")],
                &[("mac", "w1"), ("box", "w2"), ("mac", "empty")],
            ),
            lane("b", vec![card("box", "codex", "w3")], &[("box", "w3")]),
        ];
        let labels = |_: &HostId, workspace: Option<&str>, _: Option<&str>| -> Vec<String> {
            match workspace {
                Some("w1") => vec!["infra".into()],
                Some("w3") => vec![BACKGROUND.into()],
                _ => vec![UNLABELLED.into()],
            }
        };
        let none = LaneFilter::default();
        let all = filter_lanes(&lanes, &none, labels);
        assert!(Rc::ptr_eq(&all[0], &lanes[0]) && Rc::ptr_eq(&all[1], &lanes[1]));

        // A host: only its cards and workspaces, and lane b goes.
        let mac = LaneFilter {
            hosts: [HostId::new("mac")].into(),
            ..Default::default()
        };
        let shown = filter_lanes(&lanes, &mac, labels);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].cards.len(), 1);
        assert_eq!(shown[0].workspaces.len(), 2);
        assert_eq!(shown[0].hosts, [HostId::new("mac")]);

        // A tool: by provider id, ignoring the model; an empty workspace
        // has no agent of that tool, so it goes.
        let claude = LaneFilter {
            tools: ["claude".to_string()].into(),
            ..Default::default()
        };
        let shown = filter_lanes(&lanes, &claude, labels);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].workspaces.len(), 1);

        // A label: by the workspace's labels.
        let infra = LaneFilter {
            labels: ["infra".to_string()].into(),
            ..Default::default()
        };
        let shown = filter_lanes(&lanes, &infra, labels);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].cards[0].workspace_id.as_deref(), Some("w1"));

        // Background: exactly the background workspace, and Unlabelled the
        // others with no label, empty ones included.
        let background = LaneFilter {
            labels: [BACKGROUND.to_string()].into(),
            ..Default::default()
        };
        let shown = filter_lanes(&lanes, &background, labels);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].key, "b");
        let unlabelled = LaneFilter {
            labels: [UNLABELLED.to_string()].into(),
            ..Default::default()
        };
        let shown = filter_lanes(&lanes, &unlabelled, labels);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].cards.len(), 1);
        assert_eq!(shown[0].workspaces.len(), 2, "w2 and the empty one");

        // Kinds combine: codex on mac matches nothing.
        let both = LaneFilter {
            hosts: [HostId::new("mac")].into(),
            tools: ["codex".to_string()].into(),
            ..Default::default()
        };
        assert!(filter_lanes(&lanes, &both, labels).is_empty());
    }
}

#[cfg(test)]
mod collapsed_tab_tests {
    use super::*;
    use fleet::lanes::LaneKind;

    fn lane(key: &str, collapsed: bool) -> Rc<Lane> {
        Rc::new(Lane {
            key: key.into(),
            title: key.into(),
            kind: LaneKind::Other,
            color: None,
            label_keys: Vec::new(),
            hosts: Vec::new(),
            cards: Vec::new(),
            workspaces: Vec::new(),
            collapsed,
            band: None,
        })
    }

    #[test]
    fn displayed_snapshot_excludes_hidden_lanes_and_preserves_order() {
        let lanes = vec![
            lane("hidden-first", true),
            lane("a", false),
            lane("hidden-middle", true),
            lane("b", false),
        ];
        let visible = displayed_lanes(&lanes);
        assert_eq!(
            visible
                .iter()
                .map(|lane| lane.key.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(Rc::ptr_eq(&visible[1], &lanes[3]));
    }

    #[test]
    fn displayed_snapshot_tracks_restored_flags_in_an_empty_board() {
        let mut lanes = vec![lane("a", true), lane("b", true), lane("c", true)];
        assert!(displayed_lanes(&lanes).is_empty());
        Rc::make_mut(&mut lanes[1]).collapsed = false;
        assert_eq!(displayed_lanes(&lanes)[0].key, "b");
        Rc::make_mut(&mut lanes[0]).collapsed = false;
        Rc::make_mut(&mut lanes[2]).collapsed = false;
        let visible = displayed_lanes(&lanes);
        assert_eq!(visible[1].key, "b");
        assert_eq!(visible[2].key, "c");
    }
}
