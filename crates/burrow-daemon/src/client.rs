//! The client behind Burrow's own commands (`burrow status`, `burrow ls`, …):
//! one WebSocket connection to a running Burrow (or any daemon that speaks
//! Paseo's protocol), said hello to as Paseo's CLI does, with each request
//! answered by its id.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use paseo_client::ssh;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// How long connecting, the hello and an ordinary request may each take.
pub const WAIT: Duration = Duration::from_secs(10);

/// How long connecting through SSH may take: ssh may make two hops and a
/// key exchange first.
const SSH_WAIT: Duration = Duration::from_secs(30);

/// Paseo's port, used when an address names none.
pub const DEFAULT_PORT: u16 = 6767;

/// What the WebSocket runs over: a TCP connection, or ssh's standard streams.
trait Transport: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Transport for T {}

type Ws = WebSocketStream<Box<dyn Transport>>;

/// A connection a hello has been answered on.
pub struct Client {
    ws: Ws,
    /// Where it is connected, as messages name it.
    url: String,
    next: u64,
    /// The daemon's `server_info` payload.
    pub info: Value,
}

/// `host`, `host:port`, `ws://host:port/ws`: the `host:port` to dial. An
/// `ssh://` address is kept as it is.
pub fn endpoint(address: &str) -> String {
    if address.trim().starts_with("ssh://") {
        return address.trim().to_string();
    }
    let bare = address
        .trim()
        .trim_start_matches("ws://")
        .trim_end_matches('/')
        .trim_end_matches("/ws");
    let has_port = match bare.rsplit_once(':') {
        // `[v6]` alone has colons but no port; `[v6]:port` and `host:port` do.
        Some((head, port)) => {
            port.parse::<u16>().is_ok() && (!head.contains(':') || head.ends_with(']'))
        }
        None => false,
    };
    if has_port {
        bare.to_string()
    } else {
        format!("{bare}:{DEFAULT_PORT}")
    }
}

impl Client {
    /// Connects to `endpoint` (`host:port`, or `ssh://…` through ssh) and
    /// waits for the hello's answer.
    pub async fn connect(endpoint: &str) -> Result<Client, String> {
        let (mut ws, url) = match ssh::Target::parse(endpoint) {
            Some(target) => {
                let target = target.map_err(|err| format!("{endpoint}: {err}"))?;
                (through_ssh(endpoint, &target).await?, endpoint.to_string())
            }
            None => {
                let url = format!("ws://{endpoint}/ws");
                (direct(endpoint, &url).await?, url)
            }
        };
        let hello = json!({
            "type": "hello", "clientId": format!("cid_burrow_cli_{}", std::process::id()),
            "clientType": "cli", "protocolVersion": 1,
            "appVersion": concat!("burrow-cli-", env!("CARGO_PKG_VERSION")),
            "capabilities": {},
        });
        if let Err(err) = ws.send(Message::text(hello.to_string())).await {
            return Err(dropped(&url, &err.to_string()));
        }
        let mut client = Client {
            ws,
            url,
            next: 0,
            info: Value::Null,
        };
        let info = client
            .next_message(Some(WAIT), |message| {
                message["payload"]["status"] == "server_info"
            })
            .await?;
        client.info = info["payload"].clone();
        Ok(client)
    }

    /// Where it is connected: `ws://host:port/ws`, or the `ssh://` address.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Sends `message` (a session message, without its `requestId`) and
    /// returns the payload of the reply that carries its id. An `rpc_error`
    /// reply is an error with its message. `within` bounds the wait; `None`
    /// waits as long as the reply takes (`wait_for_finish_request`).
    pub async fn request(
        &mut self,
        mut message: Value,
        within: Option<Duration>,
    ) -> Result<Value, String> {
        self.next += 1;
        let id = format!("burrow-cli-{}", self.next);
        message["requestId"] = id.clone().into();
        let kind = message["type"].as_str().unwrap_or("request").to_string();
        let frame = json!({"type": "session", "message": message});
        if let Err(err) = self.ws.send(Message::text(frame.to_string())).await {
            return Err(dropped(&self.url, &err.to_string()));
        }
        let reply = self
            .next_message(within, |message| {
                message["payload"]["requestId"] == id.as_str()
            })
            .await
            .map_err(|err| format!("{kind}: {err}"))?;
        if reply["type"] == "rpc_error" {
            let what = reply["payload"]["message"]
                .as_str()
                .or_else(|| reply["payload"]["error"].as_str())
                .unwrap_or("the request failed");
            return Err(what.to_string());
        }
        Ok(reply["payload"].clone())
    }

