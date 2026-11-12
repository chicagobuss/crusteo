//! Relabelling from the board: the label pickers, moving cards and
//! workspaces between lanes, and following them there.

use fleet::HostId;
use std::time::Duration;

use fleet::lanes::{Lane, LaneGrouping, LaneKind, order_with_new_lanes};
use gpui_kit::gpui::{App, AppContext, Context, Entity, Window};

use crate::app::store::{LabelBatch, LabelChange};
use crate::widgets::label_picker::{ApplyLabels, LabelPicker};

use super::*;

pub(super) struct RelabelPending {
    pub(super) source: String,
    pub(super) host: HostId,
    /// The card relabeled from, followed to its new lane; None for a
    /// workspace relabeled from its box.
    pub(super) agent: Option<String>,
    pub(super) workspace: String,
    /// In reef mode, the agents whose own labels changed; the progress
    /// then names labels by id. Empty when the workspace's changed.
    pub(super) agents: Vec<String>,
    pub(super) progress: placement::Progress,
    pub(super) follow: bool,
    pub(super) token: u64,
    /// Every lane key, in board order, when the relabel began: a lane not
    /// among them is new, and is placed right after `source`.
    pub(super) known: Vec<String>,
}

/// The labels a workspace made from `lane` takes so it lands in that lane:
/// the lane's labels, named and coloured as `host` has them. None for a
/// lane that is not a label's.
pub(super) fn lane_labels(lane: &Lane, host: &HostId, fleet: &fleet::Fleet) -> Vec<LabelChange> {
    if !matches!(lane.kind, LaneKind::Label | LaneKind::Combination) {
        return Vec::new();
    }
    let defs = fleet
        .host(host)
        .map(|host| host.directory.labels.clone())
        .unwrap_or_default();
    lane.label_keys
        .iter()
        .map(|key| {
            let def = defs
                .iter()
                .find(|def| fleet::directory::label_key(&def.name) == *key);
            LabelChange {
                name: def.map(|def| def.name.clone()).unwrap_or_else(|| {
                    if lane.label_keys.len() == 1 {
                        lane.title.clone()
                    } else {
                        key.clone()
                    }
                }),
                color: def
                    .map(|def| def.color.clone())
                    .or_else(|| lane.color.clone())
                    .unwrap_or_else(|| "violet".into()),
                assigned: true,
            }
        })
        .collect()
}

