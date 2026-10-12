//! Transcript navigation geometry, separate from daemon data.
use fleet::prompt_index::PromptMark;
use std::collections::BTreeMap;

#[derive(Default)]
pub struct PromptNavigation {
    pub loaded: Vec<PromptMark>,
    pub rows: BTreeMap<u64, usize>,
    pub hovered: Option<u64>,
    pub reading_row: Option<usize>,
    pub pending: Option<u64>,
}
impl PromptNavigation {
    pub fn set_reading_row(&mut self, row: Option<usize>) -> bool {
        if self.reading_row == row {
            false
        } else {
            self.reading_row = row;
            true
        }
    }
}

pub fn magnification(distance: usize) -> f32 {
    if distance >= 3 {
        return 0.;
    }
    (1. + (std::f32::consts::PI * distance as f32 / 3.).cos()) / 2.
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_layout_scroll_notifications_do_not_request_more_frames() {
        let mut navigation = PromptNavigation::default();
        assert!(!navigation.set_reading_row(None));
        assert!(navigation.set_reading_row(Some(4)));
        assert!(!navigation.set_reading_row(Some(4)));
        assert!(navigation.set_reading_row(None));
        assert!(!navigation.set_reading_row(None));
    }
}
