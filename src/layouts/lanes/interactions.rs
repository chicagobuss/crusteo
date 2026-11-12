//! Keyboard and pointer: moving between columns and cards, activating,
//! and the drags that reorder lanes and cards.

use fleet::HostId;

use fleet::lanes::{Family, Lane, LaneCard};
use gpui_kit::gpui::{
    AnyElement, AppContext, Context, Div, DragMoveEvent, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Pixels, Point, SharedString, Stateful, StatefulInteractiveElement,
    Styled, Window, div, px,
};

use crate::layouts::columns::COLUMNS_PADDING;
use crate::theme::palette;

use super::*;

/// A lane being dragged by its grip, onto another lane.
#[derive(Clone)]
pub(super) struct LaneDrag {
    pub(super) key: String,
    pub(super) title: SharedString,
}

/// A card being dragged to another place in its workspace's box.
#[derive(Clone)]
struct CardDrag {
    pub(super) lane: String,
    pub(super) host: HostId,
    pub(super) agent: String,
    pub(super) workspace: String,
    pub(super) title: SharedString,
}

/// Where a dragged lane would land: the board shows its lanes in `order`,
/// the keys of the lanes on screen, until it is dropped or let go.
pub(super) struct LaneDragPreview {
    pub(super) key: String,
    pub(super) order: Vec<String>,
}

impl Lanes {
    pub(super) fn move_column(&mut self, delta: isize, cx: &mut Context<Self>) {
        let lanes = self.visible_lanes(cx);
        if lanes.is_empty() {
            return;
        }
        let index = self
            .focused_lane
            .as_ref()
            .and_then(|key| lanes.iter().position(|lane| &lane.key == key))
            .unwrap_or(0);
        let next = (index as isize + delta).clamp(0, lanes.len() as isize - 1) as usize;
        let lanes = self.arranged(&lanes, cx);
        let lane = &lanes[next];
        self.focused_row = None;
        let keep = self.focused_agent.as_ref().is_some_and(|(host, agent)| {
            lane.cards
                .iter()
                .any(|card| &card.host == host && &card.agent_id == agent)
        });
        if !keep {
            self.focused_agent = lane
                .cards
                .first()
                .map(|card| (card.host.clone(), card.agent_id.clone()));
        }
        self.focused_lane = Some(lane.key.clone());
        self.side.handle.scroll_to_item(next);
        cx.notify();
    }

    fn move_card(&mut self, delta: isize, cx: &mut Context<Self>) {
        let lanes = self.arranged(&self.current_lanes(cx), cx);
        if self.focused_lane.is_none() {
            self.move_column(0, cx);
            return;
        }
        let Some(lane) = lanes
            .iter()
            .find(|lane| Some(&lane.key) == self.focused_lane.as_ref())
            .cloned()
        else {
            return;
        };
        if lane.collapsed {
            self.move_column(0, cx);
            return;
        }
        /// Where focus can rest, with the list row it is drawn in.
        enum Slot {
            Card((HostId, String), usize),
            Row(RowFocus, usize),
        }
        let mut slots = Vec::new();
        let box_single = self.core.nav.read(cx).box_single_workspaces;
        let rows = self.rows(&lane, box_single, cx);
        for (ix, row) in rows.into_iter().enumerate() {
            match row {
                Row::Header { .. } => {}
                Row::Card { card, .. } => {
                    let card = &lane.cards[card];
                    slots.push(Slot::Card((card.host.clone(), card.agent_id.clone()), ix));
                }
                Row::Fold { .. } => slots.push(Slot::Row(RowFocus::Fold, ix)),
                Row::Subagents { card, .. } => {
                    let card = &lane.cards[card];
                    let focus = RowFocus::Subagents(card.host.clone(), card.agent_id.clone());
                    slots.push(Slot::Row(focus, ix));
                }
                Row::Terminal { owner, index, .. } => {
                    if let Some((host, workspace)) = owner.workspace(&lane)
                        && let Some(id) = self
                            .core
                            .store
                            .read(cx)
                            .workspace_terminal_ids(host, workspace)
                            .nth(index)
                    {
                        let focus = RowFocus::Terminal(host.clone(), id.clone());
                        slots.push(Slot::Row(focus, ix));
                    }
                }
                Row::Empty { workspace, .. } => {
                    let ws = &lane.workspaces[workspace];
                    let focus = RowFocus::Workspace(ws.host.clone(), ws.workspace_id.clone());
                    slots.push(Slot::Row(focus, ix));
                }
            }
        }
        if slots.is_empty() {
            return;
        }
        let current = if let Some(focus) = &self.focused_row {
            slots
                .iter()
                .position(|slot| matches!(slot, Slot::Row(row, _) if row == focus))
                .unwrap_or(0)
        } else {
            self.focused_agent
                .as_ref()
                .and_then(|agent| {
                    slots
                        .iter()
                        .position(|slot| matches!(slot, Slot::Card(card, _) if card == agent))
                })
                .unwrap_or(0)
        };
        let next = (current as isize + delta).clamp(0, slots.len() as isize - 1) as usize;
        let row = match &slots[next] {
            Slot::Card(agent, row) => {
                self.focused_row = None;
                self.focused_agent = Some(agent.clone());
                *row
            }
            Slot::Row(focus, row) => {
                self.focused_row = Some(focus.clone());
                *row
            }
        };
        if let Some(list) = self.lists.get(&lane.key) {
            list.state.scroll_to_reveal_item(row);
        }
        cx.notify();
    }

