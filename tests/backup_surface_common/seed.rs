//! Normal-port seeding + backup-port readiness helpers for the
//! backup-surface sweep.

use std::time::{Duration, Instant};

use crate::common::{cmd_one_shot, TOKEN};

/// Retry one seeding write over the NORMAL port until `ok` accepts the
/// reply (the first writes can race the listener's startup).
async fn seed(addr: &str, args: &[&[u8]], ok: impl Fn(&[u8]) -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = cmd_one_shot(addr, TOKEN, args).await;
        if ok(&r) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "seed {what} never took: {args:?} -> {:?}",
            String::from_utf8_lossy(&r)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// `+OK`-shaped seeding write.
async fn seed_ok(addr: &str, args: &[&[u8]], what: &str) {
    seed(addr, args, |r| r == b"+OK", what).await;
}

/// Poll the backup port until it serves a +PONG (it binds slightly after
/// the normal listener); fails with the node's stderr tail if it never does.
pub async fn wait_backup_ready(backup: &str, node: &crate::common::ProcNode, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let r = cmd_one_shot(backup, TOKEN, &[b"PING"]).await;
        if r == b"+PONG" {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "backup listener never served PING ({r:?})\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Seed one key of every data family over the NORMAL port (single node,
/// empty topology: it serves every slot locally). This proves the gate is
/// per-listener (the normal port keeps writing) while giving the read
/// table realistic keys/args; the backup store stays empty, so every
/// backup-port read below pins a missing-key shape.
pub async fn seed_all(addr: &str) {
    // String (+ a TTL'd one) and same-tag pairs for multi-key reads.
    seed_ok(addr, &[b"SET", b"sk", b"hello"], "SET sk").await;
    seed_ok(
        addr,
        &[b"SET", b"tk", b"ttlval", b"EX", b"100"],
        "SET tk EX",
    )
    .await;
    seed_ok(addr, &[b"SET", b"{b}k1", b"v1"], "SET {b}k1").await;
    seed_ok(addr, &[b"SET", b"{b}k2", b"v2"], "SET {b}k2").await;
    // Hash.
    seed(
        addr,
        &[b"HSET", b"hk", b"f1", b"v1", b"f2", b"v2"],
        |r| r == b":2",
        "HSET hk",
    )
    .await;
    // Sets: one plain, one same-tag pair family for set algebra.
    seed(
        addr,
        &[b"SADD", b"setk", b"m1", b"m2"],
        |r| r == b":2",
        "SADD setk",
    )
    .await;
    seed(
        addr,
        &[b"SADD", b"{s}sa", b"m1", b"m2"],
        |r| r == b":2",
        "SADD {s}sa",
    )
    .await;
    seed(
        addr,
        &[b"SADD", b"{s}sb", b"m2", b"m3"],
        |r| r == b":2",
        "SADD {s}sb",
    )
    .await;
    // ZSet.
    seed(
        addr,
        &[b"ZADD", b"zsk", b"1", b"a", b"2", b"b", b"3", b"c"],
        |r| r == b":3",
        "ZADD zsk",
    )
    .await;
    // List.
    seed(
        addr,
        &[b"RPUSH", b"lk", b"a", b"b", b"c"],
        |r| r == b":3",
        "RPUSH lk",
    )
    .await;
    // Stream + a consumer group (names need the parent/child form).
    // Full stream-name XADD replies a bulk id (only bare parents get
    // the auto-pick child array reply).
    let id = cmd_one_shot(addr, TOKEN, &[b"XADD", b"st/q1", b"*", b"f", b"one"]).await;
    assert!(id.starts_with(b"$"), "XADD id reply {id:?}");
    seed(
        addr,
        &[b"XGROUP", b"CREATE", b"st/q1", b"g1", b"0-0"],
        |r| r == b"+OK",
        "XGROUP CREATE g1",
    )
    .await;
    let id2 = cmd_one_shot(addr, TOKEN, &[b"XADD", b"st/q1", b"*", b"f", b"two"]).await;
    assert!(id2.starts_with(b"$"), "XADD id2 reply {id2:?}");
    // JSON document.
    seed_ok(
        addr,
        &[b"JSON.SET", b"jk", b"$", br#"{"a":1,"b":{"c":[1,2]}}"#],
        "JSON.SET jk",
    )
    .await;
    // Search index + one doc (FT.ADD bodies are JSON documents here).
    seed_ok(
        addr,
        &[b"FT.CREATE", b"fidx", b"SCHEMA", b"title", b"TEXT"],
        "FT.CREATE fidx",
    )
    .await;
    seed(
        addr,
        &[b"FT.ADD", b"fidx", b"doc1", br#"{"title":"seed doc"}"#],
        |r| r == b":1",
        "FT.ADD doc1",
    )
    .await;
}
