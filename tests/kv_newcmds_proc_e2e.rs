//! Process-level e2e for the new Redis command surface (wave-2): spawn
//! the REAL `rdb` binary as a lone RAFT_BOOTSTRAP node, AUTH with the
//! harness token over raw RESP2, and drive every new command with EXACT
//! wire replies -- string counters, SETEX/TTL/GETDEL, the bit suite,
//! BITOP, HMSET/HGET, ZADD + ZREVRANGE WITHSCORES, SADD + SINTERCARD,
//! RPUSH + LMPOP, XADD + XREVRANGE (newest first), COMMAND
//! COUNT/GETKEYS, INFO, DBSIZE, SELECT and one MULTI/EXEC replay. Then
//! SIGKILL (kill -9) the process, respawn it on the SAME data dir and
//! prove the fsynced state survived: the counter is bit-for-bit intact,
//! the bit key unchanged, both stream entries still newest-first, the
//! LMPOP'd list stays popped, the GETDEL'd key stays gone, and a TTL
//! set BEFORE the kill is still positive after restart (absolute
//! deadlines). FLUSHDB over the wire then empties the keyspace.

mod common;

use std::time::Duration;

use common::lite::{frame, text};
use common::{contains_bytes, spawn_node, wait_resp_ready, TOKEN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Per-read socket timeout; loopback plus a debug-profile handler stays
/// far below this, so a timeout means something is genuinely stuck.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// `XREVRANGE st/c + -` frame: both entries, newest (2-1) first, each
/// entry a nested `[id, [field, value]]` array. Shared by warm-up and
/// post-restart.
const XREV_FRAME: &[u8] = b"*2\r\n*2\r\n$3\r\n2-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n\
                          *2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n";

/// The final value of the counter key `k` after the MULTI/EXEC replay
/// (GETSET 100 -> INCR -> APPEND "x"): what kill -9 must NOT lose.
const K_FINAL: &[u8] = b"$4\r\n101x\r\n";

// ---- tiny RESP2 client: full frames, not just single lines ----

/// Pull more bytes off the socket; false on EOF / IO error / timeout.
async fn fill(sock: &mut TcpStream, buf: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; 4096];
    let n = match tokio::time::timeout(REPLY_TIMEOUT, sock.read(&mut chunk)).await {
        Ok(Ok(n)) if n > 0 => n,
        _ => return false,
    };
    buf.extend_from_slice(&chunk[..n]);
    true
}

/// Pop one CRLF-terminated line (CRLF included); None on EOF/timeout.
async fn take_line(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    loop {
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            return Some(buf.drain(..pos + 2).collect());
        }
        if !fill(sock, buf).await {
            return None;
        }
    }
}

/// Pop exactly `n` bytes; None on EOF/timeout.
async fn take_n(sock: &mut TcpStream, buf: &mut Vec<u8>, n: usize) -> Option<Vec<u8>> {
    while buf.len() < n {
        if !fill(sock, buf).await {
            return None;
        }
    }
    Some(buf.drain(..n).collect())
}

