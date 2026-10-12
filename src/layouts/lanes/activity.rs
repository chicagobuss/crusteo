//! Card age text and the balanced lane activity summary.

pub fn label(at: Option<&str>, now: i64) -> String {
    let Some(then) = at.and_then(crate::app::store::parse_iso) else {
        return "No activity yet".into();
    };
    let minutes = now.saturating_sub(then).max(0) / 60_000;
    let (value, unit) = if minutes < 120 {
        (minutes, "minute")
    } else if minutes < 2 * 24 * 60 {
        (minutes / 60, "hour")
    } else {
        (minutes / (24 * 60), "day")
    };
    format!("{value} {unit}{}", if value == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_boundaries_and_missing_or_future_activity() {
        let at = "2026-10-04T00:00:00Z";
        let then = crate::app::store::parse_iso(at).unwrap();
        for (minutes, expected) in [
            (0, "0 minutes"),
            (1, "1 minute"),
            (119, "119 minutes"),
            (120, "2 hours"),
            (32 * 60, "32 hours"),
            (48 * 60 - 1, "47 hours"),
            (48 * 60, "2 days"),
            (7 * 24 * 60, "7 days"),
        ] {
            assert_eq!(label(Some(at), then + minutes * 60_000), expected);
        }
        assert_eq!(label(Some(at), then - 60_000), "0 minutes");
        assert_eq!(label(None, then), "No activity yet");
        assert_eq!(label(Some("invalid"), then), "No activity yet");
    }
}

/// Fresh activity establishes the baseline. Aging blocked/error cards can
/// warm a busy lane; finished-turn notifications have a gentler influence.
#[derive(Clone, Copy, Default)]
pub struct Summary {
    latest: Option<i64>,
    blocked: Option<i64>,
    notification: Option<i64>,
    blocked_count: usize,
    total: usize,
}
impl Summary {
    pub fn from_cards(cards: &[fleet::LaneCard]) -> Self {
        Self::from_timestamps(cards.iter().filter_map(|card| {
            Some((
                crate::app::store::parse_iso(card.updated_at.as_deref()?)?,
                card.bucket,
            ))
        }))
    }
    fn from_timestamps(cards: impl IntoIterator<Item = (i64, fleet::Bucket)>) -> Self {
        let mut summary = Self::default();
        for (at, bucket) in cards {
            summary.total += 1;
            summary.latest = Some(summary.latest.map_or(at, |latest| latest.max(at)));
            match bucket {
                fleet::Bucket::NeedsInput | fleet::Bucket::Failed => {
                    summary.blocked_count += 1;
                    summary.blocked = Some(summary.blocked.map_or(at, |oldest| oldest.min(at)));
                }
                fleet::Bucket::Attention => {
                    summary.notification =
                        Some(summary.notification.map_or(at, |oldest| oldest.min(at)))
                }
                _ => {}
            }
        }
        summary
    }
    pub fn score(self, now: i64) -> Option<f32> {
        let baseline = heat(self.latest?, now);
        let share = self.blocked_count as f32 / self.total.max(1) as f32;
        let blocked = self
            .blocked
            .map_or(0., |at| heat(at, now) * (0.75 + 0.20 * share));
        let notification = self.notification.map_or(0., |at| heat(at, now) * 0.35);
        Some(baseline.max(blocked).max(notification))
    }
}

fn heat(at: i64, now: i64) -> f32 {
    let minutes = now.saturating_sub(at).max(0) as f32 / 60_000.;
    let stops = [(15., 0.), (360., 1. / 3.), (2880., 2. / 3.), (10080., 1.)];
    if minutes <= stops[0].0 {
        return 0.;
    }
    for pair in stops.windows(2) {
        if minutes <= pair[1].0 {
            return pair[0].1
                + (pair[1].1 - pair[0].1) * (minutes - pair[0].0) / (pair[1].0 - pair[0].0);
        }
    }
    1.
}

#[cfg(test)]
mod summary_tests {
    use super::*;
    #[test]
    fn recency_age_boundaries_and_missing_timestamps() {
        assert_eq!(Summary::default().score(0), None);
        let summary = Summary {
            latest: Some(0),
            ..Summary::default()
        };
        assert_eq!(summary.score(-60_000), Some(0.));
        for (minutes, expected) in [
            (15, 0.),
            (360, 1. / 3.),
            (2880, 2. / 3.),
            (10080, 1.),
            (100000, 1.),
        ] {
            assert_eq!(summary.score(minutes * 60_000), Some(expected));
        }
    }
    #[test]
    fn smart_balance_surfaces_old_blockers_without_overheating_finished_work() {
        use fleet::Bucket;
        let now = 10 * 24 * 60 * 60_000;
        let old = now - 7 * 24 * 60 * 60_000;
        let healthy = Summary::from_timestamps([(now, Bucket::Running), (old, Bucket::Done)]);
        let notification =
            Summary::from_timestamps([(now, Bucket::Running), (old, Bucket::Attention)]);
        let blocked = Summary::from_timestamps([(now, Bucket::Running), (old, Bucket::NeedsInput)]);
        let many_blocked = Summary::from_timestamps([
            (now, Bucket::Running),
            (old, Bucket::NeedsInput),
            (old, Bucket::Failed),
        ]);
        assert_eq!(healthy.score(now), Some(0.));
        assert!(notification.score(now).unwrap() < blocked.score(now).unwrap());
        assert!(blocked.score(now).unwrap() >= 0.8);
        assert!(many_blocked.score(now).unwrap() > blocked.score(now).unwrap());
        let fresh_blocker =
            Summary::from_timestamps([(now, Bucket::Running), (now, Bucket::NeedsInput)]);
        assert_eq!(fresh_blocker.score(now), Some(0.));
    }
}
