//! Raw-TCP S3 client helpers: one request per connection (server closes).

use super::fixture::{Node, TOKEN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub(super) fn split_response(buf: &[u8]) -> (u16, String, Vec<u8>) {
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head \\r\\n\\r\\n");
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let len: usize = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let body = buf[head_end + 4..].to_vec();
    // HEAD responses advertise Content-Length without sending a body.
    assert!(
        body.len() >= len || body.is_empty(),
        "short body: head\n{head}"
    );
    (status, head, body[..len.min(body.len())].to_vec())
}

pub(super) async fn s3(
    addr: &str,
    method: &str,
    target: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let mut heads = String::new();
    // extra may carry its own Authorization (auth-focused tests)
    if !extra
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("authorization"))
    {
        heads.push_str(&format!("Authorization: Bearer {TOKEN}\r\n"));
    }
    for (n, v) in extra {
        heads.push_str(&format!("{n}: {v}\r\n"));
    }
    let req = format!(
        "{method} {target} HTTP/1.1\r\nHost: s3-e2e\r\nContent-Length: {}\r\n{heads}\r\n",
        body.len()
    );
    let mut sock = TcpStream::connect(addr).await.expect("connect s3");
    sock.write_all(req.as_bytes()).await.expect("write head");
    sock.write_all(body).await.expect("write body");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => break, // front is Connection: close
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let (s, head, out) = split_response(&buf);
    (s, head, out)
}

/// First XML text of `<tag>...</tag>` in `body` (None when absent).
pub(super) fn xml_text<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let i = body.find(&open)? + open.len();
    let j = body[i..].find(&close)? + i;
    Some(&body[i..j])
}

/// Every `<tag>...</tag>` text in order.
pub(super) fn all_tags(body: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(after[..j].to_string());
        rest = &after[j + close.len()..];
    }
    out
}

pub(super) async fn put(node: &Node, key: &str, body: &[u8]) -> (u16, String, String) {
    let (s, head, _) = s3(&node.http, "PUT", &format!("/rdb/{key}"), &[], body).await;
    (s, head.to_ascii_lowercase(), head)
}

// ---- checkpoint-set helpers ----------------------------------------------

/// On-disk `ckpt_*` names under the node's checkpoint root, sorted
/// (ids are ms timestamps, so string order == age order).
pub(super) fn ckpt_ids_on_disk(ckpt_root: &std::path::Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(ckpt_root)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ckpt_"))
        .collect();
    ids.sort();
    ids
}

/// Checkpoint ids visible over the S3 face: the folded prefixes of
/// `GET /rdb?list-type=2&prefix=rocksdb/<bind>/&delimiter=/`.
pub(super) async fn ckpt_ids_over_s3(node: &Node) -> Vec<String> {
    let target = format!("/rdb?list-type=2&prefix=rocksdb/{}/&delimiter=/", node.bind);
    let (s, _, body) = s3(&node.http, "GET", &target, &[], b"").await;
    if s == 404 {
        return Vec::new(); // no bucket until the first publish lands
    }
    assert_eq!(s, 200, "list checkpoints over s3");
    let doc = String::from_utf8_lossy(&body).to_string();
    let want = format!("rocksdb/{}/ckpt_", node.bind);
    let mut ids: Vec<String> = all_tags(&doc, "Prefix")
        .into_iter()
        .filter(|p| p.starts_with(&want) && p.ends_with('/'))
        .map(|p| {
            p.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    ids.sort();
    ids
}

/// Extend `ever` with ids not seen before (sampling bookkeeping).
pub(super) fn push_unique(ever: &mut Vec<String>, ids: &[String]) {
    for id in ids {
        if !ever.contains(id) {
            ever.push(id.clone());
        }
    }
}

// ---- tests ---------------------------------------------------------------
