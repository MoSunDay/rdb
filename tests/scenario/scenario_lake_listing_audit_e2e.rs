//! Multi-store scenario #144 -- lake listing audit: an audit tool walks
//! the S3 plane as two SDK generations do -- ListObjects v1 by `marker`/
//! `NextMarker` and ListObjectsV2 by `start-after`/`continuation-token` --
//! over ten `lake/list144/` objects (eight parts, a key with a space, a key
//! with a `+`), reconciling every listed (key, size) with a SQL manifest.
//! Pinned en route: ListBuckets, max-keys 0 / -1 / garbage / 5000,
//! encoding-type=url, HEAD Content-Length, InvalidBucketName, NoSuchBucket.
//! SURFACES: S3 REST + MySQL. One REAL node, S3 + RESP + MySQL.
//! Deviation: `NextMarker`/`NextContinuationToken` are ALWAYS percent-encoded,
//! even without encoding-type (src/s3/xml.rs:118-127; AWS returns the raw key)
//! and the v1 reply carries no `<Marker>` echo; an object-level GET on a
//! missing bucket answers NoSuchKey, not NoSuchBucket (src/s3/router.rs:361-369).

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, int, one, rows, run, s};
use mysql_async::Value as MVal;
use scenario_common::s3_extra::header_value;
use scenario_common::{FrontSpec, front_world, s3_call};
use std::time::Duration;

const TAG: &str = "list144";
const BUCKET: &str = "rdb";
const PREFIX: &str = "lake/list144/";
const STRAY: &str = "other-list144/stray.bin";
const PARTS: usize = 8;

/// The ten audited keys in byte order: `a b.bin` (space) and `c+d.bin`
/// (plus) sort ahead of the eight `part-NN`.
fn audited_keys() -> Vec<String> {
    let mut keys = vec![format!("{PREFIX}a b.bin"), format!("{PREFIX}c+d.bin")];
    keys.extend((0..PARTS).map(|i| format!("{PREFIX}part-{i:02}")));
    keys
}

/// Deterministic body of `key` (its length is what the manifest records).
fn body_of(key: &str) -> Vec<u8> {
    format!("list144|{key}|{}", "x".repeat(key.len() % 13)).into_bytes()
}

/// Percent-encode a path/query component the way an SDK does:
/// unreserved bytes and `/` stay, everything else becomes `%XX`.
fn penc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Text of every `<tag>...</tag>` occurrence, document order.
fn xml_all(doc: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    doc.split(&open)
        .skip(1)
        .filter_map(|rest| rest.split_once(&close).map(|(inner, _)| inner.to_string()))
        .collect()
}

/// First `<tag>` text, if any.
fn xml_tag(doc: &str, tag: &str) -> Option<String> {
    xml_all(doc, tag).into_iter().next()
}

/// (key, size) of every `<Contents>` block, document order.
fn contents(doc: &str) -> Vec<(String, i64)> {
    xml_all(doc, "Contents")
        .iter()
        .map(|blk| {
            let size = xml_tag(blk, "Size").and_then(|v| v.parse().ok()).unwrap_or(-1);
            (xml_tag(blk, "Key").unwrap_or_default(), size)
        })
        .collect()
}

/// Keys of every `<Contents>` block, document order.
fn keys(doc: &str) -> Vec<String> {
    contents(doc).into_iter().map(|(k, _)| k).collect()
}

/// Text cell of a text-protocol row.
fn cell_text(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => panic!("expected a text cell, got {other:?}"),
    }
}

/// PUT `key` with its deterministic body; returns the size written.
async fn put(s3: &str, key: &str) -> i64 {
    let body = body_of(key);
    let (status, head, _) = s3_call(s3, "PUT", &format!("/{BUCKET}/{}", penc(key)), &body).await;
    assert_eq!(status, 200, "put {key}: {head}");
    body.len() as i64
}

/// One S3 GET: (status, head, body as text).
async fn get_text(s3: &str, target: &str) -> (u16, String, String) {
    let (status, head, body) = s3_call(s3, "GET", target, b"").await;
    (status, head, String::from_utf8_lossy(&body).into_owned())
}

/// The S3 error document's (Code, Message, Resource).
fn error_doc(doc: &str) -> (String, String, String) {
    assert!(doc.starts_with("<?xml"), "error is an XML document: {doc}");
    assert!(xml_tag(doc, "RequestId").is_some(), "RequestId present: {doc}");
    (
        xml_tag(doc, "Code").unwrap_or_default(),
        xml_tag(doc, "Message").unwrap_or_default(),
        xml_tag(doc, "Resource").unwrap_or_default(),
    )
}

