//! HTTP control API (Rust port of Go `internal/rcache/http.go`): `/get`,
//! `/join` and `/depart` served over a hand-rolled HTTP/1.1 listener.
//!
//! Byte-compat notes from the Go original:
//! - no method check: every HTTP method is accepted, known routes always
//!   answer status 200;
//! - `/get` reads the FSM live (Go `CM.Get`);
//! - `/join` and `/depart` with a wrong token log "join cluster failed"
//!   but still respond "ok" (a bug in the Go original, kept on purpose);
//! - unknown paths get Go `http.ServeMux`'s plain 404 page.

use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::rcache::fsm::KvMap;
use crate::rcache::transport;
use crate::rcache::{NodeId, RdbRaft};

/// Go `http.DefaultMaxHeaderBytes`.
const MAX_HEAD_BYTES: usize = 1 << 20;
/// Read deadline for one request head; the Go server has none, but a
/// stalled client must not pin a task forever.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Bind `addr` and serve the control API forever (Go `http.Serve`).
pub async fn serve(addr: &str, raft: Arc<RdbRaft>, kv: KvMap, token: String) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    serve_on(listener, raft, kv, token).await
}

/// Serve on an already bound listener (tests use ephemeral ports).
pub async fn serve_on(
    listener: TcpListener,
    raft: Arc<RdbRaft>,
    kv: KvMap,
    token: String,
) -> io::Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (raft, kv, token) = (raft.clone(), kv.clone(), token.clone());
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, &raft, &kv, &token).await {
                        if e.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("rcache http: connection failed: {e}");
                        }
                    }
                });
            }
            Err(e) => eprintln!("rcache http: accept failed: {e}"),
        }
    }
}

/// One request: parse head, route, answer, close (Connection: close).
async fn handle_conn(
    mut stream: TcpStream,
    raft: &RdbRaft,
    kv: &KvMap,
    token: &str,
) -> io::Result<()> {
    let (head, _rest) = read_head(&mut stream).await?;
    let Some(target) = request_target(&head) else {
        return write_response(&mut stream, "400 Bad Request", "").await;
    };
    let (path, query) = split_target(target);
    let params = parse_query(query);
    let (status, body) = match path {
        "/get" => ("200 OK", do_get(kv, token, &params)),
        "/join" => ("200 OK", do_join(raft, token, &params).await),
        "/depart" => ("200 OK", do_depart(raft, token, &params).await),
        // Go http.ServeMux plain 404.
        _ => ("404 Not Found", "404 page not found\n".to_string()),
    };
    write_response(&mut stream, status, &body).await
}

/// Read the request head up to (and including) `\r\n\r\n`; returns the head
/// plus any bytes already consumed past it (start of a pipelined body).
pub(crate) async fn read_head(stream: &mut TcpStream) -> io::Result<(String, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::new();
    let mut scanned = 0usize;
    loop {
        let mut tmp = [0u8; 512];
        let read = tokio::time::timeout(HEAD_TIMEOUT, stream.read(&mut tmp)).await;
        let n = read.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "head timeout"))??;
        if n == 0 {
            let err = io::Error::new(io::ErrorKind::UnexpectedEof, "head closed early");
            return Err(err);
        }
        buf.extend_from_slice(&tmp[..n]);
        let start = scanned.saturating_sub(3);
        if let Some(off) = buf[start..].windows(4).position(|w| w == b"\r\n\r\n") {
            let head_end = start + off + 4;
            let rest = buf.split_off(head_end);
            return String::from_utf8(buf)
                .map(|h| (h, rest))
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "head not UTF-8"));
        }
        scanned = buf.len();
        if buf.len() > MAX_HEAD_BYTES {
            let err = io::Error::new(io::ErrorKind::InvalidData, "head too large");
            return Err(err);
        }
    }
}

/// Second whitespace-delimited word of the request line (the target).
fn request_target(head: &str) -> Option<&str> {
    head.split("\r\n").next()?.split_whitespace().nth(1)
}

