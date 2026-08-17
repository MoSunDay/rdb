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

/// Go `JoinRaftCluster`: GET `/join?peerAddress=<raft-tcp-addr>&raft-token=
/// <token>` on the join address; the body must be exactly "ok".
pub async fn join_cluster(join_addr: &str, raft_tcp_addr: &str, token: &str) -> Result<(), String> {
    let url = format!("http://{join_addr}/join?peerAddress={raft_tcp_addr}&raft-token={token}");
    let body = http_get(&url).await?;
    if body != "ok" {
        return Err(format!("Error joining cluster: {body}"));
    }
    Ok(())
}

/// Minimal HTTP/1.1 GET client over a raw TcpStream (no new dependencies):
/// writes one request with Connection: close, then reads the response head
/// and exactly Content-Length body bytes (EOF when the header is absent).
pub async fn http_get(url: &str) -> Result<String, String> {
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
    let content_length = head.split("\r\n").skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim().eq_ignore_ascii_case("content-length")).then(|| value.trim())
    });

    match content_length.and_then(|v| v.parse::<usize>().ok()) {
        Some(n) => {
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
            body.truncate(n);
        }
        None => {
            tokio::time::timeout(JOIN_TIMEOUT, stream.read_to_end(&mut body))
                .await
                .map_err(|_| "read body timed out".to_string())?
                .map_err(io_err)?;
        }
    };
    Ok(String::from_utf8_lossy(&body).into_owned())
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
}
