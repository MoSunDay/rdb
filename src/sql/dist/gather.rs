//! M3 scatter-gather reads: distributed single-table table scans.
//!
//! ## Why
//! A node's store holds only its slot band, so a local scan sees a
//! slice of the table. Reads fan out instead: the coordinator cuts
//! 0..=16383 into per-owner bands ([`super::bands`]), scans ITS band
//! locally through the very same `visible_versions_between` core the
//! single-node path uses, and asks every other owner concurrently for
//! its band via one `ScanBand` RPC. Bands are disjoint and a pk's slot
//! is a pure function of the pk, so each pk arrives from exactly one
//! owner: merging into a pk-keyed map is duplicate-free by
//! construction (aggregates over gathered rows never double-count).
//!
//! ## Semantics
//! - `read_ts` is pinned by the CALLER: the txn's `read_ts` inside an
//!   explicit BEGIN (repeatable read holds through the gather -- the
//!   participants filter `ts <= read_ts` remotely), the oracle's
//!   `now()` otherwise. The row bytes a participant returns are the
//!   raw version payloads its local scan would decode, so gathered
//!   rows are indistinguishable from locally scanned ones.
//! - The txn overlay (`tx::merge_rows`) and the whole downstream
//!   pipeline (filter, aggregates/GROUP BY, order, limit) run on the
//!   coordinator over the MERGED rows -- single code path, single
//!   node behavior when the cluster is not ready.
//! - v1 scope: plain single-table FROMs only (JOINs keep the local
//!   nested-loop materialization) and no index usage in cluster mode
//!   (a node's secondary indexes only cover its band, so an
//!   IndexLookup could silently miss remote rows -- the planner is
//!   bypassed entirely on the gather path).
//!
//! ## Failures
//! A node that cannot be reached (or errors mid-scan) fails the WHOLE
//! query with SQL error 1027 ("cluster node ... unreachable"):
//! partial results would be silently wrong, so none are served. One
//! immediate retry per node covers transient transport blips; HA
//! failover of SQL reads (backup bands, retries against re-formed
//! topologies) is future work. Stale rows a node still holds outside
//! its current band (pre-cluster writes) are NOT gathered -- the
//! bands describe ownership, not physical layout.

use std::collections::BTreeMap;

use futures::future::join_all;

use super::client;
use super::proto::{Req, Resp};
use super::server::sql_rpc_of;
use super::{bands, routing, Band};
use crate::sql::exec::scan::{self, table_side, FromScope, Source};
use crate::sql::parse::ast::{Expr, TableRef};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::row::{self, HEADER_LIVE};
use crate::sql::storage::schema::{TableSchema, Value};
use crate::sql::tx::Txn;
use crate::state::Shared;

/// Scatter-gather applies to this FROM when the cluster is ready with
/// more than one node and the FROM is one plain table; the answer is
/// the per-owner band list (this node's band included).
pub fn gatherable(shared: &Shared, tref: &TableRef) -> Option<Vec<Band>> {
    let TableRef::Table { .. } = tref else {
        return None; // joins stay local-scan v1
    };
    let r = routing(shared)?;
    (r.addrs.len() > 1).then(|| bands(&r))
}

/// EXPLAIN headline of the distributed plan ("Gather(bands=N)").
pub fn headline(shared: &Shared, tref: &TableRef) -> Option<String> {
    gatherable(shared, tref).map(|bs| format!("Gather(bands={})", bs.len()))
}

/// Gather-aware FROM materialization for SELECTs. Single plain table
/// in a ready multi-node cluster -> band scatter-gather; everything
/// else -> the exact single-node `scan::materialize` path (joins, or
/// no cluster at all). The residual filter stays downstream in both
/// paths; it is only forwarded to the local planner in the fallback.
pub async fn materialize(
    shared: &Shared,
    tref: &TableRef,
    read_ts: u64,
    txn: Option<&Txn>,
    filter: Option<&Expr>,
) -> SqlResult<Source> {
    let TableRef::Table { name, alias } = tref else {
        return scan::materialize(shared, tref, read_ts, txn, filter);
    };
    let Some(bs) = gatherable(shared, tref) else {
        return scan::materialize(shared, tref, read_ts, txn, filter);
    };
    let schema = catalog::lookup(shared, name)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(name))?;
    let rows = gather_rows(shared, &bs, &schema, read_ts).await?;
    let rows = match txn {
        Some(t) => crate::sql::tx::merge_rows(&schema, rows, t)?,
        None => rows,
    };
    let mut scope = FromScope::default();
    scope.sides.push(table_side(&schema, alias));
    Ok(Source { scope, rows })
}

