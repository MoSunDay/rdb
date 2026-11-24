//! Multi-store scenario #125 -- a data-lake export run's full object
//! lifecycle on the S3 plane, reconciled against a SQL export manifest:
//! CreateBucket (200, the repeat 409 BucketAlreadyOwnedByYou), 25 part
//! objects with deterministic bodies, ListObjectsV2 paged at
//! max-keys=10 (pages 10/10/5, the union is all 25 keys sorted, each
//! listed Size == the manifest's), delimiter folding (exactly one
//! CommonPrefixes `lake/`), DeleteBucket while non-empty (409
//! BucketNotEmpty), 25 object deletes (204), DeleteBucket (204) and
//! HEAD -> 404. The fixture's pre-declared `rdb` bucket is the
//! checkpoint publisher's target (its `rocksdb/` object sets land from
//! 3s after boot and would pollute the listing and block the bucket
//! delete), so the lifecycle rides its own bucket. One REAL node, S3 +
//! MySQL + RESP.

use crate::common;
use crate::scenario_common;
use common::mysql::{col, connect_root, ddl, ints, pairs, run, s};
use scenario_common::{FrontSpec, front_world, s3_call};
use std::time::Duration;

const BUCKET: &str = "lake125";
const PARTS: usize = 25;

/// Object key of part `i`.
fn part_key(i: usize) -> String {
    format!("lake/part-{i:02}")
}

