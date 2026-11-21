//! Multi-store scenario #50 -- an image thumbnail pipeline: each asset
//! is a JSON doc whose `stages` array walks the pipeline (ARRINSERT
//! injects a late watermark stage, ARRTRIM keeps the last three,
//! OBJLEN/STRLEN/TYPE read the shape), the rendered thumbnail bytes
//! land on the S3 front (PUT/GET roundtrip + prefix LIST), and MySQL
//! registers every asset the pipeline finished. One REAL node, RESP +
//! S3 + MySQL.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, int_of, r, s3_call, FrontSpec};

#[tokio::test]
async fn thumbnail_pipeline_stages_upload_and_register() {
    let spec = FrontSpec {
        mysql: true,
        s3: true,
        ..Default::default()
    };
    let (front, a) = front_world("img50", &spec).await;
    let (s3, resp) = (front.s3.clone(), a.clone());
    let mut c = connect_root(&front.node).await;
    let _ = resp;

    // a) The asset doc: an original + two render stages.
    scenario_common::ok(
        &r(&a, &[b"JSON.SET", b"img:a1", b"$", br#"{"uri":"s3://imgs/a1.png","stages":["decode","resize"]}"#]).await,
        "set asset",
    );

    // b) A late watermark stage lands at index 1; the pipeline grows.
    assert_eq!(
        int_of(&r(&a, &[b"JSON.ARRINSERT", b"img:a1", b"$.stages", b"1", b"\"watermark\""]).await),
        3
    );
    assert_eq!(int_of(&r(&a, &[b"JSON.ARRLEN", b"img:a1", b"$.stages"]).await), 3);
    // A bogus index is refused, leaving the doc alone.
    let oor = r(&a, &[b"JSON.ARRINSERT", b"img:a1", b"$.stages", b"99", b"\"nope\""]).await;
    assert!(oor.starts_with(b"-ERR"), "range guard: {oor:?}");
    assert_eq!(int_of(&r(&a, &[b"JSON.ARRLEN", b"img:a1", b"$.stages"]).await), 3);

    // c) Policy: keep the first three stages; ARRTRIM reasserts 0..2.
    assert_eq!(
        int_of(&r(&a, &[b"JSON.ARRTRIM", b"img:a1", b"$.stages", b"0", b"2"]).await),
        3
    );
    let stages = scenario_common::rf(&a, &[b"JSON.GET", b"img:a1", b"$.stages"]).await;
    assert!(
        stages.contains("decode") && stages.contains("watermark") && stages.contains("resize"),
        "{stages}"
    );

    // d) Doc shape reads: OBJLEN counts top fields, STRLEN sizes the
    //    uri, TYPE names each path's kind.
    assert_eq!(int_of(&r(&a, &[b"JSON.OBJLEN", b"img:a1", b"$"]).await), 2);
    assert_eq!(int_of(&r(&a, &[b"JSON.STRLEN", b"img:a1", b"$.uri"]).await), 16);
    assert_eq!(rb_pub(&a, &[b"JSON.TYPE", b"img:a1", b"$.stages"]).await.trim_start_matches('+'), "array");
    assert_eq!(rb_pub(&a, &[b"JSON.TYPE", b"img:a1", b"$.uri"]).await.trim_start_matches('+'), "string");
    assert_eq!(rb_pub(&a, &[b"JSON.TYPE", b"img:a1", b"$.ghost"]).await, "$-1", "missing path nil");

    // e) The rendered bytes hit S3: PUT, byte-exact GET, prefix LIST.
    let thumb = b"\x89PNG-fake-thumbnail-bytes-for-a1";
    let (status, body, _) = s3_call(&s3, "PUT", "/rdb/a1/thumb.png", thumb).await;
    assert_eq!(status, 200, "{body}");
    let (status, body, got) = s3_call(&s3, "GET", "/rdb/a1/thumb.png", b"").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(got, thumb, "byte-exact roundtrip");
    let (status, head, list) = s3_call(&s3, "GET", "/rdb?list-type=2&prefix=a1/", b"").await;
    assert_eq!(status, 200, "{head}");
    assert!(
        String::from_utf8_lossy(&list).contains("thumb.png"),
        "listing sees the object"
    );

    // f) MySQL registers the finished asset + stage count.
    ddl(
        &mut c,
        "CREATE TABLE assets (id BIGINT PRIMARY KEY, uri VARCHAR(64) NOT NULL, stages INT NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO assets (id, uri, stages) VALUES (1, 's3://imgs/a1.png', 3)").await;
    assert_eq!(
        ints(&mut c, "SELECT stages FROM assets WHERE uri = 's3://imgs/a1.png'").await,
        vec![3]
    );
}

/// JSON.TYPE replies a simple string (+name) or a null bulk ($-1).
async fn rb_pub(a: &str, args: &[&[u8]]) -> String {
    scenario_common::rt(a, args).await
}
