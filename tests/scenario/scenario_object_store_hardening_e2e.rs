//! Multi-store scenario #158 -- an object-store hardening contract: a rogue client
//! drops the bearer, picks a wrong method (POST/COPY), forgets Content-Length,
//! announces a 1 GiB+ body, asks for chunked transfer or sends a 66 KB head without
//! its blank line and gets the documented status + XML `<Code>` (401 AccessDenied,
//! 405 MethodNotAllowed, 411, 501, 413, 431 BadRequest); key-path rules answer 400 and
//! a missing bucket 404 NoSuchBucket. The normal path still works: identity headers,
//! Content-Type echo, idempotent DELETE, Range edges (206/206/416/200) and a MySQL
//! manifest whose rows == GET/HEAD sizes == the prefix listing.
//! SURFACES: S3 + MySQL. One REAL node, fronts s3 + mysql.
//! Deviation: 405 carries NO Allow header (src/s3/router.rs:262); the only 501 is a
//! Transfer-Encoding request (src/s3/http.rs:215); `/<bucket>/` is bucket level (router.rs:30);
//! 413 via the announced Content-Length; 431 only while the blank line is missing (http.rs:356-369).

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, int, ints, rows, run, s};
use scenario_common::s3_extra::{header_value, s3_call_headers};
use scenario_common::{front_world, s3_call, FrontSpec};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TAG: &str = "s3hard158";
const PREFIX: &str = "hard158";
/// Announced PUT length: one byte over the 1 GiB body cap (http.rs:30).
const OVER_CAP: &str = "1073741825";
/// Head padding past the 64 KiB head cap (http.rs:29).
const HEAD_PAD: usize = 66_000;

/// One raw HTTP/1.1 round trip with VERBATIM header lines -- nothing is
/// folded in (no bearer, no Content-Length): (status, head, body). A
/// refused request may close mid-write/mid-read; bytes read so far win.
async fn http_raw(
    addr: &str,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String, String) {
    let mut req = format!("{method} {target} HTTP/1.1\r\nHost: s3-e2e\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    let mut raw = req.into_bytes();
    raw.extend_from_slice(body);
    roundtrip(addr, &raw).await
}

/// Send `raw` bytes, read the reply to EOF, split head/body: (status,
/// head, body). Used directly for a head that never terminates.
async fn roundtrip(addr: &str, raw: &[u8]) -> (u16, String, String) {
    let mut sock = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(_) => return (0, String::new(), String::new()),
    };
    let _ = sock.write_all(raw).await;
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0u8; 8192];
        match tokio::time::timeout(Duration::from_secs(10), sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

/// Bearer-authenticated raw GET (no body).
async fn authed_get(s3: &str, bearer: &str, target: &str) -> (u16, String, String) {
    http_raw(s3, "GET", target, &[("Authorization", bearer)], b"").await
}

/// Bearer-authenticated raw PUT with an empty, length-declared body.
async fn authed_put(s3: &str, bearer: &str, target: &str) -> (u16, String, String) {
    http_raw(
        s3,
        "PUT",
        target,
        &[("Authorization", bearer), ("Content-Length", "0")],
        b"",
    )
    .await
}

/// Text of the first `<tag>` in an S3 XML body ("" when absent).
fn xml_tag(doc: &str, tag: &str) -> String {
    doc.split_once(&format!("<{tag}>"))
        .and_then(|(_, rest)| rest.split_once(&format!("</{tag}>")))
        .map(|(inner, _)| inner.to_string())
        .unwrap_or_default()
}

/// RFC 7231 HTTP-date shape: `Tue, 15 Sep 2026 05:34:56 GMT`.
fn is_http_date(v: &str) -> bool {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    let t: Vec<&str> = v.split_whitespace().collect();
    t.len() == 6
        && t[0].strip_suffix(',').is_some_and(|d| DAYS.contains(&d))
        && MONTHS.contains(&t[2])
        && t[5] == "GMT"
        && t[1].len() == 2
        && digits(t[1])
        && t[3].len() == 4
        && digits(t[3])
        && t[4].split(':').count() == 3
        && t[4].bytes().all(|b| b.is_ascii_digit() || b == b':')
}

/// Every reply carries the wire identity (http.rs:304-309): status line,
/// Server, Date, x-amz-request-id `s3-<16 hex>`, Connection: close.
fn assert_identity(head: &str, what: &str) {
    assert!(head.starts_with("HTTP/1.1 "), "{what}: {head}");
    assert_eq!(header_value(head, "Server"), Some("rdb-s3"), "{what}: {head}");
    assert_eq!(header_value(head, "Connection"), Some("close"), "{what}: {head}");
    let date = header_value(head, "Date").unwrap_or_default();
    assert!(is_http_date(date), "{what}: Date {date:?} in {head}");
    let id = header_value(head, "x-amz-request-id").unwrap_or_default();
    assert!(
        id.strip_prefix("s3-")
            .is_some_and(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit())),
        "{what}: request id {id:?} in {head}"
    );
}