/// Union of every band's rows visible at `read_ts`, ordered by pk_key
/// bytes (the same deterministic order a local scan produces). The
/// self band scans this store; every remote owner answers one
/// concurrent `ScanBand`. First failure aborts the whole read.
async fn gather_rows(
    shared: &Shared,
    bs: &[Band],
    schema: &TableSchema,
    read_ts: u64,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut merged: BTreeMap<Vec<u8>, Vec<Value>> = BTreeMap::new();
    let mut remote = Vec::new();
    for b in bs {
        if b.owner == shared.conf.bind {
            let rows = scan::visible_versions_between(&shared.store, schema, read_ts, b.lo, b.hi)?;
            for (pk, raw) in rows {
                merged.insert(pk, decode_band_row(schema, &raw)?);
            }
        } else {
            remote.push(scan_remote(shared, schema, b, read_ts));
        }
    }
    for (owner, band) in join_all(remote).await {
        match band {
            Ok(rows) => {
                for (pk, raw) in rows {
                    merged.insert(pk, decode_band_row(schema, &raw)?);
                }
            }
            Err(why) => {
                return Err(SqlError::new(
                    ErrorCode::NodeUnreachable,
                    format!("cluster node {owner} unreachable: {why}"),
                ))
            }
        }
    }
    Ok(merged.into_values().collect())
}

/// One remote owner's band scan: resolve its sql_rpc port through the
/// raft-replicated `sql_nodes` registry, exchange one request. The
/// Err side carries the REASON; the caller renders the node error.
async fn scan_remote(
    shared: &Shared,
    schema: &TableSchema,
    band: &Band,
    read_ts: u64,
) -> (String, Result<Vec<(Vec<u8>, Vec<u8>)>, String>) {
    let owner = band.owner.clone();
    let req = Req::ScanBand {
        table_id: schema.id,
        slot_lo: band.lo,
        slot_hi: band.hi,
        read_ts,
    };
    let res = match sql_rpc_of(shared, &owner) {
        None => Err("no sql_rpc registration".to_string()),
        Some(addr) => request_band(&addr, &req).await,
    };
    (owner, res)
}

/// One ScanBand exchange with a single immediate retry on transport
/// errors (connection blips); participant-side scan failures are
/// deterministic and never retried.
async fn request_band(sql_rpc: &str, req: &Req) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
    match client::request(sql_rpc, req).await {
        Ok(Resp::BandRows { rows, error }) if error.is_empty() => Ok(rows),
        Ok(Resp::BandRows { error, .. }) => Err(format!("scan failed: {error}")),
        Ok(other) => Err(format!("unexpected reply {other:?}")),
        Err(e) => match client::request(sql_rpc, req).await {
            Ok(Resp::BandRows { rows, error }) if error.is_empty() => Ok(rows),
            Ok(Resp::BandRows { error, .. }) => Err(format!("{e}; retry scan failed: {error}")),
            second => Err(format!("{e}; retry {second:?}")),
        },
    }
}

/// Decode one gathered version payload. Participants only send live
/// versions; a foreign header means a protocol bug, fail loudly
/// rather than resurrect or drop rows silently.
fn decode_band_row(schema: &TableSchema, raw: &[u8]) -> SqlResult<Vec<Value>> {
    let (header, values) = row::decode_version(schema, raw).map_err(SqlError::from)?;
    if header != HEADER_LIVE {
        return Err(SqlError::new(
            ErrorCode::Unknown,
            format!("gather received a non-live version (header {header:#x})"),
        ));
    }
    Ok(values)
}

#[cfg(test)]
#[path = "gather_tests.rs"]
mod tests;
