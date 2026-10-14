//! Answer a permission or stop a turn without opening the conversation.
//! Fleet cards and Lanes cards both use this.

use fleet::timeline::ToolCall;
use fleet::{Agent, Bucket};
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use gpui_kit::prelude::FluentBuilder;
use serde_json::json;

use crate::app::Core;
use crate::theme::{palette, size};

/// What stands for the buttons when the agent asks a question: questions
/// are answered in the conversation's form, as Accept here would send no
/// answers.
fn answer_in_conversation() -> AnyElement {
    div()
        .flex()
        .justify_end()
        .text_size(size::sm())
        .text_color(palette().muted)
        .child("Open to answer")
        .into_any_element()
}

/// `scope` keeps element ids unique when the same agent is drawn in two columns.
pub fn controls(core: &Core, host: &str, agent: &Agent, scope: &str) -> Option<AnyElement> {
    let p = palette();
    if let Some(request) = agent.pending_permissions.first()
        && fleet::questions::is_question(request)
    {
        let asked = fleet::questions::parse(&request["input"])
            .and_then(|questions| questions.into_iter().next())
            .map(|question| question.question);
        return Some(
            div()
                .mt(px(4.))
                .pt(px(6.))
                .border_t_1()
                .border_color(p.border)
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.status_warning)
                        .child("Question"),
                )
                .when_some(asked, |this, asked| {
                    this.child(
                        div()
                            .text_size(size::sm())
                            .text_color(p.foreground)
                            .line_clamp(3)
                            .child(asked),
                    )
                })
                .child(answer_in_conversation())
                .into_any_element(),
        );
    }
    if let Some(request) = agent.pending_permissions.first() {
        let permission_id = request["id"].as_str()?.to_string();
        let title = request["title"]
            .as_str()
            .or(request["name"].as_str())
            .unwrap_or("Permission")
            .to_string();
        let detail = ToolCall {
            identity: permission_id.clone(),
            call_id: permission_id.clone(),
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
        let summary = fleet::display::tool_display(&detail, &agent.cwd).summary;
        let mut actions: Vec<(String, String, bool)> = request["actions"]
            .as_array()
            .map(|actions| {
                actions
                    .iter()
                    .map(|action| {
                        (
                            action["id"].as_str().unwrap_or_default().to_string(),
                            action["label"].as_str().unwrap_or("OK").to_string(),
                            action["behavior"] == "allow",
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if actions.is_empty() {
            actions = vec![
                ("reject".into(), "Deny".into(), false),
                ("accept".into(), "Accept".into(), true),
            ];
        }
        let mut row = div().flex().gap(px(6.)).justify_end();
        for (action_id, label, allow) in actions {
            let core = core.clone();
            let host = host.to_string();
            let agent_id = agent.id.clone();
            let permission = permission_id.clone();
            let mut button = Button::new(SharedString::from(format!(
                "fp-{scope}-{}-{action_id}",
                agent.id
            )))
            .label(label)
            .xsmall();
            if allow {
                button = button.primary();
            }
            row = row.child(button.on_click(move |_, _, cx| {
                cx.stop_propagation();
                let response = if allow {
                    json!({"behavior": "allow", "selectedActionId": action_id})
                } else {
                    json!({
                        "behavior": "deny",
                        "selectedActionId": action_id,
                        "message": "Denied by user"
                    })
                };
                core.store.update(cx, |store, cx| {
                    store.answer_permission(&host, &agent_id, &permission, response, cx)
                });
            }));
        }
        return Some(
            div()
                .mt(px(4.))
                .pt(px(6.))
                .border_t_1()
                .border_color(p.border)
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.status_warning)
                        .child(title),
                )
                .when_some(summary, |this, summary| {
                    this.child(
                        div()
                            .font_family(crate::theme::mono_font())
                            .text_size(size::code())
                            .text_color(p.foreground)
                            .truncate()
                            .child(summary),
                    )
                })
                .child(row)
                .into_any_element(),
        );
    }
    if agent.bucket() == Bucket::Running {
        let core = core.clone();
        let host = host.to_string();
        let agent_id = agent.id.clone();
        return Some(
            div()
                .flex()
                .justify_end()
                .child(
                    Button::new(SharedString::from(format!("fstop-{scope}-{}", agent.id)))
                        .label("Stop")
                        .ghost()
                        .xsmall()
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            core.store
                                .update(cx, |store, cx| store.cancel(&host, &agent_id, cx));
                        }),
                )
                .into_any_element(),
        );
    }
    None
}

/// Allow / Deny for the agent's first pending permission. `id_prefix` keeps
/// element ids unique when Fleet cards and the Needs-you strip both draw it.
pub fn permission_buttons(
    core: &Core,
    host: &str,
    agent: &Agent,
    id_prefix: &str,
) -> Option<gpui_kit::gpui::AnyElement> {
    use gpui_kit::component::Sizable;
    use gpui_kit::component::button::{Button, ButtonVariants};
    use serde_json::json;

    let request = agent.pending_permissions.first()?;
    if fleet::questions::is_question(request) {
        return Some(answer_in_conversation());
    }
    let permission_id = request["id"].as_str()?.to_string();
    let mut actions: Vec<(String, String, bool)> = request["actions"]
        .as_array()
        .map(|actions| {
            actions
                .iter()
                .map(|action| {
                    (
                        action["id"].as_str().unwrap_or_default().to_string(),
                        action["label"].as_str().unwrap_or("OK").to_string(),
                        action["behavior"] == "allow",
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if actions.is_empty() {
        actions = vec![
            ("reject".into(), "Deny".into(), false),
            ("accept".into(), "Accept".into(), true),
        ];
    }
    let mut row = div().flex().gap(px(6.)).justify_end();
    for (action_id, label, allow) in actions {
        let core = core.clone();
        let host = host.to_string();
        let agent_id = agent.id.clone();
        let permission = permission_id.clone();
        let mut button = Button::new(SharedString::from(format!(
            "{id_prefix}-{}-{action_id}",
            agent.id
        )))
        .label(label)
        .xsmall();
        if allow {
            button = button.primary();
        }
        row = row.child(button.on_click(move |_, _, cx| {
            cx.stop_propagation();
            let response = if allow {
                json!({"behavior": "allow", "selectedActionId": action_id})
            } else {
                json!({
                    "behavior": "deny",
                    "selectedActionId": action_id,
                    "message": "Denied by user"
                })
            };
            core.store.update(cx, |store, cx| {
                store.answer_permission(&host, &agent_id, &permission, response, cx)
            });
        }));
    }
    Some(row.into_any_element())
}
