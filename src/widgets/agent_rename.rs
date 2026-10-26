//! A title edited in place: Enter or clicking away saves, Escape cancels.
//! Used for an agent by the expanded Lanes card and the active Paseo agent
//! tab, and for a workspace by its Lanes box.

use fleet::HostId;
use fleet::requests;
use gpui_kit::component::Sizable;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::gpui::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, Styled, Subscription, Window, div, px,
};

use crate::app::Core;

/// The edit ended, saved or not; the owner drops the editor.
pub struct RenameDone;

/// What is being renamed.
#[derive(Clone, PartialEq, Eq)]
enum Target {
    Agent(String),
    Workspace(String),
}

pub struct AgentRename {
    core: Core,
    host: HostId,
    target: Target,
    original: String,
    input: Entity<InputState>,
    finished: bool,
    _input_events: Subscription,
}

impl EventEmitter<RenameDone> for AgentRename {}

impl AgentRename {
    /// Starts editing an agent's title, selected.
    pub fn new(
        core: &Core,
        host: &HostId,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let original = core
            .store
            .read(cx)
            .fleet
            .agent(host, agent)
            .map(|agent| agent.display_title())
            .unwrap_or_default();
        Self::start(
            core,
            host,
            Target::Agent(agent.to_string()),
            original,
            window,
            cx,
        )
    }

    /// Starts editing a workspace's title, selected.
    pub fn workspace(
        core: &Core,
        host: &HostId,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let original = core
            .store
            .read(cx)
            .fleet
            .workspace(host, workspace)
            .map(|w| w.title.clone().unwrap_or_else(|| w.name.clone()))
            .unwrap_or_default();
        let target = Target::Workspace(workspace.to_string());
        Self::start(core, host, target, original, window, cx)
    }

    fn start(
        core: &Core,
        host: &HostId,
        target: Target,
        original: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(original.clone()));
        let _input_events = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, _, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => this.finish(true, cx),
                _ => {}
            },
        );
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        Self {
            core: core.clone(),
            host: host.clone(),
            target,
            original,
            input,
            finished: false,
            _input_events,
        }
    }

    /// Whether this edits `agent` on `host`.
    pub fn edits(&self, host: &HostId, agent: &str) -> bool {
        &self.host == host && self.target == Target::Agent(agent.to_string())
    }

    /// Whether this edits `workspace` on `host`.
    pub fn edits_workspace(&self, host: &HostId, workspace: &str) -> bool {
        &self.host == host && self.target == Target::Workspace(workspace.to_string())
    }

    fn finish(&mut self, save: bool, cx: &mut Context<Self>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let name = self.input.read(cx).value().trim().to_string();
        if save && !name.is_empty() && name != self.original {
            let message = match &self.target {
                Target::Agent(agent) => requests::rename_agent(agent, &name),
                Target::Workspace(workspace) => requests::rename_workspace(workspace, Some(&name)),
            };
            let host = self.host.clone();
            self.core
                .store
                .update(cx, |store, cx| store.act(&host, message, cx));
        }
        cx.emit(RenameDone);
    }
}

impl Render for AgentRename {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_1()
            .min_w(px(0.))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.finish(false, cx);
                    cx.stop_propagation();
                }
            }))
            .child(Input::new(&self.input).xsmall())
    }
}
