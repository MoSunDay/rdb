//! Multi-store scenario #173 -- a data-lake export tool ships objects with the `Expect: 100-continue` handshake:
//! the interim line is written BEFORE auth (a bearer-less PUT still gets `100 Continue` and then 401 AccessDenied),
//! a zero-length PUT and a GET with `Expect` get NO interim, an upload cut short (Content-Length 10, 4 bytes, half-close)
//! answers 400 IncompleteBody within the read budget and leaves NO object and no staging file, and a garbage
//! Content-Length answers 400 BadRequest before auth. The published set is reconciled across faces: ETag == quoted md5,
//! GET bodies and HEAD headers, a sidecar-less object degrades to `"<size>-<mtime_secs>"`, the XML listing carries the
//! escaped keys (`<>&'"`, space), and a RESP hash catalog plus a MySQL manifest equal the Rust md5 of the served body.
//! SURFACES: S3 raw HTTP + RESP + MySQL. One REAL node, fronts s3 + mysql.
//! Deviation (pinned): `x-amz-meta-*` is never stored or echoed (src/s3/router.rs:283-285 keeps content-type only);
//! a plain INSERT re-registering an existing PRIMARY KEY silently overwrites instead of 1062 (src/sql/exec/write.rs:182-190);
//! the checkpoint publisher keeps adding `rocksdb/...` keys to bucket `rdb`, so the listing is read under the export prefix.

use crate::common;
use crate::scenario_common;
use common::mysql::{aff, col, connect_root, ddl, ints, s};
use md5::{Digest, Md5};
use scenario_common::s3_extra::{header_value, s3_call_headers};
use scenario_common::{front_world, rb, rt, s3_call, FrontSpec};
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TAG: &str = "s3cont173";
/// Export prefix: the checkpoint publisher shares bucket `rdb`, so every
/// key of this scenario lives under one prefix and the listing is filtered.
const PREFIX: &str = "exp173/";
const NO_BEARER: &str = "missing or invalid bearer credentials";
const MALFORMED_LEN: &str = "malformed content-length";
const PEER_CLOSED: &str = "request body read failed: peer closed mid-body";
const NO_SUCH_KEY: &str = "the specified key does not exist";
const EMPTY_MD5: &str = "d41d8cd98f00b204e9800998ecf8427e";
const OCTET: &str = "application/octet-stream";
const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";
/// Every read of the raw exchange is bounded (a server parked on the 60s
/// BODY_TIMEOUT path shows up as status 0, never as a hang).
const READ_BUDGET: Duration = Duration::from_secs(5);

fn key(name: &str) -> String {
    format!("{PREFIX}{name}")
}

/// md5 hex of `bytes` -- the ETag the server must quote.
fn md5_hex(bytes: &[u8]) -> String {
    hex::encode(Md5::digest(bytes))
}

/// S3-quoted ETag form.
fn quoted(hex: &str) -> String {
    format!("\"{hex}\"")
}

/// Request head with VERBATIM header lines (nothing is folded in).
fn head(method: &str, target: &str, headers: &[(&str, &str)]) -> String {
    let mut out = format!("{method} {target} HTTP/1.1\r\nHost: s3-e2e\r\nConnection: close\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out
}

/// One read into `buf`; false on EOF, error or READ_BUDGET timeout.
async fn pull(sock: &mut TcpStream, buf: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; 8192];
    match tokio::time::timeout(READ_BUDGET, sock.read(&mut chunk)).await {
        Ok(Ok(n)) if n > 0 => {
            buf.extend_from_slice(&chunk[..n]);
            true
        }
        _ => false,
    }
}

