//! cargo run -p paseo-client --example probe -- 127.0.0.1:6767
use paseo_client::{Client, Config, Event, generate_client_id};
use serde_json::json;

fn main() {
    let endpoint = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:6790".into());
    let client = Client::connect(Config::new(endpoint, generate_client_id()));
    let events = client.events();
    paseo_client::block_on(async move {
        loop {
            match events.recv().await.unwrap() {
                Event::State(state) => {
                    println!(
                        "state: {}",
                        format!("{state:?}").chars().take(200).collect::<String>()
                    );
                    if let paseo_client::ConnState::Connected(info) = state {
                        println!(
                            "connected to {} {} v{}, {} features",
                            info.hostname,
                            info.server_id,
                            info.version,
                            info.features.len()
                        );
                        break;
                    }
                }
                other => println!("event: {other:?}"),
            }
        }
        let agents = client
            .request(json!({"type":"fetch_agents_request","scope":"active","page":{"limit":5}}))
            .await;
        match agents {
            Ok(payload) => println!(
                "agents: {} entries, hasMore={}",
                payload["entries"].as_array().map(|a| a.len()).unwrap_or(0),
                payload["pageInfo"]["hasMore"]
            ),
            Err(err) => println!("agents error: {err}"),
        }
        let ws = client
            .request(json!({"type":"fetch_workspaces_request","page":{"limit":200}}))
            .await;
        match ws {
            Ok(payload) => {
                let entries = payload["entries"].as_array().cloned().unwrap_or_default();
                println!("workspaces: {}", entries.len());
                for entry in entries.iter().take(5) {
                    let w: paseo_client::proto::Workspace =
                        serde_json::from_value(entry.clone()).unwrap();
                    println!(
                        "  {} [{}] {} / {}",
                        w.id, w.status, w.project_display_name, w.name
                    );
                }
            }
            Err(err) => println!("workspaces error: {err}"),
        }
        let bad = client
            .request(json!({"type":"fetch_workspaces_request","sync":{},"filter":{"query":"x"}}))
            .await;
        println!("bad request -> {bad:?}");
    });
}