impl Lanes {
    pub(super) fn label_picker(
        &mut self,
        lane: &str,
        host: &HostId,
        agent: &str,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<LabelPicker> {
        let key = (
            lane.to_string(),
            host.clone(),
            agent.to_string(),
            workspace.to_string(),
        );
        if let Some(picker) = self.label_pickers.get(&key) {
            return picker.clone();
        }
        let picker = self.new_label_picker(lane, host, Some(agent), false, workspace, window, cx);
        self.label_pickers.insert(key, picker.clone());
        picker
    }

    /// A picker for `workspace`'s labels that relabels it from `lane`,
    /// following `agent`'s card when there is one. In reef mode it changes
    /// that agent's own labels, or, for a box (`whole_box`), those of the
    /// box's agents in the lane.
    #[allow(clippy::too_many_arguments)]
    fn new_label_picker(
        &mut self,
        lane: &str,
        host: &HostId,
        agent: Option<&str>,
        whole_box: bool,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<LabelPicker> {
        let core = self.core.clone();
        let card = agent.filter(|_| !whole_box);
        let agents = self.relabel_targets(lane, host, card, workspace, cx);
        let picker =
            cx.new(|cx| LabelPicker::new(core, host, workspace, agents.clone(), window, cx));
        cx.observe(&picker, |_, _, cx| cx.notify()).detach();
        let source = lane.to_string();
        let host = host.clone();
        let agent = agent.map(str::to_string);
        let workspace = workspace.to_string();
        cx.subscribe(&picker, move |this, _, event: &ApplyLabels, cx| {
            this.start_relabel(
                &source,
                &host,
                agent.as_deref(),
                &workspace,
                &agents,
                event.0.clone(),
                cx,
            );
        })
        .detach();
        picker
    }

    /// Reef mode: once per catalog revision, lane keys saved by label name
    /// become label ids ([`crate::app::nav::Nav::adopt_label_ids`]).
    pub(super) fn adopt_label_ids(&mut self, cx: &mut Context<Self>) {
        let store = self.core.store.read(cx);
        let Some(catalog) = &store.fleet.labels else {
            return;
        };
        if self.labels_adopted == Some(catalog.revision) {
            return;
        }
        self.labels_adopted = Some(catalog.revision);
        let catalog = catalog.clone();
        self.core
            .nav
            .update(cx, |nav, cx| nav.adopt_label_ids(&catalog, cx));
    }

    /// In reef mode, whose labels a picker changes: the card's agent, or,
    /// from a workspace's box, its agents in that lane. Empty outside reef
    /// mode, and for a box with no agents: those change the workspace's
    /// Paseo labels, which its agents without their own inherit.
    fn relabel_targets(
        &self,
        lane: &str,
        host: &HostId,
        agent: Option<&str>,
        workspace: &str,
        cx: &App,
    ) -> Vec<String> {
        if self.core.store.read(cx).fleet.labels.is_none() {
            return Vec::new();
        }
        if let Some(agent) = agent {
            return vec![agent.to_string()];
        }
        let lanes = self.current_lanes(cx);
        let Some(lane) = lanes.iter().find(|l| l.key == lane) else {
            return Vec::new();
        };
        (lane.cards.iter())
            .filter(|card| &card.host == host && card.workspace_id.as_deref() == Some(workspace))
            .map(|card| card.agent_id.clone())
            .collect()
    }

    /// Opens the label picker for a workspace box's header.
    pub(super) fn open_box_picker(
        &mut self,
        lane: &str,
        host: &HostId,
        agent: Option<&str>,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picker = self.new_label_picker(lane, host, agent, true, workspace, window, cx);
        picker.update(cx, |picker, cx| picker.set_open(true, window, cx));
        // The header shows the picker while it is open.
        cx.observe(&picker, |this, picker, cx| {
            if !picker.read(cx).is_open()
                && this
                    .box_picker
                    .as_ref()
                    .is_some_and(|open| open.picker == picker)
            {
                this.box_picker = None;
                cx.notify();
            }
        })
        .detach();
        self.box_picker = Some(BoxPicker {
            lane: lane.to_string(),
            host: host.clone(),
            workspace: workspace.to_string(),
            picker,
        });
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn start_relabel(
        &mut self,
        source: &str,
        host: &HostId,
        agent: Option<&str>,
        workspace: &str,
        agents: &[String],
        changes: Vec<LabelChange>,
        cx: &mut Context<Self>,
    ) {
        if self.relabel_pending.is_some() {
            self.core.store.update(cx, |store, cx| {
                store.toast(
                    "Another label update is still in progress. Please try again.",
                    cx,
                )
            });
            return;
        }
        if let Some(agent) = agent
            && !self
                .core
                .store
                .read(cx)
                .fleet
                .agent(host, agent)
                .is_some_and(|agent| agent.workspace_id.as_deref() == Some(workspace))
        {
            self.core.store.update(cx, |store, cx| store.toast("This card moved to a different workspace. Reopen Relabel to update its labels.", cx));
            return;
        }
        self.next_relabel += 1;
        let token = self.next_relabel;
        let known = self
            .current_lanes(cx)
            .iter()
            .map(|lane| lane.key.clone())
            .collect();
        self.relabel_pending = Some(RelabelPending {
            known,
            source: source.into(),
            host: host.clone(),
            agent: agent.map(str::to_string),
            workspace: workspace.into(),
            agents: agents.to_vec(),
            progress: placement::Progress::Writing,
            follow: self.core.nav.read(cx).follow_relabel,
            token,
        });
        let task = if agents.is_empty() {
            (self.core.store.read(cx)).set_workspace_labels(host, workspace, changes, cx)
        } else {
            let agents = agents.to_vec();
            (self.core.store).update(cx, |store, cx| {
                store.relabel_agents(host, agents, changes, cx)
            })
        };
        cx.spawn(async move |this, cx| {
            let batch = task.await;
            let _ = this.update(cx, |this, cx| this.label_batch_done(token, batch, cx));
        })
        .detach();
        cx.notify();
    }

    fn label_batch_done(&mut self, token: u64, batch: LabelBatch, cx: &mut Context<Self>) {
        let Some(job) = &mut self.relabel_pending else {
            return;
        };
        if job.token != token {
            return;
        }
        job.progress = placement::Progress::Acknowledged(batch.applied);
        if let Some(error) = batch.error {
            self.core
                .store
                .update(cx, |store, cx| store.toast(error, cx));
        }
        self.finish_relabel(false, cx);
        if self.relabel_pending.is_some() {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(10)).await;
                let _ = this.update(cx, |this, cx| {
                    if this.relabel_pending.as_ref().is_some_and(|job| job.token == token) {
                        this.core.store.update(cx, |store, cx| store.toast(
                            "Labels were acknowledged, but the updated workspace has not arrived yet.", cx
                        ));
                        this.finish_relabel(true, cx);
                    }
                });
            }).detach();
        }
        cx.notify();
    }

