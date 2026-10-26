//! The listener. One port serves Skiff at `/` and the protocol's
//! WebSocket at `/ws`. A phone must say hello with a device token before
//! anything else, unless Reef is open; Reef only hears from phones
//! that said hello.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};

use crate::config::Device;
use crate::proto::{self, ClientMsg, Limits, code};
use crate::reef::FromServer;

/// Skiff, the page phones load: one file, no build step.
const PAGE: &str = include_str!("web/index.html");
/// Set to a file to serve the page from it, read afresh on every request, so
/// a page change shows on refresh without a rebuild.
const PAGE_ENV: &str = "REEF_PAGE";
/// [`PAGE_ENV`]'s name before the rename, still read for one release.
const LEGACY_PAGE_ENV: &str = "CRUSTEO_HUB_PAGE";

/// Requests bigger than this are not a phone's.
const MAX_HEAD: usize = 16 * 1024;
/// Frames from a phone are small; anything bigger is refused.
const MAX_FRAME: usize = 64 * 1024;
/// Except a message with images, which the phone shrinks first; only once
/// it has said hello.
const MAX_IMAGE_FRAME: usize = 12 * 1024 * 1024;
/// What a WebSocket may send before its hello is accepted. tungstenite
/// cannot lower its frame limit for the start of a socket, so the socket
/// itself stops reading past this, and a stranger's huge first frame is
/// dropped long before it is buffered.
const BEFORE_HELLO: usize = MAX_FRAME;
/// Connections not yet signed in, at once; more are turned away.
const MAX_STRANGERS: usize = 16;

/// Who may use Reef, and by which names it may be reached.
#[derive(Debug, Clone)]
pub(crate) struct Gate {
    pub devices: Vec<Device>,
    pub open: bool,
    pub host_names: Vec<String>,
}

/// A connection that has not signed in yet. It counts against
/// [`MAX_STRANGERS`] until it says hello, or goes.
struct Stranger(Option<Arc<AtomicUsize>>);

impl Stranger {
    fn welcome(&mut self) {
        if let Some(count) = self.0.take() {
            count.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for Stranger {
    fn drop(&mut self) {
        self.welcome();
    }
}

pub(crate) async fn serve(
    listener: TcpListener,
    gate: Arc<Gate>,
    limits: Limits,
    reef: mpsc::UnboundedSender<FromServer>,
) {
    let next = Arc::new(AtomicU64::new(1));
    let strangers = Arc::new(AtomicUsize::new(0));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        if strangers.fetch_add(1, Ordering::SeqCst) >= MAX_STRANGERS {
            // Too many sockets that have not signed in: this one goes.
            strangers.fetch_sub(1, Ordering::SeqCst);
            drop(stream);
            continue;
        }
        let stranger = Stranger(Some(strangers.clone()));
        let gate = gate.clone();
        let limits = limits.clone();
        let reef = reef.clone();
        let conn = next.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let _ = stream.set_nodelay(true);
            handle(stream, conn, stranger, gate, limits, reef).await;
        });
    }
}

/// The request line and the headers this server looks at.
struct Head {
    method: String,
    path: String,
    upgrade: bool,
    host: Option<String>,
    origin: Option<String>,
    content_length: Option<usize>,
}

fn parse_head(bytes: &[u8]) -> Option<Head> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.split("\r\n");
    let mut request = lines.next()?.split(' ');
    let method = request.next()?.to_string();
    let path = request.next()?.to_string();
    let mut head = Head {
        method,
        path,
        upgrade: false,
        host: None,
        origin: None,
        content_length: None,
    };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "upgrade" => head.upgrade = value.eq_ignore_ascii_case("websocket"),
            "host" => head.host = Some(value.to_string()),
            "origin" => head.origin = Some(value.to_string()),
            "content-length" => head.content_length = value.parse().ok(),
            _ => {}
        }
    }
    Some(head)
}

/// The name in a Host header, without its port or IPv6 brackets.
fn host_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or_default();
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