/// Raw HTTP/1.1 exchange on a fresh connection: `(status, final head,
/// body, interim 100-continue blocks)`. `staged` = the RFC 7231 handshake:
/// send the head alone, WAIT for the interim block, then send the body.
/// `half_close` shuts the write side after the body (the IncompleteBody
/// signal). Interim blocks are stripped before the final head is parsed.
async fn exchange(
    addr: &str,
    head: &str,
    body: &[u8],
    staged: bool,
    half_close: bool,
) -> (u16, String, Vec<u8>, usize) {
    let mut sock = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(_) => return (0, String::new(), Vec::new(), 0),
    };
    let _ = sock.write_all(head.as_bytes()).await;
    let mut buf = Vec::new();
    if staged {
        while !buf.ends_with(b"\r\n\r\n") && pull(&mut sock, &mut buf).await {}
    }
    let _ = sock.write_all(body).await;
    if half_close {
        let _ = sock.shutdown().await;
    }
    while pull(&mut sock, &mut buf).await {}
    let mut interim = 0;
    while buf.starts_with(CONTINUE) {
        buf.drain(..CONTINUE.len());
        interim += 1;
    }
    let cut = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(buf.len());
    let text = String::from_utf8_lossy(&buf[..cut]).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    let body = if cut + 4 <= buf.len() { buf[cut + 4..].to_vec() } else { Vec::new() };
    (status, text, body, interim)
}

/// Text of the first `<tag>` in an XML document ("" when absent).
fn xml_tag(doc: &[u8], tag: &str) -> String {
    String::from_utf8_lossy(doc)
        .split_once(&format!("<{tag}>"))
        .and_then(|(_, rest)| rest.split_once(&format!("</{tag}>")))
        .map(|(inner, _)| inner.to_string())
        .unwrap_or_default()
}

/// `(escaped key, ETag, size)` of every `<Contents>` block of a listing.
fn contents(doc: &[u8]) -> Vec<(String, String, u64)> {
    String::from_utf8_lossy(doc)
        .split("<Contents>")
        .skip(1)
        .filter_map(|blk| {
            let blk = blk.split_once("</Contents>")?.0;
            Some((
                xml_tag(blk.as_bytes(), "Key"),
                xml_tag(blk.as_bytes(), "ETag"),
                xml_tag(blk.as_bytes(), "Size").parse().ok()?,
            ))
        })
        .collect()
}

/// Undo the server's XML text escaping (`<>&"'` -> entities), the
/// inverse of `xml::escape` (src/s3/xml.rs:14-28).
fn xml_unescape(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let (ch, len) = [("&lt;", "<"), ("&gt;", ">"), ("&amp;", "&"), ("&quot;", "\""), ("&apos;", "'")]
            .into_iter()
            .find(|(e, _)| rest.starts_with(e))
            .map(|(e, c)| (c, e.len()))
            .unwrap_or(("&", 1));
        out.push_str(ch);
        rest = &rest[len..];
    }
    out.push_str(rest);
    out
}

