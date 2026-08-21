//! HTTP control API (Rust port of Go `internal/rcache/http.go`): `/get`,
//! `/join` and `/depart` served over a hand-rolled HTTP/1.1 listener.
//!
//! Byte-compat notes from the Go original:
//! - no method check: every HTTP method is accepted, known routes answer
//!   status 200 (except the 401 below);
//! - `/get` reads the FSM live (Go `CM.Get`);
//! - `/join` and `/depart` are serialized per server by a membership
//!   mutex covering the full add_learner/read-voters/change_membership
//!   sequence (deviation from Go, where concurrent requests race; see
//!   [`MembershipMux`]);
//! - `/join` and `/depart` with a wrong token respond `401
//!   unauthorized` (deviation from the Go original, which logged "join
//!   cluster failed" but still answered "ok" -- silent fake success);
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

/// Serializes membership mutations on ONE server instance: the whole
/// `add_learner` -> read voters -> `change_membership` (or read voters ->
/// `change_membership`) sequence runs under it. Without the lock, two
/// concurrent `/join`s (or a `/join` racing a `/depart`) are a
/// check-then-act on the voter snapshot: one update is lost, and openraft
/// rejects the overlapping `change_membership` with `internal error`,
/// which kills the losing joiner process.
pub type MembershipMux = Arc<tokio::sync::Mutex<()>>;

/// A fresh per-server membership mux.
pub fn membership_mux() -> MembershipMux {
    Arc::new(tokio::sync::Mutex::new(()))
}

/// Go `http.DefaultMaxHeaderBytes`.
const MAX_HEAD_BYTES: usize = 1 << 20;

/// M3: late-bound handle to the data store behind `/sql2pc/status`.
/// The control API binds (and starts answering) before the normal
/// listener's store opens, so the slot starts empty and main fills it
/// once the store is up; an empty slot keeps the pre-M3 route set (the
/// route answers the plain 404).
pub type StoreSlot = Arc<std::sync::RwLock<Option<Arc<crate::store::Store>>>>;

/// A fresh empty store slot.
pub fn store_slot() -> StoreSlot {
    Arc::new(std::sync::RwLock::new(None))
}
/// Read deadline for one request head; the Go server has none, but a
/// stalled client must not pin a task forever.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound for one mux-held membership mutation (`/join` or
/// `/depart`): add_learner with blocking=true waits for the learner to
/// catch up, so an unreachable peer would hold the mux forever and wedge
/// every control-plane op on this server. No Go/config counterpart
/// exists; 30s is a generous bound for a healthy catch-up.
const MEMBERSHIP_TIMEOUT: Duration = Duration::from_secs(30);

/// Bind `addr` and serve the control API forever (Go `http.Serve`).
pub async fn serve(
    addr: &str,
    raft: Arc<RdbRaft>,
    kv: KvMap,
    token: String,
    mux: MembershipMux,
    ts: Option<Arc<crate::sql::tx::ClusterTs>>,
    store: StoreSlot,
) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    serve_on(listener, raft, kv, token, mux, ts, store).await
}