/// Whether a request names Reef by an address it answers to: an IP
/// address, localhost, or a name given with `--host-name`. A page that
/// rebinds its own DNS name to Reef's address sends its own name, and
/// is refused.
fn host_allowed(host: Option<&str>, names: &[String]) -> bool {
    let Some(host) = host else {
        return false;
    };
    let name = host_name(host).to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name.parse::<std::net::IpAddr>().is_ok()
        || names
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&name))
}

/// A browser page on another site may open a WebSocket to Reef; only
/// Reef's own page, or a client with no Origin, may.
fn origin_allowed(head: &Head) -> bool {
    let Some(origin) = head.origin.as_deref() else {
        return true;
    };
    let origin_host = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    head.host.as_deref() == Some(origin_host)
}

async fn handle(
    mut stream: TcpStream,
    conn: u64,
    stranger: Stranger,
    gate: Arc<Gate>,
    limits: Limits,
    reef: mpsc::UnboundedSender<FromServer>,
) {
    // Read the request head; the WebSocket handshake reads it again from
    // what was buffered here.
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let read_head = async {
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..n]);
            if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                return Some(end + 4);
            }
            if buffer.len() > MAX_HEAD {
                return None;
            }
        }
    };
    let Ok(Some(end)) = timeout(limits.hello_timeout, read_head).await else {
        return;
    };
    let Some(head) = parse_head(&buffer[..end]) else {
        return;
    };
    let path = head.path.split('?').next().unwrap_or("/");
    if !host_allowed(head.host.as_deref(), &gate.host_names) {
        let _ = respond(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            "Host not allowed: start Reef with --host-name <the name you use>",
        )
        .await;
        return;
    }
    if head.upgrade && path == "/ws" {
        if !origin_allowed(&head) {
            let _ = respond(
                &mut stream,
                "403 Forbidden",
                "text/plain",
                "Origin not allowed",
            )
            .await;
            return;
        }
        let signed_in = Arc::new(AtomicBool::new(false));
        let stream = Prefixed {
            prefix: buffer,
            at: 0,
            inner: stream,
            read: 0,
            signed_in: signed_in.clone(),
        };
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_IMAGE_FRAME))
            .max_frame_size(Some(MAX_IMAGE_FRAME));
        let Ok(socket) = tokio_tungstenite::accept_async_with_config(stream, Some(config)).await
        else {
            return;
        };
        session(socket, conn, stranger, signed_in, gate, limits, reef).await;
        return;
    }
    let _ = match (head.method.as_str(), path) {
        ("GET", "/" | "/index.html") => {
            let page = crate::legacy::var_os(PAGE_ENV, LEGACY_PAGE_ENV)
                .and_then(|path| std::fs::read_to_string(path).ok());
            let page = page.as_deref().unwrap_or(PAGE);
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", page).await
        }
        ("GET", "/healthz") => respond(&mut stream, "200 OK", "text/plain", "ok").await,
        ("GET" | "POST", "/api/labels") => {
            let allowed =
                gate.open || query(&head.path, "t").is_some_and(|t| known(&gate.devices, &t));
            // An edit from a page on another site, which a browser would send
            // with its Origin, is refused.
            let edit = head.method == "POST";
            if !allowed {
                respond(
                    &mut stream,
                    "401 Unauthorized",
                    "text/plain",
                    "Unauthorized",
                )
                .await
            } else if edit && !origin_allowed(&head) {
                respond(
                    &mut stream,
                    "403 Forbidden",
                    "text/plain",
                    "Origin not allowed",
                )
                .await
            } else {
                let body = if edit {
                    read_body(&mut stream, &buffer[end..], head.content_length, &limits).await
                } else {
                    Ok(None)
                };
                match body {
                    Ok(edit) => labels(&mut stream, edit, &reef).await,
                    Err(problem) => {
                        respond(&mut stream, "400 Bad Request", "text/plain", problem).await
                    }
                }
            }
        }
        ("GET", "/api/hosts") => {
            let allowed =
                gate.open || query(&head.path, "t").is_some_and(|t| known(&gate.devices, &t));
            if !allowed {
                respond(
                    &mut stream,
                    "401 Unauthorized",
                    "text/plain",
                    "Unauthorized",
                )
                .await
            } else {
                hosts(&mut stream, &reef).await
            }
        }
        ("GET", p) if p.starts_with("/image/") => {
            let allowed =
                gate.open || query(&head.path, "t").is_some_and(|t| known(&gate.devices, &t));
            if !allowed {
                respond(
                    &mut stream,
                    "401 Unauthorized",
                    "text/plain",
                    "Unauthorized",
                )
                .await
            } else {
                image(&mut stream, &p["/image/".len()..], &reef).await
            }
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", "Not found").await,
    };
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

/// One value from a path's query string, percent-decoded: the page encodes
/// what it sends, and a token may hold `+`, `/` or `=`.
fn query(path: &str, key: &str) -> Option<String> {
    path.split_once('?')?
        .1
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| percent_decode(v))
}

