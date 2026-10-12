//! A "New Agent" tab: pick provider, model and thinking, write the first
//! prompt, and the tab becomes the agent once the daemon creates it.

use fleet::providers::Provider;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{Icon, Sizable};
use gpui_kit::gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    WeakEntity, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;
use serde_json::json;

use super::attachments::{self, AttachmentTray};
use crate::app::{Core, Tab};
use crate::theme::{palette, size};

pub struct Draft {
    core: Core,
    host: String,
    workspace: String,
    draft_id: String,
    input: Entity<TextareaState>,
    attachments: AttachmentTray,
    provider: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    creating: bool,
    error: Option<String>,
}

impl Draft {
    pub fn new(
        core: Core,
        host: &str,
        workspace: &str,
        draft_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 14)
                .submit_on_enter(true)
                .placeholder("What should the agent do?")
        });
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.create(window, cx);
            }
        })
        .detach();
        core.store
            .update(cx, |store, cx| store.ensure_providers(host, cx));
        cx.observe(&core.store, |_, _, cx| cx.notify()).detach();
        input.update(cx, |input, cx| input.focus(window, cx));
        // Set by the fuel "use it" chip before the draft tab is opened.
        let provider = core.nav.update(cx, |nav, _| nav.pending_provider.take());
        Self {
            core,
            host: host.to_string(),
            workspace: workspace.to_string(),
            draft_id: draft_id.to_string(),
            input,
            attachments: AttachmentTray::default(),
            provider,
            model: None,
            thinking: None,
            creating: false,
            error: None,
        }
    }

    fn providers(&self, cx: &gpui_kit::gpui::App) -> Vec<Provider> {
        self.core
            .store
            .read(cx)
            .providers
            .get(&self.host)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(Provider::ready)
            .collect()
    }

    /// The effective choice: what the user picked, else the defaults.
    fn resolved(
        &self,
        cx: &gpui_kit::gpui::App,
    ) -> Option<(Provider, Option<String>, Option<String>)> {
        let providers = self.providers(cx);
        let provider = self
            .provider
            .as_ref()
            .and_then(|id| providers.iter().find(|p| &p.id == id))
            .or_else(|| providers.iter().find(|p| p.id == "claude"))
            .or_else(|| providers.first())?
            .clone();
        let model = self
            .model
            .as_ref()
            .and_then(|id| provider.models.iter().find(|m| &m.id == id))
            .or_else(|| provider.default_model());
        let thinking = model.and_then(|m| {
            self.thinking
                .clone()
                .filter(|t| m.thinking.iter().any(|c| &c.id == t))
                .or_else(|| m.default_thinking.clone())
        });
        let model_id = model.map(|m| m.id.clone());
        Some((provider, model_id, thinking))
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating || self.attachments.pending {
            return;
        }
        let prompt = self
            .attachments
            .prompt(self.input.read(cx).value().trim().to_string());
        let Some((provider, model, thinking)) = self.resolved(cx) else {
            self.error = Some("No ready providers on this host".into());
            cx.notify();
            return;
        };
        let Some(workspace) = self
            .core
            .store
            .read(cx)
            .fleet
            .workspace(&self.host, &self.workspace)
            .cloned()
        else {
            return;
        };
        let mut config = json!({ "provider": provider.id, "cwd": workspace.directory() });
        if let Some(model) = model {
            config["model"] = json!(model);
        }
        if let Some(thinking) = thinking {
            config["thinkingOptionId"] = json!(thinking);
        }
        if let Some(mode) = provider.default_mode.clone() {
            config["modeId"] = json!(mode);
        }
        self.creating = true;
        self.error = None;
        cx.notify();
        let task = self.core.store.update(cx, |store, cx| {
            store.create_agent(&self.host, &self.workspace, config, Some(prompt), cx)
        });
        let key = format!("{}:{}", self.host, self.workspace);
        let draft = Tab::Draft(self.draft_id.clone());
        let nav = self.core.nav.clone();
        let _ = window;
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.creating = false;
                match result {
                    Ok(agent_id) => {
                        nav.update(cx, |nav, cx| nav.promote_draft(&key, &draft, &agent_id, cx))
                    }
                    Err(err) => this.error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for Draft {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let providers = self.providers(cx);
        let resolved = self.resolved(cx);
        let loading = !self.core.store.read(cx).providers.contains_key(&self.host);
        let this = cx.entity().downgrade();

        let provider_label = resolved
            .as_ref()
            .map(|(p, _, _)| p.label.clone())
            .unwrap_or_else(|| {
                if loading {
                    "Loading providers…".into()
                } else {
                    "No providers".into()
                }
            });
        let model_label = resolved
            .as_ref()
            .and_then(|(provider, model, _)| {
                let id = model.as_ref()?;
                provider
                    .models
                    .iter()
                    .find(|m| &m.id == id)
                    .map(|m| m.label.clone())
            })
            .unwrap_or_else(|| "Default model".into());
        let thinking_choices = resolved
            .as_ref()
            .and_then(|(provider, model, _)| {
                let id = model.as_ref()?;
                provider
                    .models
                    .iter()
                    .find(|m| &m.id == id)
                    .map(|m| m.thinking.clone())
            })
            .unwrap_or_default();
        let thinking_label = resolved
            .as_ref()
            .and_then(|(_, _, t)| t.clone())
            .and_then(|t| {
                thinking_choices
                    .iter()
                    .find(|c| c.id == t)
                    .map(|c| c.label.clone())
            });

        let provider_menu = {
            let providers = providers.clone();
            let this = this.clone();
            Button::new("draft-provider")
                .label(provider_label)
                .ghost()
                .small()
                .dropdown_menu(move |mut menu, _, _| {
                    for provider in &providers {
                        let this = this.clone();
                        let id = provider.id.clone();
                        menu = menu.item(PopupMenuItem::new(provider.label.clone()).on_click(
                            move |_, _, cx| {
                                let id = id.clone();
                                let _ = this.update(cx, |this, cx| {
                                    this.provider = Some(id);
                                    this.model = None;
                                    this.thinking = None;
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu
                })
        };
        let model_menu = {
            let models = resolved
                .as_ref()
                .map(|(p, _, _)| p.models.clone())
                .unwrap_or_default();
            let this = this.clone();
            Button::new("draft-model")
                .label(model_label)
                .ghost()
                .small()
                .dropdown_menu(move |mut menu, _, _| {
                    for model in &models {
                        let this = this.clone();
                        let id = model.id.clone();
                        menu = menu.item(PopupMenuItem::new(model.label.clone()).on_click(
                            move |_, _, cx| {
                                let id = id.clone();
                                let _ = this.update(cx, |this, cx| {
                                    this.model = Some(id);
                                    this.thinking = None;
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu
                })
        };
        let thinking_menu = (!thinking_choices.is_empty()).then(|| {
            let this = this.clone();
            Button::new("draft-thinking")
                .label(thinking_label.unwrap_or_else(|| "Thinking".into()))
                .ghost()
                .small()
                .dropdown_menu(move |mut menu, _, _| {
                    for choice in &thinking_choices {
                        let this = this.clone();
                        let id = choice.id.clone();
                        menu = menu.item(PopupMenuItem::new(choice.label.clone()).on_click(
                            move |_, _, cx| {
                                let id = id.clone();
                                let _ = this.update(cx, |this, cx| {
                                    this.thinking = Some(id);
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu
                })
        });

        let workspace_name = self
            .core
            .store
            .read(cx)
            .fleet
            .workspace(&self.host, &self.workspace)
            .map(|w| w.name.clone())
            .unwrap_or_default();

        div()
            .size_full()
            .bg(p.surface0)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px(px(24.))
            .child(
                div()
                    .w_full()
                    .max_w(px(size::COMPOSER_MAX_WIDTH))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .text_color(p.foreground)
                            .child(format!("New agent in {workspace_name}")),
                    )
                    .child(
                        div()
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
                            .child(
                                Textarea::new(&self.input)
                                    .appearance(false)
                                    .text_size(size::content())
                                    .on_paste(attachments::paste_handler(cx.entity().downgrade())),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(provider_menu)
                                    .child(model_menu)
                                    .when_some(thinking_menu, |d, menu| d.child(menu))
                                    .child(div().flex_1())
                                    .child(
                                        Button::new("draft-create")
                                            .icon(Icon::empty().path("icons/arrow-up.svg"))
                                            .primary()
                                            .small()
                                            .loading(self.creating)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.create(window, cx)
                                            })),
                                    ),
                            ),
                    )
                    .when_some(
                        self.attachments
                            .error
                            .clone()
                            .or_else(|| self.error.clone()),
                        |d, error| {
                            d.child(
                                div()
                                    .text_size(size::sm())
                                    .text_color(p.status_danger)
                                    .child(error),
                            )
                        },
                    )
                    .child(div().text_size(size::sm()).text_color(p.extra_muted).child(
                        SharedString::from("Enter creates the agent · Shift+Enter for a new line"),
                    )),
            )
    }
}

impl attachments::HasAttachments for Draft {
    fn attachments(&mut self) -> &mut AttachmentTray {
        &mut self.attachments
    }
    fn attachments_disabled(&self) -> bool {
        self.creating
    }
}
