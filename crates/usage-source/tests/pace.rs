//! Pace, severity and the constraining window, against a fixed clock.

use usage_source::{
    Pool, Severity, Window, constraining_window, needle_degrees, pace, pace_brief, ratio_text,
    severity,
};

const NOW: i64 = 1_791_419_140;

fn window(key: &str, remaining: f64, seconds: f64, resets_at: f64) -> Window {
    Window {
        key: key.into(),
        label: key.into(),
        used_percent: 100. - remaining,
        remaining_percent: remaining,
        window_seconds: seconds,
        resets_at,
        ..Default::default()
    }
}

fn pool(provider: &str, windows: Vec<Window>) -> Pool {
    Pool {
        id: provider.into(),
        provider: provider.into(),
        account_label: String::new(),
        status: "ok".into(),
        windows,
        ..Default::default()
    }
}

const WEEK: f64 = 604_800.;
const FIVE_HOURS: f64 = 18_000.;

#[test]
fn codex_burns_too_fast() {
    let codex = pool(
        "codex",
        vec![window("codex:primary", 56., WEEK, 1_791_948_607.)],
    );
    let binding = constraining_window(&codex, NOW).unwrap();
    assert_eq!(binding.key, "codex:primary");
    let pace = pace(binding, NOW).unwrap();
    assert_eq!(pace.slack.round(), -32.);
    assert_eq!(ratio_text(&pace).unwrap(), "5.5x too fast");
    assert_eq!(pace_brief(&pace).unwrap(), "5.5× too fast");
    assert_eq!(severity(binding, NOW), Severity::Critical);
    assert!(needle_degrees(&pace).unwrap() > 60.);
}

#[test]
fn the_most_used_on_pace_window_constrains_gemini() {
    let gemini = pool(
        "gemini",
        vec![
            window("gemini-weekly", 69.3, WEEK, 1_791_486_149.),
            window("gemini-5h", 100., FIVE_HOURS, 1_791_436_462.),
            window("3p-weekly", 89.69, WEEK, 1_791_936_002.),
            window("3p-5h", 100., FIVE_HOURS, 1_791_436_462.),
        ],
    );
    let binding = constraining_window(&gemini, NOW).unwrap();
    assert_eq!(binding.key, "3p-weekly");
    let pace = pace(binding, NOW).unwrap();
    assert_eq!(pace.slack.round(), 4.);
    assert_eq!(ratio_text(&pace).unwrap(), "1.5x your pace available");
    assert_eq!(pace_brief(&pace).unwrap(), "1.5× room");
}

#[test]
fn a_warning_week_outranks_a_banked_session() {
    let claude = pool(
        "claude",
        vec![
            window("session", 71., FIVE_HOURS, 1_791_421_199.55),
            window("weekly_all", 52., WEEK, 1_791_827_999.55),
            window("weekly_scoped:Fable", 100., WEEK, 1_791_828_000.),
        ],
    );
    let binding = constraining_window(&claude, NOW).unwrap();
    assert_eq!(binding.key, "weekly_all");
    let pace = pace(binding, NOW).unwrap();
    assert_eq!(pace.slack.round(), -16.);
    assert_eq!(ratio_text(&pace).unwrap(), "1.9x too fast");
    assert_eq!(severity(binding, NOW), Severity::Warn);
}

#[test]
fn cursor_headlines_its_own_models() {
    let cursor = pool(
        "cursor",
        vec![
            window("cursor_models", 95.68, 2_678_400., 1_793_644_018.),
            window("other_models", 10., 2_678_400., 1_793_644_018.),
        ],
    );
    assert_eq!(
        constraining_window(&cursor, NOW).unwrap().key,
        "cursor_models"
    );
}

#[test]
fn the_needle_spreads_room_up_to_99_times_and_too_fast_up_to_10() {
    let at = |ratio: f64| {
        let pace = usage_source::Pace {
            slack: 0.,
            ratio: Some(ratio),
            remaining: 50.,
            used: 50.,
            time_left_percent: 50.,
        };
        needle_degrees(&pace).unwrap().round()
    };
    assert_eq!(at(1.), 0.);
    assert_eq!(at(10.), -45., "10× room is halfway to the stop");
    assert!(at(30.) < at(10.) - 15., "30× room reads apart from 10×");
    assert_eq!(at(99.), -90.);
    assert_eq!(at(500.), -90.);
    assert_eq!(at(0.1), 90., "10× too fast is the stop");
    assert_eq!(at(0.01), 90.);
}

#[test]
fn an_unused_window_pegs_the_needle_to_surplus() {
    let fresh = window("session", 100., FIVE_HOURS, (NOW + 9_000) as f64);
    let pace = pace(&fresh, NOW).unwrap();
    assert_eq!(needle_degrees(&pace), Some(-90.));
    assert_eq!(ratio_text(&pace).unwrap(), "unused so far");
    assert_eq!(pace_brief(&pace).unwrap(), "unused");
    assert!(usage_source::pace(&window("x", 50., 0., 0.), NOW).is_none());
}
