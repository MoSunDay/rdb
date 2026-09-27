//! Two process-level scenarios against the REAL binary:
//! (a) the ASKING gate of `CLUSTER SETSLOT <slot> IMPORTING <src-id>` on
//!     a 3-node cluster: a non-owner node serving an importing slot
//!     redirects `-MOVED <slot> <src-resp-addr>` to everyone, but serves
//!     exactly ONE routed command per ASKING on the same connection.
//! (b) the `RESTORE` success paths over the wire (only error paths were
//!     covered before): payloads built by the real `dump_key` from an
//!     in-process source store, re-rooting of typed families onto the
//!     target key, BUSYKEY/REPLACE, absolute-TTL, ABSTTL no-op and the
//!     arity/payload/TTL error texts.

mod common;

use std::time::{Duration, Instant};

use common::lite::cmd_full_reply;
use common::{
    all_ctx, cmd_one_shot, contains_bytes, spawn_node, start_cluster, wait_resp_ready, TOKEN,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Per-reply socket timeout for the persistent connection.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Encode one RESP array command frame.
fn encode_cmd(args: &[&[u8]]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// Read ONE reply line (CRLF excluded) from the shared buffer.
async fn read_line(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + REPLY_TIMEOUT;
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            let line: Vec<u8> = buf.drain(..pos).collect();
            buf.drain(..2);
            return line;
        }
        match tokio::time::timeout_at(deadline, sock.read(&mut chunk)).await {
            Ok(Ok(0)) => return b"<EOF>".to_vec(),
            Ok(Ok(k)) => buf.extend_from_slice(&chunk[..k]),
            Ok(Err(_)) => return b"<IO-ERR>".to_vec(),
            Err(_) => return b"<TIMEOUT>".to_vec(),
        }
    }
}

/// Drain `n` bytes (best effort on timeout/EOF) from the shared buffer.
async fn read_n(sock: &mut TcpStream, buf: &mut Vec<u8>, n: usize) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + REPLY_TIMEOUT;
    let mut chunk = [0u8; 1024];
    while buf.len() < n {
        match tokio::time::timeout_at(deadline, sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(k)) => buf.extend_from_slice(&chunk[..k]),
        }
    }
    buf.drain(..buf.len().min(n)).collect()
}

/// One full reply: simple/error/int line, or `$N\r\n<payload>` bulk
/// (trailing CRLF stripped) -- same shapes `cmd_one_shot` returns.
async fn read_reply(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Vec<u8> {
    let line = read_line(sock, buf).await;
    let n = if line.starts_with(b"$") && line != b"$-1" {
        std::str::from_utf8(&line[1..])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
    } else {
        None
    };
    match n {
        Some(n) => {
            let mut reply = line.clone();
            reply.extend_from_slice(b"\r\n");
            reply.extend_from_slice(&read_n(sock, buf, n).await);
            let _ = read_n(sock, buf, 2).await;
            reply
        }
        None => line,
    }
}

/// One AUTHed persistent connection (ASKING is per-connection state).
struct Wire {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl Wire {
    async fn connect(addr: &str, token: &str) -> Wire {
        let mut w = Wire {
            sock: TcpStream::connect(addr).await.expect("connect"),
            buf: Vec::new(),
        };
        assert_eq!(w.rpc(&[b"AUTH", token.as_bytes()]).await, b"+OK");
        w
    }

    async fn rpc(&mut self, args: &[&[u8]]) -> Vec<u8> {
        self.sock.write_all(&encode_cmd(args)).await.expect("write");
        read_reply(&mut self.sock, &mut self.buf).await
    }
}

/// `CLUSTER KEYSLOT <key>` -> u16 (cmd_one_shot strips the CRLF).
async fn keyslot(nodes: &[common::ProcNode], leader: usize, key: &[u8]) -> u16 {
    let r = cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"cluster", b"keyslot", key]).await;
    String::from_utf8_lossy(&r[1..])
        .parse()
        .unwrap_or_else(|_| panic!("keyslot not numeric: {r:?}"))
}