/// Read ONE complete RESP frame of any shape: line replies keep their
/// CRLF, bulk payloads carry theirs, arrays expand their elements. An
/// explicit worklist replaces recursion (async fns cannot recurse).
/// Truncated replies keep a `<INCOMPLETE>` marker so exact-equality
/// assertion failures still print something diagnosable.
async fn read_frame(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Vec<u8> {
    // Element frames still owed: one outermost, one per open array.
    let mut owed: Vec<i64> = vec![1];
    let mut frame = Vec::new();
    loop {
        let Some(&n) = owed.last() else {
            return frame;
        };
        if n == 0 {
            owed.pop();
            continue;
        }
        if let Some(last) = owed.last_mut() {
            *last -= 1;
        }
        let Some(line) = take_line(sock, buf).await else {
            frame.extend_from_slice(b"<INCOMPLETE>");
            return frame;
        };
        frame.extend_from_slice(&line);
        // Header digits between the type byte and the trailing CRLF.
        let head = String::from_utf8_lossy(&line).into_owned();
        let digits = head
            .get(1..head.len().saturating_sub(2))
            .map(str::to_string);
        match (
            line.first().copied(),
            digits.and_then(|d| d.parse::<i64>().ok()),
        ) {
            // Bulk body: n payload bytes plus its CRLF.
            (Some(b'$'), Some(n)) if n >= 0 => match take_n(sock, buf, n as usize + 2).await {
                Some(body) => frame.extend_from_slice(&body),
                None => {
                    frame.extend_from_slice(b"<INCOMPLETE>");
                    return frame;
                }
            },
            // Array: n more element frames owed (null/empty arrays owe 0).
            (Some(b'*'), Some(n)) if n > 0 => owed.push(n),
            _ => {}
        }
    }
}

/// Connect and AUTH; the +OK is consumed and asserted so the caller's
/// first `ask` reads only its own reply.
async fn session(addr: &str) -> Option<(TcpStream, Vec<u8>)> {
    let mut sock = TcpStream::connect(addr).await.ok()?;
    sock.write_all(&frame(&[b"AUTH", TOKEN.as_bytes()]))
        .await
        .ok()?;
    let mut buf = Vec::new();
    let authed = read_frame(&mut sock, &mut buf).await;
    assert_eq!(authed, b"+OK\r\n", "AUTH on {addr}");
    Some((sock, buf))
}

/// Write one command, read exactly one complete frame back.
async fn ask(sock: &mut TcpStream, buf: &mut Vec<u8>, args: &[&[u8]]) -> Vec<u8> {
    if sock.write_all(&frame(args)).await.is_err() {
        return b"<WRITE-ERR>".to_vec();
    }
    read_frame(sock, buf).await
}

/// AUTH + one command on a fresh connection: the command's full frame.
async fn cmd(addr: &str, args: &[&[u8]]) -> Vec<u8> {
    match session(addr).await {
        Some((mut sock, mut buf)) => ask(&mut sock, &mut buf, args).await,
        None => b"<CONN-ERR>".to_vec(),
    }
}

/// Parse a `:N\r\n` integer reply (panics with the raw reply otherwise).
fn int_of(reply: &[u8]) -> i64 {
    let s = text(reply);
    s.strip_prefix(':')
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("not an integer reply: {s}"))
}

/// `keys=` of the INFO keyspace `db0:keys=N,expires=M` line.
fn info_keys(reply: &[u8]) -> i64 {
    let s = text(reply);
    let line = s
        .lines()
        .find(|l| l.starts_with("db0:keys="))
        .unwrap_or_else(|| panic!("no db0:keys= line in INFO: {s}"));
    line["db0:keys=".len()..]
        .split(',')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("bad keyspace line: {line}"))
}