    /// Focuses a lane's empty workspace or fold toggle.
    pub(super) fn focus_row(&mut self, lane: &str, focus: RowFocus, cx: &mut Context<Self>) {
        self.focused_lane = Some(lane.to_string());
        self.focused_row = Some(focus);
        cx.notify();
    }

    pub(super) fn toggle_empty(&mut self, lane: &str, cx: &mut Context<Self>) {
        if !self.empty_open.remove(lane) {
            self.empty_open.insert(lane.to_string());
        }
        self.focus_row(lane, RowFocus::Fold, cx);
    }

    fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lane_key) = self.focused_lane.clone() else {
            self.move_column(0, cx);
            return;
        };
        self.reveal_lane(&lane_key, cx);
        match self.focused_row.clone() {
            Some(RowFocus::Fold) => {
                self.toggle_empty(&lane_key, cx);
                return;
            }
            Some(RowFocus::Subagents(host, agent)) => {
                self.toggle_subagents(&lane_key, &host, &agent, cx);
                return;
            }
            Some(RowFocus::Terminal(host, id)) => {
                self.expand_terminal(&lane_key, &host, &id, window, cx);
                return;
            }
            Some(RowFocus::Workspace(host, workspace)) => {
                // An empty workspace's action is a new agent in it.
                if let Some(lane) = self
                    .current_lanes(cx)
                    .iter()
                    .find(|lane| lane.key == lane_key)
                    .cloned()
                {
                    self.open_draft_in(&lane, (host, workspace), window, cx);
                }
                return;
            }
            None => {}
        }
        let Some((host, agent)) = self.focused_agent.clone() else {
            return;
        };
        self.expand(&lane_key, &host, &agent, window, cx);
    }

    /// Escape closes a blank composer, or the focused column's card.
    pub(super) fn on_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let expanded: Vec<_> = self
            .expanded
            .iter()
            .map(|(lane, pair)| (lane.clone(), pair.clone()))
            .collect();
        for (lane, (host, agent)) in &expanded {
            let Some(view) = self.conversations.get(&(host.clone(), agent.clone())) else {
                continue;
            };
            if !view.read(cx).composer_is_focused(window, cx) {
                continue;
            }
            if view.read(cx).composer_is_blank(cx) {
                self.collapse_lane(lane, window, cx);
                return true;
            }
            return false;
        }
        if self.focus.is_focused(window)
            && let Some(lane) = self.focused_lane.clone()
            && self.expanded.contains_key(&lane)
        {
            self.collapse_lane(&lane, window, cx);
            return true;
        }
        false
    }

    /// Positive `dx` reveals later columns.
    pub(super) fn nudge_scroll(&mut self, dx: f32, cx: &mut Context<Self>) {
        self.side.nudge(dx, cx);
    }

    /// Drags an open card's bottom edge or an open column's right edge.
    /// The pointer's part in scrolling is [`columns::listen`]'s.
    pub(super) fn pointer_moved(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if let Some(resize) = &self.resizing_height {
            let height = (resize.start_height + (position.y - resize.start_y))
                .clamp(resize.min_height, resize.max_height.max(resize.min_height))
                .round();
            // Down to the bottom, the card fills the lane again.
            let height = (height < resize.max_height - px(4.)).then_some(height);
            if self.expanded_heights.get(&resize.lane).copied() != height {
                match height {
                    Some(height) => self.expanded_heights.insert(resize.lane.clone(), height),
                    None => self.expanded_heights.remove(&resize.lane),
                };
                cx.notify();
            }
            return;
        }
        if let Some(resize) = &self.resizing {
            let widest = (self.side.handle.bounds().size.width - px(COLUMNS_PADDING * 2.))
                .max(px(EXPANDED_RESIZE_MIN));
            let width = (resize.start_width + (position.x - resize.start_x))
                .clamp(px(EXPANDED_RESIZE_MIN), widest)
                .round();
            if self.expanded_widths.get(&resize.lane) != Some(&width) {
                self.expanded_widths.insert(resize.lane.clone(), width);
                cx.notify();
            }
        }
    }

    pub(super) fn end_resize(&mut self, cx: &mut Context<Self>) {
        if self.resizing.take().is_some() | self.resizing_height.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn stop_middle(&mut self, cx: &mut Context<Self>) -> bool {
        self.side.stop_middle(cx)
    }

    pub(super) fn on_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus.is_focused(window) {
            return;
        }
        let mods = &event.keystroke.modifiers;
        if mods.control || mods.platform || mods.alt {
            return;
        }
        let key = event.keystroke.key.as_str();
        match (key, mods.shift) {
            ("left", true) => self.nudge_scroll(-(self.column_width / px(1.)), cx),
            ("right", true) => self.nudge_scroll(self.column_width / px(1.), cx),
            ("left", false) => self.move_column(-1, cx),
            ("right", false) => self.move_column(1, cx),
            ("up", false) => self.move_card(-1, cx),
            ("down", false) => self.move_card(1, cx),
            ("enter", false) => self.activate(window, cx),
            ("f", false) => {
                if let (Some(lane), Some((host, agent))) =
                    (self.focused_lane.clone(), self.focused_agent.clone())
                {
                    self.toggle_focus_mode(&lane, &host, &agent, window, cx);
                }
            }
            ("escape", false) => {
                self.on_escape(window, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    pub(super) fn card_focused(&self, lane: &str, card: &LaneCard) -> bool {
        self.focused_row.is_none()
            && self.focused_lane.as_deref() == Some(lane)
            && self
                .focused_agent
                .as_ref()
                .is_some_and(|(host, agent)| host == &card.host && agent == &card.agent_id)
    }

    /// Moves a lane one place left or right in the board's current order.
    pub(super) fn reorder_lane(&mut self, key: &str, step: isize, cx: &mut Context<Self>) {
        let keys: Vec<String> = self.lanes(cx).iter().map(|lane| lane.key.clone()).collect();
        self.core
            .nav
            .update(cx, |nav, cx| nav.reorder_lane(&keys, key, step, cx));
    }

    /// While a lane is dragged over `over`, shows it there: it takes that
    /// lane's place and the lanes between move aside.
    pub(super) fn preview_lane_drag(&mut self, key: &str, over: &str, cx: &mut Context<Self>) {
        if key == over {
            return;
        }
        let shown = match &self.lane_drag {
            Some(drag) if drag.key == key => drag.order.clone(),
            _ => self
                .arranged(&self.visible_lanes(cx), cx)
                .iter()
                .map(|lane| lane.key.clone())
                .collect(),
        };
        if let Some(order) = crate::app::nav::move_lane_to(&shown, key, over) {
            self.lane_drag = Some(LaneDragPreview {
                key: key.to_string(),
                order,
            });
            cx.notify();
        }
    }

    /// Keeps the order a dragged lane was shown in when it is dropped.
    pub(super) fn drop_lane(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(drag) = self.lane_drag.take().filter(|drag| drag.key == key) else {
            return;
        };
        let keys: Vec<String> = self.lanes(cx).iter().map(|lane| lane.key.clone()).collect();
        self.core.nav.update(cx, |nav, cx| {
            nav.place_lane_as_shown(&keys, &drag.order, key, cx)
        });
        cx.notify();
    }

    /// A lane let go anywhere but on a lane goes back where it was. Deferred
    /// so a drop on a lane, handled by the same release, is seen first.
    pub(super) fn cancel_lane_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lane_drag.is_none() && self.card_drop.is_none() {
            return;
        }
        cx.defer_in(window, |this, _, cx| {
            let lane = this.lane_drag.take().is_some();
            let card = this.card_drop.take().is_some();
            if lane || card {
                cx.notify();
            }
        });
    }

    /// A card in a workspace's box can be dragged to another place in that
    /// box; the order is kept. Subagents go with the agent that started them.
    pub(super) fn reorderable(
        &self,
        shell: Stateful<Div>,
        lane: &Lane,
        card: &LaneCard,
        frame: Frame,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let Some(workspace) = card
            .workspace_id
            .clone()
            .filter(|_| matches!(frame, Frame::Boxed { .. }))
        else {
            return shell;
        };
        let drag = CardDrag {
            lane: lane.key.clone(),
            host: card.host.clone(),
            agent: card.agent_id.clone(),
            workspace: workspace.clone(),
            title: card.title.clone().into(),
        };
        let (lane_key, host, agent) = (lane.key.clone(), card.host.clone(), card.agent_id.clone());
        shell
            .on_drag(drag, |dragged, offset, _, cx| {
                let title = dragged.title.clone();
                cx.new(|_| crate::toolbars::customize::DragChip::new(title, offset))
            })
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<CardDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let dragged = event.drag(cx);
                    let here = dragged.lane == lane_key
                        && dragged.host == host
                        && dragged.workspace == workspace
                        && dragged.agent != agent;
                    let target = here.then(|| {
                        let before = event.event.position.y < event.bounds.center().y;
                        (host.clone(), agent.clone(), before)
                    });
                    if this.card_drop != target {
                        this.card_drop = target;
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(|this, dragged: &CardDrag, _, cx| {
                this.drop_card(dragged, cx);
            }))
    }

    /// Puts a dropped card where the line showed, keeping that order.
    fn drop_card(&mut self, dragged: &CardDrag, cx: &mut Context<Self>) {
        let Some((host, target, before)) = self.card_drop.take() else {
            return;
        };
        let lanes = self.arranged(&self.visible_lanes(cx), cx);
        let Some(lane) = lanes.iter().find(|lane| lane.key == dragged.lane) else {
            return;
        };
        let family = Family::of(lane);
        let mut order: Vec<String> = lane
            .cards
            .iter()
            .enumerate()
            .filter(|(ix, card)| {
                family.root[*ix]
                    && card.host == host
                    && card.workspace_id.as_deref() == Some(dragged.workspace.as_str())
            })
            .map(|(_, card)| card.agent_id.clone())
            .filter(|id| *id != dragged.agent)
            .collect();
        let at = order
            .iter()
            .position(|id| *id == target)
            .map_or(order.len(), |at| if before { at } else { at + 1 });
        order.insert(at, dragged.agent.clone());
        let workspace = dragged.workspace.clone();
        self.core.nav.update(cx, |nav, cx| {
            nav.set_card_order(&host, &workspace, order, cx)
        });
        cx.notify();
    }

    /// A row with the line where a dragged card would land, above or below.
    pub(super) fn drop_line(&self, lane: &Lane, row: &Row, element: AnyElement) -> AnyElement {
        let Some((host, agent, before)) = &self.card_drop else {
            return element;
        };
        let Row::Card { card, .. } = row else {
            return element;
        };
        let card = &lane.cards[*card];
        if card.host != *host || card.agent_id != *agent {
            return element;
        }
        let line = div()
            .absolute()
            .left(px(4.))
            .right(px(4.))
            .h(px(2.))
            .rounded(px(1.))
            .bg(palette().accent);
        div()
            .relative()
            .child(element)
            .child(if *before {
                line.top(px(-2.))
            } else {
                line.bottom(px(-2.))
            })
            .into_any_element()
    }

    /// Moves a lane to the front or the back of the board's current order.
    pub(super) fn move_lane_to_end(&mut self, key: &str, first: bool, cx: &mut Context<Self>) {
        let keys: Vec<String> = self.lanes(cx).iter().map(|lane| lane.key.clone()).collect();
        self.core
            .nav
            .update(cx, |nav, cx| nav.move_lane_to_end(&keys, key, first, cx));
    }
}