/// Deterministic body of part `i` (the manifest records its size).
fn part_body(i: usize) -> Vec<u8> {
    format!("lake125|part-{i:02}|{}", "x".repeat(i)).into_bytes()
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

/// `needle` somewhere in a raw (XML) reply body.
fn body_has(body: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(body).contains(needle)
}

/// (key, size) of every `<Contents>` block.
fn contents(doc: &str) -> Vec<(String, i64)> {
    xml_all(doc, "Contents")
        .iter()
        .map(|blk| {
            let size = xml_tag(blk, "Size")
                .and_then(|v| v.parse().ok())
                .unwrap_or(-1);
            (xml_tag(blk, "Key").unwrap_or_default(), size)
        })
        .collect()
}

/// Folded prefixes: the `<Prefix>` inside every `<CommonPrefixes>`.
fn common_prefixes(doc: &str) -> Vec<String> {
    xml_all(doc, "CommonPrefixes")
        .iter()
        .map(|blk| xml_tag(blk, "Prefix").unwrap_or_default())
        .collect()
}

/// Walk ListObjectsV2 at max-keys=10; returns (per-page key counts,
/// every (key, size) listed across the pages).
async fn list_paged(s3: &str) -> (Vec<usize>, Vec<(String, i64)>) {
    let mut pages = Vec::new();
    let mut listed = Vec::new();
    let mut token = String::new();
    for _ in 0..8 {
        let target = if token.is_empty() {
            format!("/{BUCKET}?list-type=2&max-keys=10")
        } else {
            format!("/{BUCKET}?list-type=2&max-keys=10&continuation-token={token}")
        };
        let (status, _, body) = s3_call(s3, "GET", &target, b"").await;
        let doc = String::from_utf8_lossy(&body).into_owned();
        assert_eq!(status, 200, "list page {target}: {doc}");
        let entries = contents(&doc);
        pages.push(entries.len());
        listed.extend(entries);
        match xml_tag(&doc, "IsTruncated").as_deref() {
            Some("true") => {
                token = xml_tag(&doc, "NextContinuationToken").expect("next token");
            }
            _ => break,
        }
    }
    (pages, listed)
}

#[tokio::test]
async fn export_bucket_lifecycle_matches_the_sql_manifest() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        s3: true,
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("lake125", &spec).await;
    let s3 = front.s3.clone();
    let mut c = connect_root(&front.node).await;

    // a) The export manifest: one SQL row per part (key + body size).
    ddl(
        &mut c,
        "CREATE TABLE lake_export (part INT PRIMARY KEY, object_key VARCHAR(64) NOT NULL, \
         size_bytes INT NOT NULL, exported_at VARCHAR(32) NOT NULL)",
    )
    .await;
    for i in 0..PARTS {
        run(
            &mut c,
            &format!(
                "INSERT INTO lake_export VALUES ({i}, '{}', {}, '2026-11-24 10:00:00')",
                part_key(i),
                part_body(i).len()
            ),
        )
        .await;
    }
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM lake_export").await,
        vec![PARTS as i64]
    );

    // b) CreateBucket: 200 fresh, 409 BucketAlreadyOwnedByYou on the
    //    repeat, and HEAD carries the verdict (200 while it exists).
    let (status, head, _) = s3_call(&s3, "PUT", &format!("/{BUCKET}"), b"").await;
    assert_eq!(status, 200, "create bucket: {head}");
    let (status, _, body) = s3_call(&s3, "PUT", &format!("/{BUCKET}"), b"").await;
    assert_eq!(status, 409, "duplicate bucket: {body:?}");
    assert!(
        body_has(&body, "BucketAlreadyOwnedByYou"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (status, _, _) = s3_call(&s3, "HEAD", &format!("/{BUCKET}"), b"").await;
    assert_eq!(status, 200, "HEAD on the fresh bucket");

    // c) The hot block: 25 PUTs, 3 paged LISTs + the delimiter fold, the
    //    refused bucket delete, 25 object DELETEs and the bucket delete.
    let t = scenario_common::perf::now();
    for i in 0..PARTS {
        let (status, head, _) = s3_call(
            &s3,
            "PUT",
            &format!("/{BUCKET}/{}", part_key(i)),
            &part_body(i),
        )
        .await;
        assert_eq!(status, 200, "put {}: {head}", part_key(i));
    }

    let (pages, listed) = list_paged(&s3).await;
    assert_eq!(pages, vec![10, 10, 5], "three pages at max-keys=10");
    let mut expected: Vec<(String, i64)> = (0..PARTS)
        .map(|i| (part_key(i), part_body(i).len() as i64))
        .collect();
    expected.sort();
    assert_eq!(
        listed, expected,
        "paged walk == all 25 keys in key order, real sizes"
    );

    let (status, _, body) = s3_call(
        &s3,
        "GET",
        &format!("/{BUCKET}?list-type=2&delimiter=/"),
        b"",
    )
    .await;
    let doc = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(status, 200, "delimiter list: {doc}");
    assert_eq!(
        common_prefixes(&doc),
        vec!["lake/".to_string()],
        "one fold: {doc}"
    );
    assert!(
        contents(&doc).is_empty(),
        "everything folds under lake/: {doc}"
    );

    let (status, _, body) = s3_call(&s3, "DELETE", &format!("/{BUCKET}"), b"").await;
    assert_eq!(status, 409, "non-empty bucket delete: {body:?}");
    assert!(
        body_has(&body, "BucketNotEmpty"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    for i in 0..PARTS {
        let (status, head, _) =
            s3_call(&s3, "DELETE", &format!("/{BUCKET}/{}", part_key(i)), b"").await;
        assert_eq!(status, 204, "delete {}: {head}", part_key(i));
    }
    let (status, head, _) = s3_call(&s3, "DELETE", &format!("/{BUCKET}"), b"").await;
    assert_eq!(status, 204, "emptied bucket deletes: {head}");
    scenario_common::perf::within("lake125/25-put+3-list+25-delete", t, Duration::from_secs(3));

    // d) The bucket is gone for good (HEAD 404, a repeat DELETE 404).
    let (status, _, _) = s3_call(&s3, "HEAD", &format!("/{BUCKET}"), b"").await;
    assert_eq!(status, 404, "HEAD on the deleted bucket");
    let (status, _, body) = s3_call(&s3, "DELETE", &format!("/{BUCKET}"), b"").await;
    assert_eq!(status, 404, "repeat bucket delete: {body:?}");
    assert!(
        body_has(&body, "NoSuchBucket"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    // e) Reconciliation: the manifest IS the object set that was listed
    //    (keys in part order, sizes exact), and the manifest outlives the
    //    bucket (it is the export's durable record).
    let parts = pairs(
        &mut c,
        "SELECT part, object_key FROM lake_export ORDER BY part",
    )
    .await;
    assert_eq!(
        parts,
        (0..PARTS)
            .map(|i| (i as i64, part_key(i)))
            .collect::<Vec<_>>(),
        "manifest keys in part order"
    );
    let sizes = ints(&mut c, "SELECT size_bytes FROM lake_export ORDER BY part").await;
    let manifest: Vec<(String, i64)> = parts
        .into_iter()
        .zip(sizes)
        .map(|((_, k), sz)| (k, sz))
        .collect();
    assert_eq!(manifest, expected, "manifest sizes == listed sizes");
    assert_eq!(
        col(&mut c, "SELECT object_key FROM lake_export WHERE part = 7").await,
        vec![s(&part_key(7))],
        "manifest row 7"
    );
    scenario_common::perf::scenario("lake125", t0);
}
