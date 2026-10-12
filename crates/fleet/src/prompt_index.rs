//! Prompt navigation independent of transcript row heights and tool expansion.
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PromptMark {
    pub seq: u64,
    pub preview: String,
}

pub fn preview(text: &str, has_images: bool) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return if has_images {
            "Image attachment"
        } else {
            "Empty prompt"
        }
        .into();
    }
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

#[derive(Debug, PartialEq, Eq)]
pub enum HistoryLoad {
    Wait,
    Loaded,
    Unavailable,
    FetchOlder,
}

/// The driver serializes pagination; sequence numbers only identify prompts
/// within their epoch. Never jump to an equal sequence in a replacement epoch.
pub fn history_load(
    timeline: &crate::timeline::Timeline,
    epoch: &str,
    seq: u64,
    fetching: bool,
) -> HistoryLoad {
    if fetching {
        return HistoryLoad::Wait;
    }
    if !timeline
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.epoch == epoch)
    {
        return HistoryLoad::Unavailable;
    }
    if timeline.items().any(|item| {
        matches!(&item.kind, crate::timeline::ItemKind::User { seq: Some(n), .. } if *n == seq)
    }) {
        return HistoryLoad::Loaded;
    }
    if timeline.has_older {
        HistoryLoad::FetchOlder
    } else {
        HistoryLoad::Unavailable
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum JumpState {
    #[default]
    Idle,
    Loading {
        epoch: String,
        seq: u64,
    },
    Failed(String),
}

#[derive(Debug)]
pub enum PageOutcome {
    Success { older: bool, progressed: bool },
    Failure(String),
}

impl JumpState {
    pub fn advance(
        &self,
        timeline: &crate::timeline::Timeline,
        fetching: bool,
        page: Option<&PageOutcome>,
    ) -> Self {
        let Self::Loading { epoch, seq } = self else {
            return if matches!(page, Some(PageOutcome::Success { .. })) {
                Self::Idle
            } else {
                self.clone()
            };
        };
        if let Some(PageOutcome::Failure(error)) = page {
            return Self::Failed(error.clone());
        }
        match history_load(timeline, epoch, *seq, fetching) {
            HistoryLoad::Loaded => Self::Idle,
            HistoryLoad::Unavailable => {
                Self::Failed("That prompt is no longer available. Refresh the conversation.".into())
            }
            HistoryLoad::Wait => self.clone(),
            HistoryLoad::FetchOlder => {
                if matches!(
                    page,
                    Some(PageOutcome::Success {
                        older: true,
                        progressed: false
                    })
                ) {
                    Self::Failed("Could not load that prompt. Try again.".into())
                } else {
                    self.clone()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_navigation_waits_for_fetch_and_rejects_replaced_epochs() {
        use crate::timeline::Timeline;
        let mut timeline = Timeline::default();
        timeline.apply_page(&serde_json::json!({"direction":"tail", "epoch":"current", "entries":[], "hasOlder":true,
            "startCursor":{"seq":100}, "endCursor":{"seq":150}, "window":{"maxSeq":150}}));
        assert_eq!(
            history_load(&timeline, "current", 10, true),
            HistoryLoad::Wait
        );
        assert_eq!(
            history_load(&timeline, "current", 10, false),
            HistoryLoad::FetchOlder
        );
        assert_eq!(
            history_load(&timeline, "previous", 10, false),
            HistoryLoad::Unavailable
        );
        timeline.has_older = false;
        assert_eq!(
            history_load(&timeline, "current", 10, false),
            HistoryLoad::Unavailable
        );
    }
    #[test]
    fn stalled_and_failed_jumps_recover_after_a_successful_page() {
        let mut timeline = crate::timeline::Timeline::default();
        timeline.apply_page(
            &serde_json::json!({"direction":"tail", "epoch":"e", "entries":[], "hasOlder":true,
            "startCursor":{"seq":100}, "endCursor":{"seq":150}, "window":{"maxSeq":150}}),
        );
        let loading = JumpState::Loading {
            epoch: "e".into(),
            seq: 10,
        };
        let stalled = loading.advance(
            &timeline,
            false,
            Some(&PageOutcome::Success {
                older: true,
                progressed: false,
            }),
        );
        assert!(matches!(stalled, JumpState::Failed(_)));
        assert_eq!(
            stalled.advance(
                &timeline,
                false,
                Some(&PageOutcome::Success {
                    older: false,
                    progressed: true
                })
            ),
            JumpState::Idle
        );
        assert_eq!(
            loading.advance(
                &timeline,
                false,
                Some(&PageOutcome::Failure("offline".into()))
            ),
            JumpState::Failed("offline".into())
        );
        assert_eq!(loading.advance(&timeline, true, None), loading);
    }
    #[test]
    fn previews_handle_image_only_unicode_and_long_multiline_prompts() {
        assert_eq!(preview("", true), "Image attachment");
        assert_eq!(preview(" a\n b ", false), "a b");
        assert_eq!(preview(&"🦀".repeat(161), false).chars().count(), 161);
    }
}
