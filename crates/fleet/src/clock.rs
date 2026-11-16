//! Wall-clock helpers: the ISO timestamps daemons send, epoch millis, and
//! the short "3m ago" form views show. No time zone database; daemons send
//! UTC.

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

pub fn now_iso() -> String {
    iso_from_millis(now_millis())
}

/// Formats epoch millis as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC).
pub fn iso_from_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parses `YYYY-MM-DDTHH:MM:SS(.fff)Z` into epoch millis. A `+00:00`
/// suffix reads the same as `Z`; other offsets are not applied.
pub fn parse_iso(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let millis = if bytes.get(19) == Some(&b'.') {
        let frac: String = text[20..]
            .chars()
            .take_while(char::is_ascii_digit)
            .take(3)
            .collect();
        format!("{frac:0<3}").parse::<i64>().ok()?
    } else {
        0
    };
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj.rem_euclid(400);
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + s) * 1000) + millis)
}

/// "3m ago" style.
pub fn time_ago(iso: &str) -> String {
    time_ago_at(iso, now_millis())
}

/// [`time_ago`] as of `now`, in epoch millis.
pub fn time_ago_at(iso: &str, now: i64) -> String {
    let Some(then) = parse_iso(iso) else {
        return String::new();
    };
    let secs = (now - then) / 1000;
    match secs {
        s if s < 45 => "just now".into(),
        s if s < 3600 => format!("{}m ago", (s / 60).max(1)),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// How soon a view showing [`time_ago`] labels must draw again so they keep
/// up, given the newest time it labels: a label is only reworked when its
/// view draws. Half a minute while any can say "just now" or minutes, five
/// minutes once all say hours or days.
pub fn ago_redraw_after(newest: &str, now: i64) -> Option<std::time::Duration> {
    let then = parse_iso(newest)?;
    let secs = if now - then < 3_600_000 { 30 } else { 300 };
    Some(std::time::Duration::from_secs(secs))
}

/// Whether a view that wants its next draw by `wanted` must set a new timer,
/// given the one it has queued, due at `queued` (none if it has none). Only
/// a timer due later than wanted is replaced, so a fresh "just now" is not
/// left waiting on a five-minute wake. One due sooner, or at the same time,
/// stays: it draws the view, which asks again.
pub fn ages_timer_needs_replacing(
    queued: Option<std::time::Instant>,
    wanted: std::time::Instant,
) -> bool {
    queued.is_none_or(|due| wanted < due)
}

/// A random-looking v4 UUID, for message and idempotency ids. Not for
/// secrets: it comes from the standard library's hasher seeds.
pub fn uuid_v4() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_i64(now_millis());
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip() {
        let ms = parse_iso("2026-10-02T06:57:24.852Z").unwrap();
        assert_eq!(iso_from_millis(ms), "2026-10-02T06:57:24.852Z");
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso("2026-10-02T06:57:24+00:00"),
            parse_iso("2026-10-02T06:57:24Z")
        );
        assert_eq!(parse_iso("not a time"), None);
    }

    #[test]
    fn an_age_is_told_from_when_it_is_drawn_not_when_the_item_came() {
        let at = "2026-11-13T09:52:00.000Z";
        let then = parse_iso(at).unwrap();
        assert_eq!(time_ago_at(at, then + 15_000), "just now");
        assert_eq!(time_ago_at(at, then + 90_000), "1m ago");
        assert_eq!(time_ago_at(at, then + (6 * 60 + 48) * 60_000), "6h ago");
        assert_eq!(time_ago_at(at, then + 3 * 86_400_000), "3d ago");
        assert_eq!(time_ago_at("not a time", then), "");
    }

    #[test]
    fn a_just_now_label_is_redrawn_before_it_goes_stale() {
        let at = "2026-11-13T09:52:00.000Z";
        let then = parse_iso(at).unwrap();
        // "just now" lasts 45 s, so the next draw comes sooner than that.
        let soon = ago_redraw_after(at, then + 5_000).unwrap();
        assert!(soon.as_secs() < 45);
        // Hours change slowly: no need to draw as often.
        let later = ago_redraw_after(at, then + 2 * 3_600_000).unwrap();
        assert!(later > soon && later.as_secs() <= 600);
        assert_eq!(ago_redraw_after("", then), None);
    }

    #[test]
    fn a_new_item_pulls_in_a_timer_queued_for_old_ones() {
        use std::time::{Duration, Instant};
        let at = "2026-11-13T09:52:00.000Z";
        let then = parse_iso(at).unwrap();
        let start = Instant::now();

        // Nothing queued: set a timer.
        assert!(ages_timer_needs_replacing(None, start));

        // An idle view whose newest item is hours old queues the long wake.
        let long = ago_redraw_after(at, then + 2 * 3_600_000).unwrap();
        let queued = start + long;

        // Ten seconds later something new arrives and wants the short one:
        // it is due before the long wake, so it replaces it.
        let short = ago_redraw_after(at, then + 5_000).unwrap();
        let wanted = start + Duration::from_secs(10) + short;
        assert!(wanted < queued);
        assert!(ages_timer_needs_replacing(Some(queued), wanted));

        // Drawing again a few seconds on wants a later wake than the one now
        // queued, so the timer stays and does not restart on every draw.
        let redrawn = start + Duration::from_secs(15) + short;
        assert!(!ages_timer_needs_replacing(Some(wanted), redrawn));
        // The same deadline stays too.
        assert!(!ages_timer_needs_replacing(Some(wanted), wanted));
        // And a short timer is not swapped for a long one.
        assert!(!ages_timer_needs_replacing(Some(wanted), queued));
    }

    #[test]
    fn uuids_look_like_v4() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_ne!(id, uuid_v4());
    }
}
