//! Cluster join client (Rust port of Go `rcache.JoinRaftCluster`): one
//! hand-rolled HTTP GET against the existing cluster's control API. Any
//! failure is fatal for the joining process (Go `RCache.Log.Fatal`), so
//! errors bubble up to main as plain strings.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::rcache::http::read_head;

/// Generous single deadline; Go relied on net/http defaults plus the
/// process exiting on failure, with no retry loop either.
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on the response body: the only expected bodies are the
/// tiny "ok" / "unauthorized\n" strings, so anything larger means a
/// hostile or broken endpoint (or a MITM) claiming a giant
/// Content-Length or streaming without one. Without the bound the
/// joining node buffers the body unbounded.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Go `JoinRaftCluster`: GET `/join?peerAddress=<raft-tcp-addr>&raft-token=
/// <token>` on the join address; the body must be exactly "ok". Query
/// values are percent-encoded: a token containing `&`, `+` or `%` would
/// otherwise corrupt the request (wrong token presented, historically a
/// silent fake "ok" from the server's bug-parity path).
pub async fn join_cluster(join_addr: &str, raft_tcp_addr: &str, token: &str) -> Result<(), String> {
    let url = format!(
        "http://{join_addr}/join?peerAddress={}&raft-token={}",
        percent_encode(raft_tcp_addr),
        percent_encode(token)
    );
    let (status, body) = http_get_status(&url).await?;
    if status != 200 || body != "ok" {
        // Keep the Go message shape; a 401 body ("unauthorized\n") reads
        // naturally here too.
        return Err(format!("Error joining cluster: {body}"));
    }
    Ok(())
}

/// Percent-encode one query value: every byte outside the RFC 3986
/// unreserved set (ALPHA / DIGIT / `-._~`) becomes `%XX`, so reserved
/// characters (`&`, `+`, `%`, ...) cannot split or mutate the query.
/// Local pure function: no encoding dependency exists in the workspace.
fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Minimal HTTP/1.1 GET client over a raw TcpStream (no new dependencies):
/// writes one request with Connection: close, then reads the response head
/// and exactly Content-Length body bytes (EOF when the header is absent).
pub async fn http_get(url: &str) -> Result<String, String> {
    http_get_status(url).await.map(|(_, body)| body)
}

