//! cargo run -p paseo-client --example dump -- <endpoint> '<json request>'
use paseo_client::{Client, Config, ConnState, Event, generate_client_id};

fn main() {
    let mut args = std::env::args().skip(1);
    let endpoint = args.next().unwrap();
    let request: serde_json::Value = serde_json::from_str(&args.next().unwrap()).unwrap();
    let client = Client::connect(Config::new(endpoint, generate_client_id()));
    let events = client.events();
    paseo_client::block_on(async move {
        loop {
            if let Event::State(ConnState::Connected(_)) = events.recv().await.unwrap() {
                break;
            }
        }
        match client.request(request).await {
            Ok(payload) => println!("{}", serde_json::to_string_pretty(&payload).unwrap()),
            Err(err) => eprintln!("error: {err}"),
        }
    });
}
