//! The `usage` view: daemons' usage reports and a usage feed, merged by
//! the rules crusteo's gauges follow, as a snapshot and then deltas.

mod support;

use std::time::Duration;

use reef::fake::{self, FakeDaemon, FakeFeed};
use serde_json::{Value, json};
use support::{Phone, TOKEN, reef_configured};

const HOUR: i64 = 3_600;

fn claude_work(weekly: f64) -> Value {
    fake::usage_report(
        "claude:work",
        "claude",
        "sam@work.example",
        "Max",
        &[
            ("five_hour", "5h", 22., 3 * HOUR),
            ("seven_day", "Weekly", weekly, 4 * 24 * HOUR),
        ],
    )
}

fn codex_team() -> Value {
    fake::usage_report(
        "codex:team",
        "codex",
        "team@example.com",
        "Pro",
        &[
            ("five_hour", "5h", 64., 2 * HOUR),
            ("seven_day", "Weekly", 88., 30 * HOUR),
        ],
    )
}

fn gemini_home(used: f64) -> Value {
    fake::usage_report(
        "gemini:home",
        "gemini",
        "sam@home.example",
        "AI Pro",
        &[("daily", "Daily", used, 9 * HOUR)],
    )
}

/// A feed that knows the work account by its own name and plan.
fn feed_body(weekly: f64) -> String {
    let now = fleet::clock::now_millis() / 1000;
    json!({
        "revision": 1,
        "served_at": fleet::clock::iso_from_millis(now * 1000),
        "pools": {"claude:work": {
            "provider": "claude", "account_label": "sam@work.example", "plan": "max",
            "status": "ok", "windows": [
                {"key": "session", "label": "session", "used_percent": 22.0,
                 "remaining_percent": 78.0, "window_seconds": 18000.0,
                 "resets_at": (now + 3 * HOUR) as f64},
                {"key": "weekly_all", "label": "weekly", "used_percent": weekly,
                 "remaining_percent": 100. - weekly, "window_seconds": 604800.0,
                 "resets_at": (now + 4 * 24 * HOUR) as f64}
            ]}}
    })
    .to_string()
}

struct Fleet {
    a: FakeDaemon,
    b: FakeDaemon,
    feed: FakeFeed,
    addr: std::net::SocketAddr,
}

async fn fleet(every: Duration) -> Fleet {
    let a = FakeDaemon::start("srv_a").await.unwrap();
    let b = FakeDaemon::start("srv_b").await.unwrap();
    fake::seed(&a, "a");
    fake::seed(&b, "b");
    a.set_usage_reports(vec![claude_work(41.), codex_team()]);
    // The team account through a second host is the same account.
    b.set_usage_reports(vec![codex_team(), gemini_home(12.)]);
    let feed = FakeFeed::start(&feed_body(41.)).await.unwrap();
    let url = feed.url();
    let addr = reef_configured(&[("example-a", &a), ("example-b", &b)], |config| {
        config.usage = Some(url);
        config.limits.usage_ask = every;
        config.limits.usage_feed = every;
    })
    .await;
    Fleet { a, b, feed, addr }
}

async fn watch(addr: std::net::SocketAddr) -> Phone {
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "u", "view": "usage"}))
        .await;
    phone
        .until("every account", |p| p.keys("u").len() == 3)
        .await;
    phone
}

