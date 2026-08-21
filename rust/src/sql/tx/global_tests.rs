//! Unit tests for [`crate::sql::tx::global`] (sibling file so global.rs
//! stays under the 400-line budget for new files).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::sql::tx::global::*;
use crate::sql::tx::nodes::{merged_registry, parse_registry, NodeBinds};
use crate::state::RaftState;
use crate::topology;

fn binds(raft: &str, http: &str) -> NodeBinds {
    NodeBinds {
        resp: format!("resp-{raft}"),
        raft: raft.to_string(),
        http: http.to_string(),
        mysql: String::new(),
        sql_rpc: String::new(),
    }
}

fn deps_of(raft: &Arc<RwLock<RaftState>>, topo: &Arc<RwLock<topology::Topology>>) -> ClusterTsDeps {
    ClusterTsDeps {
        raft: Arc::clone(raft),
        topo: Arc::clone(topo),
        binds: binds("raft-a", "http-a"),
        token: "tok".to_string(),
    }
}

/// A stub leader RaftState (no apply channel: applies land in `kv`
/// synchronously) plus a ready topology.
fn stub_leader() -> (Arc<RwLock<RaftState>>, Arc<RwLock<topology::Topology>>) {
    let st = RaftState {
        is_leader: true,
        leader_addr: "raft-a".to_string(),
        kv: BTreeMap::from([(TS_CURSOR_KEY.to_string(), "0".to_string())]),
        ..RaftState::default()
    };
    let topo = Arc::new(RwLock::new(topology::refresh("a,b,c")));
    (Arc::new(RwLock::new(st)), topo)
}

#[test]
fn carve_math_is_dense_and_exhaustion_aware() {
    let mut st = TsState {
        block_lo: 10,
        block_hi: 15,
        ..TsState::default()
    };
    assert_eq!(carve(&mut st, 0), Some(10..10));
    assert_eq!(carve(&mut st, 3), Some(10..13));
    assert_eq!(st.block_lo, 13);
    assert_eq!(st.global_hi, 12);
    assert_eq!(carve(&mut st, 3), None, "only 2 left");
    assert_eq!(carve(&mut st, 2), Some(13..15));
    assert_eq!(st.global_hi, 14);
    assert_eq!(remaining(&st), 0);
}

#[test]
fn carve_above_floor_rejects_blocks_below_requester_grants() {
    let mut st = TsState {
        block_lo: 10,
        block_hi: 20,
        global_hi: 12,
        ..TsState::default()
    };
    // Follower already granted through 99: a local block starting at 10
    // must NOT be carved (it would overlap the follower's grants).
    assert_eq!(carve_above_floor(&mut st, 5, 100), None);
    assert_eq!(st.block_lo, 10, "nothing consumed");
    // floor == block_lo is fine (block_lo is the next un-granted ts).
    assert_eq!(carve_above_floor(&mut st, 5, 10), Some(10..15));
}

#[test]
fn fallback_is_monotonic_above_gap_and_marks_degraded() {
    let mut st = TsState {
        global_hi: 9,
        last_cursor: 10,
        ..TsState::default()
    };
    let a = fallback_range(&mut st, 3);
    let b = fallback_range(&mut st, 3);
    assert!(st.degraded);
    assert_eq!(a.start, 10 + TS_FALLBACK_GAP + 1, "above cursor + gap");
    assert_eq!(b.start, a.end, "strictly above the previous fallback");
    assert_eq!(st.global_hi, b.end - 1);
    // A global_hi dominating the gap wins (long degraded episode).
    let dominating = 10 + TS_FALLBACK_GAP + 100;
    st.global_hi = dominating;
    let c = fallback_range(&mut st, 2);
    assert_eq!(c.start, dominating + 1);
}

#[test]
fn install_block_rejects_ranges_not_above_global_hi() {
    let mut st = TsState {
        block_lo: 10,
        block_hi: 20,
        global_hi: 19,
        ..TsState::default()
    };
    assert!(!install_block(&mut st, 5, 9), "below global_hi");
    assert!(!install_block(&mut st, 19, 19), "empty");
    assert!(!install_block(&mut st, 30, 25), "inverted");
    assert!(install_block(&mut st, 20, 30));
    assert_eq!((st.block_lo, st.block_hi), (20, 30));
    assert_eq!(st.last_cursor, 30, "cursor tracks block end");
}