/// Percent-encode an object key for the request path: everything but the
/// unreserved bytes and `/` becomes `%XX` (the server decodes it as ONE key).
fn encode_key(k: &str) -> String {
    let mut out = String::new();
    for &b in k.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._/~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Register a published object: `(key, payload, ETag the listing must show)`.
fn register(stored: &mut Vec<(String, Vec<u8>, String)>, name: &str, payload: &[u8]) {
    stored.push((key(name), payload.to_vec(), quoted(&md5_hex(payload))));
}

#[tokio::test]
async fn s3_expect_continue_incomplete() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec { s3: true, mysql: true, ..Default::default() };
    let (front, a) = front_world(TAG, &spec).await;
    let (s3, bearer) = (front.s3.clone(), format!("Bearer {}", scenario_common::S3_TOKEN));
    // front_world nests: <world>/node0/<node0 = ProcNode.dir>, while the s3
    // root (`s3_store_path`) is a SIBLING of ProcNode.dir (<node.dir>/../s3data).
    let export_dir: PathBuf = front.node.dir.join("..").join("s3data/rdb").join(PREFIX.trim_end_matches('/'));
    let mut stored: Vec<(String, Vec<u8>, String)> = Vec::new();
    let target = format!("/rdb/{}", key("a"));

    // a) The interim precedes auth: a bearer-less PUT with Expect still gets
    //    `100 Continue` FIRST, then the 401 envelope (http.rs:266-283).
    let req = head("PUT", &target, &[("Content-Length", "5"), ("Expect", "100-continue")]);
    let (status, hd, body, interim) = exchange(&s3, &req, b"hello", false, false).await;
    assert_eq!(interim, 1, "exactly one interim block: {hd}");
    assert_eq!((status, xml_tag(&body, "Code"), xml_tag(&body, "Message")), (401, "AccessDenied".to_string(), NO_BEARER.to_string()), "{hd}");
    assert_eq!(header_value(&hd, "Content-Type"), Some("application/xml"), "{hd}");

    // b) The authorized handshake PUT lands the object: the client only sends
    //    the body after the interim; 200, quoted md5 ETag, empty body.
    let req = head(
        "PUT",
        &target,
        &[("Content-Length", "5"), ("Expect", "100-continue"), ("Authorization", bearer.as_str())],
    );
    let (status, hd, body, interim) = exchange(&s3, &req, b"hello", true, false).await;
    assert_eq!((status, interim, body.len()), (200, 1, 0), "{hd}");
    assert_eq!(header_value(&hd, "ETag"), Some(quoted(&md5_hex(b"hello")).as_str()), "{hd}");
    register(&mut stored, "a", b"hello");

    // c) No interim without a body to continue: a zero-length PUT with Expect
    //    goes straight to 200 (http.rs:266: method PUT AND Content-Length > 0),
    //    and a GET with Expect answers the object with no interim either.
    let target0 = format!("/rdb/{}", key("empty0"));
    let req = head("PUT", &target0, &[("Content-Length", "0"), ("Expect", "100-continue"), ("Authorization", bearer.as_str())]);
    let (status, hd, _, interim) = exchange(&s3, &req, b"", false, false).await;
    assert_eq!((status, interim), (200, 0), "{hd}");
    assert_eq!(header_value(&hd, "ETag"), Some(quoted(EMPTY_MD5).as_str()), "{hd}");
    register(&mut stored, "empty0", b"");
    let (status, _, got) = s3_call_headers(&s3, "GET", &target, &[("Expect", "100-continue")], b"").await;
    assert_eq!((status, got), (200, b"hello".to_vec()), "GET with Expect: no interim, body intact");

    // d) IncompleteBody: Content-Length 10, 4 bytes, half-close -> 400 well
    //    inside the read budget (never the 60s body-timeout path), no object,
    //    no staging file left under the export dir (router.rs:298-320).
    let short = format!("/rdb/{}", key("short"));
    let req = head("PUT", &short, &[("Content-Length", "10"), ("Authorization", bearer.as_str())]);
    let t = scenario_common::perf::now();
    let (status, hd, body, interim) = exchange(&s3, &req, b"four", false, true).await;
    scenario_common::perf::within("s3cont/incomplete-body", t, Duration::from_secs(2));
    assert_eq!((status, interim), (400, 0), "{hd}");
    assert_eq!((xml_tag(&body, "Code"), xml_tag(&body, "Message")), ("IncompleteBody".to_string(), PEER_CLOSED.to_string()), "{body:?}");
    let (status, _, got) = s3_call(&s3, "GET", &short, b"").await;
    assert_eq!((status, xml_tag(&got, "Code"), xml_tag(&got, "Message")), (404, "NoSuchKey".to_string(), NO_SUCH_KEY.to_string()), "{got:?}");
    let leftovers: Vec<String> = std::fs::read_dir(&export_dir)
        .expect("export dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(leftovers.is_empty(), "staging files left behind: {leftovers:?}");

    // e) Malformed Content-Length is a request error BEFORE auth (one probe
    //    carries no bearer at all): 400 BadRequest, exact message.
    let bare = head("PUT", &target, &[("Content-Length", "abc")]);
    let (status, _, body, interim) = exchange(&s3, &bare, b"", false, false).await;
    assert_eq!((status, interim, xml_tag(&body, "Code"), xml_tag(&body, "Message")), (400, 0, "BadRequest".to_string(), MALFORMED_LEN.to_string()), "{body:?}");
    let neg = head("PUT", &target, &[("Content-Length", "-1"), ("Authorization", bearer.as_str())]);
    let (status, _, body, _) = exchange(&s3, &neg, b"", false, false).await;
    assert_eq!((status, xml_tag(&body, "Code"), xml_tag(&body, "Message")), (400, "BadRequest".to_string(), MALFORMED_LEN.to_string()), "{body:?}");

    // f) Hot block: PUT/GET/HEAD over three sizes -- ETag == quoted md5
    //    computed in Rust, GET body == payload, HEAD Content-Length == size,
    //    default content-type application/octet-stream on reads.
    let big: Vec<u8> = (0..300).map(|i| (i % 251) as u8).collect();
    let plan: [(&str, &[u8]); 3] = [("empty", b""), ("hello", b"hello"), ("big", &big)];
    let t = scenario_common::perf::now();
    for (name, payload) in plan {
        let target = format!("/rdb/{}", key(name));
        let (status, hd, _) = s3_call(&s3, "PUT", &target, payload).await;
        assert_eq!(status, 200, "{hd}");
        let want = quoted(&md5_hex(payload));
        assert_eq!(header_value(&hd, "ETag"), Some(want.as_str()), "{name}: {hd}");
        assert_eq!(header_value(&hd, "Content-Type"), Some("application/xml"), "{name}: {hd}");
        let (status, _, got) = s3_call(&s3, "GET", &target, b"").await;
        assert_eq!((status, got), (200, payload.to_vec()), "{name}");
        let (status, hd, _) = s3_call(&s3, "HEAD", &target, b"").await;
        assert_eq!(status, 200, "{hd}");
        assert_eq!(header_value(&hd, "ETag"), Some(want.as_str()), "{name}: {hd}");
        assert_eq!(header_value(&hd, "Content-Length"), Some(payload.len().to_string().as_str()), "{name}: {hd}");
        assert_eq!(header_value(&hd, "Content-Type"), Some(OCTET), "{name}: {hd}");
        register(&mut stored, name, payload);
    }
    scenario_common::perf::within("s3cont/put-get-etag", t, Duration::from_secs(2));

    // g) Sidecar-less object: with `<obj>.s3meta.json` removed on disk, HEAD
    //    degrades to `"<size>-<mtime_secs>"` + application/octet-stream
    //    (object.rs:242-247) whatever Content-Type the PUT carried; bytes intact.
    let bare_obj = format!("/rdb/{}", key("bare"));
    let (status, _, _) = s3_call_headers(&s3, "PUT", &bare_obj, &[("Content-Type", "text/plain")], b"0123456789").await;
    assert_eq!(status, 200, "PUT bare");
    let obj = export_dir.join("bare");
    std::fs::remove_file(obj.with_file_name("bare.s3meta.json")).expect("remove sidecar");
    let secs = std::fs::metadata(&obj)
        .and_then(|m| m.modified())
        .expect("object mtime")
        .duration_since(UNIX_EPOCH)
        .expect("unix clock")
        .as_secs();
    let degraded = format!("\"10-{secs}\"");
    let (status, hd, _) = s3_call(&s3, "HEAD", &bare_obj, b"").await;
    assert_eq!(status, 200, "{hd}");
    assert_eq!(header_value(&hd, "ETag"), Some(degraded.as_str()), "{hd}");
    assert_eq!(header_value(&hd, "Content-Type"), Some(OCTET), "{hd}");
    let (status, _, got) = s3_call(&s3, "GET", &bare_obj, b"").await;
    assert_eq!((status, got), (200, b"0123456789".to_vec()), "bare bytes intact");
    stored.push((key("bare"), b"0123456789".to_vec(), degraded));

    // h) Only content-type is stored: a custom type echoes back, x-amz-meta-*
    //    never does (deviation, router.rs:283-285).
    let ndjson = format!("/rdb/{}", key("ndjson"));
    let (status, _, _) = s3_call_headers(&s3, "PUT", &ndjson, &[("Content-Type", "application/x-ndjson"), ("x-amz-meta-owner", "crew-7")], b"{}\n").await;
    assert_eq!(status, 200, "PUT ndjson");
    let (status, hd, _) = s3_call(&s3, "HEAD", &ndjson, b"").await;
    assert_eq!(status, 200, "{hd}");
    assert_eq!(header_value(&hd, "Content-Type"), Some("application/x-ndjson"), "{hd}");
    assert!(!hd.to_ascii_lowercase().contains("x-amz-meta"), "meta echoed: {hd}");
    register(&mut stored, "ndjson", b"{}\n");

    // i) XML listing under the export prefix: `<>&'"` ride as entities, the
    //    space verbatim, keys byte-sorted, ETags as derived (xml.rs:14-28).
    let odd: [(&str, &[u8]); 2] = [("q<u>&'\".txt", b"markup"), ("with space.txt", b"spaced")];
    for (name, payload) in odd {
        let (status, hd, _) = s3_call(&s3, "PUT", &format!("/rdb/{}", encode_key(&key(name))), payload).await;
        assert_eq!(status, 200, "{hd}");
        register(&mut stored, name, payload);
    }
    let mut want = stored.clone();
    want.sort_by(|x, y| x.0.cmp(&y.0));
    let (status, _, listed) = s3_call(&s3, "GET", &format!("/rdb?list-type=2&prefix={}", encode_key(PREFIX)), b"").await;
    assert_eq!(status, 200, "list");
    assert_eq!(xml_tag(&listed, "KeyCount"), want.len().to_string(), "{listed:?}");
    assert_eq!(xml_tag(&listed, "IsTruncated"), "false", "{listed:?}");
    let text = String::from_utf8_lossy(&listed).into_owned();
    assert!(text.contains("<Key>exp173/q&lt;u&gt;&amp;&apos;&quot;.txt</Key>"), "entities on the wire: {text}");
    assert!(text.contains("<Key>exp173/with space.txt</Key>"), "space verbatim: {text}");
    // The ETag's quotes ride as `&quot;` entities too (xml::escape applies to every text node).
    let first_md5 = quoted(&md5_hex(b"hello"));
    assert!(text.contains(&format!("<ETag>{}</ETag>", first_md5.replace('"', "&quot;"))), "escaped ETag on the wire: {text}");
    let got: Vec<(String, String)> = contents(&listed).into_iter().map(|(k, e, _)| (xml_unescape(&k), xml_unescape(&e))).collect();
    let want_ke: Vec<(String, String)> = want.iter().map(|(k, _, e)| (k.clone(), e.clone())).collect();
    assert_eq!(got, want_ke, "listing keys (unescaped) + ETags");

    // j) Cross-face reconciliation: a RESP hash catalog (one HSET per object)
    //    and a MySQL manifest (one multi-row INSERT) both equal the Rust md5
    //    of the body S3 actually serves for each key.
    for (k, p, _) in &stored {
        assert_eq!(rt(&a, &[b"HSET", b"s3cat:173", k.as_bytes(), md5_hex(p).as_bytes()]).await, ":1", "{k}");
    }
    assert_eq!(rt(&a, &[b"HLEN", b"s3cat:173"]).await, format!(":{}", stored.len()), "catalog size");
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE s3_catalog (obj_key VARCHAR(128) PRIMARY KEY, md5 CHAR(32) NOT NULL)").await;
    let rows_sql = stored.iter().take(3).map(|(k, p, _)| format!("('{}', '{}')", k, md5_hex(p))).collect::<Vec<_>>().join(", ");
    assert_eq!(aff(&mut c, &format!("INSERT INTO s3_catalog (obj_key, md5) VALUES {rows_sql}")).await, 3, "multi-row INSERT affects 3 rows");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM s3_catalog").await, vec![3], "manifest rows");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM s3_catalog WHERE LENGTH(md5) = 32").await, vec![3], "every md5 is 32 hex chars");
    for (k, _, _) in stored.iter().take(3) {
        let (_, _, served) = s3_call(&s3, "GET", &format!("/rdb/{}", encode_key(k)), b"").await;
        let digest = md5_hex(&served);
        assert_eq!(rb(&a, &[b"HGET", b"s3cat:173", k.as_bytes()]).await, digest, "{k}: RESP catalog == md5(S3 body)");
        assert_eq!(col(&mut c, &format!("SELECT md5 FROM s3_catalog WHERE obj_key = '{k}'")).await, vec![s(&digest)], "{k}: SQL manifest == md5(S3 body)");
    }
    //    Re-running the export re-registers the same rows: a plain INSERT on an
    //    existing PRIMARY KEY silently overwrites (deviation, write.rs:182-190).
    let (k0, p0, _) = &stored[0];
    assert_eq!(aff(&mut c, &format!("INSERT INTO s3_catalog (obj_key, md5) VALUES ('{k0}', '{}')", md5_hex(p0))).await, 1, "re-register overwrites, no 1062");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM s3_catalog").await, vec![3], "still 3 rows");
    assert_eq!(ints(&mut c, &format!("SELECT COUNT(*) FROM s3_catalog WHERE md5 = '{}'", md5_hex(p0))).await, vec![1], "the overwritten row carries the re-registered md5");
    scenario_common::perf::scenario("s3cont", t0);
}