/// `%XX` escapes to their bytes; None when they do not make UTF-8. A `+`
/// stays a `+`: the page encodes with `encodeURIComponent`.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn known(devices: &[Device], token: &str) -> bool {
    !token.is_empty() && devices.iter().any(|d| same(&d.token, token))
}

/// Sends one image from a conversation. Its key is its content, so it can
/// be kept for good: a phone fetches each image once.
/// A request body of `length` bytes as JSON, from what was read with the
/// head (`read`) and the rest of the stream.
async fn read_body(
    stream: &mut TcpStream,
    read: &[u8],
    length: Option<usize>,
    limits: &Limits,
) -> Result<Option<Value>, &'static str> {
    const MAX_BODY: usize = 64 * 1024;
    let length = length.ok_or("A body needs a Content-Length")?;
    if length > MAX_BODY {
        return Err("The body is too big");
    }
    let mut body = read.to_vec();
    let mut chunk = [0u8; 4096];
    while body.len() < length {
        let n = match timeout(limits.hello_timeout, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return Err("The body ended early"),
        };
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|_| "The body is not JSON")
}

/// `/api/labels`: the catalog, after `edit` if one came, as JSON.
async fn labels(
    stream: &mut TcpStream,
    edit: Option<Value>,
    reef: &mpsc::UnboundedSender<FromServer>,
) -> std::io::Result<()> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    if reef.send(FromServer::Labels { edit, reply }).is_err() {
        return respond(stream, "503 Service Unavailable", "text/plain", "Stopping").await;
    }
    match answer.await {
        Ok(answer) => respond(stream, "200 OK", "application/json", &answer.to_string()).await,
        Err(_) => respond(stream, "503 Service Unavailable", "text/plain", "Stopping").await,
    }
}

/// `/api/hosts`: the fleet for a desktop in reef mode, as JSON.
async fn hosts(
    stream: &mut TcpStream,
    reef: &mpsc::UnboundedSender<FromServer>,
) -> std::io::Result<()> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    if reef.send(FromServer::Hosts { reply }).is_err() {
        return respond(stream, "503 Service Unavailable", "text/plain", "Stopping").await;
    }
    match answer.await {
        Ok(hosts) => respond(stream, "200 OK", "application/json", &hosts.to_string()).await,
        Err(_) => respond(stream, "503 Service Unavailable", "text/plain", "Stopping").await,
    }
}