#[test]
fn next_block_lo_takes_max_of_cursor_floor_and_one() {
    assert_eq!(next_block_lo(0, 0), 1, "ts 0 is never granted");
    assert_eq!(next_block_lo(500, 100), 500, "crash-safety: above cursor");
    assert_eq!(
        next_block_lo(500, 900),
        900,
        "degraded recovery: above floor"
    );
}

#[test]
fn parse_ts_block_shapes() {
    assert_eq!(parse_ts_block("10 20\n"), Some((10, 20)));
    assert_eq!(parse_ts_block("10 20"), Some((10, 20)));
    assert_eq!(parse_ts_block("not leader\n"), None);
    assert_eq!(parse_ts_block("10 10"), None);
    assert_eq!(parse_ts_block(""), None);
    assert_eq!(parse_ts_block("10"), None);
}

#[tokio::test]
async fn leader_fetch_persists_cursor_before_serving_and_never_reuses() {
    let (raft, topo) = stub_leader();
    let deps = deps_of(&raft, &topo);

    // Fresh cluster: cursor 0, floor 7 (a seeded oracle's global_hi+1).
    let (lo, hi) = leader_fetch(&deps, TS_BLOCK, 7).await.unwrap();
    assert_eq!((lo, hi), (7, 7 + TS_BLOCK));
    assert_eq!(
        raft.read().unwrap().kv.get(TS_CURSOR_KEY).cloned(),
        Some((7 + TS_BLOCK).to_string()),
        "cursor persisted == block end BEFORE the range is served"
    );

    // Second fetch continues exactly at the persisted cursor.
    let (lo2, _) = leader_fetch(&deps, TS_BLOCK, 0).await.unwrap();
    assert_eq!(lo2, hi, "no overlap, no gap with the previous block");

    // A degraded node's floor pushes the cursor above its grants.
    let floor = hi + 2 * TS_BLOCK + 55;
    let (lo3, hi3) = leader_fetch(&deps, TS_BLOCK, floor).await.unwrap();
    assert_eq!((lo3, hi3), (floor, floor + TS_BLOCK));
    assert_eq!(
        raft.read().unwrap().kv.get(TS_CURSOR_KEY).cloned(),
        Some(hi3.to_string())
    );
}

#[tokio::test]
async fn leader_fetch_rejects_non_leader() {
    let (raft, topo) = stub_leader();
    raft.write().unwrap().is_leader = false;
    let err = leader_fetch(&deps_of(&raft, &topo), TS_BLOCK, 0)
        .await
        .unwrap_err();
    assert_eq!(err, "not leader");
}

#[test]
fn alloc_n_serves_block_then_degraded_fallback() {
    let (raft, topo) = stub_leader();
    let ts = ClusterTs::new(deps_of(&raft, &topo));
    install_block(&mut ts.state.lock().unwrap(), 100, 110);
    // Installing a block reserves its whole range: now() is already the
    // block end (a safe read point -- the reserved tail holds no data).
    assert_eq!(ts.now(), 109);
    assert_eq!(ts.alloc_n(4), 100..104);
    assert_eq!(ts.now(), 109);
    assert_eq!(ts.alloc_n(6), 104..110, "block now exhausted");
    // Exhausted block -> locally-bumped fallback, still monotonic.
    let f = ts.alloc_n(2);
    assert!(ts.degraded());
    assert!(f.start > 109);
    assert_eq!(ts.now(), f.end - 1);
    // A late refill block must land strictly above the fallback grants.
    assert!(install_block(
        &mut ts.state.lock().unwrap(),
        f.end,
        f.end + 10
    ));
    assert!(ts.degraded(), "cleared by the refill paths, not by install");
    assert_eq!(ts.alloc_n(3), f.end..f.end + 3);
}

#[test]
fn now_is_local_knowledge_and_lags_cluster_grants() {
    // Documented semantics: now() tracks only what THIS node has carved,
    // fetched or observed; grants elsewhere (leader serving other nodes)
    // are invisible until the next cursor/block observation.
    let (raft, topo) = stub_leader();
    let ts = ClusterTs::new(deps_of(&raft, &topo));
    assert_eq!(ts.now(), 0);
    install_block(&mut ts.state.lock().unwrap(), 500, 900);
    assert_eq!(ts.now(), 899, "installing a block advances global_hi");
    ts.observe_floor(1000);
    assert_eq!(ts.now(), 1000, "external high-water marks fold in");
    ts.observe_floor(999);
    assert_eq!(ts.now(), 1000, "never walks backwards");
}

