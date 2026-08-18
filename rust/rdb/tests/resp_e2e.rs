//! E2E tests for the RESP server layer: raw TcpStream clients speaking RESP2
//! against a real listener (no fixed ports). Covers the AUTH gate, basic
//! string commands, unknown/malformed commands, MOVED routing and the quit /
//! protocol-error close semantics.
//!
//! NOTE: `state::testutil` is `#[cfg(test)]` inside the lib, hence invisible
//! to integration tests; the helpers below replicate it exactly.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rdb::{conf, hash, monitor, resp, router, state, store, topology};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(5);

fn test_config() -> conf::Config {
    conf::Config {
        bind: "127.0.0.1:32681".to_string(),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: "127.0.0.1:22681".to_string(),
        raft_token: "test-token".to_string(),
        ..Default::default()
    }
}

/// Mirror of `state::testutil::shared_with`; `tag` keeps the store dirs of
/// the individual #[tokio::test]s in this file apart (they run in parallel).
fn test_shared(conf: conf::Config, tag: &str) -> state::Shared {
    let dir = std::env::temp_dir().join(format!("rdb-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = store::data_path(dir.to_str().unwrap(), &conf.bind);
    let st = store::open(path.to_str().unwrap()).unwrap();
    state::Shared {
        mode: state::Mode::Normal,
        store: Arc::new(st),
        topology: Arc::new(RwLock::new(topology::empty())),
        raft: Arc::new(RwLock::new(state::stub_raft(&conf))),
        monitor: Arc::new(monitor::new_collector()),
        latch: rdb::ds::latch::Latch::new(),
        wait_hub: rdb::ds::wait::WaitHub::new(),
        lite: std::sync::Arc::new(rdb::lite::new_runtime()),
        conf,
    }
}

async fn read_n(sock: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(TIMEOUT, sock.read_exact(&mut buf))
        .await
        .expect("read timed out")
        .expect("read failed (EOF?)");
    buf
}

async fn rpc(sock: &mut TcpStream, req: &[u8], expect: &[u8]) {
    sock.write_all(req).await.expect("write");
    assert_eq!(read_n(sock, expect.len()).await, expect, "req: {req:?}");
}

async fn assert_eof(sock: &mut TcpStream) {
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(TIMEOUT, sock.read(&mut b))
        .await
        .expect("eof read timed out")
        .expect("eof read failed");
    assert_eq!(n, 0, "expected EOF");
}

/// First candidate "key{i}" whose slot satisfies `pred`.
fn find_key(pred: impl Fn(u16) -> bool) -> (String, u16) {
    for i in 0..10000u32 {
        let key = format!("key{i}");
        let slot = hash::slot_number(hash::hash_tag(key.as_bytes()));
        if pred(slot) {
            return (key, slot);
        }
    }
    panic!("no candidate key found");
}

fn resp_req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

#[tokio::test]
async fn resp_server_end_to_end() {
    let shared = Arc::new(test_shared(test_config(), "main"));
    let listener = resp::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(resp::serve(listener, shared.clone()));

    let mut s = tokio::time::timeout(TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");

    // 1. Pre-auth PING is rejected.
    rpc(&mut s, &resp_req(&[b"PING"]), b"-ERR: NOAUTH\r\n").await;

    // 2. AUTH: wrong token rejected with the same NOAUTH text; right token OK.
    rpc(
        &mut s,
        &resp_req(&[b"AUTH", b"wrong-token"]),
        b"-ERR: NOAUTH\r\n",
    )
    .await;
    rpc(&mut s, &resp_req(&[b"AUTH", b"test-token"]), b"+OK\r\n").await;

    // 3. PING works after auth; the inline (telnet) form too -- sent as ONE
    //    write to prove pipelining is drained.
    let mut both = resp_req(&[b"ping"]);
    both.extend_from_slice(b"ping\r\n");
    rpc(&mut s, &both, b"+PONG\r\n+PONG\r\n").await;

    // 4. SET/GET roundtrip and null bulk for a missing key.
    rpc(&mut s, &resp_req(&[b"set", b"k", b"v"]), b"+OK\r\n").await;
    rpc(&mut s, &resp_req(&[b"get", b"k"]), b"$1\r\nv\r\n").await;
    rpc(&mut s, &resp_req(&[b"get", b"missing"]), b"$-1\r\n").await;

    // 5. Unknown command keeps the original case.
    rpc(
        &mut s,
        &resp_req(&[b"bogus"]),
        b"-ERR unknown command 'bogus'\r\n",
    )
    .await;

    // 6. Lone non-whitelisted command: Redis-standard arity error
    //    (BREAKING, approved: no more fabricated Go panic text).
    rpc(
        &mut s,
        &resp_req(&[b"get"]),
        b"-ERR wrong number of arguments for 'get' command\r\n",
    )
    .await;

    // 7. MOVED flow. Seed the cluster via `cluster init`, then force the
    //    topology (the 3s sync task only runs in the real binary).
    rpc(
        &mut s,
        b"cluster init 10.0.0.1:1,10.0.0.2:2,10.0.0.3:3\r\n",
        b"+done\r\n",
    )
    .await;
    *shared.topology.write().unwrap() = topology::refresh("10.0.0.1:1,10.0.0.2:2,10.0.0.3:3");

    let (low_key, low_slot) = find_key(|slot| slot <= 5461);
    let (high_key, high_slot) = find_key(|slot| slot > 5461);

    // Host 127.0.0.1:32681 is NOT in the list: even slot 0 gets MOVED to
    // the owner computed by router::route.
    for (key, slot) in [(&low_key, low_slot), (&high_key, high_slot)] {
        let decision = router::route(
            slot,
            &shared.topology.read().unwrap().stable_addrs.clone(),
            5461,
            "127.0.0.1:32681",
        );
        let expected = match decision {
            router::RouteDecision::Moved { slot, addr } => {
                format!("-MOVED {} {}\r\n", slot, addr).into_bytes()
            }
            router::RouteDecision::Local => panic!("expected MOVED for {key}"),
        };
        rpc(&mut s, &resp_req(&[b"get", key.as_bytes()]), &expected).await;
    }

    // Reset the topology with the real host first: low slots served locally,
    // high slots still redirect.
    *shared.topology.write().unwrap() = topology::refresh("127.0.0.1:32681,10.0.0.2:2,10.0.0.3:3");
    rpc(
        &mut s,
        &resp_req(&[b"set", low_key.as_bytes(), b"lv"]),
        b"+OK\r\n",
    )
    .await;
    rpc(
        &mut s,
        &resp_req(&[b"get", low_key.as_bytes()]),
        b"$2\r\nlv\r\n",
    )
    .await;
    let decision = router::route(
        high_slot,
        &[
            "127.0.0.1:32681".to_string(),
            "10.0.0.2:2".into(),
            "10.0.0.3:3".into(),
        ],
        5461,
        "127.0.0.1:32681",
    );
    let expected = match decision {
        router::RouteDecision::Moved { slot, addr } => {
            format!("-MOVED {} {}\r\n", slot, addr).into_bytes()
        }
        router::RouteDecision::Local => panic!("expected MOVED for {high_key}"),
    };
    rpc(&mut s, &resp_req(&[b"get", high_key.as_bytes()]), &expected).await;

    // 8. quit replies exactly one +OK (BREAKING, approved: the old fork
    //    also sent +PONG) then closes the connection.
    rpc(&mut s, &resp_req(&[b"quit"]), b"+OK\r\n").await;
    assert_eof(&mut s).await;

    // 9. Protocol error on a fresh connection: error reply, then close.
    let mut s2 = tokio::time::timeout(TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    rpc(&mut s2, &resp_req(&[b"AUTH", b"test-token"]), b"+OK\r\n").await;
    rpc(&mut s2, b"*x\r\n", b"-ERR invalid multibulk length\r\n").await;
    assert_eof(&mut s2).await;
}

#[tokio::test]
async fn empty_multibulk_replies_arity_error() {
    let shared = Arc::new(test_shared(test_config(), "empty-multibulk"));
    let listener = resp::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(resp::serve(listener, shared));

    let mut s = tokio::time::timeout(TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    rpc(&mut s, &resp_req(&[b"AUTH", b"test-token"]), b"+OK\r\n").await;
    // `*0\r\n` parses to zero args; no command name exists, so the arity
    // error carries an empty name (BREAKING, approved: replaces Go's
    // fabricated cmd.Args[0] panic text).
    rpc(
        &mut s,
        b"*0\r\n",
        b"-ERR wrong number of arguments for '' command\r\n",
    )
    .await;
    // Connection stays usable afterwards.
    rpc(&mut s, &resp_req(&[b"ping"]), b"+PONG\r\n").await;
}
