//! The same account reported twice shows once.

use usage_source::{Pool, Window, dedupe_pools, same_account};

fn window(key: &str, used: f64, resets_at: f64) -> Window {
    Window {
        key: key.into(),
        label: key.into(),
        used_percent: used,
        remaining_percent: 100. - used,
        window_seconds: 18_000.,
        resets_at,
        ..Default::default()
    }
}

fn pool(id: &str, provider: &str, windows: Vec<Window>) -> Pool {
    Pool {
        id: id.into(),
        provider: provider.into(),
        account_label: id.into(),
        status: "ok".into(),
        windows,
        ..Default::default()
    }
}

#[test]
fn one_account_through_two_hosts_and_a_hub_is_one_pool() {
    let hub = pool(
        "personal",
        "codex",
        vec![window("codex:primary", 56., 1_000_000.)],
    );
    let host_a = pool(
        "paseo:a:codex",
        "codex",
        vec![window("five_hour", 56.4, 1_000_030.)],
    );
    let host_b = pool(
        "paseo:b:codex",
        "codex",
        vec![window("five_hour", 56.4, 1_000_030.)],
    );
    let kept = dedupe_pools(vec![hub, host_a, host_b]);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id, "personal");
}

#[test]
fn different_accounts_stay_apart() {
    let work = pool("work", "codex", vec![window("w", 56., 1_000_000.)]);
    let home = pool("home", "codex", vec![window("w", 12., 1_000_000.)]);
    let later = pool("later", "codex", vec![window("w", 56., 1_090_000.)]);
    let claude = pool("claude", "claude", vec![window("w", 56., 1_000_000.)]);
    assert!(!same_account(&work, &home));
    assert!(!same_account(&work, &later));
    assert!(!same_account(&work, &claude));
    assert_eq!(dedupe_pools(vec![work, home, later, claude]).len(), 4);
}

#[test]
fn a_window_of_unknown_length_learns_it_from_the_same_window_elsewhere() {
    // Paseo 0.10 names Codex's weekly window "session"; 0.11 says "weekly".
    let mut old = window("session", 33., 1_000_000.);
    old.window_seconds = 0.;
    let mut weekly = window("weekly", 33., 1_000_000.);
    weekly.label = "Weekly".into();
    weekly.window_seconds = 604_800.;
    let kept = dedupe_pools(vec![
        pool("paseo:old:codex", "codex", vec![old]),
        pool("paseo:new:codex", "codex", vec![weekly]),
    ]);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id, "paseo:old:codex", "the first is still kept");
    assert_eq!(kept[0].windows[0].window_seconds, 604_800.);
    assert_eq!(kept[0].windows[0].label, "Weekly");
}

/// A feed names a Codex account "personal" and fingerprints it; a daemon
/// names it by email. Read either side of a reset they disagree on the
/// numbers, but it is still one account.
#[test]
fn a_feeds_fingerprint_knows_the_account_a_daemon_names_by_email() {
    let mut hub = pool(
        "codex:personal",
        "codex",
        vec![window("codex:primary", 18., 2e9)],
    );
    hub.account_label = "personal".into();
    hub.fingerprint = "openai:b7f5e7db797892b3d29cdbfa".into();
    let report = |source: &str, used: f64| {
        serde_json::json!({"report": {
            "id": format!("{source}:1"), "sourceId": source, "account": {"label": "work@example.com"},
            "fetchedAt": "2026-10-11T08:00:00Z",
            "report": {"status": "available", "windows": [
                {"id": "weekly", "label": "Weekly", "usedPct": used, "resetsAt": "2033-05-18T03:33:20Z"}
            ]}
        }})
    };
    let daemon = |source: &str, used: f64| {
        usage_source::reports::parse_reports("alpha", &[report(source, used)], 0)
            .unwrap()
            .snapshot
            .pools
    };
    let codex = daemon("codex", 87.);
    assert_eq!(codex[0].fingerprint, hub.fingerprint);
    let kept = dedupe_pools([vec![hub.clone()], codex].concat());
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].account_label, "personal", "the feed's name stays");

    // A Claude login's email can be a person's account and a team's, so it
    // says nothing on its own.
    let claude = daemon("claude", 87.);
    assert!(claude[0].fingerprint.is_empty());
    hub.provider = "claude".into();
    assert_eq!(dedupe_pools([vec![hub], claude].concat()).len(), 2);
}

#[test]
fn windows_without_resets_match_on_key_and_use() {
    let a = pool("a", "claude", vec![window("weekly", 30., 0.)]);
    let b = pool("b", "claude", vec![window("weekly", 30., 0.)]);
    let c = pool("c", "claude", vec![window("weekly", 31., 0.)]);
    assert!(same_account(&a, &b));
    assert!(!same_account(&a, &c));
}
