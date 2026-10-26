//! The listener's gates: which Host names it answers, what a socket may
//! send before it signs in, how many strangers it holds, image tokens,
//! and stopping.

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reef::{Device, ReefConfig, ReefHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "a+b/c=";

async fn reef(host_names: &[&str]) -> ReefHandle {
    let mut config = ReefConfig::new(
        Vec::new(),
        "cid_reef_test",
        vec![Device {
            name: "test phone".into(),
            token: TOKEN.into(),
        }],
    );
    config.bind = "127.0.0.1:0".parse().unwrap();
    config.host_names = host_names.iter().map(|n| n.to_string()).collect();
    reef::start(config).await.unwrap()
}

/// A raw request with the Host header given; the status line back.
async fn status(addr: SocketAddr, path: &str, host: &str, upgrade: bool) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let extra = if upgrade {
        "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
    } else {
        ""
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = vec![0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut response))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&response[..n])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn only_addresses_and_given_names_reach_the_reef() {
    let handle = reef(&["reef.example.test"]).await;
    let addr = handle.addr;
    let local = format!("127.0.0.1:{}", addr.port());
    assert!(status(addr, "/", &local, false).await.contains("200"));
    assert!(
        status(addr, "/", "reef.example.test", false)
            .await
            .contains("200")
    );
    // A page that rebinds its own name to Reef sends that name.
    for path in ["/", "/ws", "/image/ab?t=x", "/healthz"] {
        let line = status(addr, path, "rebound.example.com", path == "/ws").await;
        assert!(line.contains("403"), "{path}: {line}");
    }
    assert!(status(addr, "/ws", &local, true).await.contains("101"));
}

#[tokio::test(flavor = "multi_thread")]
async fn image_tokens_are_read_percent_decoded() {
    let handle = reef(&[]).await;
    let local = format!("127.0.0.1:{}", handle.addr.port());
    // The right token, as encodeURIComponent sends it: past the gate, to a
    // missing image.
    let right = status(handle.addr, "/image/ab?t=a%2Bb%2Fc%3D", &local, false).await;
    assert!(right.contains("404"), "{right}");
    let wrong = status(handle.addr, "/image/ab?t=a%2Bb", &local, false).await;
    assert!(wrong.contains("401"), "{wrong}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_big_first_frame_is_dropped_before_the_hello() {
    let handle = reef(&[]).await;
    let mut ws = support::connect_ws(handle.addr).await;
    let big = "x".repeat(2 * 1024 * 1024);
    let _ = ws.send(Message::text(big)).await;
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "Reef hung up");
    // A small hello still gets in.
    let mut ws = support::connect_ws(handle.addr).await;
    let hello = format!(r#"{{"type":"hello","v":0,"token":"{TOKEN}"}}"#);
    ws.send(Message::text(hello)).await.unwrap();
    let Some(Ok(Message::Text(welcome))) = ws.next().await else {
        panic!("no welcome")
    };
    assert!(welcome.as_str().contains("welcome"));
}

#[tokio::test(flavor = "multi_thread")]
async fn strangers_are_capped_and_leaving_frees_a_place() {
    let handle = reef(&[]).await;
    // Sixteen sockets that never say anything hold every place.
    let mut idle = Vec::new();
    for _ in 0..16 {
        idle.push(TcpStream::connect(handle.addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut extra = TcpStream::connect(handle.addr).await.unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), extra.read(&mut byte))
        .await
        .expect("turned away at once");
    assert!(matches!(read, Ok(0) | Err(_)), "the seventeenth is closed");
    drop(idle);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let local = format!("127.0.0.1:{}", handle.addr.port());
    assert!(
        status(handle.addr, "/healthz", &local, false)
            .await
            .contains("200")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_closes_the_listener() {
    let handle = reef(&[]).await;
    let addr = handle.addr;
    let local = format!("127.0.0.1:{}", addr.port());
    assert!(
        status(addr, "/healthz", &local, false)
            .await
            .contains("200")
    );
    handle.stop();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(TcpStream::connect(addr).await.is_err(), "nothing listens");
}