/// Walk ListObjects v1 from `first` (a target without `marker`),
/// feeding each `NextMarker` back verbatim: (per-page key counts,
/// the page documents, every listed key).
async fn walk_v1(s3: &str, first: &str) -> (Vec<usize>, Vec<String>, Vec<String>) {
    let (mut pages, mut docs, mut listed) = (Vec::new(), Vec::new(), Vec::new());
    let mut target = first.to_string();
    for _ in 0..8 {
        let (status, head, doc) = get_text(s3, &target).await;
        assert_eq!(status, 200, "v1 page {target}: {head}");
        let page = keys(&doc);
        pages.push(page.len());
        listed.extend(page);
        let marker = xml_tag(&doc, "NextMarker");
        assert_eq!(
            xml_tag(&doc, "IsTruncated").as_deref(),
            Some(if marker.is_some() { "true" } else { "false" }),
            "NextMarker iff truncated: {doc}"
        );
        docs.push(doc);
        match marker {
            Some(m) => target = format!("{first}&marker={m}"),
            None => break,
        }
    }
    (pages, docs, listed)
}

#[tokio::test]
async fn s3_listings_and_errors_match_the_manifest() {
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
    let audited = audited_keys();
    let v1 = format!("/{BUCKET}?prefix={}&max-keys=4", penc(PREFIX));

    // a) The manifest: one SQL row per audited key, sized from what the
    //    PUT wrote. The stray object is deliberately left untracked.
    ddl(&mut c, "CREATE TABLE manifest (object_key VARCHAR(128) PRIMARY KEY, size BIGINT NOT NULL)").await;
    for k in &audited {
        let size = put(&s3, k).await;
        run(&mut c, &format!("INSERT INTO manifest VALUES ('{k}', {size})")).await;
    }
    put(&s3, STRAY).await;

    // b) The hot block: ListBuckets, the v1 marker walk, the v2
    //    start-after/token walks, the max-keys edges, the name /
    //    missing-bucket errors and a HEAD over every audited object.
    let t = scenario_common::perf::now();
    let (status, head, doc) = get_text(&s3, "/").await;
    assert_eq!(status, 200, "ListBuckets: {head}");
    assert!(doc.contains("<ListAllMyBucketsResult xmlns="), "{doc}");
    assert_eq!(xml_all(&doc, "Name"), vec!["rdb".to_string()], "one bucket: {doc}");
    assert_eq!(xml_tag(&doc, "ID").as_deref(), Some("rdb"), "owner id: {doc}");
    assert_eq!(xml_tag(&doc, "DisplayName").as_deref(), Some("rdb"), "{doc}");
    assert_eq!(
        xml_all(&doc, "CreationDate"),
        vec!["1970-01-01T00:00:00.000Z".to_string()],
        "{doc}"
    );

    let (pages, docs, listed) = walk_v1(&s3, &v1).await;
    assert_eq!(pages, vec![4, 4, 2], "three v1 pages at max-keys=4");
    assert_eq!(listed, audited, "the v1 walk == the ten audited keys, sorted");
    assert_eq!(docs.len(), 3, "one document per page");
    for d in &docs {
        assert_eq!(xml_tag(d, "Name").as_deref(), Some(BUCKET), "{d}");
        assert_eq!(xml_tag(d, "Prefix").as_deref(), Some(PREFIX), "{d}");
        assert_eq!(xml_tag(d, "MaxKeys").as_deref(), Some("4"), "{d}");
        assert!(xml_tag(d, "NextContinuationToken").is_none(), "v1 never speaks tokens: {d}");
        assert!(xml_tag(d, "Marker").is_none(), "no <Marker> echo: {d}");
        assert!(xml_tag(d, "EncodingType").is_none(), "{d}");
    }
    assert_eq!(xml_tag(&docs[0], "KeyCount").as_deref(), Some("4"), "{}", docs[0]);
    assert_eq!(
        xml_tag(&docs[0], "NextMarker").as_deref(),
        Some(audited[3].as_str()),
        "NextMarker == last key of the page"
    );
    assert_eq!(xml_tag(&docs[1], "NextMarker").as_deref(), Some(audited[7].as_str()));
    assert!(xml_tag(&docs[2], "NextMarker").is_none(), "last page has no marker");

    // max-keys=1: the marker is percent-encoded even without
    // encoding-type (the documented deviation), and resumes strictly
    // after the key it names.
    let one_key = format!("/{BUCKET}?prefix={}&max-keys=1", penc(PREFIX));
    let (_, _, doc) = get_text(&s3, &one_key).await;
    assert_eq!(keys(&doc), vec![audited[0].clone()], "{doc}");
    let marker = xml_tag(&doc, "NextMarker").expect("truncated single-key page");
    assert_eq!(marker, "lake/list144/a%20b.bin", "NextMarker is percent-encoded");
    let (_, _, doc) = get_text(&s3, &format!("{one_key}&marker={marker}")).await;
    assert_eq!(keys(&doc), vec![audited[1].clone()], "marker resumes strictly after: {doc}");
    assert_eq!(xml_tag(&doc, "NextMarker").as_deref(), Some("lake/list144/c%2Bd.bin"));

    // v2: start-after is exclusive; a last page carries neither a
    //    token nor a marker; sizes are the real object sizes.
    let (status, _, doc) = get_text(
        &s3,
        &format!("/{BUCKET}?list-type=2&prefix={}&start-after={}", penc(PREFIX), penc(&audited[1])),
    )
    .await;
    assert_eq!(status, 200, "{doc}");
    assert_eq!(keys(&doc), audited[2..].to_vec(), "start-after skips the first two: {doc}");
    assert_eq!(xml_tag(&doc, "KeyCount").as_deref(), Some("8"), "{doc}");
    assert_eq!(xml_tag(&doc, "IsTruncated").as_deref(), Some("false"), "{doc}");
    assert!(xml_tag(&doc, "NextContinuationToken").is_none(), "{doc}");
    assert!(xml_tag(&doc, "NextMarker").is_none(), "v2 never speaks markers: {doc}");
    let sizes: Vec<(String, i64)> = audited[2..].iter().map(|k| (k.clone(), body_of(k).len() as i64)).collect();
    assert_eq!(contents(&doc), sizes, "listed sizes are the PUT sizes");

    // v2 tokens: the token is the percent-encoded last key of the page.
    let five = format!("/{BUCKET}?list-type=2&prefix={}&max-keys=5", penc(PREFIX));
    let (_, _, doc) = get_text(&s3, &five).await;
    assert_eq!(keys(&doc), audited[..5].to_vec(), "{doc}");
    let token = xml_tag(&doc, "NextContinuationToken").expect("token on a truncated v2 page");
    assert_eq!(token, penc(&audited[4]), "token == encoded last key");
    let (_, _, doc) = get_text(&s3, &format!("{five}&continuation-token={token}")).await;
    assert_eq!(keys(&doc), audited[5..].to_vec(), "{doc}");
    assert!(xml_tag(&doc, "NextContinuationToken").is_none(), "{doc}");

    // encoding-type=url: every key percent-encoded, EncodingType echoed.
    let (_, _, doc) = get_text(&s3, &format!("/{BUCKET}?list-type=2&prefix={}&encoding-type=url", penc(PREFIX))).await;
    assert_eq!(xml_tag(&doc, "EncodingType").as_deref(), Some("url"), "{doc}");
    let encoded: Vec<String> = audited.iter().map(|k| penc(k)).collect();
    assert_eq!(keys(&doc), encoded, "keys percent-encoded: {doc}");
    assert_eq!(keys(&doc)[1], "lake/list144/c%2Bd.bin", "`+` is %2B, `/` kept");

    // max-keys edges: 0 -> empty, NOT truncated; -1 / garbage -> 400
    //    InvalidArgument; 5000 -> clamped to 1000 (one full page); an
    //    unknown token -> an empty page, not an error.
    let (status, _, doc) = get_text(&s3, &format!("/{BUCKET}?prefix={}&max-keys=0", penc(PREFIX))).await;
    assert_eq!(status, 200, "{doc}");
    assert!(keys(&doc).is_empty(), "max-keys=0 is an empty page: {doc}");
    assert_eq!(xml_tag(&doc, "MaxKeys").as_deref(), Some("0"), "{doc}");
    assert_eq!(xml_tag(&doc, "KeyCount").as_deref(), Some("0"), "{doc}");
    assert_eq!(xml_tag(&doc, "IsTruncated").as_deref(), Some("false"), "{doc}");
    assert!(xml_tag(&doc, "NextMarker").is_none(), "{doc}");
    for bad in ["-1", "abc"] {
        let (status, _, doc) = get_text(&s3, &format!("/{BUCKET}?max-keys={bad}")).await;
        assert_eq!(status, 400, "max-keys={bad}: {doc}");
        assert_eq!(
            error_doc(&doc),
            (
                "InvalidArgument".to_string(),
                "max-keys must be a non-negative integer".to_string(),
                "/rdb".to_string()
            ),
            "max-keys={bad}"
        );
    }
    let (status, _, full) = get_text(&s3, &format!("/{BUCKET}?prefix={}&max-keys=5000", penc(PREFIX))).await;
    assert_eq!(status, 200, "{full}");
    assert_eq!(xml_tag(&full, "MaxKeys").as_deref(), Some("1000"), "clamped: {full}");
    assert_eq!(keys(&full), audited, "the whole prefix fits one page: {full}");
    let (status, _, doc) = get_text(&s3, &format!("/{BUCKET}?list-type=2&continuation-token=zzz")).await;
    assert_eq!(status, 200, "an unknown token is not an error: {doc}");
    assert!(keys(&doc).is_empty(), "{doc}");
    assert_eq!(xml_tag(&doc, "IsTruncated").as_deref(), Some("false"), "{doc}");

    // Name rules and missing buckets: an ill-formed name is refused
    //    before any lookup (400), a well-formed absent one is 404 at
    //    the bucket level, and -- the deviation -- NoSuchKey at the
    //    object level. HEAD answers with the GET's Content-Length and
    //    no body.
    let (status, _, doc) = get_text(&s3, "/UPPER?list-type=2").await;
    assert_eq!(status, 400, "{doc}");
    assert_eq!(
        error_doc(&doc),
        ("InvalidBucketName".to_string(), "the specified bucket name is ill-formed".to_string(), "/UPPER".to_string())
    );
    let (status, _, doc) = get_text(&s3, "/ab").await;
    assert_eq!(status, 404, "two-char name is well-formed but absent: {doc}");
    assert_eq!(
        error_doc(&doc),
        ("NoSuchBucket".to_string(), "the specified bucket does not exist".to_string(), "/ab".to_string())
    );
    let (status, head, body) = s3_call(&s3, "HEAD", "/no-such-bucket-list144", b"").await;
    assert_eq!(status, 404, "HEAD on a missing bucket: {head}");
    assert!(body.is_empty(), "HEAD suppresses the body");
    let head_len = header_value(&head, "Content-Length").expect("Content-Length on HEAD");
    let (status, _, body) = s3_call(&s3, "GET", "/no-such-bucket-list144", b"").await;
    assert_eq!(status, 404, "{}", String::from_utf8_lossy(&body));
    assert_eq!(head_len.parse::<usize>().ok(), Some(body.len()), "HEAD Content-Length == GET body");
    assert_eq!(error_doc(&String::from_utf8_lossy(&body)).0, "NoSuchBucket");
    let (status, _, body) = s3_call(&s3, "GET", "/no-such-bucket-list144/obj", b"").await;
    assert_eq!(status, 404, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        error_doc(&String::from_utf8_lossy(&body)),
        (
            "NoSuchKey".to_string(),
            "the specified key does not exist".to_string(),
            "/no-such-bucket-list144/obj".to_string()
        ),
        "object level on a missing bucket: NoSuchKey (deviation)"
    );

    // HEAD every audited object: Content-Length == the body's size.
    let mut head_sum = 0i64;
    for k in &audited {
        let (status, head, body) = s3_call(&s3, "HEAD", &format!("/{BUCKET}/{}", penc(k)), b"").await;
        assert_eq!(status, 200, "HEAD {k}: {head}");
        assert!(body.is_empty(), "HEAD {k} suppresses the body");
        let len: i64 = header_value(&head, "Content-Length")
            .unwrap_or_else(|| panic!("Content-Length on HEAD {k}: {head}"))
            .parse()
            .unwrap();
        assert_eq!(len, body_of(k).len() as i64, "HEAD {k} size");
        head_sum += len;
    }
    // The stray sits outside the audited prefix (the prefix filter is
    //    what keeps it out of the walk, not a delete).
    let (_, _, doc) = get_text(&s3, &format!("/{BUCKET}?prefix=other-list144/")).await;
    assert_eq!(keys(&doc), vec![STRAY.to_string()], "{doc}");
    scenario_common::perf::within("list144/list-walk", t, Duration::from_secs(2));

    // c) Reconciliation: the manifest IS the audited object set (keys
    //    and sizes, both views), the HEAD sum is SUM(size), and the
    //    full-page listing's <Size> cells are the manifest's sizes.
    let manifest = rows(&mut c, "SELECT object_key, size FROM manifest ORDER BY object_key").await;
    let want: Vec<Vec<MVal>> = audited.iter().map(|k| vec![s(k), int(body_of(k).len() as i64)]).collect();
    assert_eq!(manifest, want, "manifest == audited keys with the PUT sizes");
    assert_eq!(one(&mut c, "SELECT SUM(size) FROM manifest").await, int(head_sum), "SUM(size) == HEAD sum");
    assert_eq!(one(&mut c, "SELECT COUNT(*) FROM manifest").await, int(audited.len() as i64));
    let manifest_ks: Vec<(String, i64)> = manifest
        .iter()
        .map(|r| (cell_text(&r[0]), cell_text(&r[1]).parse().expect("size digits")))
        .collect();
    assert_eq!(contents(&full), manifest_ks, "listed (key, size) == the manifest");
    assert!(manifest_ks.iter().all(|(k, _)| !k.starts_with("other-")), "the stray was never tracked");
    scenario_common::perf::scenario("list144", t0);
}
