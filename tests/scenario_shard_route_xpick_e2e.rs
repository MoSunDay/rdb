//! Multi-store scenario #81 -- a delivery shard router: three explicit
//! queues back one topic and XPICK's strategies divide the traffic --
//! round_robin cycles the discovered queues in order, hash pins one
//! shard key to one queue forever (CRC16 mod n, asserted against the
//! same function the router uses), least_backlog sends each burst to
//! the shortest queue and follows it as backlogs shift; every pick
//! answers the full `parent/child` stream name, and the router's three
//! failure modes are verbatim errors (hash without a shard key, an
//! unknown strategy, an invalid topic name). One REAL node, RESP only.

mod common;
mod scenario_common;

use scenario_common::{front_world, r, rt, FrontSpec};

#[tokio::test]
async fn shard_router_rotates_hashes_and_balances_backlogs() {
    let (_front, a) = front_world("xr81", &FrontSpec::default()).await;

    // a) Three explicit queues with staggered backlogs: q0=3, q1=1,
    //    q2=2 parcels (explicit child XADDs answer the bare id).
    for (queue, n) in [("routes/q0", 3), ("routes/q1", 1), ("routes/q2", 2)] {
        for i in 1..=n {
            let id = format!("1-{i}");
            let reply = r(&a, &[b"XADD", queue.as_bytes(), id.as_bytes(), b"sku", b"p"]).await;
            assert!(reply.starts_with(format!("$3\r\n{id}").as_bytes()), "{queue} seed {i}: {reply:?}");
        }
    }

    // b) round_robin cycles the discovered queues in byte order; each
    //    reply is the FULL parent/child stream name.
    let pick = |strategy: &str, shard: Option<&str>| {
        let a = a.clone();
        let s = strategy.as_bytes().to_vec();
        let shard = shard.map(|x| x.as_bytes().to_vec());
        async move {
            let args: Vec<&[u8]> = match &shard {
                Some(k) => vec![b"XPICK", b"routes", &s, k],
                None => vec![b"XPICK", b"routes", &s],
            };
            rt(&a, &args).await
        }
    };
    assert_eq!(pick("round_robin", None).await.trim_end(), "$9\r\nroutes/q0");
    assert_eq!(pick("round_robin", None).await.trim_end(), "$9\r\nroutes/q1");

    // c) hash pins shard keys: the reply matches CRC16(key) mod 3 and
    //    repeats identically on every call.
    let want = format!("routes/q{}", rdb::hash::crc16(b"user-42") as usize % 3);
    for _ in 0..2 {
        let got = pick("hash", Some("user-42")).await;
        assert_eq!(got.trim_end(), format!("${}\r\n{want}", want.len()), "CRC16 queue, stable");
    }

    // d) least_backlog lands on q1 (the 1-parcel queue); after a flood
    //    makes q1 the longest, the next burst swings to q2 (2 parcels).
    assert_eq!(pick("least_backlog", None).await.trim_end(), "$9\r\nroutes/q1");
    for i in 2..=5 {
        let id = format!("1-{i}");
        let reply = r(&a, &[b"XADD", b"routes/q1", id.as_bytes(), b"sku", b"flood"]).await;
        assert!(reply.starts_with(format!("$3\r\n{id}").as_bytes()), "flood {i}: {reply:?}");
    }
    assert_eq!(pick("least_backlog", None).await.trim_end(), "$9\r\nroutes/q2");

    // e) The router's three failure modes, verbatim.
    assert_eq!(
        rt(&a, &[b"XPICK", b"routes", b"hash"]).await,
        "-ERR hash strategy requires a shard key"
    );
    assert_eq!(
        rt(&a, &[b"XPICK", b"routes", b"sticky"]).await,
        "-ERR unknown pick strategy (round_robin|hash|least_backlog)"
    );
    assert_eq!(
        rt(&a, &[b"XPICK", b"bad!name", b"round_robin"]).await,
        "-ERR invalid topic name"
    );
}
