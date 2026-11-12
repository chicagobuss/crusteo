//! Opening things on the board: expanding a card into its conversation,
//! popping it out, focus mode, terminals, renames, and new agents.

use fleet::HostId;
use std::rc::Rc;

use fleet::lanes::{Lane, lane_label_ids};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, CursorStyle, Entity, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Rgba, StatefulInteractiveElement,
    Styled, Window, div, px,
};

use gpui_kit::prelude::FluentBuilder;

use crate::app::store::TerminalWatch;
use crate::layouts::paseo::{Conversation, ConversationEvent, TerminalView};
use crate::theme::{palette, size};
use crate::ui;
use crate::widgets::agent_rename::{AgentRename, RenameDone};
use crate::widgets::new_agent::{self, Place};

use super::*;

impl Lanes {
    pub(super) fn conversation(
        &mut self,
        host: &HostId,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Conversation> {
        let key = (host.clone(), agent.to_string());
        if let Some(view) = self.conversations.get(&key) {
            return view.clone();
        }
        let core = self.core.clone();
        let view = cx.new(|cx| {
            let mut view = Conversation::new(core, host, agent, window, cx);
            // As wide as its lane: the lane's width is the user's to set.
            view.set_wide(true, cx);
            view
        });
        // A Paseo subagent opened from the track expands its own card.
        cx.subscribe_in(
            &view,
            window,
            |this, _, event: &ConversationEvent, window, cx| match event {
                ConversationEvent::OpenAgent { host, agent } => {
                    this.reveal_agent(host, agent, window, cx)
                }
            },
        )
        .detach();
        self.conversations.insert(key, view.clone());
        view
    }

    pub(super) fn expand(
        &mut self,
        lane: &str,
        host: &HostId,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One popped out into its own window is brought forward there.
        let popped = (self.core.nav.read(cx).popouts)
            .get(&(host.clone(), agent.to_string()))
            .copied();
        if let Some(popped) = popped
            && popped
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }
        // An agent opens in place of a terminal open in the lane.
        self.expanded_terminals.remove(lane);
        let previous = self
            .expanded
            .insert(lane.to_string(), (host.clone(), agent.to_string()));
        if let Some(previous) = previous {
            self.release_conversation(&previous, cx);
        }
        self.prune_label_pickers(cx);
        self.focused_lane = Some(lane.to_string());
        self.focused_agent = Some((host.clone(), agent.to_string()));
        self.focused_row = None;
        self.reveal_column = Some(lane.to_string());
        self.sync_pins(cx);
        let view = self.conversation(host, agent, window, cx);
        view.update(cx, |conversation, cx| {
            conversation.focus_composer(window, cx)
        });
        cx.notify();
    }

    /// Closes `agent`'s card in `lane` and opens it in a window of its own.
    pub(super) fn pop_out(
        &mut self,
        lane: &str,
        host: &HostId,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = (host.clone(), agent.to_string());
        let view = self.conversations.get(&key);
        if view.is_some_and(|view| view.read(cx).preparing_attachment(cx)) {
            self.core.store.update(cx, |store, cx| {
                store.toast(
                    "Wait for the image to finish preparing before popping out.",
                    cx,
                )
            });
            return;
        }
        let draft = view.and_then(|view| view.read(cx).draft(cx));
        // Open first, keeping the stream pinned until the new window owns it.
        if matches!(
            crate::popout::pop_out(&self.core, host, agent, draft, cx),
            crate::popout::PopoutResult::Opened
        ) {
            self.focus_mode = None;
            self.collapse_lane(lane, window, cx);
            self.expanded.retain(|_, agent| agent != &key);
            self.conversations.remove(&key);
            self.sync_pins(cx);
            cx.notify();
        }
    }

