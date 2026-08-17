//! Minimal RESP2 client-side codec: command encoder + reply parser.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;

/// Append one RESP array-of-bulk-strings command frame to `buf`.
pub fn encode_command(buf: &mut Vec<u8>, args: &[&[u8]]) {
    buf.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        buf.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        buf.extend_from_slice(arg);
        buf.extend_from_slice(b"\r\n");
    }
}

/// A server reply, reduced to what the bench needs: error replies keep
/// their text (e.g. `MOVED 1234 127.0.0.1:1`); everything else is fine,
/// including nil bulk replies (`$-1`) for GET on missing keys.
#[derive(PartialEq, Eq, Debug)]
pub enum Reply {
    Ok,
    Error(String),
}

/// Outcome of parsing one reply from a byte buffer.
#[derive(PartialEq, Eq, Debug)]
enum Parsed {
    /// A full reply and how many bytes it consumed.
    Complete(Reply, usize),
    /// Not enough bytes yet.
    Incomplete,
    /// Malformed frame.
    Bad(String),
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

fn parse_int(body: &[u8]) -> Option<i64> {
    std::str::from_utf8(body).ok()?.parse().ok()
}

/// Parse one top-level RESP2 reply frame from `buf` (pure). Arrays are
/// walked recursively but only their overall length matters here.
fn parse_reply(buf: &[u8]) -> Parsed {
    let Some(&first) = buf.first() else {
        return Parsed::Incomplete;
    };
    let Some(header) = find_crlf(buf) else {
        return Parsed::Incomplete;
    };
    let body = &buf[1..header];
    match first {
        b'+' | b':' => Parsed::Complete(Reply::Ok, header + 2),
        b'-' => Parsed::Complete(
            Reply::Error(String::from_utf8_lossy(body).into_owned()),
            header + 2,
        ),
        b'$' => {
            let Some(len) = parse_int(body) else {
                return Parsed::Bad("bad bulk length".to_string());
            };
            if len < 0 {
                return Parsed::Complete(Reply::Ok, header + 2); // $-1 nil bulk
            }
            let end = header + 2 + len as usize + 2;
            if buf.len() < end {
                Parsed::Incomplete
            } else {
                Parsed::Complete(Reply::Ok, end)
            }
        }
        b'*' => {
            let Some(count) = parse_int(body) else {
                return Parsed::Bad("bad array length".to_string());
            };
            if count < 0 {
                return Parsed::Complete(Reply::Ok, header + 2); // *-1
            }
            let mut offset = header + 2;
            for _ in 0..count {
                match parse_reply(&buf[offset..]) {
                    Parsed::Complete(_, used) => offset += used,
                    Parsed::Incomplete => return Parsed::Incomplete,
                    Parsed::Bad(msg) => return Parsed::Bad(msg),
                }
            }
            Parsed::Complete(Reply::Ok, offset)
        }
        other => Parsed::Bad(format!("unexpected reply type byte '{}'", other as char)),
    }
}

/// Read exactly one full reply from `rd`; `buf` carries leftover bytes of
/// previously read TCP segments across calls.
pub async fn read_reply(rd: &mut OwnedReadHalf, buf: &mut Vec<u8>) -> Result<Reply, String> {
    let mut chunk = [0u8; 4096];
    loop {
        match parse_reply(buf) {
            Parsed::Complete(reply, used) => {
                buf.drain(..used);
                return Ok(reply);
            }
            Parsed::Bad(msg) => return Err(format!("protocol error: {msg}")),
            Parsed::Incomplete => {}
        }
        let n = rd
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("connection closed by server".to_string());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Send one command frame and read its single reply (handshake helper).
pub async fn roundtrip(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    rd: &mut OwnedReadHalf,
    inbox: &mut Vec<u8>,
    args: &[&[u8]],
) -> Result<Reply, String> {
    let mut out = Vec::with_capacity(128);
    encode_command(&mut out, args);
    wr.write_all(&out)
        .await
        .map_err(|e| format!("write: {e}"))?;
    read_reply(rd, inbox).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_multibulk_frames() {
        let mut buf = Vec::new();
        encode_command(&mut buf, &[b"SET", b"k", b"v"]);
        assert_eq!(buf, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n".as_slice());
    }

    #[test]
    fn parses_all_reply_kinds_and_error_text() {
        let buf = b"+OK\r\n-ERR: NOAUTH\r\n:7\r\n$-1\r\n$5\r\nhello\r\n".as_slice();
        let mut offset = 0;
        let kinds = [
            Reply::Ok,
            Reply::Error("ERR: NOAUTH".to_string()),
            Reply::Ok,
            Reply::Ok,
            Reply::Ok,
        ];
        for want in kinds {
            let Parsed::Complete(got, used) = parse_reply(&buf[offset..]) else {
                panic!("expected complete reply at offset {offset}");
            };
            assert_eq!(got, want);
            offset += used;
        }
        assert_eq!(offset, buf.len());
        assert_eq!(parse_reply(b"$5\r\nhel"), Parsed::Incomplete);
        assert_eq!(parse_reply(b""), Parsed::Incomplete);
        assert!(matches!(parse_reply(b"x\r\n"), Parsed::Bad(_)));
    }

    #[test]
    fn parses_array_replies_inclusively() {
        // *2\r\n$1\r\na\r\n$1\r\nb\r\n plus a trailing partial frame
        let buf = b"*2\r\n$1\r\na\r\n$1\r\nb\r\n+OK\r\n".as_slice();
        match parse_reply(buf) {
            Parsed::Complete(Reply::Ok, used) => assert_eq!(used, buf.len() - 5),
            other => panic!("expected complete array, got {other:?}"),
        }
    }
}
