//! Batch 11 scenario #111 -- a video streaming edge on three planes:
//! three deterministic 4 KiB segments go through the S3 object front
//! (PUT), then 20 ranged GETs serve byte-exact slices with exact
//! `Content-Range` replies (18 closed spans + one `bytes=-512` suffix +
//! one open-ended `bytes=3000-`), 10 HEADs answer size-only, 3
//! delimiter LISTs fold `video/` into a CommonPrefix and enumerate the
//! keys under a prefix, and an unsatisfiable `bytes=9999-` is a 416
//! `InvalidRange`. The segment registry lives in SQL and its totals are
//! mirrored by a RESP `HSET` size cache. One REAL node: S3 + MySQL +
//! RESP fronts.

use std::time::Duration;

use crate::common::mysql::{connect_root, ddl, ints, run};
use crate::scenario_common;
use crate::scenario_common::s3_extra::{header_value, s3_call_headers};
use scenario_common::{front_world, rb, s3_call, FrontSpec};

const KEYS: [&str; 3] = ["seg0", "seg1", "seg2"];
const SIZE: usize = 4096;

/// Deterministic segment bytes: byte j of segment i = (i*31+j) % 251
/// (all values < 251, so every byte is ASCII-safe and non-UTF8-free).
fn seg_body(i: usize) -> Vec<u8> {
    (0..SIZE).map(|j| ((i * 31 + j) % 251) as u8).collect()
}

/// The 20 range probes: (Range header, first byte, last byte). The
/// closed spans rotate over edge/mid/whole/singleton cuts, then the
/// suffix form and the open-ended form.
fn range_probes() -> Vec<(String, usize, usize)> {
    let mut out: Vec<(String, usize, usize)> = [
        (0usize, 1023usize),
        (1024, 2047),
        (2048, 3071),
        (3072, 4095),
        (0, 4095),
        (100, 199),
        (4000, 4095),
        (512, 767),
        (0, 0),
        (4095, 4095),
        (17, 18),
        (1, 1),
        (2000, 2500),
        (3000, 3999),
        (1023, 1024),
        (7, 8),
        (999, 1000),
        (31, 32),
    ]
    .iter()
    .map(|(a, b)| (format!("bytes={a}-{b}"), *a, *b))
    .collect();
    out.push(("bytes=-512".to_string(), SIZE - 512, SIZE - 1));
    out.push(("bytes=3000-".to_string(), 3000, SIZE - 1));
    out
}

