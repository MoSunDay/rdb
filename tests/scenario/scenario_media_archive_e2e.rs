//! Multi-store scenario #9 -- media archiving across FOUR planes: the
//! object bytes go through the S3 HTTP front (PUT/GET/list, s3_e2e
//! shapes), the metadata lives in SQL, an upload-URL signing cache is a
//! short-TTL KV string, the recent-uploads strip is a List, and a full
//! process restart must leave the archived object byte-identical and
//! listed (RocksDB + S3 store persistence).

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, int_of, ok, r, rb, rf, s3_call, FrontSpec};

#[tokio::test]
async fn media_archive_upload_sign_list_and_restart() {
    let _serial = scenario_common::serial_guard().await;
    // a) One node: S3 + MySQL fronts.
    let spec = FrontSpec {
        mysql: true,
        s3: true,
        ..Default::default()
    };
    let (mut front, a) = front_world("media9", &spec).await;
    let (s3, resp) = (front.s3.clone(), a.clone());
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE media (name VARCHAR(128) PRIMARY KEY, size BIGINT NOT NULL, kind VARCHAR(16) NOT NULL)",
    )
    .await;

    // b) Upload two objects through the S3 front; the bytes carry
    //    non-UTF8 content so byte-exactness is a real claim.
    let cover = b"\x89PNG\r\n\x1a\n-fake-bytes-\xff\x00\x7f";
    let clip = b"\x00\x01\x02G\x00T\xfeK clip payload";
    for (key, body) in [("img/cover.png", &cover[..]), ("vid/clip.bin", &clip[..])] {
        let (status, head, _) = s3_call(&s3, "PUT", &format!("/rdb/{key}"), body).await;
        assert_eq!(status, 200, "put {key}: {head}");
        let (status, _, got) = s3_call(&s3, "GET", &format!("/rdb/{key}"), b"").await;
        assert_eq!(status, 200, "get {key}");
        assert_eq!(got, body.to_vec(), "byte-exact round trip {key}");
        run(
            &mut c,
            &format!(
                "INSERT INTO media (name, size, kind) VALUES ('{key}', {}, '{}')",
                body.len(),
                if key.starts_with("img") {
                    "image"
                } else {
                    "video"
                }
            ),
        )
        .await;
    }

    // c) The bucket listing shows both keys under their prefixes.
    let (status, _, body) = s3_call(&s3, "GET", "/rdb?list-type=2", b"").await;
    assert_eq!(status, 200);
    let listing = String::from_utf8_lossy(&body).into_owned();
    assert!(
        listing.contains("img/cover.png") && listing.contains("vid/clip.bin"),
        "{listing}"
    );

    // d) Upload-URL signing cache: a 5-minute TTL KV entry per object.
    for key in ["img/cover.png", "vid/clip.bin"] {
        ok(
            &r(
                &resp,
                &[
                    b"SET",
                    &format!("sign:{key}").into_bytes(),
                    b"sig-abc",
                    b"EX",
                    b"300",
                ],
            )
            .await,
            &format!("sign cache {key}"),
        );
    }
    let ttl = int_of(&r(&resp, &[b"TTL", b"sign:img/cover.png"]).await);
    assert!((1..=300).contains(&ttl), "sign ttl: {ttl}");

    // e) Recent uploads strip: newest first, capped at five.
    assert_eq!(
        int_of(
            &r(
                &resp,
                &[b"LPUSH", b"media:recent", b"vid/clip.bin", b"img/cover.png"]
            )
            .await
        ),
        2
    );
    let recent = rf(&resp, &[b"LRANGE", b"media:recent", b"0", b"-1"]).await;
    assert!(
        recent.contains("img/cover.png") && recent.contains("vid/clip.bin"),
        "{recent}"
    );

    // f) kill -9 + respawn: the object store, the SQL catalog, the KV
    //    cache and the strip all come back; bytes stay identical.
    front.node.kill_now();
    front.node.respawn();
    common::wait_resp_ready(&mut front.node, 30).await;
    common::wait_mysql_ready(&front.node, 15).await;
    let mut c = connect_root(&front.node).await; // fresh conn post-restart
    let (status, _, got) = s3_call(&s3, "GET", "/rdb/img/cover.png", b"").await;
    assert_eq!(status, 200, "cover survived restart");
    assert_eq!(got, cover.to_vec(), "byte-exact after restart");
    let sizes = ints(&mut c, "SELECT SUM(size) FROM media").await;
    assert_eq!(
        sizes,
        vec![(cover.len() + clip.len()) as i64],
        "SQL catalog intact"
    );
    assert_eq!(rb(&resp, &[b"GET", b"sign:vid/clip.bin"]).await, "sig-abc");
    assert_eq!(int_of(&r(&resp, &[b"LLEN", b"media:recent"]).await), 2);
}