/// [`http_get`] plus the parsed response status code (tests assert the
/// 401 a wrong-token `/join`/`/depart` now returns).
pub async fn http_get_status(url: &str) -> Result<(u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported url: {url}"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let io_err = |e: io::Error| e.to_string();

    let connect = tokio::time::timeout(JOIN_TIMEOUT, TcpStream::connect(host)).await;
    let mut stream = connect
        .map_err(|_| format!("connect {host} timed out"))?
        .map_err(io_err)?;

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.map_err(io_err)?;

    let (head, mut body) = read_head(&mut stream).await.map_err(io_err)?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            format!(
                "malformed status line: {}",
                head.lines().next().unwrap_or("")
            )
        })?;
    let content_length = head.split("\r\n").skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim().eq_ignore_ascii_case("content-length")).then(|| value.trim())
    });

    match content_length.and_then(|v| v.parse::<usize>().ok()) {
        Some(n) => {
            // A lying/giant Content-Length must be rejected on the header
            // alone, BEFORE any body byte is buffered.
            if n > MAX_BODY_BYTES {
                return Err("response body too large".to_string());
            }
            while body.len() < n {
                let mut tmp = vec![0u8; 4096];
                let read = tokio::time::timeout(JOIN_TIMEOUT, stream.read(&mut tmp)).await;
                let got = read
                    .map_err(|_| "read body timed out".to_string())?
                    .map_err(io_err)?;
                if got == 0 {
                    return Err("early eof".to_string());
                }
                body.extend_from_slice(&tmp[..got]);
            }
            // The server may send MORE than it promised; keep only n.
            body.truncate(n);
        }
        None => {
            // Bounded read-to-end: no Content-Length means the body ends
            // at EOF, so without a cap a hostile peer streams forever.
            loop {
                let mut tmp = vec![0u8; 4096];
                let read = tokio::time::timeout(JOIN_TIMEOUT, stream.read(&mut tmp)).await;
                let got = read
                    .map_err(|_| "read body timed out".to_string())?
                    .map_err(io_err)?;
                if got == 0 {
                    break; // EOF: body complete
                }
                body.extend_from_slice(&tmp[..got]);
                if body.len() > MAX_BODY_BYTES {
                    return Err("response body too large".to_string());
                }
            }
        }
    };
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// One-shot server answering `body` with a Content-Length response.
    async fn one_shot(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("GET /join?"), "unexpected request: {req}");
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn http_get_parses_content_length_response() {
        let addr = one_shot("ok").await;
        let body = http_get(&format!("http://{addr}/join?peerAddress=x&raft-token=t"))
            .await
            .unwrap();
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn join_cluster_requires_exact_ok_body() {
        let addr = one_shot("internal error\n").await;
        let err = join_cluster(&addr, "127.0.0.1:1", "t").await.unwrap_err();
        assert_eq!(err, "Error joining cluster: internal error\n");
    }

    #[tokio::test]
    async fn join_cluster_connect_error_surfaces() {
        // Bind then drop: the port is closed again.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let err = join_cluster(&addr, "127.0.0.1:1", "t").await.unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn percent_encode_keeps_unreserved_only() {
        assert_eq!(percent_encode("abcXYZ09-._~"), "abcXYZ09-._~");
        assert_eq!(percent_encode("a&b+c%d"), "a%26b%2Bc%25d");
        assert_eq!(percent_encode("127.0.0.1:22681"), "127.0.0.1%3A22681");
    }

    #[tokio::test]
    async fn join_cluster_percent_encodes_query_values() {
        // One-shot server echoing the request line back, answering "ok".
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
            stream.write_all(resp.as_bytes()).await.unwrap();
            req
        });
        // ':' in the address and '&'/'+', '%' in the token must all be
        // escaped, or the server would parse a corrupted token.
        join_cluster(&addr, "127.0.0.1:1", "a&b+c%d").await.unwrap();
        let req = seen.await.unwrap();
        assert!(
            req.starts_with("GET /join?peerAddress=127.0.0.1%3A1&raft-token=a%26b%2Bc%25d "),
            "unexpected request: {req}"
        );
    }

    #[tokio::test]
    async fn http_get_status_parses_status_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let _ = stream.read(&mut buf).await;
            let resp = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 13\r\n\r\nunauthorized\n";
            stream.write_all(resp.as_bytes()).await.unwrap();
        });
        let (status, body) = http_get_status(&format!("http://{addr}/join?x=1"))
            .await
            .unwrap();
        assert_eq!(status, 401);
        assert_eq!(body, "unauthorized\n");
    }

    #[tokio::test]
    async fn http_get_status_rejects_lying_content_length() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let _ = stream.read(&mut buf).await;
            // Hostile endpoint: Content-Length ~100GB with an oversized
            // body. Writes ignore errors: the client rejects on the
            // header alone and may drop the socket mid-stream.
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n")
                .await;
            let _ = stream.write_all(&vec![b'a'; MAX_BODY_BYTES + 1]).await;
        });
        let err = http_get_status(&format!("http://{addr}/join?x=1"))
            .await
            .unwrap_err();
        assert!(err.contains("too large"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn http_get_status_rejects_unbounded_lengthless_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let _ = stream.read(&mut buf).await;
            // No Content-Length (EOF-terminated body): the old
            // read_to_end path buffered this stream without bound.
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await;
            let _ = stream.write_all(&vec![b'b'; 2 * MAX_BODY_BYTES]).await;
        });
        let err = http_get_status(&format!("http://{addr}/join?x=1"))
            .await
            .unwrap_err();
        assert!(err.contains("too large"), "unexpected error: {err}");
    }
}
