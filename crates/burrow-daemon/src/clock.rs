//! Timestamps as the wire carries them.

use time::OffsetDateTime;
use time::macros::format_description;

/// Now, as ISO 8601 with milliseconds and `Z`, like JavaScript's `toISOString()`.
pub fn now_iso() -> String {
    iso(OffsetDateTime::now_utc())
}

pub fn iso(at: OffsetDateTime) -> String {
    let format =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    at.to_offset(time::UtcOffset::UTC)
        .format(&format)
        .unwrap_or_default()
}

/// Now, but strictly after `previous` (Paseo's `touchUpdatedAt`): clients
/// order snapshots by `updatedAt`, so it must grow with every change.
pub fn after(previous: &str) -> String {
    let now = OffsetDateTime::now_utc();
    match OffsetDateTime::parse(previous, &time::format_description::well_known::Rfc3339) {
        Ok(previous) if now <= previous => iso(previous + time::Duration::milliseconds(1)),
        _ => iso(now),
    }
}

/// Milliseconds since the Unix epoch as ISO 8601 (`new Date(ms).toISOString()`).
pub fn iso_from_ms(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map(iso)
        .unwrap_or_default()
}

/// Milliseconds since the Unix epoch, for `pong.serverReceivedAt` and friends.
pub fn now_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// Seconds since the Unix epoch.
pub fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// An ISO 8601 timestamp as seconds since the Unix epoch.
pub fn parse_iso(stamp: &str) -> Option<i64> {
    OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(OffsetDateTime::unix_timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_javascript() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_790_924_201_098_000_000).unwrap();
        assert_eq!(iso(at), "2026-10-02T06:56:41.098Z");
    }

    #[test]
    fn after_is_strictly_later() {
        assert_eq!(
            after("2999-01-01T00:00:00.000Z"),
            "2999-01-01T00:00:00.001Z"
        );
        assert!(after("2000-01-01T00:00:00.000Z").as_str() > "2000-01-01T00:00:00.000Z");
    }
}
