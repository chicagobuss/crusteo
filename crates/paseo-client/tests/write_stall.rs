//! A path that stops carrying data without resetting the connection, as when
//! a network drops packets for a while: the client's writes stay pending. It
//! must give the connection up, and dial again, and not wait for TCP to.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use paseo_client::{Client, Config, ConnState, Event, EventReceiver};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// A listener whose connections have a small receive buffer: what the client
/// writes to one that is never read fills the path sooner.
fn listen() -> (TcpListener, u16) {
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(16).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// Takes a connection and answers its hello with the server info.
async fn accept(listener: &TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    ws.next().await.unwrap().unwrap(); // the hello
    let info = json!({"type": "status", "payload": {
        "status": "server_info", "serverId": "srv_stall", "hostname": "buildbox", "version": "test"
    }});
    ws.send(Message::text(info.to_string())).await.unwrap();
    ws
}

async fn until_state(
    events: &EventReceiver,
    what: &str,
    wanted: impl Fn(&ConnState) -> bool,
) -> ConnState {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Event::State(state)) = events.recv().await
                && wanted(&state)
            {
                return state;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {what} within 10 s"))
}

#[test]
fn a_write_that_stalls_ends_the_connection_and_the_client_dials_again() {
    paseo_client::block_on(async {
        let (listener, port) = listen();
        let server = tokio::spawn(async move {
            // The first connection gets its hello answered and is then held
            // open and never read again, nor closed.
            let _stalled = accept(&listener).await;
            // The second answers requests.
            let mut healthy = accept(&listener).await;
            while let Some(Ok(frame)) = healthy.next().await {
                let Message::Text(text) = frame else { continue };
                let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                let request = &frame["message"];
                let Some(id) = request["requestId"].as_str() else {
                    continue;
                };
                let reply = json!({"type": "session", "message": {
                    "type": "echo.response", "payload": {"requestId": id, "ok": true}
                }});
                if healthy
                    .send(Message::text(reply.to_string()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let mut config = Config::new(format!("127.0.0.1:{port}"), "cid_write_stall");
        // The default is 15 s, as long as the heartbeat's; this test cannot wait.
        config.write_timeout = Duration::from_millis(500);
        let client = Client::connect(config);
        let events = client.events();
        until_state(&events, "connection", |s| {
            matches!(s, ConnState::Connected(_))
        })
        .await;

        // More than the path can take. Fire and forget: the client queues them
        // all at once and writes until the buffers are full.
        let filler = "x".repeat(512 * 1024);
        for n in 0..64 {
            client.send(json!({"type": "fill", "n": n, "text": filler}));
        }

        // Without a bound on the write the client is stuck in it: it never
        // reaches its heartbeat check either, and says nothing.
        let ConnState::Disconnected { reason, auth } =
            until_state(&events, "report of the lost connection", |s| {
                matches!(s, ConnState::Disconnected { .. })
            })
            .await
        else {
            unreachable!()
        };
        assert_eq!(reason, "write timed out");
        assert!(!auth, "a stalled path is tried again");

        // It dials again after its backoff, and the new connection works.
        let ConnState::Connected(info) = until_state(&events, "second connection", |s| {
            matches!(s, ConnState::Connected(_))
        })
        .await
        else {
            unreachable!()
        };
        assert_eq!(info.server_id, "srv_stall");
        let reply = client
            .request_with_timeout(json!({"type": "echo"}), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(reply["ok"], true);

        drop(client);
        timeout(Duration::from_secs(5), server)
            .await
            .expect("the server saw the client go")
            .unwrap();
    });
}
