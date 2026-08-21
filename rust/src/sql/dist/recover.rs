//! In-doubt txn recovery: a startup pass plus a periodic sweep that
//! finishes or aborts every `sql2pc/` marker this node still holds.
//!
//! Per marker (this node was a participant and never saw the Decide):
//!
//! 1. ask the coordinator's HTTP control API `/sql2pc/status` -- a
//!    Committed answer carries OUR index ops, so even a lost Decide is
//!    fully replayable;
//! 2. an Aborted answer runs the abort batch;
//! 3. Unknown/unreachable markers older than the 60s lease abort
//!    locally: the coordinator wrote any real outcome BEFORE its
//!    first Decide, so an outcome that cannot be found anywhere for a
//!    full lease means no participant was ever told to commit.
//!
//! The same loop garbage-collects outcome records (`sql2pc_out/`)
//! older than ~5 minutes: by then every participant has either
//! applied the decision or timed out its lease and aborted.

use std::sync::Arc;

use rocksdb::WriteBatch;

use super::participant::{self, Marker};
use super::{now_secs, OUTCOME_GC_SECS};
use crate::state::Shared;
use crate::store::ops;

/// Sweep cadence; also the recovery delay after startup.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Spawn the recovery loop (idempotent per process; main.rs owns it).
pub fn spawn_recover(shared: Arc<Shared>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        ticker.tick().await; // first real pass after one interval
        loop {
            ticker.tick().await;
            if let Err(e) = sweep_once(&shared).await {
                eprintln!("sql2pc recovery: sweep failed: {e}");
            }
        }
    });
}

/// One recovery + GC pass over this node's 2PC state.
pub async fn sweep_once(shared: &Shared) -> Result<(), String> {
    for (txn_id, marker) in participant::markers(&shared.store) {
        match participant::read_outcome(&shared.store, &txn_id) {
            // Decided here already: the marker is gone in the same
            // batch, so a leftover pair means a mid-decide crash --
            // replaying the decision is idempotent, so just do it.
            Some(rec) => {
                let ops = if rec.commit {
                    rec.own_ops.clone()
                } else {
                    Vec::new()
                };
                decide_blocking(shared, &txn_id, rec.commit, &ops).await?;
            }
            None => resolve_in_doubt(shared, &txn_id, &marker).await?,
        }
    }
    gc_outcomes(&shared.store)?;
    Ok(())
}

/// Resolve one marker with no local outcome: coordinator status first,
/// then every other cluster node (any participant that already applied
/// a Decide holds a committed record), lease expiry as the fallback.
async fn resolve_in_doubt(shared: &Shared, txn_id: &str, marker: &Marker) -> Result<(), String> {
    let mut peers: Vec<String> = vec![marker.coordinator.clone()];
    {
        let topo = shared.topology.read().unwrap();
        for addr in &topo.stable_addrs {
            if let Some(binds) = crate::sql::tx::nodes::binds_by_resp(&shared.raft, addr) {
                if !binds.http.is_empty() && !peers.contains(&binds.http) {
                    peers.push(binds.http);
                }
            }
        }
    }
    let mut definite_abort = false;
    for peer in &peers {
        if let Some((commit, ops)) = ask_node(shared, peer, txn_id).await {
            if commit {
                // A committed outcome anywhere wins: the deciding
                // node's own slice already committed.
                return decide_blocking(shared, txn_id, true, &ops).await;
            }
            definite_abort = true;
        }
    }
    if definite_abort || participant::lease_expired(marker) {
        if !definite_abort {
            eprintln!("sql2pc recovery: lease expired, aborting in-doubt txn {txn_id}");
        }
        return decide_blocking(shared, txn_id, false, &[]).await;
    }
    Ok(()) // still inside the lease: next sweep re-asks
}

/// HTTP status query to one node; None = Unknown/unreachable/aborted
/// is returned as Some only when definite -- see `parse_status_body`.
async fn ask_node(
    shared: &Shared,
    http_addr: &str,
    txn_id: &str,
) -> Option<(bool, Vec<super::proto::WireOp>)> {
    let url = format!(
        "http://{http_addr}/sql2pc/status?id={}&node={}&raft-token={}",
        crate::rcache::join::percent_encode(txn_id),
        crate::rcache::join::percent_encode(&shared.conf.bind),
        crate::rcache::join::percent_encode(&shared.conf.raft_token),
    );
    let (status, body) = crate::rcache::join::http_get_status(&url).await.ok()?;
    if status != 200 {
        return None;
    }
    parse_status_body(&body)
}