/// Equal-split owner index for `slot` on a 3-node cluster.
fn band_owner(slot: u16) -> usize {
    if slot <= 5461 {
        0
    } else if slot <= 10922 {
        1
    } else {
        2
    }
}

/// Retry a command until it replies exactly `want` (fresh clusters take
/// a few seconds to settle routing after CLUSTER INIT).
async fn retry_reply(nodes: &[common::ProcNode], idx: usize, args: &[&[u8]], want: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = Vec::new();
    while Instant::now() < deadline {
        last = cmd_one_shot(&nodes[idx].resp, TOKEN, args).await;
        if last == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    panic!(
        "never {want:?} on n{idx}; last={last:?}\n{}",
        all_ctx(nodes)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn importing_slot_asking_gate_on_the_wire() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (nodes, leader) = start_cluster(dir.path(), 3).await;

    let key = b"{ask}k1";
    let slot = keyslot(&nodes, leader, key).await;
    let src = band_owner(slot);
    let dst = (src + 1) % 3;
    let moved = format!("-MOVED {slot} {}", nodes[src].resp);
    eprintln!("slot {slot} src=n{src} dst=n{dst}");

    // Pre-state: the non-owner already MOVEDs by band ownership.
    retry_reply(&nodes, dst, &[b"set", key, b"v0"], moved.as_bytes()).await;

    // IMPORTING takes the SOURCE node id = md5-with-40 of its resp addr.
    let src_id = rdb::utils::md5_with40(&nodes[src].resp);
    let r = cmd_one_shot(
        &nodes[dst].resp,
        TOKEN,
        &[
            b"cluster",
            b"setslot",
            slot.to_string().as_bytes(),
            b"IMPORTING",
            src_id.as_bytes(),
        ],
    )
    .await;
    assert_eq!(r, b"+OK", "setslot importing\n{}", all_ctx(&nodes));

    // Fresh connection, no ASKING: still -MOVED to the exact source addr.
    let r = cmd_one_shot(&nodes[dst].resp, TOKEN, &[b"set", key, b"v1"]).await;
    assert_eq!(r, moved.as_bytes(), "gate must MOVED\n{}", all_ctx(&nodes));

    // Same connection: ASKING arms exactly ONE routed command.
    let mut w = Wire::connect(&nodes[dst].resp, TOKEN).await;
    assert_eq!(w.rpc(&[b"ASKING"]).await, b"+OK");
    assert_eq!(w.rpc(&[b"set", key, b"v1"]).await, b"+OK");
    assert_eq!(
        w.rpc(&[b"set", key, b"v2"]).await,
        moved.as_bytes(),
        "flag must be consumed by the first routed command"
    );
    // The ASKING write really landed on the importer's local store.
    assert_eq!(w.rpc(&[b"ASKING"]).await, b"+OK");
    assert_eq!(w.rpc(&[b"get", key]).await, b"$2\r\nv1");

    // STABLE clears the gate: ownership routing MOVEDs again, and ASKING
    // no longer changes anything (it only ever bypassed the gate).
    let r = cmd_one_shot(
        &nodes[dst].resp,
        TOKEN,
        &[
            b"cluster",
            b"setslot",
            slot.to_string().as_bytes(),
            b"STABLE",
        ],
    )
    .await;
    assert_eq!(r, b"+OK", "setslot stable\n{}", all_ctx(&nodes));
    assert_eq!(w.rpc(&[b"set", key, b"v3"]).await, moved.as_bytes());
    assert_eq!(w.rpc(&[b"ASKING"]).await, b"+OK");
    assert_eq!(w.rpc(&[b"set", key, b"v3"]).await, moved.as_bytes());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_wire_success_paths() {
    // In-process source store (own tag): one raw string, one hash and a
    // second string for the REPLACE-overwrite proof; dump payloads are
    // built by the real dump_key. lite::call block_on's its own runtime,
    // so the seeding runs on a blocking thread.
    let (s1, s2, h) = tokio::task::spawn_blocking(|| {
        let (shared, _dir) = common::lite::shared_at("45901");
        assert_eq!(
            common::lite::call(&shared, "set", &[b"srcs1", b"val1"]),
            b"+OK\r\n"
        );
        assert_eq!(
            common::lite::call(&shared, "set", &[b"srcs2", b"val2"]),
            b"+OK\r\n"
        );
        assert_eq!(
            common::lite::call(&shared, "hset", &[b"srch", b"f1", b"hv1", b"f2", b"hv2"]),
            b":2\r\n"
        );
        let dump = |k: &[u8]| {
            let (_, prefix) = rdb::hash::slot_with_prefix(rdb::hash::hash_tag(k));
            rdb::ds::dump::dump_key(&shared.store, &prefix, k, rdb::ds::expire::now_ms())
                .unwrap_or_else(|| panic!("dump {k:?}"))
        };
        (dump(b"srcs1"), dump(b"srcs2"), dump(b"srch"))
    })
    .await
    .expect("seed+dumps");

    let dir = tempfile::tempdir().expect("tempdir");
    let mut node = spawn_node(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    let a = node.resp.as_str();

    // Raw string restored as a DIFFERENT key name.
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"RESTORE", b"dstk", b"0", &s1]).await,
        b"+OK"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"GET", b"dstk"]).await,
        b"$4\r\nval1"
    );

    // Typed families re-root: a hash dumped from `srch` serves dsth.
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"RESTORE", b"dsth", b"0", &h]).await,
        b"+OK"
    );
    let r = cmd_full_reply(a, TOKEN, &[b"HGETALL", b"dsth"], 400).await;
    assert!(
        contains_bytes(&r, b"f1") && contains_bytes(&r, b"hv1"),
        "f1: {r:?}"
    );
    assert!(
        contains_bytes(&r, b"f2") && contains_bytes(&r, b"hv2"),
        "f2: {r:?}"
    );
    assert_eq!(cmd_one_shot(a, TOKEN, &[b"HLEN", b"dsth"]).await, b":2");

    // Existing target without REPLACE: the Redis BUSYKEY text.
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"RESTORE", b"dstk", b"0", &s1]).await,
        b"-BUSYKEY Target key name already exists."
    );
    // REPLACE overwrites with the new payload.
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"RESTORE", b"dstk", b"0", &s2, b"REPLACE"]).await,
        b"+OK"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"GET", b"dstk"]).await,
        b"$4\r\nval2"
    );

    // The ttl argument is an ABSOLUTE ms deadline (ABSTTL is a no-op:
    // our TTLs are always absolute). Future deadline -> live key, PTTL>0.
    let deadline = rdb::ds::expire::now_ms() + 60_000;
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[
                b"RESTORE",
                b"ttlk",
                deadline.to_string().as_bytes(),
                &s1,
                b"ABSTTL"
            ]
        )
        .await,
        b"+OK"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"GET", b"ttlk"]).await,
        b"$4\r\nval1"
    );
    let pttl = cmd_one_shot(a, TOKEN, &[b"PTTL", b"ttlk"]).await;
    let ms: i64 = String::from_utf8_lossy(&pttl[1..]).parse().unwrap_or(-1);
    assert!(ms > 0 && ms <= 60_000, "pttl {pttl:?}");

    // Error texts: negative/non-numeric TTL, corrupt payload, arity.
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"RESTORE", b"ek", b"-1", &s1]).await,
        b"-ERR Invalid TTL value, must be >= 0"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"RESTORE", b"ek", b"x", &s1]).await,
        b"-ERR Invalid TTL value, must be >= 0"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"RESTORE", b"ek", b"0", b"junk"]).await,
        b"-ERR DUMP payload version or checksum are wrong"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"RESTORE", b"ek", b"0"]).await,
        b"-ERR wrong number of arguments for 'restore' command"
    );
}
