//! M3 end-to-end: the cluster-global SQL timestamp oracle over a REAL
//! openraft cluster (3 in-process nodes, one control API per node).
//!
//! Covered behavior:
//! - timestamps are only handed out of raft-authorized blocks: the
//!   `sql_ts_cursor` FSM key is persisted BEFORE a block is served, so
//!   ranges can never overlap across nodes or leader changes;
//! - followers lease blocks from the leader over `/sql/ts`;
//! - `sql_nodes` self-registration publishes the binds a follower needs
//!   to discover the leader's control API;
//! - a departed leader stops serving; the new leader continues exactly
//!   at the persisted cursor;
//! - the public `Oracle` switches from node-local to cluster blocks at
//!   `cluster init` without ever reusing a local grant.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use rdb::rcache::{fsm, http, join, service, transport};
use rdb::rcache::{new_raft_node, RaftNode, RdbRaft};
use rdb::sql::tx::global::{ClusterTs, ClusterTsDeps, TS_BLOCK};
use rdb::sql::tx::nodes::{leader_http_addr, register_once, NodeBinds};
use rdb::sql::tx::Oracle;
use rdb::state::{self, RaftState};
use rdb::topology::{self, Topology};

const TOKEN: &str = "sql-ts-e2e";
const TS_CURSOR_KEY: &str = "sql_ts_cursor";

/// start_node mutates the process-wide RAFT_BOOTSTRAP env var; every
/// test in this binary serializes on this lock.
static CLUSTER_MUX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Reserve an unused loopback port; reuse after drop is fine here.
async fn free_addr() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().to_string()
}

struct TestNode {
    _dir: tempfile::TempDir,
    raft: Arc<RdbRaft>,
    kv: fsm::KvMap,
    /// Control-plane view synced from openraft metrics (`is_leader`,
    /// `leader_addr`) -- the same feeding the real main.rs uses.
    raft_state: Arc<RwLock<RaftState>>,
    topo: Arc<RwLock<Topology>>,
    ts: Arc<ClusterTs>,
    tcp_addr: String,
    http_addr: String,
    id: u64,
}

/// Feed `RaftState` from openraft metrics until the raft shuts down.
fn spawn_state_sync(raft: Arc<RdbRaft>, st: Arc<RwLock<RaftState>>, addr: String) {
    tokio::spawn(async move {
        let mut rx = raft.metrics();
        loop {
            let m = rx.borrow_and_update().clone();
            state::sync_from_metrics(&mut st.write().unwrap(), &m, &addr);
            let changed = tokio::time::timeout(Duration::from_millis(200), rx.changed()).await;
            if matches!(changed, Ok(Err(_))) {
                break; // metrics channel closed: raft is gone
            }
        }
    });
}

/// One fully wired node: raft + rpc server + control-plane state +
/// cluster-ts core + HTTP control API carrying `/sql/ts`.
async fn start_node(bootstrap: bool) -> TestNode {
    if bootstrap {
        std::env::set_var("RAFT_BOOTSTRAP", "true");
    } else {
        std::env::remove_var("RAFT_BOOTSTRAP");
    }

    let tcp_addr = free_addr().await;
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("raft");
    let RaftNode { raft, kv } = new_raft_node(data_dir.to_str().unwrap(), &tcp_addr)
        .await
        .unwrap();

    // Raft TCP RPC server (Go: RCache.serveRaft).
    let raft_tcp = raft.clone();
    let addr_tcp = tcp_addr.clone();
    tokio::spawn(async move {
        if let Err(e) = service::serve(addr_tcp, raft_tcp).await {
            panic!("raft rpc serve failed: {e}");
        }
    });

    // Apply loop: RaftState.raft_apply_* funnels every control-plane
    // write (cursor, registry) through a real client_write.
    let (apply_tx, apply_rx) =
        tokio::sync::mpsc::channel::<state::ApplyReq>(state::APPLY_CHANNEL_CAPACITY);
    state::spawn_apply_loop(raft.clone(), apply_rx);

    let raft_state = Arc::new(RwLock::new(RaftState {
        is_leader: false,
        leader_addr: String::new(),
        state_label: "Follower".to_string(),
        node_desc: format!("{tcp_addr} [Follower]"),
        stats: Vec::new(),
        kv: BTreeMap::new(),
        apply_count: 0,
        live_kv: Some(kv.clone()),
        apply_tx: Some(apply_tx),
    }));
    let topo = Arc::new(RwLock::new(topology::empty()));
    spawn_state_sync(raft.clone(), raft_state.clone(), tcp_addr.clone());

    // HTTP control API on an ephemeral port; the address is known
    // before building the node binds the registry publishes.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = listener.local_addr().unwrap().to_string();
    let ts = Arc::new(ClusterTs::new(ClusterTsDeps {
        raft: raft_state.clone(),
        topo: topo.clone(),
        binds: NodeBinds {
            resp: tcp_addr.clone(),
            raft: tcp_addr.clone(),
            http: http_addr.clone(),
            mysql: String::new(),
            sql_rpc: String::new(),
        },
        token: TOKEN.to_string(),
    }));
    let (raft_http, kv_http) = (raft.clone(), kv.clone());
    let http_ts = ts.clone();
    tokio::spawn(async move {
        if let Err(e) = http::serve_on(
            listener,
            raft_http,
            kv_http,
            TOKEN.into(),
            http::membership_mux(),
            Some(http_ts),
            http::store_slot(),
        )
        .await
        {
            panic!("http serve failed: {e}");
        }
    });

    let id = transport::node_id_of(&tcp_addr);
    TestNode {
        _dir: dir,
        raft,
        kv,
        raft_state,
        topo,
        ts,
        tcp_addr,
        http_addr,
        id,
    }
}