#[tokio::test]
async fn refill_is_inactive_until_cluster_ready() {
    let (raft, _) = stub_leader();
    let not_ready = Arc::new(RwLock::new(topology::empty()));
    let ts = ClusterTs::new(deps_of(&raft, &not_ready));
    // No block, but refill must not touch raft or allocate anything.
    ts.refill_once().await.unwrap();
    assert!(!ts.has_block());
}

#[test]
fn registry_json_round_trip() {
    let a = binds("raft-a", "http-a");
    let b = binds("raft-b", "http-b");
    let json = merged_registry("", &a).unwrap();
    assert_eq!(parse_registry(&json), [("raft-a".into(), a.clone())].into());
    // Idempotent: unchanged entry -> no write needed.
    assert_eq!(merged_registry(&json, &a), None);
    // Merge keeps other nodes' entries.
    let json2 = merged_registry(&json, &b).unwrap();
    let map = parse_registry(&json2);
    assert_eq!(map.len(), 2);
    assert_eq!(map.get("raft-a"), Some(&a));
    assert_eq!(map.get("raft-b"), Some(&b));
    // Corrupt JSON degrades to an empty registry (self re-registers).
    assert!(merged_registry("not json", &a).is_some());
    assert!(parse_registry("not json").is_empty());
    // Bind changes re-register (overwrite per raft addr).
    let a2 = binds("raft-a", "http-a-new");
    let json3 = merged_registry(&json2, &a2).unwrap();
    assert_eq!(parse_registry(&json3).len(), 2);
    assert_eq!(parse_registry(&json3).get("raft-a"), Some(&a2));
}

#[tokio::test]
async fn route_sql_ts_auth_and_leader_gate() {
    let (raft, topo) = stub_leader();
    let ts = Arc::new(ClusterTs::new(deps_of(&raft, &topo)));
    let param = |k: &str, v: &str| (k.to_string(), v.to_string());

    // Not installed -> plain 404 (route set unchanged pre-M3).
    let (s, b) = route_sql_ts(None, "tok", &[]).await;
    assert_eq!((s, b.as_str()), ("404 Not Found", "404 page not found\n"));
    // Wrong token -> the control-API 401.
    let (s, _) = route_sql_ts(
        Some(&ts),
        "tok",
        &[param("n", "4"), param("raft-token", "bad")],
    )
    .await;
    assert_eq!(s, "401 Unauthorized");
    // Follower -> "not leader".
    raft.write().unwrap().is_leader = false;
    let (s, b) = route_sql_ts(Some(&ts), "tok", &[param("raft-token", "tok")]).await;
    assert_eq!((s, b.as_str()), ("404 Not Found", "not leader\n"));
    // Leader: leases n timestamps starting above the cursor (ts 0 is
    // never granted; a fresh leader leases from 1).
    raft.write().unwrap().is_leader = true;
    let (s, b) = route_sql_ts(
        Some(&ts),
        "tok",
        &[param("n", "4"), param("raft-token", "tok")],
    )
    .await;
    assert_eq!(s, "200 OK");
    assert_eq!(b.trim(), "1 5", "fresh leader: leases [1, 5)");
    // The rest of the leader's current block serves the next lease
    // directly (no raft write while the block has room).
    let (_, b2) = route_sql_ts(
        Some(&ts),
        "tok",
        &[param("n", "2"), param("raft-token", "tok")],
    )
    .await;
    assert_eq!(b2.trim(), "5 7");
    // A lease bigger than the remaining block forces the next fetch,
    // which continues at the cursor persisted by the first one.
    let (_, b3) = route_sql_ts(
        Some(&ts),
        "tok",
        &[
            (String::from("n"), TS_BLOCK.to_string()),
            param("raft-token", "tok"),
        ],
    )
    .await;
    assert_eq!(
        b3.trim(),
        format!("{} {}", 1 + TS_BLOCK, 1 + 2 * TS_BLOCK),
        "continues at the persisted cursor"
    );
}