fn row<'a>(phone: &'a Phone, key: &str) -> &'a Value {
    (phone.view("u").parts.iter())
        .find(|row| row["key"] == key)
        .unwrap_or_else(|| panic!("no row {key}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_comes_from_daemons_and_the_feed_each_account_once() {
    let fleet = fleet(Duration::from_millis(300)).await;
    let mut phone = watch(fleet.addr).await;
    assert_eq!(
        phone.keys("u"),
        ["claude:work", "paseo:codex:team", "paseo:gemini:home"],
        "the feed names the work account; the team account shows once"
    );
    let work = row(&phone, "claude:work");
    assert_eq!(work["plan"], "max");
    assert_eq!(work["provider"], "claude");
    assert_eq!(work["stale"], false);
    assert_eq!(work["windows"][1]["name"], "weekly");
    let team = row(&phone, "paseo:codex:team");
    assert_eq!(team["severity"], "warn");
    assert_eq!(team["tightest"], true, "the header gauge shows it");
    assert_eq!(team["plan"], "Pro");
    let home = row(&phone, "paseo:gemini:home");
    assert_eq!(home["tightest"], false);
    assert_eq!(
        home["use_before"],
        Value::Null,
        "nine hours out is not soon"
    );

    // Each daemon was asked with the one usage read Reef may send.
    for daemon in [&fleet.a, &fleet.b] {
        assert!(
            (daemon.requests().iter()).any(|r| r == "usage.list_reports.request"),
            "{:?}",
            daemon.requests()
        );
        assert!(daemon.refused().is_empty(), "{:?}", daemon.refused());
    }

    // A daemon's numbers move: the row comes again as a delta.
    fleet
        .b
        .set_usage_reports(vec![codex_team(), gemini_home(50.)]);
    phone
        .until("the new daily use", |p| {
            row(p, "paseo:gemini:home")["windows"][0]["used"] == 50.
        })
        .await;
    // So do the feed's.
    fleet.feed.set_body(&feed_body(45.));
    phone
        .until("the feed's new weekly use", |p| {
            row(p, "claude:work")["windows"][1]["used"] == 45.
        })
        .await;
    let view = phone.view("u");
    assert_eq!(view.snapshots, 1, "changes come as deltas");
    assert!(view.deltas >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn phones_never_make_the_reef_read_the_feed() {
    // Read once at start, then not for a minute, however many phones look.
    let fleet = fleet(Duration::from_secs(60)).await;
    let mut first = watch(fleet.addr).await;
    let mut second = watch(fleet.addr).await;
    for phone in [&mut first, &mut second] {
        let before = phone.view("u").snapshots;
        phone.send(json!({"type": "resync", "id": "u"})).await;
        phone
            .until("a fresh snapshot", |p| p.view("u").snapshots > before)
            .await;
    }
    assert_eq!(fleet.feed.reads(), 1);
}

/// As Paseo 0.12: a failed read leaves an account's last numbers on
/// screen, marked retained, and accounts stay in place however a daemon
/// orders its reports.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_read_keeps_the_last_numbers_and_rows_keep_their_places() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    daemon.set_usage_reports(vec![claude_work(41.), codex_team(), gemini_home(12.)]);
    let addr = reef_configured(&[("example-a", &daemon)], |config| {
        config.limits.usage_ask = Duration::from_millis(300);
    })
    .await;
    let mut phone = watch(addr).await;
    let order = ["paseo:claude:work", "paseo:codex:team", "paseo:gemini:home"];
    assert_eq!(phone.keys("u"), order);

    // A daemon before 0.12 sends a failed report without the last good
    // one, and lists its reports in the order they finished.
    daemon.set_usage_reports(vec![
        fake::usage_failure(&gemini_home(12.), "Rate limited", None),
        codex_team(),
        claude_work(44.),
    ]);
    phone
        .until("the failure", |p| {
            row(p, "paseo:gemini:home")["retained"] == true
        })
        .await;
    let home = row(&phone, "paseo:gemini:home");
    assert_eq!(home["windows"][0]["used"], 12., "the last numbers stay");
    assert_eq!(home["failure"]["status"], "error");
    assert_eq!(home["failure"]["problem"], "Rate limited");
    assert_eq!(home["use_before"], Value::Null);
    phone
        .until("the new weekly use", |p| {
            row(p, "paseo:claude:work")["windows"][1]["used"] == 44.
        })
        .await;
    assert_eq!(phone.keys("u"), order, "nothing moved");

    // Paseo 0.12 keeps the last good report itself.
    daemon.set_usage_reports(vec![
        claude_work(44.),
        codex_team(),
        fake::usage_failure(&gemini_home(30.), "Rate limited", Some(15 * 60)),
    ]);
    phone
        .until("the daemon's kept numbers", |p| {
            row(p, "paseo:gemini:home")["windows"][0]["used"] == 30.
        })
        .await;
    let home = row(&phone, "paseo:gemini:home");
    assert_eq!(home["retained"], true);
    assert_eq!(home["stale"], true, "fifteen minutes old");

    // A good read again.
    daemon.set_usage_reports(vec![claude_work(44.), codex_team(), gemini_home(31.)]);
    phone
        .until("fresh numbers", |p| {
            row(p, "paseo:gemini:home")["retained"] == false
        })
        .await;
    assert_eq!(row(&phone, "paseo:gemini:home")["failure"], Value::Null);
    assert_eq!(phone.keys("u"), order);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reef_with_no_usage_sends_an_empty_view() {
    let daemon = FakeDaemon::start("srv_a").await.unwrap();
    fake::seed(&daemon, "a");
    let addr = reef_configured(&[("example-a", &daemon)], |_| {}).await;
    let (mut phone, _) = Phone::connect(addr, TOKEN).await;
    phone
        .send(json!({"type": "subscribe", "id": "u", "view": "usage"}))
        .await;
    phone
        .until("the snapshot", |p| p.views.contains_key("u"))
        .await;
    assert!(phone.view("u").parts.is_empty());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !(daemon.requests().iter()).any(|r| r.starts_with("usage.")),
        "a daemon without usage sources is not asked"
    );
}