    /// Closes the connection.
    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    /// The next session message `wanted` accepts, skipping everything else
    /// the daemon pushes.
    async fn next_message(
        &mut self,
        within: Option<Duration>,
        wanted: impl Fn(&Value) -> bool,
    ) -> Result<Value, String> {
        loop {
            let next = self.ws.next();
            let frame = match within {
                Some(limit) => match timeout(limit, next).await {
                    Ok(frame) => frame,
                    Err(_) => {
                        return Err(format!(
                            "{} took the connection but did not reply in time",
                            self.url
                        ));
                    }
                },
                None => next.await,
            };
            let text = match frame {
                None => return Err(dropped(&self.url, "closed")),
                Some(Err(err)) => return Err(dropped(&self.url, &err.to_string())),
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Close(close))) => {
                    let why = close.map_or("closed".into(), |c| {
                        format!("closed: {} {}", c.code, c.reason)
                    });
                    return Err(dropped(&self.url, &why));
                }
                Some(Ok(_)) => continue,
            };
            let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            if wanted(&value["message"]) {
                return Ok(value["message"].clone());
            }
        }
    }
}

/// The WebSocket to `endpoint` (`host:port`) over TCP.
async fn direct(endpoint: &str, url: &str) -> Result<Ws, String> {
    let dial = async {
        let stream = TcpStream::connect(endpoint).await?;
        let stream: Box<dyn Transport> = Box::new(stream);
        tokio_tungstenite::client_async(url, stream)
            .await
            .map_err(std::io::Error::other)
    };
    match timeout(WAIT, dial).await {
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(err)) => Err(format!("nothing answers at {url}: {err}")),
        Err(_) => Err(format!("{url} did not answer within {}s", WAIT.as_secs())),
    }
}

/// The WebSocket to the daemon on `target`'s loopback, through ssh; what
/// ssh said when it fails.
async fn through_ssh(endpoint: &str, target: &ssh::Target) -> Result<Ws, String> {
    let (stream, watch) = ssh::open(target, None).map_err(|err| format!("{endpoint}: {err}"))?;
    let stream: Box<dyn Transport> = Box::new(stream);
    let handshake = tokio_tungstenite::client_async(target.url(), stream);
    let failed = match timeout(SSH_WAIT, handshake).await {
        Ok(Ok((ws, _))) => return Ok(ws),
        Ok(Err(err)) => err.to_string(),
        Err(_) => format!("no answer within {}s", SSH_WAIT.as_secs()),
    };
    let why = watch.failure().await.unwrap_or(failed);
    Err(format!("nothing answers at {endpoint}: {why}"))
}

fn dropped(url: &str, why: &str) -> String {
    format!(
        "{url} took the connection and then dropped it ({why}). The daemon runs but its \
         sessions fail: its log says why"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_without_a_port_gets_paseos() {
        assert_eq!(endpoint("buildbox"), "buildbox:6767");
        assert_eq!(endpoint("10.0.0.5"), "10.0.0.5:6767");
        assert_eq!(endpoint("10.0.0.5:6797"), "10.0.0.5:6797");
        assert_eq!(endpoint("ws://127.0.0.1:6767/ws"), "127.0.0.1:6767");
        assert_eq!(endpoint("[::1]"), "[::1]:6767");
        assert_eq!(endpoint("[::1]:6797"), "[::1]:6797");
        assert_eq!(endpoint(" ssh://dev@box "), "ssh://dev@box");
    }
}