/// Serve on an already bound listener (tests use ephemeral ports).
/// `ts` is the M3 cluster timestamp core serving `/sql/ts` (None keeps
/// the pre-M3 route set: the route answers the plain 404); `store` is
/// the M3 slot backing `/sql2pc/status` (same None semantics).
pub async fn serve_on(
    listener: TcpListener,
    raft: Arc<RdbRaft>,
    kv: KvMap,
    token: String,
    mux: MembershipMux,
    ts: Option<Arc<crate::sql::tx::ClusterTs>>,
    store: StoreSlot,
) -> io::Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (raft, kv, token, mux, ts, store) = (
                    raft.clone(),
                    kv.clone(),
                    token.clone(),
                    mux.clone(),
                    ts.clone(),
                    store.clone(),
                );
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, &raft, &kv, &token, &mux, &ts, &store).await
                    {
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
    mux: &MembershipMux,
    ts: &Option<Arc<crate::sql::tx::ClusterTs>>,
    store: &StoreSlot,
) -> io::Result<()> {
    let (head, _rest) = read_head(&mut stream).await?;
    let Some(target) = request_target(&head) else {
        return write_response(&mut stream, "400 Bad Request", "").await;
    };
    let (path, query) = split_target(target);
    let params = parse_query(query);
    // Only the membership-mutating routes touch `mux`; `/get` and 404s
    // stay lock-free. `/join` and `/depart` pick their own status (401 on
    // a wrong token), the rest is always 200.
    let (status, body) = match path {
        "/get" => ("200 OK", do_get(kv, token, &params)),
        "/join" => do_join(raft, mux, token, &params).await,
        "/depart" => do_depart(raft, mux, token, &params).await,
        // M3: timestamp block leases (leader-only inside the handler).
        "/sql/ts" => crate::sql::tx::global::route_sql_ts(ts.as_ref(), token, &params).await,
        // M3: follower bind registration forwarded to the leader.
        "/sql/nodes" => crate::sql::tx::nodes::route_register(ts.as_ref(), token, &params).await,
        // M3: 2PC outcome inquiry (recovery + coordinator retry).
        "/sql2pc/status" => {
            crate::sql::dist::recover::route_status(store.read().unwrap().as_ref(), token, &params)
        }
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
pub(crate) fn first_param<'a>(params: &'a [(String, String)], key: &str) -> &'a str {
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

/// Response pair (status line + body) shared by the membership-mutating
/// routes so a wrong token can answer 401 instead of the Go original's
/// fake "ok".
type RouteResponse = (&'static str, String);

/// Go `doJoin`, with two deviations from the Go bug set: a wrong token
/// answers `401 unauthorized` instead of a fake "ok", and the mux-held
/// section is bounded by [`MEMBERSHIP_TIMEOUT`].
async fn do_join(
    raft: &RdbRaft,
    mux: &MembershipMux,
    token: &str,
    params: &[(String, String)],
) -> RouteResponse {
    let peer = first_param(params, "peerAddress");
    if peer.is_empty() {
        eprintln!("invalid PeerAddress");
        return ("200 OK", "invalid peerAddress\n".to_string());
    }
    if first_param(params, "raft-token") != token {
        eprintln!("join cluster failed");
        return ("401 Unauthorized", "unauthorized\n".to_string());
    }
    // Hold the membership mux across the WHOLE add_learner -> read
    // voters -> change_membership sequence (not just change_membership):
    // the voter snapshot must not change under the caller. add_learner
    // blocks until the learner catches up, so an unreachable peer would
    // hold the mux forever; the timeout releases it and answers the same
    // internal-error body as a failed membership change.
    let joined = tokio::time::timeout(MEMBERSHIP_TIMEOUT, async {
        let _membership = mux.lock().await;
        add_voter(raft, peer).await
    })
    .await;
    match joined {
        Ok(Ok(())) => ("200 OK", "ok".to_string()),
        Ok(Err(e)) => {
            eprintln!("Error joining peer to raft, peeraddress:{peer}, err:{e}, code:500");
            ("200 OK", "internal error\n".to_string())
        }
        Err(_) => {
            eprintln!(
                "Error joining peer to raft, peeraddress:{peer}, \
                 err:membership change timed out after {}s, code:500",
                MEMBERSHIP_TIMEOUT.as_secs()
            );
            ("200 OK", "internal error\n".to_string())
        }
    }
}

/// Go `doDepart`; same skeleton as `doJoin` (same 401-on-wrong-token and
/// mux timeout deviations).
async fn do_depart(
    raft: &RdbRaft,
    mux: &MembershipMux,
    token: &str,
    params: &[(String, String)],
) -> RouteResponse {
    let peer = first_param(params, "peerAddress");
    if peer.is_empty() {
        eprintln!("invalid PeerAddress");
        return ("200 OK", "invalid peerAddress\n".to_string());
    }
    if first_param(params, "raft-token") != token {
        eprintln!("join cluster failed");
        return ("401 Unauthorized", "unauthorized\n".to_string());
    }
    // Same serialization as `/join`: a departing peer must not be
    // computed from a voter snapshot a concurrent join is replacing.
    // change_membership can also stall (e.g. a lost leader), so this
    // mux-held section is bounded just like `/join`'s.
    let departed = tokio::time::timeout(MEMBERSHIP_TIMEOUT, async {
        let _membership = mux.lock().await;
        remove_voter(raft, peer).await
    })
    .await;
    match departed {
        Ok(Ok(())) => ("200 OK", "ok".to_string()),
        Ok(Err(e)) => {
            eprintln!("Error depart peer to raft, peeraddress:{peer}, err:{e}, code:500");
            ("200 OK", "internal error\n".to_string())
        }
        Err(_) => {
            eprintln!(
                "Error depart peer to raft, peeraddress:{peer}, \
                 err:membership change timed out after {}s, code:500",
                MEMBERSHIP_TIMEOUT.as_secs()
            );
            ("200 OK", "internal error\n".to_string())
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
    // retain=false: the removed voter must leave the cluster entirely;
    // openraft's retain=true would keep it as a learner forever, so a
    // departed node could never really rejoin.
    raft.change_membership(members, false)
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
