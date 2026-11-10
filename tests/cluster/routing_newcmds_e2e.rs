//! Wire-level e2e tests for the MOVED routing of the newer / aggregate
//! commands (INCR, SETBIT, SETRANGE, GETDEL, LMPOP, SINTERCARD, BITOP)
//! plus the keyless commands that must never redirect. Two-node
//! topologies are injected straight into `shared.topology` (the sync
//! task only runs in the real binary), and expected -MOVED bytes are
//! recomputed through `router::route` exactly like the server does.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rdb::{command::cmd_meta, conf, hash, monitor, resp, router, state, store, topology};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Slots per node in a 2-node cluster: 16384 / 2.
const PER_NODE: usize = 8192;
/// The other (unreachable) shard, used as the MOVED target.
const PEER: &str = "10.9.9.9:9";

fn test_config(port: u16) -> conf::Config {
    conf::Config {
        bind: format!("127.0.0.1:{port}"),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: format!("127.0.0.1:227{port}"),
        raft_token: "test-token".to_string(),
        ..Default::default()
    }
}

/// Mirror of `state::testutil::shared_with`; `tag` keeps the parallel
/// tests' store directories apart.
fn test_shared(conf: conf::Config, tag: &str) -> state::Shared {
    let dir = std::env::temp_dir().join(format!("rdb-route-e2e-{}-{tag}", std::process::id()));
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
        lite: Arc::new(rdb::lite::new_runtime()),
        sql_ts: Arc::new(rdb::sql::tx::Oracle::new()),
        migrating: Arc::new(RwLock::new(std::collections::HashMap::new())),
        importing: Arc::new(RwLock::new(std::collections::HashMap::new())),
        migrate_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        conf,
    }
}

/// Start a listener whose node is the LAST member of a 2-node cluster
/// (`PEER` first), so it locally owns exactly the slots above PER_NODE.
async fn spawn_shard(port: u16, tag: &str) -> (std::net::SocketAddr, String, Vec<String>) {
    let conf = test_config(port);
    let shared = Arc::new(test_shared(conf.clone(), tag));
    *shared.topology.write().unwrap() = topology::refresh(&format!("{PEER},{}", conf.bind));
    let addrs = vec![PEER.to_string(), conf.bind.clone()];
    let listener = resp::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(resp::serve(listener, shared));
    (addr, conf.bind, addrs)
}