#[tokio::test]
async fn video_segments_ranged_gets_head_list_and_sql_registry() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        s3: true,
        ..Default::default()
    };
    let (front, resp) = front_world("range111", &spec).await;
    let s3 = front.s3.clone();

    // a) Three segments with deterministic bodies land in the bucket.
    let bodies: Vec<Vec<u8>> = (0..KEYS.len()).map(seg_body).collect();
    for (i, key) in KEYS.iter().enumerate() {
        let (status, head, _) = s3_call(&s3, "PUT", &format!("/rdb/video/{key}"), &bodies[i]).await;
        assert_eq!(status, 200, "put {key}: {head}");
    }

    // b) The hot block: 20 ranged GETs + 10 HEADs + 3 LISTs.
    let probes = range_probes();
    let t = scenario_common::perf::now();
    for (k, (range, first, last)) in probes.iter().enumerate() {
        let i = k % KEYS.len();
        let key = KEYS[i];
        let (status, head, got) = s3_call_headers(
            &s3,
            "GET",
            &format!("/rdb/video/{key}"),
            &[("Range", range.as_str())],
            b"",
        )
        .await;
        assert_eq!(status, 206, "range {range} on {key}: {head}");
        let want_range = format!("bytes {first}-{last}/{SIZE}");
        assert_eq!(
            header_value(&head, "Content-Range"),
            Some(want_range.as_str()),
            "exact Content-Range for {range} on {key}"
        );
        assert_eq!(
            got,
            bodies[i][*first..=*last].to_vec(),
            "byte-exact slice for {range} on {key}"
        );
    }
    for k in 0..10 {
        let key = KEYS[k % KEYS.len()];
        let (status, head, got) =
            s3_call_headers(&s3, "HEAD", &format!("/rdb/video/{key}"), &[], b"").await;
        assert_eq!(status, 200, "head {key}: {head}");
        assert_eq!(
            header_value(&head, "Content-Length"),
            Some(SIZE.to_string().as_str()),
            "HEAD {key} carries the size"
        );
        assert!(got.is_empty(), "HEAD {key} must not carry a body");
    }
    // An unsatisfiable span: 416 + the InvalidRange error document.
    let (status, _, got) = s3_call_headers(
        &s3,
        "GET",
        "/rdb/video/seg0",
        &[("Range", "bytes=9999-")],
        b"",
    )
    .await;
    assert_eq!(status, 416, "start past the end is 416");
    let doc = String::from_utf8_lossy(&got).into_owned();
    assert!(doc.contains("InvalidRange"), "416 body: {doc}");
    // Delimiter LIST: the whole bucket folds into one CommonPrefix.
    let (status, _, got) = s3_call(&s3, "GET", "/rdb?list-type=2&delimiter=/", b"").await;
    assert_eq!(status, 200, "list delimiter=/");
    let list = String::from_utf8_lossy(&got).into_owned();
    assert!(
        list.contains("<CommonPrefixes><Prefix>video/</Prefix></CommonPrefixes>"),
        "video/ folds into a CommonPrefix: {list}"
    );
    assert!(!list.contains("<Key>"), "delimiter=/ folds every key away");
    // Delimiter LIST under the prefix: the three segments, no fold left.
    let (status, _, got) = s3_call(
        &s3,
        "GET",
        "/rdb?list-type=2&prefix=video/&delimiter=/",
        b"",
    )
    .await;
    assert_eq!(status, 200, "list prefix=video/");
    let list = String::from_utf8_lossy(&got).into_owned();
    for key in KEYS {
        assert!(
            list.contains(&format!("<Key>video/{key}</Key>")),
            "{key} listed in Contents: {list}"
        );
    }
    assert!(
        !list.contains("<CommonPrefixes>"),
        "flat keys under video/ fold nothing: {list}"
    );
    // Third LIST: a key-prefix scan narrows to one segment.
    let (status, _, got) = s3_call(&s3, "GET", "/rdb?list-type=2&prefix=video/seg1", b"").await;
    assert_eq!(status, 200, "list prefix=video/seg1");
    let list = String::from_utf8_lossy(&got).into_owned();
    assert!(list.contains("<Key>video/seg1</Key>"), "{list}");
    assert!(!list.contains("<Key>video/seg0</Key>"), "{list}");
    scenario_common::perf::within(
        "range111/20-range+10-head+3-list",
        t,
        Duration::from_secs(3),
    );

    // c) The segment registry in SQL: three rows summing to 12288.
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE video_segments(id BIGINT PRIMARY KEY, name VARCHAR(16), size BIGINT)",
    )
    .await;
    for (i, key) in KEYS.iter().enumerate() {
        run(
            &mut c,
            &format!(
                "INSERT INTO video_segments (id, name, size) VALUES ({i}, '{key}', {})",
                bodies[i].len()
            ),
        )
        .await;
    }
    assert_eq!(
        ints(&mut c, "SELECT SUM(size) FROM video_segments").await,
        vec![12288],
        "SQL SUM(size) over the three segments"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM video_segments").await,
        vec![3],
        "SQL row count"
    );

    // d) The RESP size cache mirrors the registry: same three values,
    //    same total.
    for (i, key) in KEYS.iter().enumerate() {
        let set = scenario_common::r(
            &resp,
            &[
                b"HSET",
                b"video:sizes",
                key.as_bytes(),
                bodies[i].len().to_string().as_bytes(),
            ],
        )
        .await;
        assert!(set.starts_with(b":1"), "hset {key}: {set:?}");
    }
    let mut total = 0i64;
    for key in KEYS {
        let v = rb(&resp, &[b"HGET", b"video:sizes", key.as_bytes()]).await;
        let n: i64 = v
            .parse()
            .unwrap_or_else(|_| panic!("HGET video:sizes {key} replied {v:?}"));
        assert_eq!(n, SIZE as i64, "cached size of {key}");
        total += n;
    }
    assert_eq!(total, 12288, "cache total == SQL SUM(size)");
    scenario_common::perf::scenario("range111", t0);
}