/// Assert a refused raw request (status + XML `<Code>` + identity) and
/// hand the reply head back for follow-up header probes.
fn refused(what: &str, reply: (u16, String, String), status: u16, code: &str) -> String {
    let (got, head, body) = reply;
    assert_eq!(got, status, "{what}: {head}{body}");
    assert_eq!(xml_tag(&body, "Code"), code, "{what}: {body}");
    assert_identity(&head, what);
    head
}

/// (key, size) of every `<Contents>` block of a ListObjectsV2 page.
fn contents(doc: &str) -> Vec<(String, i64)> {
    doc.split("<Contents>")
        .skip(1)
        .filter_map(|blk| {
            let blk = blk.split_once("</Contents>")?.0;
            Some((xml_tag(blk, "Key"), xml_tag(blk, "Size").parse().ok()?))
        })
        .collect()
}

/// Content type an object echoes back: an empty plan = no header sent,
/// which the PUT path defaults (router.rs:283-285).
fn stored_ct(planned: &str) -> &str {
    if planned.is_empty() {
        "application/octet-stream"
    } else {
        planned
    }
}

#[tokio::test]
async fn object_store_hardening_contract() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        s3: true,
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world(TAG, &spec).await;
    let s3 = front.s3.clone();
    let mut c = connect_root(&front.node).await;
    let bearer = format!("Bearer {}", scenario_common::S3_TOKEN);
    // The export plan: (key, Content-Type sent, body); `""` = no header.
    let objects: [(&str, &str, &[u8]); 3] = [
        ("hard158/a.json", "application/json", br#"{"hard158":"alpha","n":1}"#),
        ("hard158/b.txt", "text/plain", b"hard158 beta line\n"),
        ("hard158/c.bin", "", &[0u8, 1, 2, 3, 250, 255]),
    ];

    // a) The export manifest: one SQL row per object = the PUT plan.
    ddl(
        &mut c,
        "CREATE TABLE object_manifest (obj_key VARCHAR(128) PRIMARY KEY, \
         size_bytes INT NOT NULL, content_type VARCHAR(64) NOT NULL)",
    )
    .await;
    for (key, ct, body) in objects {
        run(
            &mut c,
            &format!(
                "INSERT INTO object_manifest VALUES ('{key}', {}, '{}')",
                body.len(),
                stored_ct(ct)
            ),
        )
        .await;
    }
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM object_manifest").await, vec![3]);

    // b) The hardening block: every protocol refusal is the documented
    //    status + code, and nothing it announces is ever stored.
    let t = scenario_common::perf::now();
    refused("GET without bearer", http_raw(&s3, "GET", "/rdb/k", &[], b"").await, 401, "AccessDenied");
    refused(
        "GET with a wrong bearer",
        http_raw(&s3, "GET", "/rdb/k", &[("Authorization", "Bearer wrong-token")], b"").await,
        401,
        "AccessDenied",
    );
    let head = refused(
        "POST on an object path",
        http_raw(
            &s3,
            "POST",
            &format!("/rdb/{}", objects[0].0),
            &[("Authorization", bearer.as_str()), ("Content-Length", "0")],
            b"",
        )
        .await,
        405,
        "MethodNotAllowed",
    );
    assert!(header_value(&head, "Allow").is_none(), "no Allow header: {head}");
    refused("PUT without Content-Length", http_raw(&s3, "PUT", "/rdb/hard158/nolen.json", &[("Authorization", bearer.as_str())], b"").await, 411, "MissingContentLength");
    refused(
        "chunked transfer",
        http_raw(
            &s3,
            "PUT",
            "/rdb/hard158/chunked",
            &[("Authorization", bearer.as_str()), ("Transfer-Encoding", "chunked")],
            b"",
        )
        .await,
        501,
        "NotImplemented",
    );
    refused(
        "body over the 1 GiB cap",
        http_raw(
            &s3,
            "PUT",
            "/rdb/hard158/huge",
            &[("Authorization", bearer.as_str()), ("Content-Length", OVER_CAP)],
            b"",
        )
        .await,
        413,
        "EntityTooLarge",
    );
    // The 431 fires only while the terminator is still missing: the
    // reader checks `\r\n\r\n` BEFORE the cap (http.rs:356-359), so the
    // probe sends a 66 KB head and never the blank line.
    let pad = "a".repeat(HEAD_PAD);
    let open_head = format!(
        "GET /rdb/hard158/a.json HTTP/1.1\r\nHost: s3-e2e\r\nAuthorization: {bearer}\r\nX-Pad: {pad}"
    );
    refused("head over the 64 KiB cap", roundtrip(&s3, open_head.as_bytes()).await, 431, "BadRequest");
    // Key-path rules (object.rs:67-75 -> router.rs:245) and buckets.
    let long = format!("/rdb/{PREFIX}/{}", "k".repeat(1025));
    for (what, target, code) in [
        ("empty key component", format!("/rdb/{PREFIX}//double"), "InvalidArgument"),
        ("dot-dot key", format!("/rdb/{PREFIX}/../secret"), "InvalidArgument"),
        ("dot component", format!("/rdb/{PREFIX}/./here"), "InvalidArgument"),
        ("trailing slash key", format!("/rdb/{PREFIX}/dir/"), "InvalidArgument"),
        ("NUL key", format!("/rdb/{PREFIX}/bad%00key"), "InvalidArgument"),
        ("1025-byte key", long, "InvalidArgument"),
        ("bad percent-escape", format!("/rdb/{PREFIX}/bad%ZZkey"), "BadRequest"),
    ] {
        refused(what, authed_get(&s3, &bearer, &target).await, 400, code);
    }
    refused("invalid bucket name", authed_put(&s3, &bearer, "/UPPER").await, 400, "InvalidBucketName");
    refused("uppercase bucket on an object", authed_get(&s3, &bearer, "/UPPER/x").await, 400, "InvalidArgument");
    refused("missing bucket", authed_get(&s3, &bearer, "/s3hard158-nobucket").await, 404, "NoSuchBucket");
    // HEAD keeps the status but never carries the XML body (http.rs:287).
    let (status, head, body) =
        http_raw(&s3, "HEAD", "/s3hard158-nobucket", &[("Authorization", bearer.as_str())], b"").await;
    assert_eq!((status, body.as_str()), (404, ""), "HEAD a missing bucket: {head}");
    assert_identity(&head, "HEAD a missing bucket");
    scenario_common::perf::within("s3hard158/hardening-probes", t, Duration::from_secs(2));

    // c) The normal path: PUT echoes ETag + the stored Content-Type, GET
    //    returns the bytes, HEAD the size; nothing the rogue client
    //    announced was stored.
    let t = scenario_common::perf::now();
    for (key, ct, body) in objects {
        let headers: &[(&str, &str)] = if ct.is_empty() { &[] } else { &[("Content-Type", ct)] };
        let (status, head, _) = s3_call_headers(&s3, "PUT", &format!("/rdb/{key}"), headers, body).await;
        assert_eq!(status, 200, "PUT {key}: {head}");
        assert_identity(&head, "PUT");
        let etag = header_value(&head, "ETag").unwrap_or_default().to_string();
        assert!(
            etag.len() == 34
                && etag.starts_with('"')
                && etag.ends_with('"')
                && etag.bytes().skip(1).take(32).all(|b| b.is_ascii_hexdigit()),
            "ETag is a quoted md5: {etag}"
        );
        let (status, head, got) = s3_call(&s3, "GET", &format!("/rdb/{key}"), b"").await;
        assert_eq!((status, got.as_slice()), (200, body), "GET {key}");
        assert_identity(&head, "GET");
        assert_eq!(header_value(&head, "Content-Type"), Some(stored_ct(ct)), "GET {key}");
        let len = body.len().to_string();
        assert_eq!(header_value(&head, "Content-Length"), Some(len.as_str()), "GET {key}");
        assert_eq!(header_value(&head, "Accept-Ranges"), Some("bytes"));
        assert_eq!(header_value(&head, "ETag"), Some(etag.as_str()), "GET {key}");
        let (status, head, got) = s3_call(&s3, "HEAD", &format!("/rdb/{key}"), b"").await;
        assert_eq!((status, got.len()), (200, 0), "HEAD {key} carries no body: {head}");
        assert_eq!(header_value(&head, "Content-Type"), Some(stored_ct(ct)), "HEAD {key}");
        assert_eq!(header_value(&head, "Content-Length"), Some(len.as_str()), "HEAD {key}");
    }
    for ghost in ["hard158/nolen.json", "hard158/huge", "hard158/chunked"] {
        let (status, _, body) = s3_call(&s3, "HEAD", &format!("/rdb/{ghost}"), b"").await;
        assert_eq!(status, 404, "nothing stored for {ghost}: {body:?}");
    }

    // d) DELETE is idempotent: a missing key answers 204 twice (204
    //    carries no entity headers, http.rs:313-321).
    for _ in 0..2 {
        let (status, head, body) = s3_call(&s3, "DELETE", &format!("/rdb/{PREFIX}/never"), b"").await;
        assert_eq!((status, body.len()), (204, 0), "DELETE missing key: {head}");
        assert!(header_value(&head, "Content-Type").is_none(), "no Content-Type on 204: {head}");
        assert_identity(&head, "DELETE");
    }

    // e) Range edges (mod.rs:121-153): a span, a suffix, a start past EOF,
    //    and specs that degrade to the full 200 body.
    let (key, _, body) = objects[0];
    let (size, target) = (body.len(), format!("/rdb/{key}"));
    let (status, head, got) = s3_call_headers(&s3, "GET", &target, &[("Range", "bytes=0-3")], b"").await;
    assert_eq!((status, got.as_slice()), (206, &body[..4]), "bytes=0-3");
    let span = format!("bytes 0-3/{size}");
    assert_eq!(header_value(&head, "Content-Range"), Some(span.as_str()));
    let (status, head, got) = s3_call_headers(&s3, "GET", &target, &[("Range", "bytes=-3")], b"").await;
    assert_eq!((status, got.as_slice()), (206, &body[size - 3..]), "bytes=-3");
    let span = format!("bytes {}-{}/{}", size - 3, size - 1, size);
    assert_eq!(header_value(&head, "Content-Range"), Some(span.as_str()));
    let (status, _, got) = s3_call_headers(&s3, "GET", &target, &[("Range", "bytes=100-200")], b"").await;
    assert_eq!(status, 416, "start past EOF");
    assert_eq!(xml_tag(&String::from_utf8_lossy(&got), "Code"), "InvalidRange");
    for spec in ["bytes=abc-def", "bytes=5-2", "bytes=0-1,3-4", "items=0-2"] {
        let (status, head, got) = s3_call_headers(&s3, "GET", &target, &[("Range", spec)], b"").await;
        assert_eq!((status, got.as_slice()), (200, body), "ignored range {spec}");
        assert!(header_value(&head, "Content-Range").is_none(), "{spec}: {head}");
    }
    scenario_common::perf::within("s3hard158/put-get-head-range", t, Duration::from_secs(2));

    // f) Reconciliation: the manifest rows are the PUT parameters, the
    //    prefix listing is the same set with the same sizes.
    let mut plan: Vec<(String, i64, &str)> = objects
        .iter()
        .map(|(k, ct, b)| (k.to_string(), b.len() as i64, stored_ct(ct)))
        .collect();
    plan.sort_by(|a, b| a.0.cmp(&b.0));
    let (status, _, listed) = s3_call(&s3, "GET", &format!("/rdb?list-type=2&prefix={PREFIX}/"), b"").await;
    assert_eq!(status, 200);
    assert_eq!(
        contents(&String::from_utf8_lossy(&listed)),
        plan.iter().map(|(k, sz, _)| (k.clone(), *sz)).collect::<Vec<_>>(),
        "prefix listing == manifest keys/sizes"
    );
    assert_eq!(
        rows(&mut c, "SELECT obj_key, size_bytes, content_type FROM object_manifest ORDER BY obj_key").await,
        plan.iter()
            .map(|(k, sz, ct)| vec![s(k), int(*sz), s(ct)])
            .collect::<Vec<_>>(),
        "manifest (key, size, content_type) == the PUT plan"
    );
    assert_eq!(
        ints(&mut c, "SELECT size_bytes FROM object_manifest WHERE obj_key LIKE '%a.json'").await,
        vec![objects[0].2.len() as i64],
        "manifest size == GET body size"
    );
    scenario_common::perf::scenario(TAG, t0);
}
