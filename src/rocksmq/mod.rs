//! RocksMQ-style minimal HTTP API (P4): `/produce`, `/consume`,
//! `/ack` over a hand-rolled HTTP/1.1 listener (the `rcache/http.rs` /
//! `es/http.rs` school; no hyper, no new crates). An independent
//! front, not an rcache import: it talks to the Lite engine through
//! `command::dispatch` (see `api.rs` for why).
//!
//! Transport contract:
//! - keep-alive is the default (HTTP/1.1); `Connection: close` or
//!   HTTP/1.0 closes after the reply. Pipelined bytes left in the
//!   buffer after a body feed the next request;
//! - head capped at 32 KiB (431), body at [`MAX_BODY_BYTES`] = 4 MiB
//!   (413); `Expect: 100-continue` is answered before the body read;
//! - chunked request bodies are refused (501): sized producers always
//!   send Content-Length;
//! - unknown paths 404; known paths with a non-POST method 405;
//!   malformed heads close with a 400;
//! - no auth (same posture as the Kafka front): bind to a
//!   loopback/port-restricted address in untrusted networks;
//! - wiring: `rocksmq_bind` (empty = disabled; the backup listener
//!   never wires this front).
//!
//! Full interface contract + deviations from real RocksMQ:
//! `features/rocksmq-http.md`.

pub mod api;
pub mod query;
pub mod respv;

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::state::Shared;

use api::HttpReply;
use query::Query;

/// Request body cap: a RocksMQ-style payload front has no use for
/// multi-megabyte messages; 4 MiB matches the ES front's per-doc
/// ceiling mindset. Over it: 413.
pub const MAX_BODY_BYTES: usize = 4 << 20;
/// `/consume` batch cap (`n` parameter ceiling).
pub const MAX_BATCH: usize = 100;

