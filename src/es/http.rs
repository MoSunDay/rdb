//! ES HTTP/1.1 transport: hand-rolled like `rcache/http.rs` and
//! `monitor.rs` (no hyper). v1 contract: ONE request per connection,
//! always `Connection: close`; head capped at 32 KiB, body at 64 MiB;
//! `Expect: 100-continue` is answered before the body read (curl
//! sends it for >1 KiB bodies). Chunked bodies are refused (501) --
//! clients that size their payloads (every ES client) always send
//! Content-Length. Auth: a non-empty configured token requires
//! `Authorization: Bearer <token>` (simple compare; not
//! timing-hardened, same as the RESP AUTH path). Handler failures are
//! `Reply`s already; the transport never panics a connection task.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::state::Shared;

use super::reply::{self, Reply};
use super::router;

/// Bind `addr`. Same contract and error text as `resp::bind` /
/// `sql::front::bind` ("listen <addr> failed: <err>") so startup
/// output stays uniform. Synchronous so the caller can grab
/// `local_addr()` before handing the listener to [`serve`].
pub fn bind(addr: &str) -> Result<TcpListener, String> {
    let std_listener =
        std::net::TcpListener::bind(addr).map_err(|e| format!("listen {addr} failed: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("listen {addr} failed: {e}"))?;
    TcpListener::from_std(std_listener).map_err(|e| format!("listen {addr} failed: {e}"))
}

/// Accept loop: one task per connection, 10ms backoff on accept
/// errors (same policy as `resp::serve`).
pub async fn serve(listener: TcpListener, shared: Arc<Shared>, token: String) {
    loop {
        match listener.accept().await {
            Ok((sock, _)) => {
                let shared = shared.clone();
                let token = token.clone();
                tokio::spawn(handle_conn(sock, shared, token));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

/// One decoded request. `path` = percent-decoded '/'-segments (empty
/// segments dropped); `query` = raw text after '?'.
pub struct Request {
    pub method: String,
    pub raw_path: String,
    pub path: Vec<String>,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

const MAX_HEAD_BYTES: usize = 32 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(60);

async fn handle_conn(mut sock: TcpStream, shared: Arc<Shared>, token: String) {
    let rep = match read_request(&mut sock).await {
        Ok(Some(req)) => {
            if !authorized(&req, &token) {
                reply::error(
                    401,
                    "security_exception",
                    "missing or invalid bearer credentials",
                )
            } else {
                router::route(&shared, &req).await
            }
        }
        Ok(None) => return, // peer closed before a full head
        Err(rep) => rep,
    };
    let _ = write_reply(&mut sock, &rep).await;
    let _ = sock.shutdown().await;
}

/// Read one request (head + body) off the socket. `Ok(None)` = the
/// peer vanished mid-head (just close); `Err(reply)` = protocol
/// error, answer it and close.
async fn read_request(sock: &mut TcpStream) -> Result<Option<Request>, Reply> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(reply::error(
                413,
                "i_o_exception",
                "request head exceeds the maximum allowed length",
            ));
        }
        let n = match tokio::time::timeout(HEAD_TIMEOUT, sock.read(&mut chunk)).await {
            Ok(Ok(0)) => return Ok(None),
            Ok(Ok(n)) => n,
            Ok(Err(_)) | Err(_) => return Ok(None),
        };
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = parse_head(&buf[..head_end]).map_err(|e| reply::error(400, "bad_request", e))?;
    if head.header("transfer-encoding").is_some() {
        return Err(reply::error(
            501,
            "not_implemented",
            "chunked transfer encoding is not supported",
        ));
    }
    // curl waits for the interim response before sending >1 KiB bodies
    if head.expects_continue() && head.content_length.unwrap_or(0) > 0 {
        let _ = sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
    }
    // POST/PUT without Content-Length read as body-less (GET-style);
    // body-less methods ignore any body bytes entirely.
    let want: u64 = if head.body_semantics() {
        head.content_length.unwrap_or(0)
    } else {
        0
    };
    if want > MAX_BODY_BYTES as u64 {
        return Err(reply::error(
            413,
            "i_o_exception",
            "request body exceeds the maximum allowed content length",
        ));
    }
    // the first read may already carry part (or all) of the body
    let mut rest = buf[head_end + 4..].to_vec();
    let remaining = want.saturating_sub(rest.len() as u64) as usize;
    if rest.len() as u64 > want {
        rest.truncate(want as usize); // pipelined bytes: v1 ignores them
    }
    let mut body = rest;
    if remaining > 0 {
        let at = body.len();
        body.resize(at + remaining, 0);
        let tail = &mut body[at..];
        match tokio::time::timeout(BODY_TIMEOUT, sock.read_exact(tail)).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => return Ok(None),
        }
    }
    build_request(head, body)
        .map(Some)
        .map_err(|e| reply::error(400, "bad_request", e))
}

/// Bearer check; both sides are lowercased (scheme is
/// case-insensitive), the token itself must match verbatim.
fn authorized(req: &Request, token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    let expected = format!("bearer {}", token.to_ascii_lowercase());
    req.headers
        .iter()
        .any(|(n, v)| n == "authorization" && v.trim().to_ascii_lowercase() == expected)
}

async fn write_reply(sock: &mut TcpStream, rep: &Reply) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        rep.status,
        reply::reason(rep.status),
        rep.content_type,
        rep.body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(&rep.body).await?;
    Ok(())
}

/// Offset of the `\r\n\r\n` head terminator, if fully buffered.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parsed head: method, request target and headers (names
/// lowercased). `content_length` is pre-parsed (malformed values are
/// request errors, like the Go http server).
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    content_length: Option<u64>,
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

    /// Only POST/PUT carry bodies in this frontend.
    fn body_semantics(&self) -> bool {
        self.method == "POST" || self.method == "PUT"
    }
}

/// Parse the head section (everything before `\r\n\r\n`). Pure.
fn parse_head(bytes: &[u8]) -> Result<Head, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "request head must be ASCII/UTF-8")?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or("empty request head")?;
    // request line: `METHOD SP TARGET SP HTTP/1.x`
    let mut parts = request_line.split(' ');
    let method = parts.next().ok_or("malformed request line")?;
    let target = parts.next().ok_or("malformed request line")?;
    let version = parts.next().ok_or("malformed request line")?;
    if parts.next().is_some()
        || method.is_empty()
        || !target.starts_with('/')
        || !version.starts_with("HTTP/1.")
        || !method
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b'_' || b == b'-')
    {
        return Err("malformed request line");
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':').ok_or("malformed header line")?;
        if name.is_empty() || name.contains(' ') {
            return Err("malformed header line");
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    let content_length = match headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .map(|(_, v)| v.as_str())
    {
        None => None,
        Some(v) => Some(v.parse::<u64>().map_err(|_| "malformed content-length")?),
    };
    Ok(Head {
        method: method.to_string(),
        target: target.to_string(),
        headers,
        content_length,
    })
}