#[tokio::test]
async fn new_commands_warmup_kill9_persistence_and_flushdb() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut node = spawn_node(dir.path(), 0, true, None);
    // 30s spawn window (a loaded CI box can take a while; the child-exit
    // fast-fail inside wait_resp_ready still catches startup crashes).
    wait_resp_ready(&mut node, 30).await;
    let a = node.resp.clone();

    // ---- warm-up matrix: every reply asserted exactly ----
    let warmup: &[(&[&[u8]], &[u8])] = &[
        // counter chain: each step's result feeds the next
        (&[b"set", b"k", b"10"], b"+OK\r\n"),
        (&[b"incr", b"k"], b":11\r\n"),
        (&[b"decr", b"k"], b":10\r\n"),
        (&[b"incrby", b"k", b"5"], b":15\r\n"),
        (&[b"incrbyfloat", b"k", b"0.5"], b"$4\r\n15.5\r\n"),
        (&[b"append", b"k", b"!"], b":5\r\n"),
        (&[b"strlen", b"k"], b":5\r\n"),
        (&[b"getset", b"k", b"100"], b"$5\r\n15.5!\r\n"),
        (&[b"setnx", b"nxk", b"v"], b":1\r\n"),
        (&[b"setnx", b"nxk", b"w"], b":0\r\n"),
        // one bit key through the whole bit suite
        (&[b"setbit", b"bits", b"0", b"1"], b":0\r\n"),
        (&[b"setbit", b"bits", b"3", b"1"], b":0\r\n"),
        (&[b"getbit", b"bits", b"0"], b":1\r\n"),
        (&[b"bitcount", b"bits"], b":2\r\n"),
        (&[b"bitpos", b"bits", b"1"], b":0\r\n"),
        // 'w' & 'o' = 'g': a printable AND, same slot via the {p} tag
        (&[b"set", b"{p}a", b"w"], b"+OK\r\n"),
        (&[b"set", b"{p}b", b"o"], b"+OK\r\n"),
        (&[b"bitop", b"and", b"{p}dst", b"{p}a", b"{p}b"], b":1\r\n"),
        (&[b"get", b"{p}dst"], b"$1\r\ng\r\n"),
        (&[b"hmset", b"h", b"f", b"v"], b"+OK\r\n"),
        (&[b"hget", b"h", b"f"], b"$1\r\nv\r\n"),
        (
            &[b"zadd", b"z", b"1", b"one", b"2", b"two", b"3", b"three"],
            b":3\r\n",
        ),
        (
            &[b"zrevrange", b"z", b"0", b"-1", b"withscores"],
            b"*6\r\n$5\r\nthree\r\n$1\r\n3\r\n$3\r\ntwo\r\n$1\r\n2\r\n\
              $3\r\none\r\n$1\r\n1\r\n",
        ),
        (&[b"sadd", b"{g}s1", b"a", b"b"], b":2\r\n"),
        (&[b"sadd", b"{g}s2", b"b", b"c"], b":2\r\n"),
        (&[b"sintercard", b"2", b"{g}s1", b"{g}s2"], b":1\r\n"),
        (&[b"rpush", b"lst", b"a", b"b", b"c"], b":3\r\n"),
        (
            &[b"lmpop", b"1", b"lst", b"LEFT", b"COUNT", b"2"],
            b"*2\r\n$3\r\nlst\r\n*2\r\n$1\r\na\r\n$1\r\nb\r\n",
        ),
        (&[b"xadd", b"st/c", b"1-1", b"f", b"v"], b"$3\r\n1-1\r\n"),
        (&[b"xadd", b"st/c", b"2-1", b"f", b"v"], b"$3\r\n2-1\r\n"),
        (&[b"xrevrange", b"st/c", b"+", b"-"], XREV_FRAME),
        (&[b"command", b"count"], b":188\r\n"),
        (
            &[b"command", b"getkeys", b"set", b"a", b"b"],
            b"*1\r\n$1\r\na\r\n",
        ),
        (&[b"select", b"0"], b"+OK\r\n"),
    ];
    for (args, want) in warmup {
        let got = cmd(&a, args).await;
        assert_eq!(
            got,
            *want,
            "warm-up {:?} (want {:?})\n{}",
            text(&frame(args)),
            text(want),
            node.ctx()
        );
    }

    // SETEX + TTL + GETDEL: the deadline lands in (0, 200], the value
    // comes back exactly once, then the key is gone.
    assert_eq!(cmd(&a, &[b"setex", b"k2", b"200", b"v"]).await, b"+OK\r\n");
    let ttl = int_of(&cmd(&a, &[b"ttl", b"k2"]).await);
    assert!(ttl > 0 && ttl <= 200, "ttl k2 in (0,200], got {ttl}");
    assert_eq!(cmd(&a, &[b"getdel", b"k2"]).await, b"$1\r\nv\r\n");
    assert_eq!(cmd(&a, &[b"exists", b"k2"]).await, b":0\r\n");

    // INFO carries the Server section; DBSIZE sees the warm-up keys.
    assert!(
        contains_bytes(&cmd(&a, &[b"info"]).await, b"redis_version:"),
        "INFO must contain redis_version:\n{}",
        node.ctx()
    );
    let dbsize_before = int_of(&cmd(&a, &[b"dbsize"]).await);
    assert!(dbsize_before > 0, "dbsize after warm-up");

    // ---- MULTI/EXEC on ONE connection (queue state is per-conn) ----
    let (mut sock, mut buf) = session(&a).await.expect("connect for MULTI");
    assert_eq!(ask(&mut sock, &mut buf, &[b"multi"]).await, b"+OK\r\n");
    assert_eq!(
        ask(&mut sock, &mut buf, &[b"incr", b"k"]).await,
        b"+QUEUED\r\n"
    );
    assert_eq!(
        ask(&mut sock, &mut buf, &[b"append", b"k", b"x"]).await,
        b"+QUEUED\r\n"
    );
    // pre-EXEC k = "100": INCR -> 101, APPEND "101x" -> len 4.
    assert_eq!(
        ask(&mut sock, &mut buf, &[b"exec"]).await,
        b"*2\r\n:101\r\n:4\r\n"
    );
    drop(sock);

    // The EXEC'd counter is the value kill -9 must not lose...
    assert_eq!(cmd(&a, &[b"get", b"k"]).await, K_FINAL);
    // ...and one more TTL'd key written BEFORE the kill.
    assert_eq!(
        cmd(&a, &[b"setex", b"ttlkeep", b"100", b"v"]).await,
        b"+OK\r\n"
    );

    // ---- kill -9, respawn on the same data dir ----
    node.kill_now();
    node.respawn();
    // 60s window: the restarted node re-opens RocksDB (WAL replay) plus
    // its raft log before binding RESP; generous on a loaded CI box.
    wait_resp_ready(&mut node, 60).await;

    // fsynced increments survive: exactly the pre-kill value.
    assert_eq!(
        cmd(&a, &[b"get", b"k"]).await,
        K_FINAL,
        "counter k\n{}",
        node.ctx()
    );
    // the bit key is unchanged
    assert_eq!(cmd(&a, &[b"getbit", b"bits", b"0"]).await, b":1\r\n");
    assert_eq!(cmd(&a, &[b"bitcount", b"bits"]).await, b":2\r\n");
    // both stream entries survive, newest-first order intact
    assert_eq!(
        cmd(&a, &[b"xrevrange", b"st/c", b"+", b"-"]).await,
        XREV_FRAME
    );
    // the LMPOP'd list stays popped (only "c" remains)
    assert_eq!(cmd(&a, &[b"llen", b"lst"]).await, b":1\r\n");
    // the GETDEL'd key stays gone
    assert_eq!(cmd(&a, &[b"exists", b"k2"]).await, b":0\r\n");
    // absolute deadlines: the TTL set before the kill is still positive
    let ttl_keep = int_of(&cmd(&a, &[b"ttl", b"ttlkeep"]).await);
    assert!(
        ttl_keep > 0 && ttl_keep <= 100,
        "ttl ttlkeep in (0,100] after restart, got {ttl_keep}\n{}",
        node.ctx()
    );

    // DBSIZE/INFO keyspace reflect the surviving keys: exactly one key
    // more than the pre-kill sample (only ttlkeep was written since).
    let dbsize_after = int_of(&cmd(&a, &[b"dbsize"]).await);
    assert_eq!(
        dbsize_after,
        dbsize_before + 1,
        "only ttlkeep was added after the dbsize sample\n{}",
        node.ctx()
    );
    let info = cmd(&a, &[b"info", b"keyspace"]).await;
    assert!(
        contains_bytes(&info, b"db0:keys="),
        "INFO keyspace after restart: {}",
        text(&info)
    );
    assert_eq!(info_keys(&info), dbsize_after, "INFO keys == DBSIZE");

    // ---- FLUSHDB over the wire, then an empty keyspace ----
    assert_eq!(
        cmd(&a, &[b"flushdb"]).await,
        b"+OK\r\n",
        "flushdb\n{}",
        node.ctx()
    );
    assert_eq!(
        cmd(&a, &[b"dbsize"]).await,
        b":0\r\n",
        "dbsize after flushdb"
    );
    assert_eq!(cmd(&a, &[b"get", b"k"]).await, b"$-1\r\n", "k must be gone");
}