const MAX_HEAD_BYTES: usize = 32 << 10;
/// One read-step budget (head first byte included): an idle keep-alive
/// connection is dropped silently after it.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Bind `addr` ("listen <addr> failed: <err>" on failure, like every
/// other front) synchronously so the caller can inspect `local_addr`.
pub fn bind(addr: &str) -> Result<TcpListener, String> {
    let std_listener =
        std::net::TcpListener::bind(addr).map_err(|e| format!("listen {addr} failed: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("listen {addr} failed: {e}"))?;
    TcpListener::from_std(std_listener).map_err(|e| format!("listen {addr} failed: {e}"))
}

/// Accept loop (the `resp::serve` pattern): one task per connection,
/// 10ms backoff on accept errors so an error storm cannot busy-spin.
pub async fn serve(listener: TcpListener, shared: Arc<Shared>) -> ! {
    loop {
        match listener.accept().await {
            Ok((sock, _)) => {
                let shared = shared.clone();
                tokio::spawn(handle_conn(sock, shared));
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

/// Keep-alive requests until the peer closes, times out or asks for
/// `Connection: close`.
async fn handle_conn(sock: TcpStream, shared: Arc<Shared>) {
    let (mut rd, mut wr) = sock.into_split();
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    loop {
        // ---- head: buffer until CRLFCRLF ----
        let head_end = loop {
            if let Some(i) = find_head_end(&buf) {
                break i;
            }
            if buf.len() > MAX_HEAD_BYTES {
                let _ = write_reply(
                    &mut wr,
                    &HttpReply::text(431, "request head too large"),
                    false,
                )
                .await;
                return;
            }
            match read_some(&mut rd, &mut buf).await {
                Some(()) => {}
                None => return, // EOF / error / idle timeout: just close
            }
        };
        let head = match parse_head(&buf[..head_end]) {
            Ok(h) => h,
            Err(e) => {
                let _ = write_reply(&mut wr, &HttpReply::text(400, &e), false).await;
                return;
            }
        };
        buf.drain(..head_end + 4);
        // ---- body ----
        if head.chunked() {
            let _ = write_reply(
                &mut wr,
                &HttpReply::text(501, "chunked bodies not supported"),
                false,
            )
            .await;
            return;
        }
        let len = head.content_length.unwrap_or(0) as usize;
        if len > MAX_BODY_BYTES {
            let _ = write_reply(
                &mut wr,
                &HttpReply::text(413, "body exceeds 4 MiB limit"),
                false,
            )
            .await;
            return;
        }
        if head.expects_continue()
            && len > 0
            && wr
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .await
                .is_err()
        {
            return;
        }
        while buf.len() < len {
            match read_some(&mut rd, &mut buf).await {
                Some(()) => {}
                None => return, // truncated body: close without a reply
            }
        }
        let body: Vec<u8> = buf.drain(..len).collect();
        // ---- route + reply ----
        let keep = head.keep_alive();
        let reply = route(&shared, &head, &body).await;
        if write_reply(&mut wr, &reply, keep).await.is_err() {
            return;
        }
        if !keep {
            return;
        }
    }
}

/// One bounded read step; `None` = EOF, error or timeout (the caller
/// drops the connection silently in all three cases).
async fn read_some(rd: &mut tokio::net::tcp::OwnedReadHalf, buf: &mut Vec<u8>) -> Option<()> {
    let mut chunk = [0u8; 4096];
    match tokio::time::timeout(READ_TIMEOUT, rd.read(&mut chunk)).await {
        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => None,
        Ok(Ok(n)) => {
            buf.extend_from_slice(&chunk[..n]);
            Some(())
        }
    }
}

/// Path (known paths answer; anything else 404) then method (non-POST
/// on a known path 405) -- the order the interface doc promises.
async fn route(shared: &Shared, head: &Head, body: &[u8]) -> HttpReply {
    let (path, raw_query) = split_target(&head.target);
    if !matches!(path, "/produce" | "/consume" | "/ack") {
        return HttpReply::text(404, "not found");
    }
    if head.method != "POST" {
        return HttpReply {
            status: 405,
            content_type: "text/plain",
            body: b"method not allowed".to_vec(),
            allow_post: true,
        };
    }
    let query = match Query::parse(raw_query) {
        Ok(q) => q,
        Err(e) => return HttpReply::text(400, &format!("bad query string: {e}")),
    };
    match path {
        "/produce" => api::produce(shared, &query, body).await,
        "/consume" => api::consume(shared, &query).await,
        _ => api::ack(shared, &query).await,
    }
}

/// Parsed request head: method, target, headers (names lowercased),
/// pre-parsed content-length.
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    content_length: Option<u64>,
    http11: bool,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn expects_continue(&self) -> bool {
        self.header("expect")
            .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
    }

    fn chunked(&self) -> bool {
        self.header("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    }

    /// HTTP/1.1 defaults to keep-alive; HTTP/1.0 defaults to close; an
    /// explicit `Connection` token overrides either way.
    fn keep_alive(&self) -> bool {
        match self.header("connection").map(|v| v.to_ascii_lowercase()) {
            Some(v) if v.split(',').any(|t| t.trim() == "close") => false,
            Some(v) if v.split(',').any(|t| t.trim() == "keep-alive") => true,
            _ => self.http11,
        }
    }
}

/// Parse the head section (everything before `\r\n\r\n`). Pure.
fn parse_head(bytes: &[u8]) -> Result<Head, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "request head must be ASCII/UTF-8".to_string())?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or("empty request head")?;
    let mut parts = request_line.split(' ');
    let method = parts.next().ok_or("malformed request line")?;
    let target = parts.next().ok_or("malformed request line")?;
    let version = parts.next().ok_or("malformed request line")?;
    if parts.next().is_some()
        || method.is_empty()
        || !target.starts_with('/')
        || !method
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b'_' || b == b'-')
    {
        return Err("malformed request line".to_string());
    }
    let http11 = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        v if v.starts_with("HTTP/1.") => true, // HTTP/1.2-ish: treat as 1.1
        _ => return Err("unsupported HTTP version".to_string()),
    };
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':').ok_or("malformed header line")?;
        if name.is_empty() || name.contains(' ') {
            return Err("malformed header line".to_string());
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    let content_length = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .map(|(_, v)| {
            v.parse::<u64>()
                .map_err(|_| "malformed content-length".to_string())
        })
        .transpose()?;
    Ok(Head {
        method: method.to_string(),
        target: target.to_string(),
        headers,
        content_length,
        http11,
    })
}

/// `/path?query` -> (`/path`, `query`); `#fragments` are cut off.
fn split_target(target: &str) -> (&str, &str) {
    let no_frag = target.split('#').next().unwrap_or(target);
    match no_frag.split_once('?') {
        Some((p, q)) => (p, q),
        None => (no_frag, ""),
    }
}

/// Offset of the `\r\n\r\n` head terminator, if fully buffered.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn write_reply(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    reply: &HttpReply,
    keep: bool,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n{}\r\n",
        reply.status,
        reason(reply.status),
        reply.content_type,
        reply.body.len(),
        if keep { "keep-alive" } else { "close" },
        if reply.allow_post {
            "Allow: POST\r\n"
        } else {
            ""
        },
    );
    wr.write_all(head.as_bytes()).await?;
    wr.write_all(&reply.body).await?;
    wr.flush().await
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_parsing_and_keep_alive() {
        let h = parse_head(b"POST /produce?channel=a HTTP/1.1\r\nContent-Length: 3").unwrap();
        assert_eq!(h.method, "POST");
        assert_eq!(h.target, "/produce?channel=a");
        assert_eq!(h.content_length, Some(3));
        assert!(h.keep_alive());
        assert!(!h.expects_continue());
        let h = parse_head(b"GET / HTTP/1.0").unwrap();
        assert!(!h.keep_alive());
        let h = parse_head(b"GET / HTTP/1.0\r\nConnection: keep-alive").unwrap();
        assert!(h.keep_alive());
        let h = parse_head(b"POST /produce HTTP/1.1\r\nConnection: close").unwrap();
        assert!(!h.keep_alive());
        let h = parse_head(b"POST /produce HTTP/1.1\r\nTransfer-Encoding: chunked").unwrap();
        assert!(h.chunked());
        assert!(parse_head(b"POST /produce FTP/1.1").is_err());
        assert!(parse_head(b"POST /produce HTTP/1.1\r\nContent-Length: x").is_err());
    }

    #[test]
    fn target_split() {
        assert_eq!(
            split_target("/produce?channel=a&n=2"),
            ("/produce", "channel=a&n=2")
        );
        assert_eq!(split_target("/ack"), ("/ack", ""));
        assert_eq!(split_target("/x?a#f"), ("/x", "a"));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\n"), Some(14));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\nX: 1\r\n"), None);
    }
}
