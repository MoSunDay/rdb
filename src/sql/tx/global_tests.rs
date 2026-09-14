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
fn carve_never_serves_below_observed_commits() {
    // A participant applied a commit at ts 42 (`advance_to` -> global_hi),
    // while its own block was granted before that commit.
    let mut st = TsState {
        block_lo: 10,
        block_hi: 30,
        global_hi: 42,
        observed_floor: 42,
        ..TsState::default()
    };
    // Serving 10.. would stamp versions below the commit at 42; a later
    // snapshot (read_ts >= 42) would shadow them -> silently lost write.
    assert_eq!(carve(&mut st, 5), None, "poisoned tail must not serve");
    assert_eq!(
        remaining(&st),
        0,
        "stale remainder discarded so the refiller re-anchors"
    );
    assert_eq!(
        carve(&mut st, 5),
        None,
        "discarded block stays exhausted (fallback handles allocation)"
    );
    // A fresh block strictly above the observed floor keeps serving, and
    // `global_hi` covering the reserved tail is NOT staleness (install
    // folds block_hi-1 into it by design: the reserved tail is servable).
    let mut fresh = TsState {
        block_lo: 43,
        block_hi: 50,
        global_hi: 49,
        observed_floor: 42,
        ..TsState::default()
    };
    assert_eq!(carve(&mut fresh, 2), Some(43..45));
    assert_eq!(fresh.block_lo, 45);
    assert_eq!(fresh.global_hi, 49);
}

