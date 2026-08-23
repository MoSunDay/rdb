//! Minimal outbound RESP client for server-to-server transport: MIGRATE's
//! DUMP/RESTORE hop and the `migrate task` orchestration. Plain functions
//! over a `TcpStream`; no connection pooling (one migration opens one).

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

/// One RESP reply frame from the peer.
#[derive(Debug, PartialEq)]
pub enum Reply {
    Simple(String),
    Error(String),
    Integer(i64),
    Bulk(Vec<u8>),
    Null,
    Array(Vec<Reply>),
}

/// Largest accepted single reply line (headers are tiny; guards a rogue
/// peer from pinning memory).
const MAX_LINE: usize = 64 * 1024;

/// Open a connection to `addr`, authenticate with the raft token and
/// expect `+OK`. `timeout_ms` bounds connect + auth (0 = no bound).
pub async fn connect_authed(addr: &str, token: &str, timeout_ms: u64) -> Result<TcpStream, String> {
    let fut = async {
        let mut stream = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connect {addr} failed: {e}"))?;
        send_command(&mut stream, &[b"AUTH", token.as_bytes()]).await?;
        match read_reply(&mut stream).await? {
            Reply::Simple(_) => Ok(stream),
            Reply::Error(e) => Err(format!("auth to {addr} failed: {e}")),
            other => Err(format!("auth to {addr}: unexpected reply {other:?}")),
        }
    };
    if timeout_ms == 0 {
        fut.await
    } else {
        timeout(Duration::from_millis(timeout_ms), fut)
            .await
            .map_err(|_| format!("timeout talking to {addr}"))?
    }
}

/// Frame one command as a RESP array and write it.
pub async fn send_command(stream: &mut TcpStream, args: &[&[u8]]) -> Result<(), String> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"*");
    buf.extend_from_slice(args.len().to_string().as_bytes());
    buf.extend_from_slice(b"\r\n");
    for a in args {
        buf.extend_from_slice(b"$");
        buf.extend_from_slice(a.len().to_string().as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    stream.write_all(&buf).await.map_err(|e| e.to_string())
}

/// Read one reply frame. `timeout_ms` bounds the whole read (0 = none).
pub async fn read_reply_timed(stream: &mut TcpStream, timeout_ms: u64) -> Result<Reply, String> {
    let fut = read_reply(stream);
    if timeout_ms == 0 {
        fut.await
    } else {
        timeout(Duration::from_millis(timeout_ms), fut)
            .await
            .map_err(|_| "timeout reading reply".to_string())?
    }
}

/// Read one reply frame (unbounded wait; callers on a deadline use
/// [`read_reply_timed`]).
pub async fn read_reply(stream: &mut TcpStream) -> Result<Reply, String> {
    let mut line = Vec::new();
    read_line(stream, &mut line).await?;
    match line.first() {
        Some(b'+') => Ok(Reply::Simple(
            String::from_utf8_lossy(&line[1..]).into_owned(),
        )),
        Some(b'-') => Ok(Reply::Error(
            String::from_utf8_lossy(&line[1..]).into_owned(),
        )),
        Some(b':') => {
            let n = std::str::from_utf8(&line[1..])
                .map_err(|_| "bad integer reply".to_string())?
                .trim()
                .parse()
                .map_err(|_| "bad integer reply".to_string())?;
            Ok(Reply::Integer(n))
        }
        Some(b'$') => {
            let n: i64 = parse_len(&line[1..])?;
            if n < 0 {
                return Ok(Reply::Null);
            }
            let mut data = vec![0u8; n as usize];
            stream
                .read_exact(&mut data)
                .await
                .map_err(|e| e.to_string())?;
            let mut crlf = [0u8; 2];
            stream
                .read_exact(&mut crlf)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Reply::Bulk(data))
        }
        Some(b'*') => {
            let n: i64 = parse_len(&line[1..])?;
            if n < 0 {
                return Ok(Reply::Null);
            }
            let mut items = Vec::with_capacity(n as usize);
            for _ in 0..n {
                items.push(Box::pin(read_reply(stream)).await?);
            }
            Ok(Reply::Array(items))
        }
        _ => Err("malformed reply".to_string()),
    }
}

fn parse_len(s: &[u8]) -> Result<i64, String> {
    std::str::from_utf8(s)
        .map_err(|_| "bad reply length".to_string())?
        .trim()
        .parse()
        .map_err(|_| "bad reply length".to_string())
}

/// Read one CRLF-terminated line (without the CRLF) into `buf`.
async fn read_line(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<(), String> {
    buf.clear();
    let mut byte = [0u8; 1];
    loop {
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|e| e.to_string())?;
        if byte[0] == b'\r' {
            let mut next = [0u8; 1];
            stream
                .read_exact(&mut next)
                .await
                .map_err(|e| e.to_string())?;
            if next[0] != b'\n' {
                return Err("malformed reply line".to_string());
            }
            return Ok(());
        }
        buf.push(byte[0]);
        if buf.len() > MAX_LINE {
            return Err("reply line too long".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Accept one connection, read the exact AUTH frame `send_command`
    /// produces, then reply with a canned frame from `reply`.
    async fn spawn_server(reply: Vec<u8>) -> (tokio::task::JoinHandle<()>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut frame = [0u8; 23]; // *2\r\n$4\r\nAUTH\r\n$3\r\ntok\r\n
            sock.read_exact(&mut frame).await.unwrap();
            assert_eq!(&frame[..], b"*2\r\n$4\r\nAUTH\r\n$3\r\ntok\r\n");
            sock.write_all(&reply).await.unwrap();
        });
        (handle, addr)
    }

    #[tokio::test]
    async fn auth_then_ping_roundtrip() {
        let (server, addr) = spawn_server(b"+OK\r\n+EMPTY\r\n".to_vec()).await;
        let mut stream = connect_authed(&addr, "tok", 5000).await.unwrap();
        send_command(&mut stream, &[b"PING"]).await.unwrap();
        assert_eq!(
            read_reply(&mut stream).await.unwrap(),
            Reply::Simple("EMPTY".into())
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn parses_array_bulk_integer_null() {
        // AUTH gets +OK, the follow-up PING gets a mixed array.
        let (server, addr) =
            spawn_server(b"+OK\r\n*3\r\n$2\r\nhi\r\n:-7\r\n$-1\r\n".to_vec()).await;
        let mut stream = connect_authed(&addr, "tok", 5000).await.unwrap();
        send_command(&mut stream, &[b"PING"]).await.unwrap();
        assert_eq!(
            read_reply(&mut stream).await.unwrap(),
            Reply::Array(vec![
                Reply::Bulk(b"hi".to_vec()),
                Reply::Integer(-7),
                Reply::Null,
            ])
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn auth_rejection_is_an_error() {
        let (server, addr) = spawn_server(b"-ERR invalid password\r\n".to_vec()).await;
        let err = connect_authed(&addr, "tok", 5000).await.unwrap_err();
        assert!(err.contains("auth to"));
        server.await.unwrap();
    }
}