    pub(super) fn finish_relabel(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(job) = &self.relabel_pending else {
            return;
        };
        let store = self.core.store.read(cx);
        let landed = if job.agents.is_empty() {
            let labels = (store.fleet.workspace(&job.host, &job.workspace))
                .map(|workspace| workspace.labels.as_slice());
            job.progress.can_finish(labels, force)
        } else {
            // Each agent's labels, by id, as the progress names them.
            let catalog = store.fleet.labels.as_ref();
            job.agents.iter().all(|agent| {
                // An agent gone meanwhile has nothing to wait for.
                let (Some(found), Some(catalog)) = (store.fleet.agent(&job.host, agent), catalog)
                else {
                    return job.progress.can_finish(None, true);
                };
                let workspace = (found.workspace_id.as_deref())
                    .and_then(|w| store.fleet.workspace(&job.host, w));
                let ids: Vec<String> = (fleet::labels::effective(found, workspace, catalog).iter())
                    .map(|id| id.to_string())
                    .collect();
                job.progress.can_finish(Some(&ids), force)
            })
        };
        if !landed {
            return;
        }
        let job = self.relabel_pending.take().unwrap();
        self.place_new_lanes(&job.known, &job.source, cx);
        let Some(job_agent) = job.agent.clone() else {
            // A workspace relabeled from its box has no card to follow.
            self.reconcile_expansions(None, cx);
            self.reconcile_view(false, cx);
            self.sync_pins(cx);
            cx.notify();
            return;
        };
        let lanes = self.current_lanes(cx);
        let label_grouping = self.core.nav.read(cx).lane_grouping == LaneGrouping::Label;
        let destination = placement::destination(
            &lanes,
            &job.source,
            &job.host,
            &job_agent,
            job.progress.changes(),
            label_grouping,
        );
        let agent = (job.host.clone(), job_agent);
        let was_expanded = self.expanded.values().any(|pair| pair == &agent);
        let requested = destination
            .filter(|_| was_expanded)
            .map(|index| (&agent, lanes[index].key.as_str()));
        self.reconcile_expansions(requested, cx);
        if let Some(index) = destination.filter(|_| was_expanded) {
            let lane = &lanes[index];
            if job.follow {
                self.focused_lane = Some(lane.key.clone());
                self.focused_agent = Some(agent.clone());
                self.focused_row = None;
                self.reveal_lane(&lane.key, cx);
            }
        }
        let preferred = destination
            .filter(|_| was_expanded && self.focused_agent.as_ref() == Some(&agent))
            .map(|index| lanes[index].key.as_str());
        self.reconcile_view_at(job.follow, preferred, cx);
        self.sync_pins(cx);
        cx.notify();
    }

    /// Lanes a relabel created appear right after the lane it was made from,
    /// rather than wherever a new lane sorts. This fixes the board's order
    /// as it stands, as moving a lane by hand does.
    fn place_new_lanes(&mut self, known: &[String], source: &str, cx: &mut Context<Self>) {
        let current: Vec<String> = self
            .current_lanes(cx)
            .iter()
            .map(|lane| lane.key.clone())
            .collect();
        let Some(order) = order_with_new_lanes(known, source, &current) else {
            return;
        };
        self.core.nav.update(cx, |nav, cx| {
            nav.lane_order = order;
            nav.save();
            cx.notify();
        });
    }
}
