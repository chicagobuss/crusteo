//! A question an agent asks, such as Claude Code's AskUserQuestion, as a
//! form: each question's options to choose from, a text box where another
//! answer is allowed, and Submit, which sends the answers the way Paseo's
//! own form does. See [`fleet::questions`].

use std::collections::BTreeSet;

use fleet::HostId;
use fleet::questions::{self, Question};
use gpui_kit::base::Disableable as _;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::gpui::{
    AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::prelude::FluentBuilder;
use serde_json::Value;

use crate::app::Core;
use crate::theme::{palette, size};
use crate::ui;

pub struct QuestionForm {
    core: Core,
    host: HostId,
    agent: String,
    permission: String,
    request: Value,
    questions: Vec<Question>,
    /// The options chosen, per question.
    selected: Vec<BTreeSet<usize>>,
    /// The text box of each question that has one.
    others: Vec<Option<Entity<InputState>>>,
    /// The answer was sent; the card stays until the daemon resolves it.
    sent: bool,
}

impl QuestionForm {
    /// The form for `request`, holding the `questions` parsed from it.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        core: Core,
        host: &HostId,
        agent: &str,
        permission: &str,
        request: Value,
        questions: Vec<Question>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let others = questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                question.shows_text().then(|| {
                    let placeholder = question.placeholder.clone().unwrap_or_else(|| {
                        if question.options.is_empty() {
                            "Your answer".into()
                        } else {
                            "Something else…".into()
                        }
                    });
                    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                    cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
                        if let InputEvent::Change = event {
                            // A typed answer replaces a single choice. The
                            // box emptied by a choice leaves it alone.
                            let typed = !input.read(cx).value().trim().is_empty();
                            if typed && !this.questions[index].multi_select {
                                this.selected[index].clear();
                            }
                            cx.notify();
                        }
                    })
                    .detach();
                    input
                })
            })
            .collect();
        Self {
            core,
            host: host.clone(),
            agent: agent.to_string(),
            permission: permission.to_string(),
            selected: vec![BTreeSet::new(); questions.len()],
            questions,
            others,
            request,
            sent: false,
        }
    }

    fn other_texts(&self, cx: &Context<Self>) -> Vec<String> {
        (self.others.iter())
            .map(|input| {
                input
                    .as_ref()
                    .map_or_else(String::new, |input| input.read(cx).value().to_string())
            })
            .collect()
    }

    fn choose(
        &mut self,
        question: usize,
        option: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi = self.questions[question].multi_select;
        let chosen = &mut self.selected[question];
        if multi {
            if !chosen.remove(&option) {
                chosen.insert(option);
            }
        } else {
            *chosen = BTreeSet::from([option]);
            // A choice replaces a typed answer.
            if let Some(input) = &self.others[question] {
                input.update(cx, |input, cx| input.set_value("", window, cx));
            }
        }
        cx.notify();
    }

    fn respond(&mut self, response: Value, cx: &mut Context<Self>) {
        self.sent = true;
        let (host, agent, permission) = (
            self.host.clone(),
            self.agent.clone(),
            self.permission.clone(),
        );
        self.core.store.update(cx, |store, cx| {
            store.answer_permission(&host, &agent, &permission, response, cx)
        });
        cx.notify();
    }
}

impl Render for QuestionForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let texts = self.other_texts(cx);
        let ready = !self.sent
            && (self.questions.iter().enumerate())
                .all(|(index, question)| question.answered(&self.selected[index], &texts[index]));
        let id = |part: &str| SharedString::from(format!("question-{}-{part}", self.permission));
        let mut card = div()
            .mb(px(16.))
            .rounded(px(8.))
            .border_1()
            .border_color(p.status_warning)
            .bg(p.surface1)
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_color(p.foreground)
                    .child(ui::icon("circle-question-mark", 14., p.dot_warning))
                    .child(if self.questions.len() == 1 {
                        "Question"
                    } else {
                        "Questions"
                    }),
            );
        for (index, question) in self.questions.iter().enumerate() {
            let mut block = div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(size::sm())
                        .text_color(p.muted)
                        .child(question.header.clone()),
                )
                .child(
                    div()
                        .text_color(p.foreground)
                        .child(question.question.clone()),
                );
            for (option, choice) in question.options.iter().enumerate() {
                let chosen = self.selected[index].contains(&option);
                let mark = div()
                    .mt(px(2.))
                    .size(px(14.))
                    .flex_shrink_0()
                    .border_1()
                    .border_color(if chosen { p.accent } else { p.muted })
                    .when(question.multi_select, |mark| mark.rounded(px(3.)))
                    .when(!question.multi_select, |mark| mark.rounded_full())
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(chosen, |mark| {
                        mark.child(
                            div()
                                .size(px(8.))
                                .bg(p.accent)
                                .when(question.multi_select, |dot| dot.rounded(px(2.)))
                                .when(!question.multi_select, |dot| dot.rounded_full()),
                        )
                    });
                block = block.child(
                    div()
                        .id(id(&format!("{index}-{option}")))
                        .px(px(8.))
                        .py(px(6.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(if chosen { p.accent } else { p.border })
                        .when(!self.sent, |row| row.hover(|row| row.bg(p.surface2)))
                        .flex()
                        .items_start()
                        .gap(px(8.))
                        .child(mark)
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(div().text_color(p.foreground).child(choice.label.clone()))
                                .when_some(choice.description.clone(), |text, description| {
                                    text.child(
                                        div()
                                            .text_size(size::sm())
                                            .text_color(p.muted)
                                            .child(description),
                                    )
                                }),
                        )
                        .when(!self.sent, |row| {
                            row.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.choose(index, option, window, cx);
                            }))
                        }),
                );
            }
            if let Some(input) = &self.others[index] {
                block = block.child(Input::new(input).small().disabled(self.sent));
            }
            card = card.child(block);
        }
        let dismiss = Button::new(id("dismiss"))
            .label(questions::dismiss_label(&self.questions).to_string())
            .small()
            .disabled(self.sent)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                let texts = this.other_texts(cx);
                let response =
                    questions::dismiss(&this.request, &this.questions, &this.selected, &texts);
                this.respond(response, cx);
            }));
        let submit = Button::new(id("submit"))
            .label(if self.sent { "Sending…" } else { "Submit" })
            .small()
            .primary()
            .disabled(!ready)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                let texts = this.other_texts(cx);
                let answers = questions::answers(&this.questions, &this.selected, &texts);
                let response = questions::submit(&this.request, answers);
                this.respond(response, cx);
            }));
        card.child(
            div()
                .flex()
                .gap(px(8.))
                .justify_end()
                .child(dismiss)
                .child(submit),
        )
    }
}
