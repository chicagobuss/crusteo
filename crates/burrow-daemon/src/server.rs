//! The listener: HTTP upgrade checks, then one task per socket.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tracing::{debug, info, warn};

use crate::daemon::{Config, Daemon};

/// A running daemon. Dropping it does not stop it; call [`Running::shutdown`].
pub struct Running {
    pub daemon: Arc<Daemon>,
    pub local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Running {
    /// `ws://<addr>/ws`.
    pub fn url(&self) -> String {
        format!("ws://{}/ws", self.local_addr)
    }

    /// Closes every socket with 1001 and stops accepting.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// Binds `config.listen` and starts serving. The socket is listening when
/// this returns: connects queue in the backlog until the accept loop runs.
pub async fn start(daemon: Arc<Daemon>) -> std::io::Result<Running> {
    let listener = TcpListener::bind(daemon.config.listen).await?;
    let local_addr = listener.local_addr()?;
    let (shutdown, shutdown_rx) = watch::channel(false);
    info!(%local_addr, server_id = %daemon.config.server_id, "listening on ws://{local_addr}/ws");
    let task = tokio::spawn(accept_loop(daemon.clone(), listener, shutdown_rx));
    Ok(Running {
        daemon,
        local_addr,
        shutdown,
        task,
    })
}

async fn accept_loop(
    daemon: Arc<Daemon>,
    listener: TcpListener,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    connections.spawn(upgrade(daemon.clone(), stream, peer, shutdown.clone()));
                }
                Err(err) => warn!(%err, "accept failed"),
            },
            _ = shutdown.changed() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    // Sockets saw the same signal and are closing with 1001.
    while connections.join_next().await.is_some() {}
}

#[allow(clippy::result_large_err)] // tungstenite's callback error type
async fn upgrade(
    daemon: Arc<Daemon>,
    stream: TcpStream,
    peer: SocketAddr,
    shutdown: watch::Receiver<bool>,
) {
    let config = daemon.config.clone();
    let check =
        move |request: &Request, response: Response| check_upgrade(&config, request, response);
    match tokio_tungstenite::accept_hdr_async(stream, check).await {
        Ok(socket) => crate::connection::run(daemon, socket, peer, shutdown).await,
        Err(err) => debug!(%peer, %err, "upgrade refused"),
    }
}

fn refuse(status: StatusCode, body: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(body.to_string()));
    *response.status_mut() = status;
    response
}

/// Paseo's upgrade rules: path `/ws`; a `Host` that is localhost, a
/// `*.localhost` name, an IP literal or a configured name; an `Origin` that
/// is absent, allowlisted or the same as the host. No subprotocol is echoed.
#[allow(clippy::result_large_err)] // the error type is tungstenite's
fn check_upgrade(
    config: &Config,
    request: &Request,
    response: Response,
) -> Result<Response, ErrorResponse> {
    if request.uri().path() != "/ws" {
        return Err(refuse(StatusCode::BAD_REQUEST, "Not a WebSocket endpoint"));
    }
    let host = request
        .headers()
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !host_allowed(config, host) {
        return Err(refuse(StatusCode::FORBIDDEN, "Host not allowed"));
    }
    if let Some(origin) = request.headers().get("origin") {
        let origin = origin.to_str().unwrap_or_default();
        if !origin_allowed(config, origin, host) {
            return Err(refuse(StatusCode::FORBIDDEN, "Origin not allowed"));
        }
    }
    Ok(response)
}

fn host_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or_default();
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

fn host_allowed(config: &Config, host: &str) -> bool {
    let name = host_name(host).to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name.parse::<IpAddr>().is_ok()
        || config
            .hostnames
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&name))
}

fn origin_allowed(config: &Config, origin: &str, host: &str) -> bool {
    if origin == "*"
        || config
            .allowed_origins
            .iter()
            .any(|allowed| allowed == origin)
    {
        return true;
    }
    let authority = origin
        .split_once("://")
        .map_or(origin, |(_, rest)| rest)
        .trim_end_matches('/');
    authority.eq_ignore_ascii_case(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_rules() {
        let mut config = Config::ephemeral();
        assert!(host_allowed(&config, "127.0.0.1:6797"));
        assert!(host_allowed(&config, "[::1]:6797"));
        assert!(host_allowed(&config, "localhost:6797"));
        assert!(host_allowed(&config, "app.localhost"));
        assert!(!host_allowed(&config, "example-host:6797"));
        config.hostnames.push("example-host".into());
        assert!(host_allowed(&config, "example-host:6797"));
    }

    #[test]
    fn origin_rules() {
        let config = Config::ephemeral();
        assert!(origin_allowed(
            &config,
            "http://127.0.0.1:6797",
            "127.0.0.1:6797"
        ));
        assert!(!origin_allowed(
            &config,
            "https://elsewhere.example",
            "127.0.0.1:6797"
        ));
    }
}
