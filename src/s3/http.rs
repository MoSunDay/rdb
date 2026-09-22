//! S3 HTTP/1.1 transport, hand-rolled like `es/http.rs` (no hyper):
//! ONE request per connection, always `Connection: close` -- aws
//! cli/boto3/curl all cope, and framing stays trivial. Head capped at
//! 64 KiB (431), body at 1 GiB (413); `Expect: 100-continue` is
//! answered before the body read; chunked bodies are refused (501).
//! Auth is a configured Bearer token (NOT SigV4; same posture as
//! `es_token` -- bind to a trusted network). Routing itself lives in
//! `router.rs`; this file owns the wire: framing, auth, request
//! parsing and response serialization (large bodies stream from disk
//! in 64 KiB chunks; PUT bodies stream into the staged tmp file).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::state::Shared;

use super::object::{self, ObjectStore};
use super::{http_date, now_ms, request_id, xml};

const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Request-body ceiling: 1 GiB (the S3 single-PUT class).
pub(crate) const MAX_BODY_BYTES: u64 = 1 << 30;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const BODY_TIMEOUT: Duration = Duration::from_secs(60);
/// GET streaming chunk; also the PUT staging buffer.
pub(crate) const CHUNK: usize = 64 * 1024;

/// Bind `addr` ("listen <addr> failed: <err>", like every front).
pub fn bind(addr: &str) -> Result<TcpListener, String> {
    let std_listener =
        std::net::TcpListener::bind(addr).map_err(|e| format!("listen {addr} failed: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("listen {addr} failed: {e}"))?;
    TcpListener::from_std(std_listener).map_err(|e| format!("listen {addr} failed: {e}"))
}

/// Accept loop: one task per connection, 10ms backoff on accept
/// errors. Config snapshot happens once up front (same defaults as
/// `checkpoint::spawn_publisher`).
pub async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    let conf = &shared.conf;
    let bucket = if conf.s3_bucket.is_empty() { "rdb".to_string() } else { conf.s3_bucket.clone() };
    let root = if conf.s3_store_path.is_empty() {
        PathBuf::from(&conf.store_path).join("s3")
    } else {
        PathBuf::from(&conf.s3_store_path)
    };
    let ctx = match object::open(&root) {
        Ok(store) => Arc::new(Ctx { store, token: conf.s3_token.clone() }),
        Err(e) => {
            eprintln!("s3: open object store at {} failed: {e}", root.display());
            return;
        }
    };
    eprintln!("s3: front bound for bucket \"{bucket}\" under {}", root.display());
    loop {
        match listener.accept().await {
            Ok((sock, _)) => {
                let ctx = ctx.clone();
                tokio::spawn(handle_conn(sock, ctx));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

/// Per-listener snapshot: store handle + bearer token.
pub(crate) struct Ctx {
    pub(crate) store: ObjectStore,
    pub(crate) token: String,
}

/// One parsed request: routing info + lowercased headers + query.
pub(crate) struct Req {
    pub(crate) method: String,
    pub(crate) bucket: String,
    /// `None` = `/<bucket>` (bucket level); `Some(k)` = object level
    /// (possibly `""` for `/<bucket>/` bucket queries).
    pub(crate) key: Option<String>,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) headers: Vec<(String, String)>,
}

impl Req {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
    pub(crate) fn q(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Reply carrier; the body may stream from a file (path, offset, len).
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) content_type: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Body,
}

pub(crate) enum Body {
    Empty,
    Bytes(Vec<u8>),
    File(PathBuf, u64, u64),
}

impl Response {
    fn len(&self) -> u64 {
        match &self.body {
            Body::Empty => 0,
            Body::Bytes(b) => b.len() as u64,
            Body::File(_, _, n) => *n,
        }
    }
}

pub(crate) fn xml_reply(status: u16, doc: String) -> Response {
    Response {
        status,
        content_type: "application/xml".to_string(),
        headers: Vec::new(),
        body: Body::Bytes(doc.into_bytes()),
    }
}

pub(crate) fn error_reply(status: u16, code: &str, msg: &str, resource: &str) -> Response {
    xml_reply(status, xml::error(code, msg, resource))
}

pub(crate) fn method_not_allowed(resource: &str) -> Response {
    error_reply(405, "MethodNotAllowed", "method not allowed against this resource", resource)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Error",
    }
}

async fn handle_conn(mut sock: TcpStream, ctx: Arc<Ctx>) {
    let (head, mut leftover) = match read_head(&mut sock).await {
        Ok(Some(v)) => v,
        Ok(None) => return, // peer closed before a full head
        Err(rep) => return reply_and_close(&mut sock, rep).await,
    };
    // ---- framing ----
    if head.headers.iter().any(|(n, _)| n == "transfer-encoding") {
        return reply_and_close(&mut sock, error_reply(501, "NotImplemented", "chunked transfer encoding is not supported", "/")).await;
    }
    let len = if head.method == "PUT" { head.content_length.unwrap_or(0) } else { 0 };
    if len > MAX_BODY_BYTES {
        return reply_and_close(&mut sock, error_reply(413, "EntityTooLarge", "request body exceeds the 1 GiB limit", "/")).await;
    }
    let expects_continue = head
        .headers
        .iter()
        .any(|(n, v)| n == "expect" && v.eq_ignore_ascii_case("100-continue"));
    if expects_continue && len > 0 {
        let _ = sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
    }
    // ---- auth: non-empty token requires `Bearer <token>` (scheme
    // case-insensitive, token verbatim; not timing-hardened, like the
    // RESP AUTH path) ----
    if !ctx.token.is_empty() {
        let expected = format!("bearer {}", ctx.token);
        let ok = head
            .headers
            .iter()
            .any(|(n, v)| n == "authorization" && v.trim().to_ascii_lowercase() == expected);
        if !ok {
            return reply_and_close(&mut sock, error_reply(401, "AccessDenied", "missing or invalid bearer credentials", "/")).await;
        }
    }
    // ---- route ----
    let rep = match build_req(&head) {
        Ok(req) => super::router::route(&ctx, req, &mut sock, &mut leftover, len).await,
        Err(e) => error_reply(400, "BadRequest", &e, "/"),
    };
    let _ = write_response(&mut sock, &rep, head.method == "HEAD").await;
    let _ = sock.shutdown().await;
}

async fn reply_and_close(sock: &mut TcpStream, rep: Response) {
    let _ = write_response(sock, &rep, false).await;
    let _ = sock.shutdown().await;
}

/// Serialize + send. `is_head` sends headers only (Content-Length
/// still reflects the body GET would have carried, per RFC). Every
/// reply carries Date/Server/x-amz-request-id and Connection: close.
async fn write_response(sock: &mut TcpStream, rep: &Response, is_head: bool) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nDate: {}\r\nServer: rdb-s3\r\nx-amz-request-id: {}\r\n",
        rep.status,
        reason(rep.status),
        http_date(now_ms() / 1000),
        request_id()
    );
    if rep.status != 204 {
        head.push_str(&format!("Content-Type: {}\r\nContent-Length: {}\r\n", rep.content_type, rep.len()));
    }
    for (n, v) in &rep.headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    sock.write_all(head.as_bytes()).await?;
    if is_head {
        return Ok(());
    }
    match &rep.body {
        Body::Empty => {}
        Body::Bytes(b) => sock.write_all(b).await?,
        Body::File(path, offset, len) => {
            let mut file = tokio::fs::File::open(path).await?;
            file.seek(std::io::SeekFrom::Start(*offset)).await?;
            let mut buf = vec![0u8; CHUNK];
            let mut sent = 0u64;
            while sent < *len {
                let want = std::cmp::min(CHUNK as u64, *len - sent) as usize;
                let n = file.read(&mut buf[..want]).await?;
                if n == 0 {
                    break; // truncated underneath us: stop, never panic
                }
                sock.write_all(&buf[..n]).await?;
                sent += n as u64;
            }
        }
    }
    Ok(())
}

/// Read until `\r\n\r\n`; `Ok(None)` = silent close, `Err` = reply.
async fn read_head(sock: &mut TcpStream) -> Result<Option<(Head, Vec<u8>)>, Response> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 8192];
    let end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(error_reply(431, "BadRequest", "request head exceeds the maximum allowed length", "/"));
        }
        let n = match tokio::time::timeout(HEAD_TIMEOUT, sock.read(&mut chunk)).await {
            Ok(Ok(0)) => return Ok(None),
            Ok(Ok(n)) => n,
            Ok(Err(_)) | Err(_) => return Ok(None),
        };
        buf.extend_from_slice(&chunk[..n]);
    };
    let leftover = buf[end + 4..].to_vec();
    parse_head(&buf[..end]).map(|head| Some((head, leftover)))
}

