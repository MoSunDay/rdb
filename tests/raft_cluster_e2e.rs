//! End-to-end check of the full control-plane wiring (P2 TODO 8): a real
//! two-node cluster assembled with `new_raft_node` (store, fsm, transport,
//! raft and env-gated bootstrap), the HTTP control API (`/get`, `/join` and
//! `/depart`), and the Go-style join client.
//!
//! Node1 bootstraps (RAFT_BOOTSTRAP=true), node2 joins through the HTTP
//! API exactly like a second process would; a client write then has to be
//! visible through `/get` on both nodes before node2 departs again.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rdb::rcache::{fsm, http, join, service, transport};
use rdb::rcache::{new_raft_node, RaftNode, RdbRaft};
use rdb::rtypes::RaftLogEntryData;

const TOKEN: &str = "e2e-token";

/// start_node mutates the process-wide RAFT_BOOTSTRAP env var, so the
/// bootstrap-dependent tests in this binary serialize on this lock
/// (cargo runs them on parallel threads otherwise).
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
    tcp_addr: String,
    http_addr: String,
    id: u64,
}

/// One fully wired node: `new_raft_node` + raft RPC server + HTTP API on
/// an ephemeral port. `bootstrap` toggles RAFT_BOOTSTRAP before building.
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

    // HTTP control API on an ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = listener.local_addr().unwrap().to_string();
    let (raft_http, kv_http) = (raft.clone(), kv.clone());
    tokio::spawn(async move {
        if let Err(e) = http::serve_on(
            listener,
            raft_http,
            kv_http,
            TOKEN.into(),
            http::membership_mux(),
            None,
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
        tcp_addr,
        http_addr,
        id,
    }
}

/// Poll until the voter set equals `want`.
async fn wait_for_voters(raft: &RdbRaft, want: &[u64], what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let want: BTreeSet<u64> = want.iter().copied().collect();
    loop {
        let voters: BTreeSet<u64> = raft
            .metrics()
            .borrow()
            .membership_config
            .voter_ids()
            .collect();
        if voters == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "voters never became {what}: {voters:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll the live FSM map until `key` maps to `value`.
async fn wait_for_kv(kv: &fsm::KvMap, key: &str, value: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if kv.read().unwrap().get(key).map(String::as_str) == Some(value) {
            return;
        }
        assert!(Instant::now() < deadline, "FSM never applied {key}={value}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn http_body(node: &TestNode, path_and_query: &str) -> String {
    let url = format!("http://{}{path_and_query}", node.http_addr);
    join::http_get(&url).await.unwrap()
}

/// Status code plus body, for the routes that no longer answer 200.
async fn http_status_and_body(node: &TestNode, path_and_query: &str) -> (u16, String) {
    let url = format!("http://{}{path_and_query}", node.http_addr);
    join::http_get_status(&url).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_replicate_get_depart() {
    let _cluster = CLUSTER_MUX.lock().await;
    // Node1: single-voter cluster via RAFT_BOOTSTRAP.
    let n1 = start_node(true).await;
    n1.raft
        .wait(Some(Duration::from_secs(10)))
        .current_leader(n1.id, "node1 must lead after bootstrap")
        .await
        .unwrap();

    // Node2 joins over the HTTP API (Go JoinRaftCluster).
    let n2 = start_node(false).await;
    join::join_cluster(&n1.http_addr, &n2.tcp_addr, TOKEN)
        .await
        .unwrap();
    wait_for_voters(&n1.raft, &[n1.id, n2.id], "{n1,n2}").await;

    // Replicate a write through the leader; both FSMs must apply it.
    n1.raft
        .client_write(RaftLogEntryData {
            key: "store/set".into(),
            value: "hello".into(),
        })
        .await
        .unwrap();
    wait_for_kv(&n1.kv, "store/set", "hello").await;
    wait_for_kv(&n2.kv, "store/set", "hello").await;

    // /get body semantics (Go doGet), on both nodes.
    let get = "/get?key=store%2Fset&raft-token=e2e-token";
    assert_eq!(http_body(&n1, get).await, "hello\n");
    assert_eq!(http_body(&n2, get).await, "hello\n");
    assert_eq!(
        http_body(&n1, "/get?key=store%2Fset&raft-token=bad").await,
        "\n"
    );
    // Go doGet: missing key keeps ret="" and prints "%s\n"; empty key
    // short-circuits to an empty body without the newline.
    assert_eq!(
        http_body(&n1, "/get?key=missing&raft-token=e2e-token").await,
        "\n"
    );
    assert_eq!(http_body(&n1, "/get?raft-token=e2e-token").await, "");
    assert_eq!(http_body(&n1, "/nope").await, "404 page not found\n");

    // /join with a wrong token is a real failure now: HTTP 401 with a
    // non-"ok" body (the Go original logged but still answered "ok").
    let bad = format!("/join?peerAddress={}&raft-token=bad", n2.tcp_addr);
    let (status, body) = http_status_and_body(&n1, &bad).await;
    assert_eq!(status, 401);
    assert_eq!(body, "unauthorized\n");
    // And the rejected join mutated nothing.
    wait_for_voters(&n1.raft, &[n1.id, n2.id], "{n1,n2} after bad-token join").await;

    // /depart with a wrong token fails the same way (401, no mutation).
    let bad_depart = format!("/depart?peerAddress={}&raft-token=bad", n2.tcp_addr);
    let (status, body) = http_status_and_body(&n1, &bad_depart).await;
    assert_eq!(status, 401);
    assert_eq!(body, "unauthorized\n");
    wait_for_voters(&n1.raft, &[n1.id, n2.id], "{n1,n2} after bad-token depart").await;

    // /depart removes node2 from the voter set.
    let depart = format!("/depart?peerAddress={}&raft-token=e2e-token", n2.tcp_addr);
    assert_eq!(http_body(&n1, &depart).await, "ok");
    wait_for_voters(&n1.raft, &[n1.id], "{n1} after depart").await;
}

/// Two joins issued simultaneously (multi-node startup without staggering)
/// must both land: the membership mux serializes each server's
/// add_learner -> read voters -> change_membership sequence, so the second
/// join re-reads the voter set the first one just extended instead of
/// racing it into a lost update or an openraft `internal error`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_joins_keep_all_voters() {
    let _cluster = CLUSTER_MUX.lock().await;
    let n1 = start_node(true).await;
    n1.raft
        .wait(Some(Duration::from_secs(10)))
        .current_leader(n1.id, "node1 must lead after bootstrap")
        .await
        .unwrap();

    let n2 = start_node(false).await;
    let n3 = start_node(false).await;

    let (j2, j3) = tokio::join!(
        join::join_cluster(&n1.http_addr, &n2.tcp_addr, TOKEN),
        join::join_cluster(&n1.http_addr, &n3.tcp_addr, TOKEN),
    );
    j2.unwrap();
    j3.unwrap();

    let want = [n1.id, n2.id, n3.id];
    wait_for_voters(&n1.raft, &want, "{n1,n2,n3} after concurrent joins").await;
}