/// Split the target into path and query; fragments are dropped.
fn split_target(target: &str) -> (&str, &str) {
    let target = target.split('#').next().unwrap_or(target);
    match target.find('?') {
        Some(i) => (&target[..i], &target[i + 1..]),
        None => (target, ""),
    }
}

/// Go `url.ParseQuery` semantics (Go 1.17: '&' is the only separator):
/// split each pair on the first '=', unescape both sides, skip pairs with
/// an invalid escape, keep duplicate keys in order (`Get` returns the
/// first one).
fn parse_query(query: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for part in query.split('&') {
        if part.is_empty() {
            continue;
        }
        let (name, value) = match part.find('=') {
            Some(i) => (&part[..i], &part[i + 1..]),
            None => (part, ""),
        };
        let (Ok(key), Ok(val)) = (query_unescape(name), query_unescape(value)) else {
            continue;
        };
        pairs.push((key, val));
    }
    pairs
}

/// Go `url.QueryUnescape`: '+' is a space, `%XX` a hex byte, the result
/// must be valid UTF-8.
fn query_unescape(s: &str) -> Result<String, ()> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(());
                }
                let hi = hex_digit(bytes[i + 1]).ok_or(())?;
                let lo = hex_digit(bytes[i + 2]).ok_or(())?;
                out.push(hi * 16 + lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(drop)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Go `url.Values.Get`: first value of the key, "" when absent.
fn first_param<'a>(params: &'a [(String, String)], key: &str) -> &'a str {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

/// Go `doGet`: empty key -> zero-byte body; token mismatch -> "\n";
/// otherwise the live FSM value + "\n" (both-empty tokens pass).
fn do_get(kv: &KvMap, token: &str, params: &[(String, String)]) -> String {
    let key = first_param(params, "key");
    if key.is_empty() {
        eprintln!("doGet() error, get nil key");
        return String::new();
    }
    let mut ret = String::new();
    if first_param(params, "raft-token") == token {
        ret = kv.read().unwrap().get(key).cloned().unwrap_or_default();
    }
    format!("{ret}\n")
}

/// Go `doJoin`. Wrong token is a logged no-op that still answers "ok"
/// (bug parity with the Go original, see module docs).
async fn do_join(raft: &RdbRaft, token: &str, params: &[(String, String)]) -> String {
    let peer = first_param(params, "peerAddress");
    if peer.is_empty() {
        eprintln!("invalid PeerAddress");
        return "invalid peerAddress\n".to_string();
    }
    if first_param(params, "raft-token") != token {
        eprintln!("join cluster failed");
        return "ok".to_string();
    }
    match add_voter(raft, peer).await {
        Ok(()) => "ok".to_string(),
        Err(e) => {
            eprintln!("Error joining peer to raft, peeraddress:{peer}, err:{e}, code:500");
            "internal error\n".to_string()
        }
    }
}

/// Go `doDepart`; same skeleton (and same wrong-token bug) as `doJoin`.
async fn do_depart(raft: &RdbRaft, token: &str, params: &[(String, String)]) -> String {
    let peer = first_param(params, "peerAddress");
    if peer.is_empty() {
        eprintln!("invalid PeerAddress");
        return "invalid peerAddress\n".to_string();
    }
    if first_param(params, "raft-token") != token {
        eprintln!("join cluster failed");
        return "ok".to_string();
    }
    match remove_voter(raft, peer).await {
        Ok(()) => "ok".to_string(),
        Err(e) => {
            eprintln!("Error depart peer to raft, peeraddress:{peer}, err:{e}, code:500");
            "internal error\n".to_string()
        }
    }
}