/// Target -> (path, query), dropping any `#fragment`.
fn split_target(target: &str) -> (&str, &str) {
    let target = target.split('#').next().unwrap_or(target);
    match target.find('?') {
        Some(i) => (&target[..i], &target[i + 1..]),
        None => (target, ""),
    }
}

/// `%XX` decoding for path segments (`+` stays a literal plus).
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = hex_digit(bytes.get(i + 1).copied()?)?;
                let lo = hex_digit(bytes.get(i + 2).copied()?)?;
                out.push(hi * 16 + lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
#[cfg(test)]
#[path = "http_tests.rs"]
mod http_tests;

/// `/{index}/_doc/{id}` -> `["index", "_doc", "id"]`; empty segments
/// drop, each segment percent-decoded (so `%2F` yields a '/'
/// *inside* a segment -- name validators reject it later).
fn path_segments(path: &str) -> Option<Vec<String>> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect()
}

/// Pure end-to-end parse of one head+body buffer (the incremental
/// socket path runs the same helpers).
fn build_request(head: Head, body: Vec<u8>) -> Result<Request, &'static str> {
    let (raw_path, query) = split_target(&head.target);
    let path = path_segments(raw_path).ok_or("bad percent-escape in path")?;
    Ok(Request {
        method: head.method,
        raw_path: raw_path.to_string(),
        path,
        query: query.to_string(),
        headers: head.headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head_of(req: &str) -> Head {
        let end = find_head_end(req.as_bytes()).expect("head end");
        parse_head(&req.as_bytes()[..end]).expect("head")
    }

    #[test]
    fn request_line_and_headers() {
        let h = head_of("PUT /i%20x/_doc/a%2Fb?op_type=create HTTP/1.1\r\nHost: h\r\nContent-Length: 7\r\n\r\n{\"a\":1}");
        assert_eq!(h.method, "PUT");
        assert_eq!(h.target, "/i%20x/_doc/a%2Fb?op_type=create");
        assert_eq!(h.content_length, Some(7));
        // names lowercased, values trimmed
        assert_eq!(h.header("host"), Some("h"));
        assert_eq!(h.header("CONTENT-LENGTH"), None);
        for bad in [
            "GET / HTTP/1.1 extra\r\n\r\n",
            "G ET / HTTP/1.1\r\n\r\n",
            "GET no-slash HTTP/1.1\r\n\r\n",
            "GET / HTTP/2.0\r\n\r\n",
            "GET\r\n\r\n",
        ] {
            let end = find_head_end(bad.as_bytes()).expect("head end");
            assert!(parse_head(&bad.as_bytes()[..end]).is_err(), "{bad}");
        }
        // malformed content-length is a request error
        let end = find_head_end(b"GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n").unwrap();
        assert!(parse_head(&b"GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n"[..end]).is_err());
    }

    #[test]
    fn target_split_and_percent_decoding() {
        assert_eq!(split_target("/a/b?x=1&y=2"), ("/a/b", "x=1&y=2"));
        assert_eq!(split_target("/a?z#frag"), ("/a", "z"));
        let req = build_request(head_of("GET /i%20x//_doc/a%2Fb?q HTTP/1.1\r\n\r\n"), vec![])
            .expect("request");
        assert_eq!(
            req.path,
            vec!["i x".to_string(), "_doc".to_string(), "a/b".to_string()]
        );
        assert_eq!(req.query, "q");
        assert!(build_request(head_of("GET /a%zz HTTP/1.1\r\n\r\n"), vec![]).is_err());
        assert!(build_request(head_of("GET /a%2 HTTP/1.1\r\n\r\n"), vec![]).is_err());
    }

    #[test]
    fn body_framing_rules() {
        let head = head_of("POST /_bulk HTTP/1.1\r\nContent-Length: 5\r\n\r\n");
        assert_eq!(head.content_length, Some(5));
        assert!(head.body_semantics());
        // GET-style: no body semantics even with a content-length
        let head = head_of("GET / HTTP/1.1\r\nContent-Length: 5\r\n\r\n");
        assert!(!head.body_semantics());
        // 100-continue is detected case-insensitively
        let head = head_of("POST / HTTP/1.1\r\nExpect: 100-Continue\r\nContent-Length: 9\r\n\r\n");
        assert!(head.expects_continue());
    }

    #[test]
    fn bearer_authorization() {
        let req = |v: &str| {
            build_request(
                head_of(&format!("GET / HTTP/1.1\r\nAuthorization: {v}\r\n\r\n")),
                vec![],
            )
            .unwrap()
        };
        assert!(authorized(&req("Bearer tok"), "tok"));
        assert!(authorized(&req("  bearer TOK "), "tok"));
        assert!(!authorized(&req("Bearer nope"), "tok"));
        assert!(!authorized(&req("Basic dXNlcg=="), "tok"));
        assert!(authorized(&req("Bearer anything"), "")); // no token configured
    }
}