/// Parsed head section (headers lowercased, content-length pre-parsed:
/// malformed values are request errors, like the Go http server).
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    content_length: Option<u64>,
}

fn parse_head(bytes: &[u8]) -> Result<Head, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "request head must be ASCII/UTF-8".to_string())?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or("empty request head")?;
    let mut parts = request_line.split(' ');
    let (method, target, version) = (
        parts.next().ok_or("malformed request line")?,
        parts.next().ok_or("malformed request line")?,
        parts.next().ok_or("malformed request line")?,
    );
    if parts.next().is_some()
        || method.is_empty()
        || !target.starts_with('/')
        || !version.starts_with("HTTP/1.")
        || !method.bytes().all(|b| b.is_ascii_uppercase() || b == b'_' || b == b'-')
    {
        return Err("malformed request line".to_string());
    }
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
        .map(|(_, v)| v.parse::<u64>().map_err(|_| "malformed content-length".to_string()))
        .transpose()?;
    Ok(Head { method: method.to_string(), target: target.to_string(), headers, content_length })
}

/// Head -> routing request: segment 1 is the RAW bucket, everything
/// after the first `/` percent-decodes as ONE key (`%2F` yields a
/// literal `/` inside the key).
fn build_req(head: &Head) -> Result<Req, String> {
    let target = head.target.split('#').next().unwrap_or("");
    let (raw_path, raw_query) = match target.find('?') {
        Some(i) => (&target[..i], &target[i + 1..]),
        None => (target, ""),
    };
    let trimmed = raw_path.strip_prefix('/').unwrap_or(raw_path);
    let (bucket, raw_key) = match trimmed.find('/') {
        None => (trimmed.to_string(), None),
        Some(i) => (trimmed[..i].to_string(), Some(percent_decode(&trimmed[i + 1..])?)),
    };
    let mut query = Vec::new();
    for part in raw_query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        query.push((decode_query(k), decode_query(v)));
    }
    Ok(Req {
        method: head.method.clone(),
        bucket,
        key: raw_key,
        query,
        headers: head.headers.clone(),
    })
}

/// `%XX` decoding for path segments (`+` stays a literal plus).
fn percent_decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = hex_digit(bytes.get(i + 1).copied()).ok_or("bad percent-escape")?;
                let lo = hex_digit(bytes.get(i + 2).copied()).ok_or("bad percent-escape")?;
                out.push(hi * 16 + lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "bad percent-escape".to_string())
}

/// Query-component decoding: `+` = space, `%XX` = byte; malformed
/// escapes pass through verbatim (query keys are advisory here).
fn decode_query(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match (bytes.get(i + 1).and_then(|b| hex_digit(*b)), bytes.get(i + 2).and_then(|b| hex_digit(*b))) {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