/// Poll until `cond` holds (50ms cadence, 30s ceiling).
async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll until the voter set equals `want`.
async fn wait_for_voters(raft: &RdbRaft, want: &[u64], what: &str) {
    let want: BTreeSet<u64> = want.iter().copied().collect();
    wait_until(&format!("voters == {want:?} ({what})"), || {
        raft.metrics()
            .borrow()
            .membership_config
            .voter_ids()
            .collect::<BTreeSet<u64>>()
            == want
    })
    .await;
}

/// Poll the FSM map until `key` maps to `value` (raft-replicated).
async fn wait_for_kv(kv: &fsm::KvMap, key: &str, value: &str) {
    wait_until(&format!("FSM applies {key}={value}"), || {
        kv.read().unwrap().get(key).map(String::as_str) == Some(value)
    })
    .await;
}

/// First node whose synced state says it leads.
async fn wait_elect<'a>(nodes: &[&'a TestNode]) -> &'a TestNode {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(l) = nodes
            .iter()
            .find(|n| n.raft_state.read().unwrap().is_leader)
        {
            return l;
        }
        assert!(Instant::now() < deadline, "no node became leader");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Status code plus body of a control-API GET.
async fn http_status_and_body(node: &TestNode, path_and_query: &str) -> (u16, String) {
    let url = format!("http://{}{path_and_query}", node.http_addr);
    join::http_get_status(&url).await.unwrap()
}

