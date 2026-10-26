//! The message box under a conversation: Enter sends, Shift+Enter breaks a
//! line, Cmd/Ctrl+Enter queues while a turn runs, Escape or Stop interrupts.

use fleet::HostId;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{Icon, Sizable};
use gpui_kit::gpui::{
    App, AppContext, ClickEvent, Context, Entity, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;
use serde_json::json;

use super::attachments::{self, AttachmentTray};
use crate::app::Core;
use crate::theme::{palette, size};
use crate::ui;

gpui_kit::actions!(
    composer,
    [
        Interrupt,
        SlashNext,
        SlashPrevious,
        SlashAccept,
        SlashDismiss
    ]
);

pub fn bind_keys(cx: &mut App) {
    use gpui_kit::gpui::KeyBinding;
    // While the slash list is open these outrank the text box's own keys
    // (bindings registered later win at the same depth).
    let completing = Some("SlashCompletion > Input");
    cx.bind_keys([
        KeyBinding::new("escape", Interrupt, Some("Composer")),
        KeyBinding::new("down", SlashNext, completing),
        KeyBinding::new("up", SlashPrevious, completing),
        KeyBinding::new("tab", SlashAccept, completing),
        KeyBinding::new("enter", SlashAccept, completing),
        KeyBinding::new("escape", SlashDismiss, completing),
    ]);
}

#[derive(Clone)]
struct SlashCommand {
    name: String,
    description: String,
    hint: String,
}

pub struct Composer {
    core: Core,
    host: HostId,
    agent_id: String,
    input: Entity<TextareaState>,
    attachments: AttachmentTray,
    submission_error: Option<String>,
    /// The agent's slash commands, fetched the first time `/` is typed.
    commands: Option<Vec<SlashCommand>>,
    loading_commands: bool,
    /// Matches for the `/word` being typed, and the highlighted one.
    completions: Vec<SlashCommand>,
    completion_index: usize,
    /// Fills its width rather than keeping to the reading column; see
    /// [`super::Conversation::set_wide`].
    wide: bool,
}

impl Composer {
    pub fn set_wide(&mut self, wide: bool, cx: &mut Context<Self>) {
        self.wide = wide;
        cx.notify();
    }

    pub fn new(
        core: Core,
        host: &HostId,
        agent_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 12)
                .submit_on_enter(true)
                .placeholder("Message the agent…")
        });
        cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.update_completions(cx),
                InputEvent::PressEnter {
                    secondary,
                    shift: false,
                } => this.submit(*secondary, window, cx),
                InputEvent::Focus => {
                    let (host, agent) = (this.host.clone(), this.agent_id.clone());
                    this.core
                        .store
                        .update(cx, |store, cx| store.clear_attention(&host, &agent, cx));
                }
                _ => {}
            },
        )
        .detach();
        if let Some(session) = core.store.read(cx).session(host, agent_id) {
            cx.observe(&session, |_, _, cx| cx.notify()).detach();
        }
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        cx.observe(&core.usage, |_, _, cx| cx.notify()).detach();
        cx.observe_in(core.draft_handoffs(), window, |this, _, window, cx| {
            this.restore_returned_draft(window, cx);
        })
        .detach();
        cx.observe_in(&core.nav, window, |this, _, window, cx| {
            this.restore_returned_draft(window, cx);
        })
        .detach();
        core.store
            .update(cx, |store, cx| store.ensure_providers(host, cx));
        let mut composer = Self {
            core,
            host: host.clone(),
            agent_id: agent_id.to_string(),
            input,
            attachments: AttachmentTray::default(),
            submission_error: None,
            commands: None,
            loading_commands: false,
            completions: Vec::new(),
            completion_index: 0,
            wide: false,
        };
        if let Some(draft) = composer.core.take_draft(host, agent_id, cx) {
            composer.restore_draft(draft, window, cx);
        }
        composer
    }

    /// `/foo` with no space yet: show matching commands.
    fn update_completions(&mut self, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value().to_string();
        let query = value
            .strip_prefix('/')
            .filter(|rest| !rest.contains(char::is_whitespace))
            .map(str::to_lowercase);
        let Some(query) = query else {
            if !self.completions.is_empty() {
                self.completions.clear();
                cx.notify();
            }
            return;
        };
        let Some(commands) = self.commands.clone() else {
            self.load_commands(cx);
            return;
        };
        let mut matches: Vec<SlashCommand> = commands
            .into_iter()
            .filter(|c| !c.name.starts_with("__") && c.name.to_lowercase().contains(&query))
            .collect();
        matches.sort_by_key(|c| (!c.name.to_lowercase().starts_with(&query), c.name.clone()));
        matches.truncate(8);
        self.completions = matches;
        self.completion_index = 0;
        cx.notify();
    }

    fn load_commands(&mut self, cx: &mut Context<Self>) {
        if self.loading_commands {
            return;
        }
        self.loading_commands = true;
        let message = json!({ "type": "list_commands_request", "agentId": self.agent_id });
        let task = self.core.store.read(cx).request(&self.host, message, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading_commands = false;
                let commands = result
                    .ok()
                    .and_then(|payload| payload["commands"].as_array().cloned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|c| {
                        Some(SlashCommand {
                            name: c["name"].as_str()?.to_string(),
                            description: c["description"].as_str().unwrap_or_default().to_string(),
                            hint: c["argumentHint"].as_str().unwrap_or_default().to_string(),
                        })
                    })
                    .collect();
                this.commands = Some(commands);
                this.update_completions(cx);
            });
        })
        .detach();
    }

    fn accept_completion(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(command) = self.completions.get(index).cloned() else {
            return;
        };
        let text = format!("/{} ", command.name);
        let end = text.len();
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.set_selected_range(end..end, cx);
        });
        self.completions.clear();
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn restore_returned_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Hidden layouts and the closing pop-out must not consume the next
        // main-window composer's handoff. Nav changes retry delivery when a
        // cached layout or tab becomes current, without restoring in render.
        if self.core.main_window() != Some(window.window_handle()) {
            return;
        }
        let nav = self.core.nav.read(cx);
        let visible = if self.wide {
            nav.layout == "lanes"
        } else {
            nav.layout == "paseo" && nav.selection.as_ref().is_some_and(|selection| {
                selection.host == self.host && matches!(nav.focused_tab(&crate::app::nav::workspace_key(&selection.host, &selection.workspace)), Some(crate::app::Tab::Agent(agent)) if agent == &self.agent_id)
            })
        };
        if visible {
            let draft = self.core.take_draft(&self.host, &self.agent_id, cx);
            if let Some(draft) = draft {
                self.restore_draft(draft, window, cx);
            }
        }
    }

    pub fn draft(&self, cx: &App) -> fleet::prompt::Prompt {
        self.attachments
            .prompt(self.input.read(cx).value().to_string())
    }

    pub fn preparing_attachment(&self) -> bool {
        self.attachments.pending
    }

    pub fn restore_draft(
        &mut self,
        draft: fleet::prompt::Prompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.attachments.images.clear();
        self.attachments.restore(&draft.images);
        self.input
            .update(cx, |input, cx| input.set_value(draft.text, window, cx));
        cx.notify();
    }

    pub fn is_blank(&self, cx: &App) -> bool {
        self.input.read(cx).value().trim().is_empty()
            && self.attachments.images.is_empty()
            && !self.attachments.pending
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    fn running(&self, cx: &App) -> bool {
        self.core
            .store
            .read(cx)
            .session(&self.host, &self.agent_id)
            .is_some_and(|s| s.read(cx).timeline.is_running())
    }

    fn restore_failed(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = self
            .core
            .store
            .read(cx)
            .failed_prompt(&self.host, &self.agent_id, id, cx);
        if let Some(prompt) = prompt {
            if !self.attachments.restore(&prompt.images) {
                cx.notify();
                return;
            }
            self.core.store.update(cx, |store, cx| {
                store.take_failed(&self.host, &self.agent_id, id, cx);
            });
            let current = self.input.read(cx).value().to_string();
            let text = if current.is_empty() {
                prompt.text
            } else {
                format!("{}\n{}", prompt.text, current)
            };
            self.input
                .update(cx, |input, cx| input.set_value(text, window, cx));
            cx.notify();
        }
    }

    fn submit(&mut self, alternate: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.attachments.pending {
            return;
        }
        let text = self.input.read(cx).value().to_string();
        let text = text.trim().to_string();
        if text.is_empty() && self.attachments.images.is_empty() {
            return;
        }
        let prompt = self.attachments.prompt(text);
        let running = self.running(cx);
        let (host, agent) = (self.host.clone(), self.agent_id.clone());
        let pending_permission = self
            .core
            .store
            .read(cx)
            .session(&host, &agent)
            .is_some_and(|s| !s.read(cx).timeline.permissions.is_empty());
        let result = self.core.store.update(cx, |store, cx| {
            if running && alternate && !pending_permission {
                store.queue_message(&host, &agent, prompt, cx)
            } else if running {
                store.send_prompt(&host, &agent, prompt, Some("steer"), cx)
            } else {
                store.send_prompt(&host, &agent, prompt, None, cx)
            }
        });
        match result {
            Ok(()) => {
                self.submission_error = None;
                self.attachments.images.clear();
                self.attachments.error = None;
                self.input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
            Err(error) => self.submission_error = Some(error),
        }
        cx.notify();
    }

    fn interrupt(&mut self, _: &Interrupt, _: &mut Window, cx: &mut Context<Self>) {
        if self.running(cx) {
            let (host, agent) = (self.host.clone(), self.agent_id.clone());
            self.core
                .store
                .update(cx, |store, cx| store.cancel(&host, &agent, cx));
        } else {
            cx.propagate();
        }
    }

    fn slash_next(&mut self, _: &SlashNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.completions.is_empty() {
            self.completion_index = (self.completion_index + 1) % self.completions.len();
            cx.notify();
        }
    }

    fn slash_previous(&mut self, _: &SlashPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if !self.completions.is_empty() {
            self.completion_index = self
                .completion_index
                .checked_sub(1)
                .unwrap_or(self.completions.len() - 1);
            cx.notify();
        }
    }

    fn slash_accept(&mut self, _: &SlashAccept, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.completion_index;
        self.accept_completion(index, window, cx);
    }

    fn slash_dismiss(&mut self, _: &SlashDismiss, _: &mut Window, cx: &mut Context<Self>) {
        self.completions.clear();
        cx.notify();
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let running = self.running(cx);
        let store = self.core.store.read(cx);
        // Borrowed: an agent carries its raw JSON.
        let agent = store.fleet.agent(&self.host, &self.agent_id);
        let session = store.session(&self.host, &self.agent_id);
        let queue: Vec<_> = session
            .as_ref()
            .map(|s| {
                s.read(cx)
                    .queue
                    .iter()
                    .map(|prompt| {
                        (
                            prompt.text.lines().next().unwrap_or("Image").to_string(),
                            prompt.images.len(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let failed: Vec<_> = session
            .as_ref()
            .map(|s| {
                s.read(cx)
                    .failed_prompts
                    .iter()
                    .map(|(id, prompt)| (id.clone(), prompt.images.len()))
                    .collect()
            })
            .unwrap_or_default();

        let modes: Vec<(String, String)> = agent
            .as_ref()
            .and_then(|a| a.raw["availableModes"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|m| {
                Some((
                    m["id"].as_str()?.to_string(),
                    m["label"].as_str().unwrap_or("").to_string(),
                ))
            })
            .collect();
        let current_mode = agent
            .as_ref()
            .and_then(|a| a.raw["currentModeId"].as_str().map(str::to_string));
        let mode_label = current_mode
            .as_ref()
            .and_then(|id| modes.iter().find(|(m, _)| m == id).map(|(_, l)| l.clone()))
            .or(current_mode.clone())
            .unwrap_or_else(|| "Mode".into());
        let model = agent
            .as_ref()
            .map(|a| {
                let provider = a.provider_id().to_string();
                match a.model_label() {
                    Some(model) => format!("{provider} · {model}"),
                    None => provider,
                }
            })
            .unwrap_or_default();
        let fuel_tint = agent.as_ref().and_then(|agent| {
            crate::fuel::label_tint(
                agent.provider_id(),
                self.core.usage.read(cx).snapshot.as_ref(),
            )
        });
        let usage = agent.as_ref().and_then(|a| {
            let used = a.raw["lastUsage"]["contextWindowUsedTokens"].as_f64()?;
            let max = a.raw["lastUsage"]["contextWindowMaxTokens"].as_f64()?;
            (max > 0.).then(|| format!("{:.0}% context", used / max * 100.))
        });

        let core = self.core.clone();
        let host = self.host.clone();
        let agent_id = self.agent_id.clone();
        let mode_menu = Button::new(SharedString::from(format!("mode-menu-{}", self.agent_id)))
            .label(mode_label)
            .ghost()
            .xsmall()
            .dropdown_menu(move |mut menu, _, _| {
                for (id, label) in &modes {
                    let core = core.clone();
                    let host = host.clone();
                    let agent_id = agent_id.clone();
                    let id = id.clone();
                    let checked = current_mode.as_deref() == Some(id.as_str());
                    menu = menu.item(PopupMenuItem::new(label.clone()).checked(checked).on_click(move |_, _, cx| {
                        let message = json!({"type": "set_agent_mode_request", "agentId": agent_id, "modeId": id});
                        core.store.update(cx, |store, cx| store.act(&host, message, cx));
                    }));
                }
                menu
            });

        // Model and thinking pickers come from the host's providers snapshot.
        let provider = agent.as_ref().and_then(|a| {
            store
                .providers
                .get(&self.host)?
                .iter()
                .find(|p| p.id == a.provider_id())
                .cloned()
        });
        let current_model = agent.as_ref().and_then(|a| a.model.clone());
        let current_thinking = agent
            .as_ref()
            .and_then(|a| a.raw["thinkingOptionId"].as_str().map(str::to_string));
        let model_entry = provider.as_ref().and_then(|p| {
            current_model
                .as_ref()
                .and_then(|id| p.models.iter().find(|m| &m.id == id))
                .or_else(|| p.default_model())
                .cloned()
        });
        let model_menu = provider.clone().map(|provider| {
            let label = model_entry.as_ref().map(|m| m.label.clone()).unwrap_or_else(|| model.clone());
            let core = self.core.clone();
            let host = self.host.clone();
            let agent_id = self.agent_id.clone();
            let current = current_model.clone();
            Button::new(SharedString::from(format!("model-menu-{}", self.agent_id)))
                .label(label)
                .ghost()
                .xsmall()
                .when_some(fuel_tint, |button, color| button.text_color(color))
                .dropdown_menu(move |mut menu, _, _| {
                for m in &provider.models {
                    let core = core.clone();
                    let host = host.clone();
                    let agent_id = agent_id.clone();
                    let id = m.id.clone();
                    menu = menu.item(
                        PopupMenuItem::new(m.label.clone())
                            .checked(current.as_deref() == Some(m.id.as_str()))
                            .on_click(move |_, _, cx| {
                                let message = json!({"type": "set_agent_model_request", "agentId": agent_id, "modelId": id});
                                core.store.update(cx, |store, cx| store.act(&host, message, cx));
                            }),
                    );
                }
                menu.max_h(px(420.))
            })
        });
        let thinking_menu = model_entry.filter(|m| m.thinking.len() > 1).map(|m| {
            let label = current_thinking
                .as_ref()
                .or(m.default_thinking.as_ref())
                .and_then(|id| m.thinking.iter().find(|c| &c.id == id))
                .map(|c| c.label.clone())
                .unwrap_or_else(|| "Thinking".into());
            let core = self.core.clone();
            let host = self.host.clone();
            let agent_id = self.agent_id.clone();
            let current = current_thinking.clone();
            Button::new(SharedString::from(format!(
                "thinking-menu-{}",
                self.agent_id
            )))
            .label(label)
            .ghost()
            .xsmall()
            .dropdown_menu(move |mut menu, _, _| {
                for choice in &m.thinking {
                    let core = core.clone();
                    let host = host.clone();
                    let agent_id = agent_id.clone();
                    let id = choice.id.clone();
                    menu = menu.item(
                        PopupMenuItem::new(choice.label.clone())
                            .checked(current.as_deref() == Some(choice.id.as_str()))
                            .on_click(move |_, _, cx| {
                                let message = json!({"type": "set_agent_thinking_request", "agentId": agent_id, "thinkingOptionId": id});
                                core.store.update(cx, |store, cx| store.act(&host, message, cx));
                            }),
                    );
                }
                menu
            })
        });
        let needs_providers = provider.is_none() && agent.is_some();

        let action: gpui_kit::gpui::AnyElement = if running {
            Button::new(SharedString::from(format!("stop-{}", self.agent_id)))
                .icon(Icon::empty().path("icons/square.svg"))
                .small()
                .danger()
                .tooltip("Interrupt (Esc)")
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.interrupt(&Interrupt, window, cx)
                }))
                .into_any_element()
        } else {
            Button::new(SharedString::from(format!("send-{}", self.agent_id)))
                .icon(Icon::empty().path("icons/arrow-up.svg"))
                .small()
                .primary()
                .tooltip("Send (Enter)")
                .on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.submit(false, window, cx)),
                )
                .into_any_element()
        };

        div()
            .w_full()
            .flex()
            .justify_center()
            // A Lanes card's composer uses the room the transcript keeps for
            // its prompt index; centred in Paseo, it lines up with the
            // transcript instead.
            .map(|row| match self.wide {
                true => row.px(px(size::CONVERSATION_PADDING_RIGHT)),
                false => row
                    .pl(px(size::CONVERSATION_PADDING_LEFT))
                    .pr(px(size::CONVERSATION_PADDING_RIGHT)),
            })
            .pb(px(16.))
            .pt(px(4.))
            .child(
                div()
                    .w_full()
                    .when(!self.wide, |column| {
                        column.max_w(px(size::COMPOSER_MAX_WIDTH))
                    })
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .when_some(self.submission_error.clone(), |d, error| {
                        d.child(
                            div()
                                .text_size(size::sm())
                                .text_color(p.dot_danger)
                                .child(error),
                        )
                    })
                    .children(failed.iter().map(|(id, count)| {
                        let id = id.clone();
                        Button::new(SharedString::from(format!("restore-failed-{id}")))
                            .label(if *count == 0 {
                                "Restore failed message".into()
                            } else {
                                format!("Restore failed message ({count} image(s))")
                            })
                            .ghost()
                            .small()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.restore_failed(&id, window, cx)
                            }))
                    }))
                    .children(queue.iter().enumerate().map(|(i, text)| {
                        div()
                            .id(SharedString::from(format!("queued-{i}")))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(12.))
                            .py(px(6.))
                            .rounded(px(8.))
                            .bg(p.surface1)
                            .text_size(size::sm())
                            .text_color(p.muted)
                            .child(ui::icon("clock", 12., p.muted))
                            .child(div().flex_1().truncate().child(format!(
                                "{}{}",
                                text.0,
                                if text.1 == 0 {
                                    String::new()
                                } else {
                                    format!(" · {} image(s)", text.1)
                                }
                            )))
                            .child("queued")
                    }))
                    .when(!self.completions.is_empty(), |d| {
                        let selected = self.completion_index;
                        d.child(
                            div()
                                .rounded(px(8.))
                                .border_1()
                                .border_color(p.border_accent)
                                .bg(p.surface1)
                                .py(px(4.))
                                .flex()
                                .flex_col()
                                .children(self.completions.iter().enumerate().map(
                                    |(i, command)| {
                                        div()
                                            .id(SharedString::from(format!("slash-{i}")))
                                            .px(px(12.))
                                            .py(px(4.))
                                            .flex()
                                            .gap(px(10.))
                                            .when(i == selected, |d| d.bg(p.surface2))
                                            .hover(|s| s.bg(p.surface2))
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_color(p.foreground)
                                                    .child(format!(
                                                        "/{} {}",
                                                        command.name, command.hint
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .truncate()
                                                    .text_size(size::sm())
                                                    .text_color(p.muted)
                                                    .child(command.description.clone()),
                                            )
                                            .on_click(cx.listener(
                                                move |this, _: &ClickEvent, window, cx| {
                                                    this.accept_completion(i, window, cx)
                                                },
                                            ))
                                    },
                                )),
                        )
                    })
                    .child(
                        div()
                            .key_context("Composer")
                            .on_action(cx.listener(Self::interrupt))
                            .rounded(px(16.))
                            .border_1()
                            .border_color(p.border_accent)
                            .bg(p.surface1)
                            .p(px(12.))
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .when(!self.attachments.images.is_empty(), |d| {
                                d.child(attachments::strip(
                                    &self.attachments.images,
                                    cx.entity().downgrade(),
                                ))
                            })
                            .when(self.attachments.pending, |d| {
                                d.child(div().text_color(p.muted).child("Preparing image…"))
                            })
                            .when_some(self.attachments.error.clone(), |d, error| {
                                d.child(div().text_color(p.status_danger).child(error))
                            })
                            .child(
                                div()
                                    .when(!self.completions.is_empty(), |d| {
                                        d.key_context("SlashCompletion")
                                    })
                                    .on_action(cx.listener(Self::slash_next))
                                    .on_action(cx.listener(Self::slash_previous))
                                    .on_action(cx.listener(Self::slash_accept))
                                    .on_action(cx.listener(Self::slash_dismiss))
                                    .child(
                                        gpui_kit::component::input::Textarea::new(&self.input)
                                            .appearance(false)
                                            .text_size(size::content())
                                            .on_paste(attachments::paste_handler(
                                                cx.entity().downgrade(),
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(self_mode_slot(mode_menu))
                                    .when_some(model_menu, |d, menu| d.child(menu))
                                    .when_some(thinking_menu, |d, menu| d.child(menu))
                                    .when(needs_providers, |d| {
                                        let muted =
                                            fuel_tint.unwrap_or_else(|| p.extra_muted.into());
                                        d.child(
                                            div()
                                                .text_size(size::sm())
                                                .text_color(muted)
                                                .child(model.clone()),
                                        )
                                    })
                                    .when_some(usage, |d, usage| {
                                        d.child(
                                            div()
                                                .text_size(size::sm())
                                                .text_color(p.extra_muted)
                                                .child(usage),
                                        )
                                    })
                                    .child(div().flex_1())
                                    .when(running, |d| {
                                        d.child(
                                            div()
                                                .text_size(size::sm())
                                                .text_color(p.extra_muted)
                                                .child("Enter steers · ⌘/Ctrl+Enter queues"),
                                        )
                                    })
                                    .child(action),
                            ),
                    ),
            )
    }
}

fn self_mode_slot(button: impl IntoElement) -> impl IntoElement {
    div().child(button)
}

impl attachments::HasAttachments for Composer {
    fn attachments(&mut self) -> &mut AttachmentTray {
        &mut self.attachments
    }
}