#[test]
fn alloc_n_reallocates_above_read_point_after_observed_commit() {
    // End-to-end shape of the regression: block granted [10,30), then the
    // node observes a commit at 50; the next allocation must not hand out
    // 10.. (below the read point) but a range above 50.
    let (raft, topo) = stub_leader();
    let ts = ClusterTs::new(deps_of(&raft, &topo));
    assert!(install_block(&mut ts.state.lock().unwrap(), 10, 30));
    ts.observe_floor(50);
    assert_eq!(ts.now(), 50);
    let r = ts.alloc_n(4);
    assert!(r.start > 50, "alloc {r:?} must sit above the read point");
    assert_eq!(ts.now(), r.end - 1);
    assert!(ts.degraded());
    // And the refiller's next fetch floor covers the observed commit, so
    // the fresh block re-anchors above it instead of reusing stale grants.
    assert!(install_block(
        &mut ts.state.lock().unwrap(),
        r.end + 1,
        r.end + 9
    ));
    assert_eq!(ts.alloc_n(2), r.end + 1..r.end + 3);
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

// ---- regression: frozen read point / stale write tail ----
// A node that leased an early ts block and then went idle used to keep
// `now()` at its block tail forever (an idle block is never below the
// refill low-water, so nothing re-anchored it), while the rest of the
// cluster granted on: reads filtered out newer versions, UPDATEs
// matched 0 rows, and commits stamped versions UNDER the cluster's
// newest -- newest-ts-wins buried them silently.

#[test]
fn now_rides_the_raft_cursor_frontier() {
    let (raft, topo) = stub_leader();
    let ts = ClusterTs::new(deps_of(&raft, &topo));
    // This node's block: [1, 4097). Its global_hi freezes at 4096.
    install_block(&mut ts.state.lock().unwrap(), 1, 1 + TS_BLOCK);
    // The cluster meanwhile granted through 5 blocks (raft cursor).
    let frontier = 5 * TS_BLOCK;
    raft.write()
        .unwrap()
        .kv
        .insert(TS_CURSOR_KEY.to_string(), frontier.to_string());
    assert_eq!(ts.now(), TS_BLOCK, "pre-sync: frozen at the block tail");
    ts.sync_cursor_frontier();
    assert_eq!(
        ts.now(),
        frontier - 1,
        "read point rides cursor - 1, not the stale block tail"
    );
    ts.note_cursor(frontier - TS_BLOCK);
    assert_eq!(ts.now(), frontier - 1, "cursor fold never walks back");
}

#[tokio::test]
async fn reserve_write_frontier_rebases_a_stale_tail_above_the_frontier() {
    // The defect-B write tail: this node carved [1, 4097) before the
    // cursor moved on. Committing at those timestamps buries the
    // versions under the cluster's newest, and the conflict veto cannot
    // catch it (the row's newest ts compares below the read point).
    // reserve_write_frontier must discard the stale tail and lease
    // above the frontier BEFORE the plan allocates.
    let (raft, topo) = stub_leader();
    let ts = ClusterTs::new(deps_of(&raft, &topo));
    install_block(&mut ts.state.lock().unwrap(), 1, 1 + TS_BLOCK);
    let frontier = 5 * TS_BLOCK;
    raft.write()
        .unwrap()
        .kv
        .insert(TS_CURSOR_KEY.to_string(), frontier.to_string());

    ts.reserve_write_frontier(frontier - 1, 4, false)
        .await
        .unwrap();

    assert!(!ts.degraded(), "a successful re-lease clears degradation");
    let r = ts.alloc_n(4);
    assert_eq!(
        r,
        frontier..frontier + 4,
        "the plan's ts range must clear the frontier, not reuse [1, 4097)"
    );
    assert_eq!(
        raft.read().unwrap().kv.get(TS_CURSOR_KEY).cloned(),
        Some((frontier + TS_BLOCK).to_string()),
        "the re-lease persisted its cursor before serving"
    );
    assert_eq!(ts.now(), frontier + TS_BLOCK - 1, "read point follows");
}

#[tokio::test]
async fn reserve_write_frontier_keeps_a_tail_already_above_the_frontier() {
    // A fresh tail is untouched: no discard, no raft write, no fetch.
    let (raft, topo) = stub_leader();
    let ts = ClusterTs::new(deps_of(&raft, &topo));
    let lo = 5 * TS_BLOCK;
    install_block(&mut ts.state.lock().unwrap(), lo, lo + TS_BLOCK);
    raft.write()
        .unwrap()
        .kv
        .insert(TS_CURSOR_KEY.to_string(), lo.to_string());

    ts.reserve_write_frontier(3 * TS_BLOCK, 4, false)
        .await
        .unwrap();

    assert_eq!(ts.alloc_n(4), lo..lo + 4, "fresh tail still serves");
    assert_eq!(
        raft.read().unwrap().kv.get(TS_CURSOR_KEY).cloned(),
        Some(lo.to_string()),
        "no extra lease was taken"
    );
}

#[tokio::test]
async fn reserve_write_frontier_refetches_a_short_tail_above_the_floor() {
    // The write-tail defect, size variant: the lease sits ABOVE the
    // read frontier but is shorter than the imminent plan's alloc (a
    // refiller tick drained it mid-burst). The alloc never fetches, so
    // it would degrade to the GAP fallback -- stamps no cursor ride can
    // ever cover, invisible to every later reader. The reserve must
    // treat a short tail exactly like a stale one: discard and lease
    // enough above the frontier.
    let (raft, topo) = stub_leader();
    let ts = ClusterTs::new(deps_of(&raft, &topo));
    let lo = 5 * TS_BLOCK;
    // Above any floor, but only 8 stamps left for a 4000-ts plan. The
    // cursor sits at the lease end (the leader authorized exactly this
    // block), so the re-lease must start there.
    install_block(
        &mut ts.state.lock().unwrap(),
        lo + TS_BLOCK - 8,
        lo + TS_BLOCK,
    );
    raft.write()
        .unwrap()
        .kv
        .insert(TS_CURSOR_KEY.to_string(), (lo + TS_BLOCK).to_string());

    ts.reserve_write_frontier(lo, 4000, false).await.unwrap();

    let r = ts.alloc_n(4000);
    assert_eq!(
        r,
        (lo + TS_BLOCK)..(lo + TS_BLOCK + 4000),
        "a short tail must be re-leased, never served from the GAP fallback"
    );
    assert!(!ts.degraded(), "the re-lease cleared degradation");
}

#[tokio::test]
async fn alloc_above_degrades_only_when_the_leader_is_unreachable() {
    // The fallback is the last resort, not a service path: with a live
    // leader, reserve_write_frontier must hand the alloc a real block.
    let (raft, topo) = stub_leader();
    let ts = ClusterTs::new(deps_of(&raft, &topo));
    ts.reserve_write_frontier(0, 2 * TS_BLOCK, false)
        .await
        .unwrap();
    assert_eq!(
        ts.alloc_n(2 * TS_BLOCK),
        1..1 + 2 * TS_BLOCK,
        "a fresh node leases a real block, not the GAP fallback"
    );
    assert!(!ts.degraded());
}

/// A follower with an unresolvable leader (empty `sql_nodes` registry:
/// `leader_http_addr` -> None) makes every block fetch fail.
fn unreachable_follower() -> ClusterTs {
    let st = RaftState {
        is_leader: false,
        leader_addr: "raft-x".to_string(),
        kv: BTreeMap::new(),
        ..RaftState::default()
    };
    let topo = Arc::new(RwLock::new(topology::refresh("a,b,c")));
    ClusterTs::new(deps_of(&Arc::new(RwLock::new(st)), &topo))
}

#[tokio::test]
async fn reserve_strict_fails_rather_than_stamp_a_gap() {
    // TSO discipline: a distributed commit (write set spans remote
    // owners) must NEVER stamp GAP-fallback versions -- no later cursor
    // ride can cover them, and newest-ts-wins would bury the write
    // silently. When the ts authority is unreachable the strict reserve
    // fails with the retryable authority error instead.
    let ts = unreachable_follower();
    let err = ts
        .reserve_write_frontier(0, 8, true)
        .await
        .expect_err("strict reserve must fail fast");
    assert_eq!(err, TS_AUTHORITY_UNREACHABLE);
    assert!(
        !ts.degraded(),
        "no alloc happened: the strict path stamped nothing"
    );
}

#[tokio::test]
async fn reserve_lenient_keeps_the_local_degraded_fallback() {
    // A purely local write keeps the frozen degraded semantics: the
    // lenient reserve stays Ok and the alloc falls back above
    // `last_cursor + TS_FALLBACK_GAP`, which a same-node refill later
    // re-anchors (see `alloc_above_degrades_only_when_the_leader_is_unreachable`).
    let ts = unreachable_follower();
    ts.reserve_write_frontier(0, 8, false)
        .await
        .expect("lenient reserve keeps best-effort semantics");
    let r = ts.alloc_n(8);
    assert!(ts.degraded(), "unreachable authority: local alloc degrades");
    assert_eq!(
        r,
        TS_FALLBACK_GAP + 1..TS_FALLBACK_GAP + 9,
        "the fallback jumps the gap above the (absent) cursor"
    );
}
