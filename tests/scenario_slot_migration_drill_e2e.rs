//! Multi-store scenario #67 -- a slot migration drill over a REAL
//! two-node cluster (node0 bootstraps, node1 joins over raft-http, then
//! `cluster init` carves the equal slot bands): every key group is
//! hash-tagged into one slot, the owner is found by probing reads past
//! -MOVED, each target is opened with CLUSTER SETSLOT IMPORTING (the
//! gate ASKING bypasses), MIGRATE ships a key's dump to the peer (no
//! COPY removes the source, COPY keeps it), the landed copy on the peer
//! answers only under a one-shot ASKING (the exact protocol MIGRATE
//! itself speaks), STABLE closes the gate again, MIGRATE KEYS moves a
//! batch and skips absent keys, and RESTORE refuses a present target
//! with BUSYKEY before it ever parses the payload (REPLACE lets it fail
//! on the payload instead). One world, RESP only.

mod common;
mod scenario_common;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::{cluster_init, wait_cluster_nodes_list_all, wait_leader, TOKEN};
use scenario_common::{front_world, ok, r, rt, FrontSpec};

#[tokio::test]
async fn migrate_restore_and_asking_semantics() {
    let spec = FrontSpec::default();
    let dir = scenario_common::world_dir("migrate67b"); // separate root: same tag would wipe node0
    let (n0, _) = front_world("migrate67", &spec).await;
    let join = n0.node.http.clone();
    let mut n1 = scenario_common::spawn_front_node(&dir, 1, false, Some(&join), &spec);
    scenario_common::wait_fronts(&mut n1, false, false, false).await;
    let nodes = [n0.node, n1.node];
    let leader = wait_leader(&nodes, 30).await;
    let peers = [nodes[0].resp.clone(), nodes[1].resp.clone()];
    // Equal slot bands need the replicated instances list: without it
    // every node routes everything Local and there is no -MOVED at all.
    let binds = peers.clone();
    cluster_init(&nodes[leader], &binds).await;
    wait_cluster_nodes_list_all(&nodes, &binds, 90).await;

    // The node whose read is NOT -MOVED owns the key's slot; probed at
    // use time so the drill holds under any slot partition.
    let owner_of = |key: &[u8]| {
        let peers = peers.clone();
        let k = key.to_vec();
        async move {
            for (i, p) in peers.iter().enumerate() {
                let reply = rt(p, &[b"GET", &k]).await;
                if !reply.starts_with("-MOVED") {
                    return i;
                }
            }
            panic!("both nodes MOVED {}", String::from_utf8_lossy(&k));
        }
    };

    // `CLUSTER KEYSLOT <key>` on node0 (whitelisted, local everywhere).
    let keyslot = |key: &[u8]| {
        let node = peers[0].clone();
        let k = key.to_vec();
        async move {
            let reply = rt(&node, &[b"CLUSTER", b"KEYSLOT", &k]).await;
            reply.strip_prefix(':').and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("keyslot: {reply}"))
        }
    };

    /// Open the destination's IMPORTING gate for `slot` (the source id
    /// is md5-with-40 of its resp addr, as `cluster nodes` prints them).
    async fn import_gate(dst: &str, slot: u16, src: &str) {
        let src_id = rdb::utils::md5_with40(src);
        ok(&r(dst, &[b"CLUSTER", b"SETSLOT", slot.to_string().as_bytes(), b"IMPORTING", src_id.as_bytes()]).await, "setslot importing");
    }

    // a) The ledger group: seed on its owner, then MIGRATE it away
    //    without COPY. The owner reads nil afterwards, the peer still
    //    answers -MOVED for a plain read, and only a one-shot ASKING
    //    reads the landed copy.
    let oi = owner_of(b"{ml}ledger").await;
    let (src, dst) = (peers[oi].clone(), peers[1 - oi].clone());
    let slot = keyslot(b"{ml}ledger").await;
    import_gate(&dst, slot, &src).await;
    ok(&r(&src, &[b"SET", b"{ml}ledger", b"v1"]).await, "seed on owner");
    let (dh, dp) = dst.rsplit_once(':').expect("host:port");
    ok(&r(&src, &[b"MIGRATE", dh.as_bytes(), dp.as_bytes(), b"", b"0", b"5000", b"KEYS", b"{ml}ledger"]).await, "migrate");
    assert_eq!(rt(&src, &[b"GET", b"{ml}ledger"]).await, "$-1", "owner no longer holds the value");
    let plain = rt(&dst, &[b"GET", b"{ml}ledger"]).await;
    assert!(plain.starts_with("-MOVED"), "gate sends the plain reader away: {plain}");
    let asked = asking_get(&dst, b"{ml}ledger").await;
    assert!(asked.ends_with(b"v1\r\n"), "ASKING reads the landed copy: {asked:?}");
    ok(&r(&dst, &[b"CLUSTER", b"SETSLOT", slot.to_string().as_bytes(), b"STABLE"]).await, "setslot stable");
    let closed = rt(&dst, &[b"GET", b"{ml}ledger"]).await;
    assert!(closed.starts_with("-MOVED"), "STABLE re-arms ownership routing: {closed}");

    // b) The config group under COPY: the source keeps serving AND the
    //    peer holds a landed copy.
    let oi = owner_of(b"{mc}cfg").await;
    let (src, dst) = (peers[oi].clone(), peers[1 - oi].clone());
    import_gate(&dst, keyslot(b"{mc}cfg").await, &src).await;
    let (dh, dp) = dst.rsplit_once(':').expect("host:port");
    ok(&r(&src, &[b"SET", b"{mc}cfg", b"keep"]).await, "seed copy source");
    ok(&r(&src, &[b"MIGRATE", dh.as_bytes(), dp.as_bytes(), b"{mc}cfg", b"0", b"5000", b"COPY"]).await, "migrate copy");
    assert_eq!(rt(&src, &[b"GET", b"{mc}cfg"]).await, "$4\r\nkeep", "source kept");
    let asked2 = asking_get(&dst, b"{mc}cfg").await;
    assert!(asked2.ends_with(b"keep\r\n"), "peer landed a copy: {asked2:?}");

    // c) MIGRATE KEYS moves a same-slot batch in one command; the
    //    absent member of the batch is skipped silently (+OK).
    let oi = owner_of(b"{mb}b1").await;
    let (src, dst) = (peers[oi].clone(), peers[1 - oi].clone());
    import_gate(&dst, keyslot(b"{mb}b1").await, &src).await;
    let (dh, dp) = dst.rsplit_once(':').expect("host:port");
    ok(&r(&src, &[b"SET", b"{mb}b1", b"x"]).await, "b1");
    assert_eq!(rt(&src, &[b"SADD", b"{mb}b2", b"m1", b"m2"]).await, ":2", "b2 sadd");
    ok(&r(&src, &[b"MIGRATE", dh.as_bytes(), dp.as_bytes(), b"", b"0", b"5000", b"KEYS", b"{mb}b1", b"{mb}b2", b"{mb}absent"]).await, "batch migrate");
    assert_eq!(rt(&src, &[b"GET", b"{mb}b1"]).await, "$-1", "b1 left the source");
    assert_eq!(rt(&src, &[b"EXISTS", b"{mb}b2"]).await, ":0", "b2 left the source");

    // d) RESTORE: a present target refuses with BUSYKEY before the
    //    payload is ever parsed; REPLACE moves the failure onto the
    //    (junk) payload; an unknown option is a syntax error.
    let oi = owner_of(b"{mr}busy").await;
    let src = peers[oi].clone();
    ok(&r(&src, &[b"SET", b"{mr}busy", b"here"]).await, "busy target");
    let busy = rt(&src, &[b"RESTORE", b"{mr}busy", b"0", b"junk"]).await;
    assert_eq!(busy, "-BUSYKEY Target key name already exists.", "presence check precedes parsing");
    let replaced = rt(&src, &[b"RESTORE", b"{mr}busy", b"0", b"junk", b"REPLACE"]).await;
    assert!(replaced.starts_with("-ERR"), "junk payload fails once REPLACE lets it parse: {replaced}");
    let syntax = rt(&src, &[b"RESTORE", b"{mr}busy", b"0", b"junk", b"WHENCEVER"]).await;
    assert_eq!(syntax, "-ERR syntax error");
}

/// Open a fresh AUTH'd connection, burn a one-shot ASKING, then read
/// one GET; returns everything the server said after the two +OKs.
async fn asking_get(addr: &str, key: &[u8]) -> Vec<u8> {
    let mut sock = TcpStream::connect(addr).await.expect("connect");
    let mut buf = format!("*2\r\n$4\r\nAUTH\r\n${}\r\n{}\r\n", TOKEN.len(), TOKEN).into_bytes();
    buf.extend_from_slice(b"*1\r\n$6\r\nASKING\r\n");
    buf.extend_from_slice(format!("*2\r\n$3\r\nGET\r\n${}\r\n", key.len()).as_bytes());
    buf.extend_from_slice(key);
    buf.extend_from_slice(b"\r\n");
    sock.write_all(&buf).await.expect("write");
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let mut chunk = [0u8; 1024];
        loop {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    out.extend_from_slice(&chunk[..n]);
                    // +OK +OK then a bulk GET answer is 4 lines total.
                    if out.iter().filter(|b| **b == b'\n').count() >= 4 {
                        break;
                    }
                }
            }
        }
    })
    .await;
    out.strip_prefix(b"+OK\r\n+OK\r\n")
        .map(<[u8]>::to_vec)
        .unwrap_or(out)
}
