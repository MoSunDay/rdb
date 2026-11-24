//! Extra S3 client surface for batch 11: the plain `s3_call` shim has
//! no room for request headers, but Range/HEAD/conditional probes need
//! them. Same one-request-per-connection, read-to-EOF shape as
//! `s3_call` (the front closes after one reply).

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::S3_TOKEN;

/// One S3 round trip with extra request headers: `(status, head,
/// body)`. `headers` ride verbatim after the fixture `Authorization`.
pub async fn s3_call_headers(
    addr: &str,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: s3-e2e\r\nContent-Length: {}\r\n\
         Authorization: Bearer {S3_TOKEN}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut sock = TcpStream::connect(addr).await.expect("connect s3");
    sock.write_all(head.as_bytes()).await.expect("write s3 head");
    sock.write_all(body).await.expect("write s3 body");
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.expect("read s3 reply");
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("s3 head \\r\\n\\r\\n");
    let text = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, text, buf[head_end + 4..].to_vec())
}

/// Case-insensitive response header lookup (`Content-Range`, `ETag`,
/// ...). `head` is the raw head block of an `s3_call*` reply.
pub fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim())
    })
}