/// Go `AddVoter(id, addr, 0, 0)`: openraft needs the learner first
/// (blocking until caught up), then a membership change to the current
/// voters plus the new id.
async fn add_voter(raft: &RdbRaft, peer: &str) -> Result<(), String> {
    let id = transport::node_id_of(peer);
    raft.add_learner(id, peer.to_string(), true)
        .await
        .map_err(|e| e.to_string())?;
    let mut members: BTreeSet<NodeId> = raft
        .metrics()
        .borrow()
        .membership_config
        .voter_ids()
        .collect();
    members.insert(id);
    raft.change_membership(members, true)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Go `RemoveServer(id, 0, 0)`: voters minus the peer. Removing an id
/// that is not a voter is an error, like hashicorp's RemoveServer.
async fn remove_voter(raft: &RdbRaft, peer: &str) -> Result<(), String> {
    let id = transport::node_id_of(peer);
    let voters: BTreeSet<NodeId> = raft
        .metrics()
        .borrow()
        .membership_config
        .voter_ids()
        .collect();
    if !voters.contains(&id) {
        return Err(format!("peer {peer} not found in configuration"));
    }
    let members: BTreeSet<NodeId> = voters.into_iter().filter(|v| *v != id).collect();
    raft.change_membership(members, true)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Go `http.ResponseWriter` defaults: text/plain, explicit Content-Length,
/// Connection: close (one request per connection).
async fn write_response(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::RwLock;

    fn kv_with(pairs: &[(&str, &str)]) -> KvMap {
        Arc::new(RwLock::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        ))
    }

    fn param(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn get_body_semantics() {
        let kv = kv_with(&[("k", "v")]);
        // empty key -> exactly zero bytes (no newline)
        assert_eq!(do_get(&kv, "tok", &[]), "");
        assert_eq!(do_get(&kv, "tok", &param(&[("key", "")])), "");
        // wrong token -> exactly "\n"
        assert_eq!(
            do_get(&kv, "tok", &param(&[("key", "k"), ("raft-token", "bad")])),
            "\n"
        );
        // token ok, key present -> value + "\n"
        assert_eq!(
            do_get(&kv, "tok", &param(&[("key", "k"), ("raft-token", "tok")])),
            "v\n"
        );
        // token ok, key missing -> "\n"
        assert_eq!(
            do_get(
                &kv,
                "tok",
                &param(&[("key", "nope"), ("raft-token", "tok")])
            ),
            "\n"
        );
        // both-empty tokens pass (Go "" == "")
        assert_eq!(
            do_get(&kv_with(&[("k", "v")]), "", &param(&[("key", "k")])),
            "v\n"
        );
    }

    #[test]
    fn query_parse_go_semantics() {
        // first value wins (url.Values.Get)
        let p = parse_query("key=a&key=b");
        assert_eq!(first_param(&p, "key"), "a");
        // %XX hex and '+' as space
        let p = parse_query("peerAddress=127.0.0.1%3A22681&x=a+b");
        assert_eq!(first_param(&p, "peerAddress"), "127.0.0.1:22681");
        assert_eq!(first_param(&p, "x"), "a b");
        // invalid escape -> pair skipped, rest survives
        let p = parse_query("bad=%zz&ok=1&trail=%2");
        assert_eq!(first_param(&p, "bad"), "");
        assert_eq!(first_param(&p, "ok"), "1");
        assert_eq!(first_param(&p, "trail"), "");
        // key without '=' -> value ""
        let p = parse_query("flag&k=v");
        assert_eq!(first_param(&p, "flag"), "");
        assert_eq!(first_param(&p, "k"), "v");
    }

    #[test]
    fn split_target_path_and_query() {
        assert_eq!(split_target("/get?key=k"), ("/get", "key=k"));
        assert_eq!(split_target("/join"), ("/join", ""));
        assert_eq!(split_target("/get?k=v#frag"), ("/get", "k=v"));
    }

    #[test]
    fn request_target_from_head() {
        let head = "GET /get?key=k HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(request_target(head), Some("/get?key=k"));
        assert_eq!(request_target("GARBAGE"), None);
    }
}
