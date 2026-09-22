//! S3 front, process-level e2e tests.

mod client;
mod fixture;

use client::{all_tags, put, s3, split_response, xml_text};
use fixture::{dir_for, spawn_s3_node, wait_accepting};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn bearer_token_gates_the_front() {
    let mut node = spawn_s3_node(&dir_for("auth"));
    let http = node.http.clone();
    wait_accepting(&mut node, &http, "s3 http").await;
    // No Authorization -> 401 AccessDenied.
    let mut sock = TcpStream::connect(&http).await.expect("connect");
    sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await.expect("write");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = sock.read(&mut chunk).await {
        if n == 0 { break; }
        buf.extend_from_slice(&chunk[..n]);
    }
    let (s, head, body) = split_response(&buf);
    assert_eq!(s, 401, "{head}");
    assert!(body.windows(b"AccessDenied".len()).any(|w| w == b"AccessDenied"), "{}", String::from_utf8_lossy(&body));
    // Right token -> 200 ListBuckets.
    let (s, _, body) = s3(&http, "GET", "/", &[], b"").await;
    assert_eq!(s, 200, "authed ListBuckets");
    assert!(body.windows(b"<ListAllMy".len()).any(|w| w == b"<ListAllMy"), "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn put_get_head_roundtrip() {
    let mut node = spawn_s3_node(&dir_for("roundtrip"));
    let http = node.http.clone();
    wait_accepting(&mut node, &http, "s3 http").await;
    let body = b"hello s3 e2e bytes";
    let (s, head, _) = put(&node, "obj/a", body).await;
    assert_eq!(s, 200, "{head:?}");
    assert!(head.contains("etag:"), "PUT must return an ETag header: {head:?}");
    // GET returns the same bytes.
    let (s, _, got) = s3(&node.http, "GET", "/rdb/obj/a", &[], b"").await;
    assert_eq!(s, 200);
    assert_eq!(got, body, "GET bytes differ");
    // HEAD carries the size in Content-Length and no body.
    let (s, head, got) = s3(&node.http, "HEAD", "/rdb/obj/a", &[], b"").await;
    assert_eq!(s, 200);
    assert!(head.to_ascii_lowercase().contains(&format!("content-length: {}", body.len())), "{head:?}");
    assert!(got.is_empty(), "HEAD must not carry a body");
}

#[tokio::test]
async fn ranged_get() {
    let mut node = spawn_s3_node(&dir_for("range"));
    let http = node.http.clone();
    wait_accepting(&mut node, &http, "s3 http").await;
    let body = b"0123456789";
    let (s, head, _) = put(&node, "obj/r", body).await;
    assert_eq!(s, 200, "{head:?}");
    let (s, head, got) = s3(&node.http, "GET", "/rdb/obj/r", &[("Range", "bytes=0-2")], b"").await;
    assert_eq!(s, 206, "{head:?}");
    assert!(head.contains(&format!("Content-Range: bytes 0-2/{}", body.len())), "{head:?}");
    assert_eq!(got, b"012");
    // Start past EOF is unsatisfiable.
    let (s, _, got) = s3(&node.http, "GET", "/rdb/obj/r", &[("Range", "bytes=20-")], b"").await;
    assert_eq!(s, 416);
    assert!(got.windows(12).any(|w| w == b"InvalidRange"), "{}", String::from_utf8_lossy(&got));
}

#[tokio::test]
async fn list_objects_v2_folding_and_paging() {
    let mut node = spawn_s3_node(&dir_for("list"));
    let http = node.http.clone();
    wait_accepting(&mut node, &http, "s3 http").await;
    for k in ["d/1", "d/2", "e"] {
        let (s, head, _) = put(&node, k, b"x").await;
        assert_eq!(s, 200, "{head:?}");
    }
    // delimiter folding: one CommonPrefixes bucket "d/" + object "e".
    let (s, _, body) = s3(&node.http, "GET", "/rdb?list-type=2&delimiter=/", &[], b"").await;
    let doc = String::from_utf8_lossy(&body).to_string();
    assert_eq!(s, 200, "{doc}");
    assert!(doc.contains("<CommonPrefixes>") && doc.contains("<Prefix>d/</Prefix>"), "{doc}");
    // prefix view: exactly the two objects under d/.
    let (s, _, body) =
        s3(&node.http, "GET", "/rdb?list-type=2&prefix=d/&delimiter=/", &[], b"").await;
    let doc = String::from_utf8_lossy(&body).to_string();
    assert_eq!(s, 200, "{doc}");
    assert_eq!(doc.matches("<Contents>").count(), 2, "{doc}");
    // paging at max-keys=1 via NextContinuationToken.
    let mut entries: Vec<String> = Vec::new();
    let mut token = String::new();
    for _ in 0..8 {
        let target = if token.is_empty() {
            "/rdb?list-type=2&delimiter=/&max-keys=1".to_string()
        } else {
            format!("/rdb?list-type=2&delimiter=/&max-keys=1&continuation-token={token}")
        };
        let (s, _, body) = s3(&node.http, "GET", &target, &[], b"").await;
        let doc = String::from_utf8_lossy(&body).to_string();
        assert_eq!(s, 200, "{doc}");
        entries.extend(all_tags(&doc, "Key")); // Contents objects
        if let Some(cpb) =
            doc.split("<CommonPrefixes>").nth(1).and_then(|r| r.split("</CommonPrefixes>").next())
        {
            entries.extend(all_tags(cpb, "Prefix")); // folded prefixes
        }
        match xml_text(&doc, "IsTruncated") {
            Some("true") => {
                token = xml_text(&doc, "NextContinuationToken").expect("next token").to_string();
            }
            _ => break,
        }
    }
    let mut sorted = entries.clone();
    sorted.sort();
    // With delimiter=/ every page folds d/1,d/2 into "d/".
    assert_eq!(sorted, ["d/", "e"], "paged entries: {entries:?}");
    // Same walk without folding pages the raw keys.
    let mut entries: Vec<String> = Vec::new();
    let mut token = String::new();
    for _ in 0..8 {
        let target = if token.is_empty() {
            "/rdb?list-type=2&max-keys=1".to_string()
        } else {
            format!("/rdb?list-type=2&max-keys=1&continuation-token={token}")
        };
        let (s, _, body) = s3(&node.http, "GET", &target, &[], b"").await;
        let doc = String::from_utf8_lossy(&body).to_string();
        assert_eq!(s, 200, "{doc}");
        entries.extend(all_tags(&doc, "Key"));
        match xml_text(&doc, "IsTruncated") {
            Some("true") => {
                token = xml_text(&doc, "NextContinuationToken").expect("next token").to_string();
            }
            _ => break,
        }
    }
    entries.sort();
    assert_eq!(entries, ["d/1", "d/2", "e"], "unfolded paged entries");
}

#[tokio::test]
async fn delete_semantics() {
    let mut node = spawn_s3_node(&dir_for("delete"));
    let http = node.http.clone();
    wait_accepting(&mut node, &http, "s3 http").await;
    for k in ["obj/a", "obj/b"] {
        let (s, head, _) = put(&node, k, b"v").await;
        assert_eq!(s, 200, "{head:?}");
    }
    let (s, head, _) = s3(&node.http, "DELETE", "/rdb/obj/a", &[], b"").await;
    assert_eq!(s, 204, "{head:?}");
    let (s, _, body) = s3(&node.http, "GET", "/rdb/obj/a", &[], b"").await;
    assert_eq!(s, 404);
    assert!(body.windows(9).any(|w| w == b"NoSuchKey"), "{}", String::from_utf8_lossy(&body));
    // deleting the same key again is still 204 (idempotent)
    let (s, _, _) = s3(&node.http, "DELETE", "/rdb/obj/a", &[], b"").await;
    assert_eq!(s, 204);
    // non-empty bucket refuses to delete
    let (s, _, body) = s3(&node.http, "DELETE", "/rdb", &[], b"").await;
    assert_eq!(s, 409);
    assert!(body.windows(14).any(|w| w == b"BucketNotEmpty"), "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn checkpoint_publisher_lands_real_files() {
    let mut node = spawn_s3_node(&dir_for("ckpt"));
    let http = node.http.clone();
    wait_accepting(&mut node, &http, "s3 http").await;
    // Poll the LOCAL filesystem: the publisher must drop a real
    // meta.json under <s3root>/rdb/rocksdb/<bind>/ckpt_*/.
    let ckpt_root = node.s3root.join("rdb/rocksdb").join(&node.bind);
    let deadline = Instant::now() + Duration::from_secs(20);
    let id = loop {
        let found = std::fs::read_dir(&ckpt_root)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n.starts_with("ckpt_") && ckpt_root.join(n).join("meta.json").is_file());
        if let Some(id) = found {
            break id;
        }
        assert!(Instant::now() < deadline, "no checkpoint in 20s; stderr:\n{}", std::fs::read_to_string(&node.stderr_path).unwrap_or_default());
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    // The same set is listable over the protocol face.
    let (s, _, body) = s3(&node.http, "GET", "/rdb?list-type=2&prefix=rocksdb/&delimiter=/", &[], b"").await;
    let doc = String::from_utf8_lossy(&body).to_string();
    assert_eq!(s, 200, "{doc}");
    assert!(doc.contains(&format!("<Prefix>rocksdb/{}/</Prefix>", node.bind)), "{doc}");
    // meta.json decodes and points at files that really exist.
    let (s, _, body) =
        s3(&node.http, "GET", &format!("/rdb/rocksdb/{}/{}/meta.json", node.bind, id), &[], b"").await;
    assert_eq!(s, 200, "GET meta.json");
    let meta: serde_json::Value = serde_json::from_slice(&body).expect("meta.json json");
    assert_eq!(meta["node"].as_str(), Some(node.bind.as_str()), "{}", String::from_utf8_lossy(&body));
    let files = meta["files"].as_array().expect("files array");
    assert!(!files.is_empty());
    // A listed data file (skip zero-byte ones like LOCK) really carries bytes.
    let key = files
        .iter()
        .find(|f| f["size"].as_u64().unwrap_or(0) > 0)
        .and_then(|f| f["key"].as_str())
        .expect("non-empty file key");
    let (s, _, got) = s3(&node.http, "GET", &format!("/rdb/{key}"), &[], b"").await;
    assert_eq!(s, 200, "GET {key}");
    assert!(!got.is_empty(), "checkpoint file must carry bytes");
}

#[tokio::test]
async fn list_buckets_root() {
    let mut node = spawn_s3_node(&dir_for("buckets"));
    let http = node.http.clone();
    wait_accepting(&mut node, &http, "s3 http").await;
    // Bucket directories are created on demand; make one before listing.
    let (s, _, _) = s3(&node.http, "PUT", "/rdb", &[], b"").await;
    assert_eq!(s, 200, "CreateBucket rdb");
    let (s, _, body) = s3(&node.http, "GET", "/", &[], b"").await;
    let doc = String::from_utf8_lossy(&body).to_string();
    assert_eq!(s, 200, "{doc}");
    assert!(doc.contains("<ListAllMyBucketsResult"), "{doc}");
    assert!(doc.contains("<Name>rdb</Name>"), "buckets: {doc}");
}
