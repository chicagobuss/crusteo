//! Questions an agent asks through a permission request of kind
//! `question`, such as Claude Code's AskUserQuestion: the form they make and
//! the answers it sends back, as Paseo's own question form does
//! (`packages/app/src/components/question-form-card-core.ts`).

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

/// One question of a request.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
    /// A free-text answer is offered beside the options.
    pub allow_other: bool,
    /// It may be left unanswered.
    pub allow_empty: bool,
    pub placeholder: Option<String>,
    pub dismiss_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

impl Question {
    /// It has a text box: it allows another answer, or has no options.
    pub fn shows_text(&self) -> bool {
        self.options.is_empty() || self.allow_other
    }

    /// Whether `selected` options and `other` text answer it.
    pub fn answered(&self, selected: &BTreeSet<usize>, other: &str) -> bool {
        !selected.is_empty()
            || (self.shows_text() && (!other.trim().is_empty() || self.allow_empty))
    }
}

/// Whether a permission request is a question rather than a permission.
pub fn is_question(request: &Value) -> bool {
    request["kind"] == "question"
}

/// The questions in a request's `input`, or None when it holds none or any
/// is malformed, as Paseo's parser decides.
pub fn parse(input: &Value) -> Option<Vec<Question>> {
    let raw = input["questions"].as_array()?;
    let text = |value: &Value| value.as_str().map(str::to_string);
    let questions = raw
        .iter()
        .map(|q| {
            let options = q["options"]
                .as_array()?
                .iter()
                .map(|o| {
                    Some(QuestionOption {
                        label: text(&o["label"])?,
                        description: text(&o["description"]),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(Question {
                header: text(&q["header"])?,
                question: text(&q["question"])?,
                options,
                multi_select: q["multiSelect"] == true,
                allow_other: q["allowOther"] == true || q["isOther"] == true,
                allow_empty: q["allowEmpty"] == true,
                placeholder: text(&q["placeholder"]),
                dismiss_label: text(&q["dismissLabel"]),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    (!questions.is_empty()).then_some(questions)
}

/// The answers, by each question's header: the chosen options' labels,
/// joined; or the text, which replaces a single choice and joins a
/// multiple one, as Claude Code's own form does.
pub fn answers(
    questions: &[Question],
    selected: &[BTreeSet<usize>],
    others: &[String],
) -> Map<String, Value> {
    let mut answers = Map::new();
    for (index, question) in questions.iter().enumerate() {
        let labels: Vec<&str> = selected
            .get(index)
            .into_iter()
            .flatten()
            .filter_map(|&option| question.options.get(option))
            .map(|option| option.label.as_str())
            .collect();
        let other = others.get(index).map_or("", |text| text.trim());
        if question.shows_text() {
            if !other.is_empty() {
                let answer = if question.multi_select {
                    [labels.as_slice(), &[other]].concat().join(", ")
                } else {
                    other.to_string()
                };
                answers.insert(question.header.clone(), json!(answer));
                continue;
            }
            if question.allow_empty && question.options.is_empty() {
                answers.insert(question.header.clone(), json!(""));
                continue;
            }
        }
        if !labels.is_empty() {
            answers.insert(question.header.clone(), json!(labels.join(", ")));
        }
    }
    answers
}

/// The response that submits the answers: the request's own input with
/// them added.
pub fn submit(request: &Value, answers: Map<String, Value>) -> Value {
    let mut input = request["input"].as_object().cloned().unwrap_or_default();
    input.insert("answers".into(), Value::Object(answers));
    json!({ "behavior": "allow", "updatedInput": input })
}

/// The response that dismisses the questions. Questions that are all free
/// text and may be left empty submit their (empty) answers instead.
pub fn dismiss(
    request: &Value,
    questions: &[Question],
    selected: &[BTreeSet<usize>],
    others: &[String],
) -> Value {
    let all_optional_text = questions
        .iter()
        .all(|question| question.allow_empty && question.options.is_empty());
    if all_optional_text {
        submit(request, answers(questions, selected, others))
    } else {
        json!({ "behavior": "deny", "message": "Dismissed by user" })
    }
}

/// What the dismiss button says.
pub fn dismiss_label(questions: &[Question]) -> &str {
    questions
        .iter()
        .find_map(|question| question.dismiss_label.as_deref())
        .unwrap_or("Dismiss")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Value {
        json!({
            "kind": "question",
            "input": {
                "questions": [
                    {
                        "id": "0",
                        "header": "Question 1",
                        "question": "Which pass?",
                        "options": [
                            { "label": "Bounded pass only" },
                            { "label": "Full campaign", "description": "Everything" }
                        ],
                        "isOther": true
                    },
                    {
                        "header": "Targets",
                        "question": "Which platforms?",
                        "multiSelect": true,
                        "options": [{ "label": "macOS" }, { "label": "Linux" }]
                    }
                ]
            }
        })
    }

    fn picks(sets: &[&[usize]]) -> Vec<BTreeSet<usize>> {
        sets.iter()
            .map(|set| set.iter().copied().collect())
            .collect()
    }

    #[test]
    fn questions_parse_with_their_options_and_flags() {
        let request = request();
        assert!(is_question(&request));
        let questions = parse(&request["input"]).unwrap();
        assert_eq!(questions.len(), 2);
        assert!(questions[0].allow_other, "isOther counts as allowOther");
        assert!(!questions[0].multi_select);
        assert_eq!(
            questions[0].options[1].description.as_deref(),
            Some("Everything")
        );
        assert!(questions[1].multi_select && !questions[1].shows_text());
        assert!(parse(&json!({ "questions": [] })).is_none());
        assert!(parse(&json!({ "questions": [{ "header": "h" }] })).is_none());
    }

    #[test]
    fn answers_go_by_header_and_text_replaces_a_single_choice() {
        let questions = parse(&request()["input"]).unwrap();
        let none = vec![String::new(), String::new()];
        let chosen = answers(&questions, &picks(&[&[0], &[0, 1]]), &none);
        assert_eq!(chosen["Question 1"], "Bounded pass only");
        assert_eq!(chosen["Targets"], "macOS, Linux");
        let typed = vec!["Just the M1".to_string(), String::new()];
        let replaced = answers(&questions, &picks(&[&[1], &[1]]), &typed);
        assert_eq!(replaced["Question 1"], "Just the M1");
        assert!(!questions[1].answered(&BTreeSet::new(), ""));
        assert!(questions[0].answered(&BTreeSet::new(), "text"));
    }

    #[test]
    fn submitting_keeps_the_input_and_dismissing_denies() {
        let request = request();
        let questions = parse(&request["input"]).unwrap();
        let selected = picks(&[&[0], &[1]]);
        let others = vec![String::new(), String::new()];
        let response = submit(&request, answers(&questions, &selected, &others));
        assert_eq!(response["behavior"], "allow");
        assert_eq!(
            response["updatedInput"]["questions"][0]["header"],
            "Question 1"
        );
        assert_eq!(response["updatedInput"]["answers"]["Targets"], "Linux");
        let dismissed = dismiss(&request, &questions, &selected, &others);
        assert_eq!(dismissed["behavior"], "deny");
        assert_eq!(dismiss_label(&questions), "Dismiss");
    }

    #[test]
    fn optional_free_text_questions_submit_empty_on_dismiss() {
        let request = json!({ "input": { "questions": [{
            "header": "Notes", "question": "Anything else?", "options": [],
            "allowEmpty": true, "dismissLabel": "Skip"
        }]}});
        let questions = parse(&request["input"]).unwrap();
        let response = dismiss(&request, &questions, &[BTreeSet::new()], &[String::new()]);
        assert_eq!(response["behavior"], "allow");
        assert_eq!(response["updatedInput"]["answers"]["Notes"], "");
        assert_eq!(dismiss_label(&questions), "Skip");
    }
}