    pub(super) fn collapse_lane(
        &mut self,
        lane: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((host, agent)) = self.expanded.remove(lane) {
            self.release_conversation(&(host, agent), cx);
        }
        if self.pending.as_ref().is_some_and(|(key, _, _)| key == lane) {
            self.pending = None;
        }
        self.prune_label_pickers(cx);
        self.sync_pins(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Edits an expanded card's title in place.
    pub(super) fn start_rename(
        &mut self,
        lane: &str,
        host: &HostId,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let editor = cx.new(|cx| AgentRename::new(&core, host, agent, window, cx));
        self.rename_lane = Some(lane.to_string());
        let done = cx.subscribe(&editor, |this, _, _: &RenameDone, cx| {
            this.renaming = None;
            cx.notify();
        });
        self.renaming = Some((editor, done));
        cx.notify();
    }

    /// The editor for `host`/`agent`'s title in `lane`, if it is being
    /// renamed there.
    pub(super) fn rename_editor(
        &self,
        lane: &str,
        host: &HostId,
        agent: &str,
        cx: &App,
    ) -> Option<Entity<AgentRename>> {
        self.renaming
            .as_ref()
            .map(|(editor, _)| editor)
            .filter(|editor| editor.read(cx).edits(host, agent))
            .filter(|_| self.rename_lane.as_deref() == Some(lane))
            .cloned()
    }

    /// Shows `agent`'s card large and centered over the dimmed board,
    /// expanding it first, or brings it back to its lane.
    pub(super) fn toggle_focus_mode(
        &mut self,
        lane: &str,
        host: &HostId,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = (lane.to_string(), host.clone(), agent.to_string());
        if self.focus_mode.as_ref() == Some(&target) {
            self.focus_mode = None;
            self.focus.focus(window, cx);
            cx.notify();
            return;
        }
        if self.expanded.get(lane) != Some(&(host.clone(), agent.to_string())) {
            self.expand(lane, host, agent, window, cx);
        }
        self.focus_mode = Some(target);
        if let Some(view) = self.conversations.get(&(host.clone(), agent.to_string())) {
            view.update(cx, |conversation, cx| {
                conversation.focus_composer(window, cx)
            });
        }
        cx.notify();
    }

    /// Whether `agent`'s card in `lane` is shown in focus mode, so its lane
    /// shows where it went instead.
    pub(super) fn in_focus_mode(&self, lane: &str, host: &HostId, agent: &str) -> bool {
        self.focus_mode
            .as_ref()
            .is_some_and(|(l, h, a)| l == lane && h == host && a == agent)
    }

    /// The focused card over a dimmed board. A click on the dimmed board or
    /// Escape brings it back. GPUI cannot blur what is behind it, so the
    /// board is only darkened.
    pub(super) fn focus_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (lane_key, host, agent) = self.focus_mode.clone()?;
        if self.expanded.get(&lane_key) != Some(&(host.clone(), agent.clone())) {
            // Its card was collapsed or moved away.
            self.focus_mode = None;
            return None;
        }
        let lanes = self.arranged(&self.current_lanes(cx), cx);
        let lane = lanes.iter().find(|lane| lane.key == lane_key)?.clone();
        let style = self.lane_style(&lane, cx);
        let card = self.expanded_card(&lane, &host, &agent, style, true, window, cx);
        let viewport = window.viewport_size();
        let width = (viewport.width * 0.72)
            .max(px(520.))
            .min(px(1100.))
            .min(viewport.width - px(32.));
        let height = (viewport.height * 0.86).min(viewport.height - px(32.));
        Some(
            div()
                .id("lanes-focus")
                .absolute()
                .inset_0()
                .occlude()
                .bg(crate::theme::scrim(0.62))
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.focus_mode = None;
                        this.focus.focus(window, cx);
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .id("lanes-focus-card")
                        .w(width)
                        .h(height)
                        .flex()
                        .flex_col()
                        .rounded(px(BOX_RADIUS))
                        .shadow_lg()
                        // Clicks on the card stay on the card.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(card),
                )
                .into_any_element(),
        )
    }