async fn image(
    stream: &mut TcpStream,
    key: &str,
    reef: &mpsc::UnboundedSender<FromServer>,
) -> std::io::Result<()> {
    use base64::Engine;
    let Ok(key) = u64::from_str_radix(key, 16) else {
        return respond(stream, "404 Not Found", "text/plain", "Not found").await;
    };
    let (reply, found) = tokio::sync::oneshot::channel();
    if reef.send(FromServer::Image { key, reply }).is_err() {
        return respond(stream, "503 Service Unavailable", "text/plain", "Stopping").await;
    }
    let found = found.await.ok().flatten();
    let decoded = found.as_ref().and_then(|image| {
        base64::engine::general_purpose::STANDARD
            .decode(image.data.trim())
            .ok()
            .map(|bytes| (image.mime_type.as_str(), bytes))
    });
    let Some((mime, bytes)) = decoded else {
        return respond(stream, "404 Not Found", "text/plain", "Not found").await;
    };
    // Only raster types as themselves: anything else, SVG included, could
    // run script on this origin if opened directly.
    let raster = [
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
        "image/avif",
        "image/bmp",
    ];
    let mime = if raster.contains(&mime) {
        mime
    } else {
        "application/octet-stream"
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\
         Cache-Control: private, max-age=31536000, immutable\r\n\
         X-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; sandbox\r\n\
         Referrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.shutdown().await
}

/// Compares without stopping at the first difference, so timing does not
/// tell how much of a guess was right.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for (i, x) in a.iter().enumerate() {
        diff |= (*x ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

async fn session<S>(
    socket: tokio_tungstenite::WebSocketStream<S>,
    conn: u64,
    mut stranger: Stranger,
    signed_in: Arc<AtomicBool>,
    gate: Arc<Gate>,
    limits: Limits,
    reef: mpsc::UnboundedSender<FromServer>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = socket.split();
    let refuse =
        |code: &str, message: &str| Message::text(proto::error(None, code, message).to_string());
    let close = |reason: &str| {
        Message::Close(Some(CloseFrame {
            code: CloseCode::Library(4401),
            reason: reason.to_string().into(),
        }))
    };

    // The first frame says who this is.
    let first = match timeout(limits.hello_timeout, stream.next()).await {
        Ok(Some(Ok(Message::Text(text)))) if text.len() <= MAX_FRAME => {
            proto::parse(text.as_str(), &limits)
        }
        _ => return,
    };
    let device = match first {
        Ok(ClientMsg::Hello {
            version,
            token,
            device,
        }) => {
            if version > proto::VERSION {
                let _ = sink
                    .send(refuse(code::VERSION, "this Reef speaks protocol version 0"))
                    .await;
                let _ = sink.send(close("Unsupported version")).await;
                return;
            }
            match gate.devices.iter().find(|d| same(&d.token, &token)) {
                Some(known) if !token.is_empty() => known.name.clone(),
                // An open Reef takes any phone, named as it names itself.
                _ if gate.open => device.unwrap_or_else(|| "phone".into()),
                _ => {
                    let _ = sink
                        .send(refuse(code::UNAUTHORIZED, "unknown device token"))
                        .await;
                    let _ = sink.send(close("Unauthorized")).await;
                    return;
                }
            }
        }
        _ => {
            let _ = sink
                .send(refuse(
                    code::UNAUTHORIZED,
                    "say hello with a device token first",
                ))
                .await;
            let _ = sink.send(close("Hello first")).await;
            return;
        }
    };

    // Signed in: big frames (images) are allowed from here on.
    signed_in.store(true, Ordering::SeqCst);
    stranger.welcome();

    let (out_tx, mut out_rx) = mpsc::channel::<String>(limits.outbox);
    if reef
        .send(FromServer::Connected {
            conn,
            device,
            out: out_tx,
        })
        .is_err()
    {
        return;
    }
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(std::time::Duration::from_secs(30));
        ping.tick().await;
        loop {
            tokio::select! {
                frame = out_rx.recv() => match frame {
                    Some(text) => {
                        if sink.send(Message::text(text)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });
    loop {
        let frame = match timeout(limits.idle_timeout, stream.next()).await {
            Ok(Some(Ok(frame))) => frame,
            _ => break,
        };
        match frame {
            Message::Text(text) => {
                let mut message = proto::parse_or_invalid(text.as_str(), &limits);
                if text.len() > MAX_FRAME && !carries_images(&message) {
                    message = ClientMsg::Invalid {
                        req: None,
                        problem: "frame too big".into(),
                    };
                }
                if reef.send(FromServer::Message { conn, message }).is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            // Pings and pongs show the phone is there; tungstenite answers
            // pings itself.
            _ => {}
        }
    }
    let _ = reef.send(FromServer::Closed { conn });
    writer.abort();
}

/// Whether a frame is a message with images, the one kind allowed to be big.
fn carries_images(message: &ClientMsg) -> bool {
    matches!(message, ClientMsg::Action { action, body, .. }
        if matches!(action.as_str(), "send" | "steer")
            && body["images"].as_array().is_some_and(|images| !images.is_empty()))
}

/// A stream that first yields bytes already read from it, and reads no
/// more than [`BEFORE_HELLO`] from the socket until it is signed in.
struct Prefixed {
    prefix: Vec<u8>,
    at: usize,
    inner: TcpStream,
    /// Bytes read from the socket so far, while not signed in.
    read: usize,
    signed_in: Arc<AtomicBool>,
}

impl AsyncRead for Prefixed {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.at < self.prefix.len() {
            let rest = &self.prefix[self.at..];
            let n = rest.len().min(buf.remaining());
            buf.put_slice(&rest[..n]);
            self.at += n;
            return Poll::Ready(Ok(()));
        }
        let signed_in = self.signed_in.load(Ordering::SeqCst);
        if !signed_in && self.read >= BEFORE_HELLO {
            return Poll::Ready(Err(std::io::Error::other("too much before hello")));
        }
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if !signed_in {
            self.read += buf.filled().len() - before;
        }
        polled
    }
}

impl AsyncWrite for Prefixed {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads_parse_and_foreign_origins_are_refused() {
        let head = parse_head(
            b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1:6798\r\nUpgrade: websocket\r\nOrigin: http://127.0.0.1:6798\r\n\r\n",
        )
        .unwrap();
        assert!(head.upgrade);
        assert_eq!(head.path, "/ws");
        assert!(origin_allowed(&head));
        let foreign = Head {
            origin: Some("https://example.com".into()),
            ..head
        };
        assert!(!origin_allowed(&foreign));
    }

    #[test]
    fn only_addresses_and_given_names_may_be_the_host() {
        let names = vec!["reef.example.ts.net".to_string()];
        for ok in [
            "127.0.0.1:6798",
            "192.0.2.20",
            "[::1]:6798",
            "localhost:6798",
            "phone.localhost",
            "REEF.example.ts.net:443",
        ] {
            assert!(host_allowed(Some(ok), &names), "{ok}");
        }
        for refused in ["evil.example.com", "reef.example.ts.net.evil.test:80", ""] {
            assert!(!host_allowed(Some(refused), &names), "{refused}");
        }
        assert!(!host_allowed(None, &names));
    }

    #[test]
    fn query_values_are_percent_decoded() {
        assert_eq!(
            query("/image/ab?t=a%2Bb%2Fc%3D", "t").as_deref(),
            Some("a+b/c=")
        );
        assert_eq!(
            query("/image/ab?x=1&t=plain", "t").as_deref(),
            Some("plain")
        );
        assert_eq!(query("/image/ab?t=bad%zz", "t"), None);
        assert_eq!(query("/image/ab", "t"), None);
    }

    /// The page is plain script with no test runner, so this only checks
    /// that the move of the old storage keys comes before anything reads
    /// the new ones, and that nothing else uses the old ones.
    #[test]
    fn skiff_moves_the_hub_pages_storage_before_reading_it() {
        let moved = PAGE
            .find(r#"[["hub-prefs", "skiff-prefs"], ["hub-token", "skiff-token"]]"#)
            .expect("the old keys are moved");
        for key in [r#"getItem("skiff-prefs")"#, r#"getItem("skiff-token")"#] {
            let read = PAGE.find(key).expect(key);
            assert!(moved < read, "{key} is read before the move");
        }
        assert_eq!(PAGE.matches("\"hub-prefs\"").count(), 1);
        assert_eq!(PAGE.matches("\"hub-token\"").count(), 1);
        assert!(PAGE.contains("<title>Skiff</title>"));
    }

    #[test]
    fn tokens_compare_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
        assert!(!same("", "a"));
    }
}
