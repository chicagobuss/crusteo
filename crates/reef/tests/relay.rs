//! A desktop in reef mode through Reef's pass-through, `/daemon/<id>/ws`:
//! a real burrow answers a real paseo-client, with Reef between them; the
//! device token is asked for before the socket opens; a host Reef cannot
//! reach says why; and Reef adds the host's password to the hello.

mod support;

use std::sync::Arc;
use std::time::Duration;

use burrow_daemon::{Config, Daemon};
use burrow_provider::echo::EchoProvider;
use futures_util::{SinkExt, StreamExt};
use paseo_client::{Client, ConnState, Event};
use reef::{Device, ReefConfig};
use serde_json::{Value, json};
use support::{TOKEN, WAIT};
use tokio_tungstenite::tungstenite::Message;

fn host(name: &str, endpoint: String, password: Option<&str>) -> fleet::HostConfig {
    fleet::HostConfig {
        id: reef::config::host_id(name),
        name: name.into(),
        endpoint: Some(endpoint),
        password: password.map(str::to_string),
    }
}

async fn reef(hosts: Vec<fleet::HostConfig>) -> std::net::SocketAddr {
    let device = Device {
        name: "test desktop".into(),
        token: TOKEN.into(),
    };
    let mut config = ReefConfig::new(hosts, "cid_reef_relay", vec![device]);
    config.bind = "127.0.0.1:0".parse().unwrap();
    reef::start(config).await.unwrap().addr
}

/// A paseo-client to `id`'s daemon through the Reef at `addr`, with `token`.
fn through(addr: std::net::SocketAddr, id: &str, token: Option<&str>) -> Client {
    let mut config =
        paseo_client::Config::new(format!("ws://{addr}/daemon/{id}/ws"), "cid_test_desktop");
    config.bearer = token.map(str::to_string);
    Client::connect(config)
}

/// How the client's first connection ends: its server_info, or why not and
/// whether that was for good (`auth`).
async fn first_state(client: &Client) -> Result<paseo_client::ServerInfo, (String, bool)> {
    let events = client.events();
    let wait = async {
        loop {
            match events.recv().await.expect("the client's events") {
                Event::State(ConnState::Connected(info)) => return Ok(info),
                Event::State(ConnState::Disconnected { reason, auth }) => {
                    return Err((reason, auth));
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(WAIT, wait).await.expect("a state")
}

/// Whether the client dials again within a while after its first try: the
/// first backoff is 1.5 seconds.
async fn dials_again(client: &Client) -> bool {
    let events = client.events();
    let again = async {
        loop {
            if let Event::State(ConnState::Connecting { .. }) =
                events.recv().await.expect("the client's events")
            {
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(4), again)
        .await
        .is_ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_desktop_reaches_burrow_through_reef_with_its_token() {
    let config = Config::ephemeral();
    let server_id = config.server_id.clone();
    let daemon = Daemon::new(config);
    daemon.agents.register(Arc::new(EchoProvider::default()));
    let running = burrow_daemon::start(daemon.clone()).await.unwrap();
    // Nothing listens on a port just let go of, and it refuses at once,
    // where a low port such as the discard port may drop packets instead.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().to_string()
    };
    let addr = reef(vec![
        host("devbox", running.local_addr.to_string(), None),
        host("gone", closed, None),
    ])
    .await;

    // Reef says it passes daemons through.
    let hosts: Value = {
        let url = format!("http://{addr}/api/hosts");
        let agent: ureq::Agent = ureq::Agent::config_builder().build().into();
        let mut answer = (agent.get(&url))
            .header("authorization", format!("Bearer {TOKEN}"))
            .call()
            .unwrap();
        serde_json::from_str(&answer.body_mut().read_to_string().unwrap()).unwrap()
    };
    assert_eq!(hosts["relay"], "/daemon/", "{hosts}");

    // The hello is burrow's to answer, and requests go both ways.
    let client = through(addr, "devbox", Some(TOKEN));
    let info = first_state(&client).await.expect("connected through Reef");
    assert_eq!(info.server_id, server_id, "burrow's own server_info");
    let workspaces = client
        .request(json!({"type": "fetch_workspaces_request"}))
        .await
        .expect("a reply through Reef");
    assert!(workspaces.is_object(), "{workspaces}");

    // No token, or a wrong one: refused before the socket opens, for good,
    // and said so; no dialling again until told to.
    for token in [None, Some("wrong")] {
        let client = through(addr, "devbox", token);
        let (refused, auth) = first_state(&client).await.unwrap_err();
        assert!(auth, "{token:?}: {refused}");
        assert!(refused.contains("401"), "{token:?}: {refused}");
        assert!(refused.contains("token"), "{token:?}: {refused}");
        assert!(!dials_again(&client).await, "{token:?} is not tried again");
        client.reconnect();
        assert!(dials_again(&client).await, "until told to reconnect");
    }
    let wrong = first_state(&through(addr, "devbox", Some("wrong")))
        .await
        .unwrap_err();
    assert_eq!(wrong.0, "Reef refused this desktop's token (HTTP 401)");
    // A host Reef has not got may be one it gets soon: tried again.
    let client = through(addr, "nope", Some(TOKEN));
    let (missing, auth) = first_state(&client).await.unwrap_err();
    assert!(missing.contains("404"), "{missing}");
    assert!(!auth, "{missing}");
    assert!(dials_again(&client).await, "{missing} is tried again");
    // One it cannot reach says so, in Reef's words, and is tried again.
    let (gone, auth) = first_state(&through(addr, "gone", Some(TOKEN)))
        .await
        .unwrap_err();
    assert!(gone.contains("Reef could not reach it"), "{gone}");
    assert!(!auth, "{gone}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reef_adds_the_hosts_password_to_the_desktops_hello() {
    // A daemon that only reports the first frame it is sent, from the test's
    // client: Reef's own connection to it says hello too.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap().to_string();
    let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = seen_tx.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                if let Some(Ok(Message::Text(text))) = ws.next().await {
                    let hello: Value = serde_json::from_str(text.as_str()).unwrap();
                    if hello["clientId"] == "cid_raw" {
                        let _ = seen.send(hello);
                    }
                }
                let _ = ws.close(None).await;
            });
        }
    });
    let addr = reef(vec![host("locked", endpoint, Some("hunter2"))]).await;

    let hello = |auth: Option<Value>| {
        let mut hello = json!({"type": "hello", "clientId": "cid_raw"});
        if let Some(auth) = auth {
            hello["auth"] = auth;
        }
        hello
    };
    for (sent, expected) in [
        (None, json!({"kind": "password", "password": "hunter2"})),
        // The desktop's own password is left as it is.
        (
            Some(json!({"kind": "password", "password": "mine"})),
            json!({"kind": "password", "password": "mine"}),
        ),
    ] {
        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                format!("ws://{addr}/daemon/locked/ws"),
            )
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        ws.send(Message::text(hello(sent).to_string()))
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("the daemon saw the hello")
            .unwrap();
        assert_eq!(got["auth"], expected, "{got}");
        assert_eq!(got["clientId"], "cid_raw");
    }
}
