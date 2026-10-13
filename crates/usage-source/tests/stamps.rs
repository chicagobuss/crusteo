//! Timestamps as the usage sources write them.

use usage_source::parse_unix;

#[test]
fn zulu_and_offsets_name_the_same_moment() {
    let zulu = parse_unix("2026-10-13T06:39:59Z").unwrap();
    // Claude's usage API: microseconds and an explicit offset.
    assert_eq!(parse_unix("2026-10-13T06:39:59.647491+00:00"), Some(zulu));
    assert_eq!(parse_unix("2026-10-13T06:39:59.000Z"), Some(zulu));
    assert_eq!(parse_unix("2026-10-13T01:39:59-05:00"), Some(zulu));
    assert_eq!(parse_unix("2026-10-13T12:09:59+0530"), Some(zulu));
}

#[test]
fn stamps_without_a_zone_or_with_a_bad_one_are_refused() {
    assert_eq!(parse_unix("2026-10-13T06:39:59"), None);
    assert_eq!(parse_unix("2026-10-13T06:39:59+5"), None);
    assert_eq!(parse_unix("2026-10-13T06:39:59+0é0"), None);
    assert_eq!(parse_unix(""), None);
}
