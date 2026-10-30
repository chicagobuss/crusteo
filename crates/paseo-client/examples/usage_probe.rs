//! Asks each daemon for its version and usage, the way crusteo's usage
//! gauges do, and prints what came back.
//!
//!   cargo run -p paseo-client --example usage_probe -- 127.0.0.1:6767 [more…]
use std::time::Duration;

use paseo_client::{Client, Config, ConnState, Event, generate_client_id};
use serde_json::json;

fn main() {
    let endpoints: Vec<String> = std::env::args().skip(1).collect();
    for endpoint in endpoints {
        let client = Client::connect(Config::new(endpoint.clone(), generate_client_id()));
        let events = client.events();
        let info = paseo_client::block_on(async {
            loop {
                match events.recv().await {
                    Ok(Event::State(ConnState::Connected(info))) => return Ok(info),
                    // The client keeps retrying; one failure is enough here.
                    Ok(Event::State(ConnState::Disconnected { reason, .. })) => return Err(reason),
                    Ok(_) => {}
                    Err(_) => return Err("events closed".to_string()),
                }
            }
        });
        let info = match info {
            Ok(info) => info,
            Err(reason) => {
                println!("{endpoint}: not reached ({reason})");
                continue;
            }
        };
        let sources = info.has("usageSources");
        println!(
            "{endpoint}: {} v{} usageSources={sources} providerUsageList={}",
            info.hostname,
            info.version,
            info.has("providerUsageList")
        );
        if sources {
            let asked = paseo_client::block_on(client.request_until(
                json!({ "type": "usage.list_reports.request" }),
                "usage.list_reports.response",
                Duration::from_secs(60),
            ));
            match asked {
                Ok((updates, last)) => {
                    println!("  reports: {}, error: {}", updates.len(), last["error"]);
                    for update in &updates {
                        let entry = &update["report"];
                        println!(
                            "    {} {} [{}] {} windows, id {}, fetched {}",
                            entry["sourceId"].as_str().unwrap_or("?"),
                            entry["account"]["label"].as_str().unwrap_or("-"),
                            entry["report"]["status"].as_str().unwrap_or("?"),
                            entry["report"]["windows"].as_array().map_or(0, Vec::len),
                            entry["id"].as_str().unwrap_or("?"),
                            entry["fetchedAt"].as_str().unwrap_or("?"),
                        );
                        for window in entry["report"]["windows"].as_array().into_iter().flatten() {
                            println!(
                                "      {} {} used={} resets={}",
                                window["id"].as_str().unwrap_or("?"),
                                window["label"].as_str().unwrap_or("?"),
                                window["usedPct"],
                                window["resetsAt"],
                            );
                        }
                    }
                }
                Err(err) => println!("  reports failed: {err}"),
            }
        }
        drop(client);
    }
}