/// Status line plus body, joined for whole-tuple asserts.
async fn http_route(node: &TestNode, path_and_query: &str) -> (String, String) {
    let (code, body) = http_status_and_body(node, path_and_query).await;
    (code.to_string(), body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocks_are_raft_authorized_and_survive_leader_change() {
    let _mux = CLUSTER_MUX.lock().await;

    // 3-voter cluster: n1 bootstraps, n2/n3 join over the HTTP API.
    let n1 = start_node(true).await;
    wait_until("n1 leads after bootstrap", || {
        n1.raft_state.read().unwrap().is_leader
    })
    .await;
    let n2 = start_node(false).await;
    let n3 = start_node(false).await;
    join::join_cluster(&n1.http_addr, &n2.tcp_addr, TOKEN)
        .await
        .unwrap();
    join::join_cluster(&n1.http_addr, &n3.tcp_addr, TOKEN)
        .await
        .unwrap();
    for n in [&n1, &n2, &n3] {
        wait_for_voters(&n.raft, &[n1.id, n2.id, n3.id], "3 voters").await;
    }

    // `cluster init` flips every node's topology to ready.
    for n in [&n1, &n2, &n3] {
        n.topo.write().unwrap().cluster_ready = true;
    }

    // Leader self-registration publishes the binds followers use for
    // leader discovery.
    assert!(register_once(n1.ts.deps()).await.unwrap());
    for f in [&n2, &n3] {
        wait_until("registry reaches follower", || {
            leader_http_addr(&f.raft_state).as_deref() == Some(n1.http_addr.as_str())
        })
        .await;
    }

    // Leader allocation: the cursor persists BEFORE the block is served.
    n1.ts.refill_once().await.unwrap();
    assert_eq!(n1.ts.alloc_n(8), 1..9);
    wait_for_kv(&n1.kv, TS_CURSOR_KEY, &(TS_BLOCK + 1).to_string()).await;

    // Follower allocation: a full block leased from the leader over
    // HTTP. The leader's own block cannot cover it, so the lease comes
    // from the NEXT raft-authorized block: disjoint and monotonic.
    n2.ts.refill_once().await.unwrap();
    let f1 = n2.ts.alloc_n(8);
    assert_eq!(f1.start, TS_BLOCK + 1);
    assert!(
        f1.start >= 9,
        "follower range must not overlap the leader's"
    );
    for n in [&n1, &n2, &n3] {
        wait_for_kv(&n.kv, TS_CURSOR_KEY, &(2 * TS_BLOCK + 1).to_string()).await;
    }

    // /sql/ts: leader serves from its remaining block (a fresh one: the
    // follower lease consumed the whole previous block); followers
    // refuse; a wrong token never reaches the allocator.
    let lease = format!("/sql/ts?n=4&raft-token={TOKEN}");
    assert_eq!(
        http_route(&n1, &lease).await,
        (
            "200".to_string(),
            format!("{} {}\n", 2 * TS_BLOCK + 1, 2 * TS_BLOCK + 5)
        )
    );
    assert_eq!(
        http_route(&n2, &lease).await,
        ("404".to_string(), "not leader\n".to_string())
    );
    assert_eq!(
        http_route(&n1, "/sql/ts?n=4&raft-token=bad").await,
        ("401".to_string(), "unauthorized\n".to_string())
    );

    // Leader change: the leader departs through its own API, a survivor
    // takes over, and the old leader stops serving timestamps.
    let depart = format!("/depart?peerAddress={}&raft-token={TOKEN}", n1.tcp_addr);
    assert_eq!(
        http_route(&n1, &depart).await,
        ("200".to_string(), "ok".to_string())
    );
    let survivors = [&n2, &n3];
    for s in &survivors {
        wait_for_voters(&s.raft, &[n2.id, n3.id], "2 survivors").await;
    }
    let new_leader = wait_elect(&survivors).await;
    wait_until("old leader steps down", || {
        !n1.raft_state.read().unwrap().is_leader
    })
    .await;
    assert_eq!(
        http_route(&n1, &lease).await,
        ("404".to_string(), "not leader\n".to_string())
    );

    // The new leader continues EXACTLY at the persisted cursor: drain
    // whatever the old block left, force a fresh fetch, and the next
    // grant must sit strictly above everything ever authorized.
    while new_leader.ts.has_block() {
        new_leader.ts.alloc_n(1);
    }
    new_leader.ts.refill_once().await.unwrap();
    assert_eq!(
        new_leader.ts.alloc_n(4),
        (3 * TS_BLOCK + 1)..(3 * TS_BLOCK + 5)
    );
    wait_for_kv(
        &new_leader.kv,
        TS_CURSOR_KEY,
        &(4 * TS_BLOCK + 1).to_string(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oracle_switches_from_local_to_cluster_blocks() {
    let _mux = CLUSTER_MUX.lock().await;

    let n1 = start_node(true).await;
    wait_until("n1 leads after bootstrap", || {
        n1.raft_state.read().unwrap().is_leader
    })
    .await;

    // Before `cluster init` the oracle keeps its node-local behavior
    // (mirrored into the cluster floor so nothing is ever reused).
    let oracle = Oracle::new();
    assert!(oracle.enable_cluster(n1.ts.clone()));
    assert!(!oracle.enable_cluster(n1.ts.clone()), "first install wins");
    assert_eq!(oracle.alloc_n(3), 1..4);

    // `cluster init`: grants move to raft-authorized blocks, never
    // below the local grants observed before the switch.
    n1.topo.write().unwrap().cluster_ready = true;
    n1.ts.refill_once().await.unwrap();
    assert_eq!(oracle.alloc_n(4), 4..8);
    assert_eq!(oracle.alloc(), 8);
    assert!(oracle.now() >= 8, "read watermark covers every grant");
    wait_for_kv(&n1.kv, TS_CURSOR_KEY, &(4 + TS_BLOCK).to_string()).await;
}
