//! Account usage for phones: the same readings crusteo's title bar draws,
//! merged and shown by the same rules ([`usage_source`]).
//!
//! Two kinds of source feed it. Every connected daemon with usage sources
//! is asked for its usage reports (`upstream.rs`, every two minutes), and
//! an optional usage feed (`--usage <url>`) is read every minute
//! ([`watch_feed`]). Reef keeps the latest reading of each and works
//! out the rows itself; phones subscribe to the `usage` view and get a
//! snapshot, then deltas when a row changes. No phone ever reaches the
//! feed.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use usage_source::{Reading, merge, view};

use crate::upstream::{Apply, Upstream};

/// The feed's reading, keyed as crusteo keys its first usage source: one
/// that names its accounts, so its names win.
pub const FEED_KEY: &str = "source:0";

/// A daemon's reading, keyed as crusteo's daemon extension keys it.
pub fn host_key(host: &str) -> String {
    format!("paseo:{host}")
}

/// How often the feed is read, as crusteo polls a usage source.
pub const FEED_EVERY: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(8);
/// A usage answer is a few KB; anything far bigger is not one.
const MAX_BODY: usize = 512 * 1024;

/// Every account's row, as phones get it, keyed by pool. The one that most
/// needs watching says `tightest: true`, for the page's header gauge.
pub fn rows(readings: &BTreeMap<String, Reading>, now: i64) -> Vec<(String, Value)> {
    let merged = merge::merge(readings);
    let rows = view::rows(&merged, now);
    let tightest = view::tightest(&rows).map(|row| row.pool.id.clone());
    rows.iter()
        .map(|row| {
            let mut json = row.to_json();
            json["tightest"] = json!(Some(&row.pool.id) == tightest.as_ref());
            (row.pool.id.clone(), json)
        })
        .collect()
}

/// Reads the feed now and then for as long as Reef runs, handing each
/// reading to Reef's task. A failed read keeps the last good one, which
/// goes stale on the phones after ten minutes.
pub fn watch_feed(base: String, every: Duration, apply: mpsc::UnboundedSender<Apply>) {
    tokio::spawn(async move {
        let url = format!("{base}{}", usage_source::feed::USAGE_PATH);
        let mut last_error = None;
        loop {
            match read_feed(&url).await {
                Ok(snapshot) => {
                    last_error = None;
                    let reading = Reading {
                        snapshot,
                        eligibility: Vec::new(),
                    };
                    let set = move |up: &mut Upstream| up.set_usage(FEED_KEY, Some(reading));
                    if apply.send(Box::new(set)).is_err() {
                        return;
                    }
                }
                Err(err) => {
                    if last_error.as_ref() != Some(&err) {
                        eprintln!("reef: usage feed: {err}");
                    }
                    last_error = Some(err);
                }
            }
            tokio::time::sleep(every).await;
        }
    });
}

async fn read_feed(url: &str) -> Result<usage_source::UsageSnapshot, String> {
    let body = timeout(TIMEOUT, get(url))
        .await
        .map_err(|_| "it did not answer".to_string())??;
    usage_source::feed::parse_usage(&body)
}

/// A plain HTTP/1.1 GET of a JSON body.
async fn get(url: &str) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or("not an http:// URL")?;
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_string()), |(a, p)| (a, format!("/{p}")));
    let mut stream = TcpStream::connect(authority)
        .await
        .map_err(|err| err.to_string())?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|err| err.to_string())?;
    let mut response = Vec::new();
    stream
        .take(MAX_BODY as u64 + 16 * 1024)
        .read_to_end(&mut response)
        .await
        .map_err(|err| err.to_string())?;
    body(&response)
}

/// The body of a whole `200` answer.
fn body(response: &[u8]) -> Result<String, String> {
    let end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no answer")?;
    let head = String::from_utf8_lossy(&response[..end]).to_ascii_lowercase();
    let status = head.split_whitespace().nth(1).unwrap_or_default();
    if status != "200" {
        return Err(format!("it answered {status}"));
    }
    if head.contains("transfer-encoding: chunked") {
        return Err("chunked answers are not read".into());
    }
    let body = &response[end + 4..];
    if body.len() > MAX_BODY {
        return Err("the answer is too big".into());
    }
    String::from_utf8(body.to_vec()).map_err(|_| "the answer is not text".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_whole_answer_is_read() {
        let ok = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"pools\":{}}";
        assert_eq!(body(ok).unwrap(), "{\"pools\":{}}");
        assert!(body(b"HTTP/1.1 404 Not Found\r\n\r\n{}").is_err());
        assert!(body(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{}").is_err());
        assert!(body(b"HTTP/1.1 200 OK").is_err());
    }

    /// crusteo's gauges check their rows against the same file
    /// (`src/app/usage.rs`), so the title bar and the phones agree.
    #[test]
    fn the_reef_makes_the_rows_crusteo_draws() {
        const NOW: i64 = 1_800_000_000;
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../usage-source/tests/fixtures/agreement"
        );
        let read = |name: &str| std::fs::read_to_string(format!("{dir}/{name}")).unwrap();
        // The feed's answer and one daemon's reports, read Reef's way.
        let mut readings = BTreeMap::new();
        let snapshot = usage_source::feed::parse_usage(&read("feed.json")).unwrap();
        readings.insert(
            FEED_KEY.to_string(),
            Reading {
                snapshot,
                eligibility: Vec::new(),
            },
        );
        let updates: Vec<Value> = serde_json::from_str(&read("reports.json")).unwrap();
        let daemon = usage_source::reports::parse_reports("alpha", &updates, NOW).unwrap();
        readings.insert(host_key("alpha"), daemon);
        let mut rows: Vec<Value> = rows(&readings, NOW).into_iter().map(|(_, v)| v).collect();
        // Which is tightest is Reef's addition for its header gauge.
        let tightest: Vec<bool> = (rows.iter_mut())
            .map(|row| row.as_object_mut().unwrap().remove("tightest") == Some(json!(true)))
            .collect();
        assert_eq!(tightest.iter().filter(|t| **t).count(), 1);
        let pinned: Value = serde_json::from_str(&read("rows.json")).unwrap();
        assert_eq!(Value::Array(rows), pinned);
    }
}