async fn dial(addr: std::net::SocketAddr) -> TcpStream {
    let sock = tokio::time::timeout(TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    sock
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

async fn rpc(sock: &mut TcpStream, req: &[u8], expect: &[u8]) {
    sock.write_all(req).await.expect("write");
    assert_eq!(read_n(sock, expect.len()).await, expect, "req: {req:?}");
}

async fn read_n(sock: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    sock.read_exact(&mut out).await.expect("read_exact");
    out
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

/// The exact -MOVED line the server must produce for `key`, recomputed
/// through router::route with the same cluster view.
fn moved(key: &str, addrs: &[String], host: &str) -> Vec<u8> {
    let slot = hash::slot_number(hash::hash_tag(key.as_bytes()));
    match router::route(slot, addrs, PER_NODE, host) {
        router::RouteDecision::Moved { slot, addr } => {
            format!("-MOVED {slot} {addr}\r\n").into_bytes()
        }
        router::RouteDecision::Local => panic!("expected MOVED for {key} (slot {slot})"),
    }
}

/// `{key}suffix` shares key's slot (hash-tag extraction), so multi-key
/// commands stay cross-slot-clean while keeping a chosen slot.
fn tagged(key: &str, suffix: &str) -> String {
    format!("{{{key}}}{suffix}")
}

#[tokio::test]
async fn non_local_slots_redirect_new_commands_by_routing_key() {
    let (addr, host, addrs) = spawn_shard(32780, "moved").await;
    let mut s = dial(addr).await;
    rpc(&mut s, &resp_req(&[b"AUTH", b"test-token"]), b"+OK\r\n").await;

    // Precondition: the argv[1] literals used below sit on the LOCAL
    // shard, so a routing-key-index bug (argv[1] instead of argv[2])
    // would serve LMPOP/SINTERCARD/BITOP locally instead of redirecting.
    let one = hash::slot_number(hash::hash_tag(b"1"));
    let two = hash::slot_number(hash::hash_tag(b"2"));
    assert!(one as usize > PER_NODE, "'1' must be a local slot");
    assert!(two as usize <= PER_NODE, "'2' must be a remote slot");

    // Plain single-key commands owned by PEER redirect with their slot.
    let (nk, nk_slot) = find_key(|slot| slot as usize <= PER_NODE);
    let mv = moved(&nk, &addrs, &host);
    for cmd in [
        vec![b"INCR".as_slice(), nk.as_bytes()],
        vec![b"SETBIT".as_slice(), nk.as_bytes(), b"0", b"1"],
        vec![b"SETRANGE".as_slice(), nk.as_bytes(), b"0", b"x"],
        vec![b"GETDEL".as_slice(), nk.as_bytes()],
    ] {
        rpc(&mut s, &resp_req(&cmd), &mv).await;
    }

    // Aggregate commands route on argv[2], never on their numeric
    // argv[1]: LMPOP 1 <key> (argv[1] local!), LMPOP 2 / SINTERCARD 2
    // (argv[1] remote but at a DIFFERENT slot), BITOP AND <dst>...
    let a = tagged(&nk, "a");
    let b = tagged(&nk, "b");
    let d = tagged(&nk, "d");
    assert_eq!(hash::slot_number(hash::hash_tag(a.as_bytes())), nk_slot);
    let k = a.as_bytes();
    for cmd in [
        vec![b"LMPOP".as_slice(), b"1", k],
        vec![b"LMPOP".as_slice(), b"2", k, b.as_bytes(), b"LEFT"],
        vec![b"SINTERCARD".as_slice(), b"2", k, b.as_bytes()],
        vec![b"BITOP".as_slice(), b"AND", d.as_bytes(), k, b.as_bytes()],
    ] {
        rpc(&mut s, &resp_req(&cmd), &mv).await;
    }

    // Nothing above may have been served (and written) locally.
    rpc(&mut s, &resp_req(&[b"DBSIZE"]), b":0\r\n").await;
}

#[tokio::test]
async fn local_slots_execute_aggregate_commands_in_place() {
    let (addr, _host, _addrs) = spawn_shard(32781, "local").await;
    let mut s = dial(addr).await;
    rpc(&mut s, &resp_req(&[b"AUTH", b"test-token"]), b"+OK\r\n").await;

    // A hash tag wholly on the local shard (slots above PER_NODE).
    let (lk, lslot) = find_key(|slot| slot as usize > PER_NODE);
    let a = tagged(&lk, "a");
    let b = tagged(&lk, "b");
    let s1 = tagged(&lk, "s1");
    let s2 = tagged(&lk, "s2");
    let d = tagged(&lk, "d");
    let sa = tagged(&lk, "sa");
    let sb = tagged(&lk, "sb");
    for k in [&a, &b, &s1, &s2, &d, &sa, &sb] {
        let ks = hash::slot_number(hash::hash_tag(k.as_bytes()));
        assert_eq!(ks, lslot, "tagged key {k} must share the tag slot");
    }
    let ab = a.as_bytes();
    let bb = b.as_bytes();

    // LMPOP pops from the first non-empty key, argv[2] is the first one.
    rpc(&mut s, &resp_req(&[b"RPUSH", ab, b"x", b"y"]), b":2\r\n").await;
    let pop = format!("*2\r\n${}\r\n{a}\r\n*1\r\n$1\r\nx\r\n", a.len());
    rpc(
        &mut s,
        &resp_req(&[b"LMPOP", b"2", ab, bb, b"LEFT"]),
        pop.as_bytes(),
    )
    .await;

    // BITOP AND merges into the destination (argv[2]): 0x61 & 0x62=0x60.
    rpc(
        &mut s,
        &resp_req(&[b"SET", s1.as_bytes(), b"a"]),
        b"+OK\r\n",
    )
    .await;
    rpc(
        &mut s,
        &resp_req(&[b"SET", s2.as_bytes(), b"b"]),
        b"+OK\r\n",
    )
    .await;
    rpc(
        &mut s,
        &resp_req(&[b"BITOP", b"AND", d.as_bytes(), s1.as_bytes(), s2.as_bytes()]),
        b":1\r\n",
    )
    .await;
    rpc(&mut s, &resp_req(&[b"GET", d.as_bytes()]), b"$1\r\n`\r\n").await;

    // SINTERCARD counts the intersection across argv[2..].
    rpc(
        &mut s,
        &resp_req(&[b"SADD", sa.as_bytes(), b"m"]),
        b":1\r\n",
    )
    .await;
    rpc(
        &mut s,
        &resp_req(&[b"SADD", sb.as_bytes(), b"m"]),
        b":1\r\n",
    )
    .await;
    rpc(
        &mut s,
        &resp_req(&[b"SINTERCARD", b"2", sa.as_bytes(), sb.as_bytes()]),
        b":1\r\n",
    )
    .await;
    let miss = tagged(&lk, "m1");
    let miss2 = tagged(&lk, "m2");
    rpc(
        &mut s,
        &resp_req(&[b"SINTERCARD", b"2", miss.as_bytes(), miss2.as_bytes()]),
        b":0\r\n",
    )
    .await;
}

#[tokio::test]
async fn keyless_commands_stay_local_even_on_a_foreign_shard() {
    // Host is absent from the address list entirely: every slotted key
    // redirects, so any accidental routing of a keyless command would
    // show up as a MOVED reply here.
    let conf = test_config(32782);
    let shared = Arc::new(test_shared(conf.clone(), "keyless"));
    let others = vec!["10.0.0.1:1".to_string(), "10.0.0.2:2".to_string()];
    *shared.topology.write().unwrap() = topology::refresh("10.0.0.1:1,10.0.0.2:2");
    let listener = resp::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(resp::serve(listener, shared));

    let mut s = dial(addr).await;
    rpc(&mut s, &resp_req(&[b"AUTH", b"test-token"]), b"+OK\r\n").await;

    // Control: routing IS active, any keyed command leaves this host.
    let probe = find_key(|_| true).0;
    rpc(
        &mut s,
        &resp_req(&[b"GET", probe.as_bytes()]),
        &moved(&probe, &others, &conf.bind),
    )
    .await;

    // Keyless surface commands must answer locally regardless.
    rpc(&mut s, &resp_req(&[b"PING"]), b"+PONG\r\n").await;
    rpc(&mut s, &resp_req(&[b"ECHO", b"hi"]), b"$2\r\nhi\r\n").await;
    let count = format!(":{}\r\n", cmd_meta::COMMANDS.len());
    rpc(&mut s, &resp_req(&[b"COMMAND", b"COUNT"]), count.as_bytes()).await;
    let info = resp_req(&[b"INFO", b"keyspace"]);
    // 0 keys: the db0 line is omitted, only the section header remains.
    rpc(&mut s, &info, b"$14\r\n# Keyspace\r\n\r\n\r\n").await;
    rpc(&mut s, &resp_req(&[b"DBSIZE"]), b":0\r\n").await;
    rpc(&mut s, &resp_req(&[b"SELECT", b"0"]), b"+OK\r\n").await;
    rpc(&mut s, &resp_req(&[b"FLUSHDB"]), b"+OK\r\n").await;
    rpc(&mut s, &resp_req(&[b"PING"]), b"+PONG\r\n").await;
}