    /// Where a card in focus mode sits in its lane while it is away.
    pub(super) fn focus_placeholder(
        &self,
        lane: &str,
        host: &HostId,
        agent: &str,
        framed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let (lane_key, host, agent) = (lane.to_string(), host.clone(), agent.to_string());
        div()
            .id(eid_parts(
                "lane-focus-placeholder",
                &[&lane_key, host.as_str(), &agent],
            ))
            .h(px(56.))
            .flex_shrink_0()
            .when(framed, |d| {
                d.rounded(px(BOX_RADIUS))
                    .border_1()
                    .border_color(p.border_accent)
            })
            .when(!framed, |d| d.border_t_1().border_color(p.border))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .text_size(size::sm())
            .text_color(p.muted)
            .cursor_pointer()
            .hover(move |d| d.bg(p.surface2))
            .child(ui::icon("maximize-2", 12., p.muted))
            .child("Open in focus · Esc to bring it back")
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.toggle_focus_mode(&lane_key, &host, &agent, window, cx);
            }))
            .into_any_element()
    }

    /// Opens a terminal in its lane, in place of an expanded agent.
    pub(super) fn expand_terminal(
        &mut self,
        lane: &str,
        host: &HostId,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = self.expanded.remove(lane) {
            self.release_conversation(&previous, cx);
        }
        let pair = (host.clone(), id.to_string());
        self.expanded_terminals
            .insert(lane.to_string(), pair.clone());
        self.focused_lane = Some(lane.to_string());
        self.focused_row = Some(RowFocus::Terminal(host.clone(), id.to_string()));
        self.reveal_column = Some(lane.to_string());
        self.sync_pins(cx);
        let view = self.terminal_view(host, id, window, cx);
        view.update(cx, |view, cx| view.focus(window, cx));
        cx.notify();
    }

    pub(super) fn collapse_terminal(
        &mut self,
        lane: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pair) = self.expanded_terminals.remove(lane)
            && !self.expanded_terminals.values().any(|other| other == &pair)
        {
            self.terminal_views.remove(&pair);
        }
        self.sync_pins(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn terminal_view(
        &mut self,
        host: &HostId,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        let key = (host.clone(), id.to_string());
        if let Some(view) = self.terminal_views.get(&key) {
            return view.clone();
        }
        let core = self.core.clone();
        let view = cx.new(|cx| TerminalView::new(core, host, id, window, cx));
        self.terminal_views.insert(key, view.clone());
        view
    }

    /// Asks the store to list the terminals of the workspaces in lanes near
    /// the view, once per set; workspaces outside that set release their terminal-list subscription.
    pub(super) fn watch_terminals(
        &mut self,
        workspaces: Vec<(HostId, String)>,
        cx: &mut Context<Self>,
    ) {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        workspaces.hash(&mut hash);
        let key = hash.finish();
        if key == self.terminal_watch_key {
            return;
        }
        self.terminal_watch_key = key;
        let store = self.core.store.read(cx);
        let wanted: Vec<TerminalWatch> = workspaces
            .into_iter()
            .filter_map(|(host, workspace)| {
                let cwd = store
                    .fleet
                    .workspace(&host, &workspace)?
                    .directory()
                    .to_string();
                (!cwd.is_empty()).then_some(TerminalWatch {
                    host,
                    workspace,
                    cwd,
                })
            })
            .collect();
        let store = self.core.store.clone();
        let nav = self.core.nav.clone();
        cx.defer(move |cx| {
            if nav.read(cx).layout == "lanes" {
                store.update(cx, |store, cx| {
                    store.ensure_terminal_watches("lanes", wanted, cx)
                });
            }
        });
    }

    /// Makes a terminal in a workspace and opens it in `lane`.
    pub(super) fn new_terminal(
        &mut self,
        lane: &str,
        host: &HostId,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cwd) = self
            .core
            .store
            .read(cx)
            .fleet
            .workspace(host, workspace)
            .map(|record| record.directory().to_string())
            .filter(|cwd| !cwd.is_empty())
        else {
            return;
        };
        let task = self.core.store.update(cx, |store, cx| {
            store.create_terminal(host, workspace, &cwd, cx)
        });
        let (lane, host) = (lane.to_string(), host.clone());
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| match result {
                    Ok(id) => this.expand_terminal(&lane, &host, &id, window, cx),
                    Err(error) => this
                        .core
                        .store
                        .update(cx, |store, cx| store.toast(error, cx)),
                });
            });
        })
        .detach();
    }

    /// Edits a workspace's title in place, from its box.
    fn start_workspace_rename(
        &mut self,
        lane: &str,
        host: &HostId,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let editor = cx.new(|cx| AgentRename::workspace(&core, host, workspace, window, cx));
        self.rename_lane = Some(lane.to_string());
        let done = cx.subscribe(&editor, |this, _, _: &RenameDone, cx| {
            this.renaming = None;
            cx.notify();
        });
        self.renaming = Some((editor, done));
        cx.notify();
    }

    /// A workspace's name in its box: its rename editor while it is being
    /// renamed, else the name, which a click starts renaming.
    pub(super) fn workspace_name(
        &self,
        lane: &str,
        host: &HostId,
        workspace: &str,
        name: String,
        color: Rgba,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        if let Some(editor) = self
            .renaming
            .as_ref()
            .map(|(editor, _)| editor)
            .filter(|editor| editor.read(cx).edits_workspace(host, workspace))
            .filter(|_| self.rename_lane.as_deref() == Some(lane))
        {
            return editor.clone().into_any_element();
        }
        let (lane_key, host, workspace) = (lane.to_string(), host.clone(), workspace.to_string());
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .child(
                div()
                    .id(eid_parts(
                        "lane-box-name",
                        &[lane, host.as_str(), &workspace],
                    ))
                    .min_w(px(0.))
                    .truncate()
                    .px(px(3.))
                    .mx(px(-3.))
                    .rounded(px(4.))
                    .text_color(color)
                    .cursor(CursorStyle::IBeam)
                    .hover(move |style| style.bg(p.surface2).text_color(p.foreground))
                    .child(name)
                    .tooltip(|window, cx| Tooltip::new("Rename workspace").build(window, cx))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.start_workspace_rename(&lane_key, &host, &workspace, window, cx);
                    })),
            )
            .into_any_element()
    }

    /// Collapses every expanded card.
    pub(super) fn collapse_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.expanded_terminals.clear();
        self.terminal_views.clear();
        for (_, pair) in std::mem::take(&mut self.expanded) {
            self.release_conversation(&pair, cx);
        }
        self.pending = None;
        self.prune_label_pickers(cx);
        self.sync_pins(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Focuses an agent's card and expands it, showing its lane if it was
    /// collapsed to the tabs and scrolling it into view. Prefers the lane
    /// where it is already expanded, then the focused lane, then the first
    /// lane holding its card.
    pub(super) fn reveal_agent(
        &mut self,
        host: &HostId,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pair = (host.clone(), agent.to_string());
        let lanes = self.current_lanes(cx);
        let holds = |lane: &Rc<Lane>| {
            lane.cards
                .iter()
                .any(|card| &card.host == host && card.agent_id == agent)
        };
        let lane = lanes
            .iter()
            .find(|lane| self.expanded.get(&lane.key) == Some(&pair))
            .or_else(|| {
                lanes.iter().find(|lane| {
                    Some(lane.key.as_str()) == self.focused_lane.as_deref() && holds(lane)
                })
            })
            .or_else(|| lanes.iter().find(|lane| holds(lane)));
        let Some(lane) = lane else {
            return;
        };
        let (key, collapsed) = (lane.key.clone(), lane.collapsed);
        if collapsed {
            self.reveal_lane(&key, cx);
        }
        if self.expanded.get(&key) == Some(&pair) {
            self.focused_lane = Some(key.clone());
            self.focused_agent = Some(pair);
            self.focused_row = None;
            self.reveal_column = Some(key);
            cx.notify();
        } else {
            self.expand(&key, host, agent, window, cx);
        }
    }

    /// Blinks the open card's outline for as long as Settings says, after
    /// a notification opened it.
    pub(super) fn flash_card(&mut self, host: &HostId, agent: &str, cx: &mut Context<Self>) {
        let seconds = self.core.nav.read(cx).notification_flash;
        if seconds == 0 {
            return;
        }
        self.flash_generation += 1;
        let generation = self.flash_generation;
        self.flash = Some(CardFlash {
            host: host.clone(),
            agent: agent.to_string(),
            lit: true,
        });
        cx.notify();
        let steps = (u128::from(seconds) * 1000 / FLASH_STEP.as_millis()) as usize;
        cx.spawn(async move |this, cx| {
            for step in 1..=steps {
                cx.background_executor().timer(FLASH_STEP).await;
                let going = this
                    .update(cx, |this, cx| {
                        if this.flash_generation != generation {
                            return false;
                        }
                        if step == steps {
                            this.flash = None;
                        } else if let Some(flash) = &mut this.flash {
                            flash.lit = !flash.lit;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Whether the card for `host`/`agent` is in the lit half of a blink.
    pub(super) fn flash_lit(&self, host: &HostId, agent: &str) -> bool {
        (self.flash.as_ref())
            .is_some_and(|flash| flash.lit && &flash.host == host && flash.agent == agent)
    }

    /// New agent from a lane: in the lane's first workspace, with the lane's
    /// labels.
    pub(super) fn open_draft(&mut self, lane: &Lane, window: &mut Window, cx: &mut Context<Self>) {
        let place = (lane.workspaces.first())
            .map(|ws| Place::Workspace(ws.host.clone(), ws.workspace_id.clone()))
            .unwrap_or(Place::NewWorkspace(None));
        self.open_new_agent(lane, place, window, cx);
    }

    /// New agent in a workspace of `lane`.
    pub(super) fn open_draft_in(
        &mut self,
        lane: &Lane,
        workspace: (HostId, String),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_new_agent(lane, Place::Workspace(workspace.0, workspace.1), window, cx);
    }

    /// New agent in a new workspace of the project `from` is in.
    pub(super) fn open_new_workspace_draft(
        &mut self,
        lane: &Lane,
        from: (HostId, String),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_new_agent(lane, Place::NewWorkspace(Some(from)), window, cx);
    }

    /// Opens New agent started from `lane`: its workspaces offered first,
    /// its labels on the agent (reef mode) or on a new workspace (else), so
    /// what is made lands in the lane. The agent made opens there.
    fn open_new_agent(
        &mut self,
        lane: &Lane,
        place: Place,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fleet = &self.core.store.read(cx).fleet;
        let labels = lane_label_ids(lane, fleet);
        let workspace_labels = fleet.labels.is_none().then(|| {
            let (lane, store) = (lane.clone(), self.core.store.clone());
            let labels: new_agent::WorkspaceLabels = Rc::new(move |host: &HostId, cx: &App| {
                lane_labels(&lane, host, &store.read(cx).fleet)
            });
            labels
        });
        let start = new_agent::Start {
            place: Some(place),
            preferred: (lane.workspaces.iter())
                .map(|ws| (ws.host.clone(), ws.workspace_id.clone()))
                .collect(),
            labels,
            workspace_labels,
            provider: None,
        };
        let (board, key) = (cx.entity().downgrade(), lane.key.clone());
        new_agent::open(&self.core, start, window, cx, move |made, window, cx| {
            let _ = board.update(cx, |board, cx| board.new_agent_made(&key, made, window, cx));
        });
    }

    /// Opens the agent New agent made, in the lane it was started from.
    fn new_agent_made(
        &mut self,
        lane: &str,
        made: &new_agent::Made,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agent) = made.agent.clone() else {
            // A workspace and no agent: it shows in its lane.
            self.focus.focus(window, cx);
            cx.notify();
            return;
        };
        let host = made.host.clone();
        self.pending = Some((lane.to_string(), host.clone(), agent.clone()));
        self.expanded
            .insert(lane.to_string(), (host.clone(), agent.clone()));
        self.focused_lane = Some(lane.to_string());
        self.focused_agent = Some((host.clone(), agent.clone()));
        self.focus_composer = Some((host, agent));
        // The directory push can precede the response, and the selected
        // mode may have changed while creation ran.
        self.prune_expanded(cx);
        self.reconcile_view(true, cx);
        cx.notify();
    }
}