/// Parse `/sql2pc/status`'s body:
/// `committed <json-index-ops>` | `aborted` | anything else = unknown.
pub fn parse_status_body(body: &str) -> Option<(bool, Vec<super::proto::WireOp>)> {
    let body = body.trim();
    if body == "aborted" {
        return Some((false, Vec::new()));
    }
    let ops = body.strip_prefix("committed ")?;
    let ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = serde_json::from_str(ops).ok()?;
    Some((true, ops))
}

/// Run one decide off the async workers (the store write fsyncs).
async fn decide_blocking(
    shared: &Shared,
    txn_id: &str,
    commit: bool,
    ops: &[(Vec<u8>, Option<Vec<u8>>)],
) -> Result<(), String> {
    let store = Arc::clone(&shared.store);
    let txn_id = txn_id.to_string();
    let ops = ops.to_vec();
    let applied =
        tokio::task::spawn_blocking(move || participant::decide(&store, &txn_id, commit, &ops))
            .await
            .map_err(|e| e.to_string())??;
    if commit {
        // Same reasoning as the Decide dispatch: recovery may apply a
        // commit whose ts this node never granted itself.
        shared.sql_ts.advance_to(applied);
    }
    Ok(())
}

/// Delete outcome records older than OUTCOME_GC_SECS.
fn gc_outcomes(store: &crate::store::Store) -> Result<(), String> {
    let stale: Vec<Vec<u8>> = participant::outcomes(store)
        .into_iter()
        .filter(|(_, rec)| now_secs().saturating_sub(rec.written_at) > OUTCOME_GC_SECS)
        .map(|(id, _)| participant::outcome_key(&id))
        .collect();
    if stale.is_empty() {
        return Ok(());
    }
    let mut batch = WriteBatch::default();
    for key in stale {
        batch.delete(key);
    }
    ops::batch_write(store, batch)
}

/// HTTP handler behind `/sql2pc/status`: token gate, then the local
/// outcome table via [`status_body`]. `None` store keeps the pre-M3
/// route set (plain 404).
pub fn route_status(
    store: Option<&std::sync::Arc<crate::store::Store>>,
    token: &str,
    params: &[(String, String)],
) -> (&'static str, String) {
    use crate::rcache::http::first_param;
    if first_param(params, "raft-token") != token {
        return ("401 Unauthorized", "unauthorized\n".to_string());
    }
    match store {
        Some(st) => (
            "200 OK",
            status_body(st, first_param(params, "id"), first_param(params, "node")),
        ),
        None => ("404 Not Found", "404 page not found\n".to_string()),
    }
}

/// Outcome rendering for [`route_status`]: body text as parsed by
/// [`parse_status_body`], `unknown` when this node never recorded the
/// txn. The `node` param selects that participant's index ops from a
/// coordinator's record; a participant answers with its own ops.
pub fn status_body(store: &crate::store::Store, txn_id: &str, node: &str) -> String {
    match participant::read_outcome(store, txn_id) {
        Some(rec) if !rec.commit => "aborted\n".to_string(),
        Some(rec) => {
            let ops = if rec.index_ops.is_empty() {
                rec.own_ops.clone()
            } else {
                rec.index_ops.get(node).cloned().unwrap_or_default()
            };
            format!(
                "committed {}\n",
                serde_json::to_string(&ops).unwrap_or_else(|_| "[]".to_string())
            )
        }
        None => "unknown\n".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_body_roundtrips_through_parser() {
        let ops = vec![(vec![1, 2], Some(vec![3])), (vec![4], None)];
        let body = format!("committed {}", serde_json::to_string(&ops).unwrap());
        let (commit, back) = parse_status_body(&body).expect("parse");
        assert!(commit);
        assert_eq!(back, ops);
        assert_eq!(parse_status_body("aborted"), Some((false, Vec::new())));
        assert_eq!(parse_status_body("unknown"), None);
        assert_eq!(parse_status_body("garbage"), None);
    }
}
