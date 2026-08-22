//! The 2PC coordinator: one function per distributed commit.
//!
//! Phase 1 asks every slot-owner to validate + stage its slice (the
//! coordinator's own slice goes through the SAME participant code, by
//! direct call instead of TCP). Any veto or transport failure aborts
//! everything staged so far -- the veto reason rides out to the
//! client as the matching SQL error (`conflict:` -> 1213,
//! `dup:` -> 1062, unreachable participant -> 1213, a retriable
//! write conflict from the client's point of view).
//!
//! Phase 2 first writes the decision to the LOCAL outcome record
//! (`sql2pc_out/<txn_id>`, fsync) -- only then does any participant
//! learn it. A participant that misses the Decide recovers it from
//! the coordinator's `/sql2pc/status`; a lost Ack is retried in the
//! background for a while (idempotent Decide), after which the
//! participant's own recovery sweep finishes the job.

use std::collections::BTreeMap;
use std::sync::Arc;

use rocksdb::WriteBatch;

use super::client;
use super::participant;
use super::plan::CommitPlan;
use super::proto::{Req, Resp};
use super::server::sql_rpc_of;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::state::Shared;
use crate::store::ops;

/// How long the coordinator keeps retrying an unacked Decide in the
/// background (the participant's recovery covers anything longer).
const ACK_RETRY_SECS: u64 = 60;

/// Run one distributed commit to completion.
pub async fn run(shared: &Shared, plan: &CommitPlan) -> SqlResult<()> {
    let mut voted: Vec<String> = Vec::new();
    let mut veto: Option<SqlError> = None;
    for (addr, pp) in &plan.participants {
        match prepare_one(shared, plan, addr, pp).await {
            Ok(()) => voted.push(addr.clone()),
            Err(e) => {
                veto = Some(e);
                break;
            }
        }
    }
    match veto {
        Some(e) => {
            // Decide abort on everyone that already staged, self first.
            decide_all(shared, plan, false, &voted).await;
            Err(e)
        }
        None => {
            decide_all(shared, plan, true, &voted).await;
            Ok(())
        }
    }
}

/// Phase 1 for one participant: `Ok` = voted yes and staged.
async fn prepare_one(
    shared: &Shared,
    plan: &CommitPlan,
    addr: &str,
    pp: &super::plan::ParticipantPlan,
) -> Result<(), SqlError> {
    if addr == shared.conf.bind {
        let store = Arc::clone(&shared.store);
        let (txn_id, coord, ts, read_ts) = (
            plan.txn_id.clone(),
            plan.coordinator_http.clone(),
            plan.commit_ts,
            plan.read_ts,
        );
        let entries = pp.entries.clone();
        let vote = tokio::task::spawn_blocking(move || {
            participant::vote(&store, &txn_id, &coord, ts, read_ts, &entries)
        })
        .await
        .map_err(|e| spill("join", e.to_string()))?
        .map_err(|e| spill("prepare self", e))?;
        return match vote {
            participant::Vote::Yes => Ok(()),
            participant::Vote::No(reason) => Err(map_veto(reason)),
        };
    }
    let sql_rpc = sql_rpc_of(shared, addr)
        .ok_or_else(|| unreachable_peer(addr, "no sql_rpc registration".to_string()))?;
    let req = Req::Prepare {
        txn_id: plan.txn_id.clone(),
        coordinator: plan.coordinator_http.clone(),
        commit_ts: plan.commit_ts,
        read_ts: plan.read_ts,
        entries: pp.entries.clone(),
    };
    match client::request(&sql_rpc, &req).await {
        Ok(Resp::Vote { yes: true, .. }) => Ok(()),
        Ok(Resp::Vote { yes: false, reason }) => Err(map_veto(reason)),
        Ok(other) => Err(unreachable_peer(
            addr,
            format!("unexpected reply {other:?}"),
        )),
        Err(e) => Err(unreachable_peer(addr, e)),
    }
}

/// Phase 2: durable outcome first, then Decide to every participant
/// (`voted` = phase-1 survivors; on abort a participant that never
/// voted simply has no marker and no-ops). Unacked remote Decides get
/// a bounded background retry.
async fn decide_all(shared: &Shared, plan: &CommitPlan, commit: bool, voted: &[String]) {
    let index_ops: BTreeMap<String, Vec<_>> = plan
        .participants
        .iter()
        .map(|(addr, pp)| (addr.clone(), pp.index_ops.clone()))
        .collect();
    let record = participant::coordinator_outcome(commit, plan.commit_ts, index_ops);
    let mut batch = WriteBatch::default();
    batch.put(
        participant::outcome_key(&plan.txn_id),
        serde_json::to_vec(&record).unwrap_or_default(),
    );
    // fsync BEFORE any Decide leaves this node: once a participant can
    // see a commit decision, every future status query must too.
    if let Err(e) = ops::batch_write_async(Arc::clone(&shared.store), batch).await {
        eprintln!("sql2pc: outcome write failed for {}: {e}", plan.txn_id);
    }
    for addr in voted {
        let ops_for_addr = plan
            .participants
            .get(addr)
            .map(|pp| pp.index_ops.clone())
            .unwrap_or_default();
        if addr == &shared.conf.bind {
            let store = Arc::clone(&shared.store);
            let txn_id = plan.txn_id.clone();
            let _ = tokio::task::spawn_blocking(move || {
                participant::decide(&store, &txn_id, commit, &ops_for_addr)
            })
            .await;
            // Self slice: the coordinator allocated the ts range, so its
            // read point already sits at/above the commit ts.
            continue;
        }
        let Some(sql_rpc) = sql_rpc_of(shared, addr) else {
            eprintln!("sql2pc: {} lost sql_rpc registration mid-txn", addr);
            continue;
        };
        let req = Req::Decide {
            txn_id: plan.txn_id.clone(),
            commit,
            watermark: plan.watermark,
            index_ops: ops_for_addr,
        };
        match client::request(&sql_rpc, &req).await {
            Ok(Resp::Ack) | Ok(Resp::Vote { .. }) => {}
            Ok(other) => retry_decide(sql_rpc, req, format!("unexpected reply {other:?}")),
            Err(e) => retry_decide(sql_rpc, req, e),
        }
    }
}

/// Background retry of an unacked Decide (idempotent): once per second
/// for ACK_RETRY_SECS, then the participant's recovery sweep owns it.
fn retry_decide(sql_rpc: String, req: Req, why: String) {
    eprintln!("sql2pc: decide unacked ({why}); retrying in background");
    tokio::spawn(async move {
        for _ in 0..ACK_RETRY_SECS {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            match client::request(&sql_rpc, &req).await {
                Ok(_) => return,
                Err(e) => eprintln!("sql2pc: decide retry failed: {e}"),
            }
        }
    });
}

/// Veto/transport reason -> client error. Unknown prefixes (e.g. a
/// participant's internal `error:` spill) surface as WriteConflict
/// too: the txn definitively did not commit, and the client may retry.
pub fn map_veto(reason: String) -> SqlError {
    if reason.starts_with("dup:") {
        SqlError::new(ErrorCode::DupEntry, reason)
    } else {
        SqlError::new(ErrorCode::WriteConflict, reason)
    }
}

fn unreachable_peer(addr: &str, why: String) -> SqlError {
    SqlError::new(
        ErrorCode::WriteConflict,
        format!("2pc participant {addr} unreachable: {why}"),
    )
}

fn spill(stage: &str, why: String) -> SqlError {
    SqlError::new(
        ErrorCode::WriteConflict,
        format!("2pc {stage} failed: {why}"),
    )
}
