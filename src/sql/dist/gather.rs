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
//! - JOINs materialize each side with this same gather logic (row
//!   tables per band, columnar tables to every member) and then run
//!   the shared nested loop (`scan::join_sources`) on the coordinator
//!   -- a join reading only the local node's slice of a side would be
//!   silent partial data, so joins NEVER fall back to local scans in
//!   a ready cluster. No index usage in cluster mode remains (a
//!   node's secondary indexes only cover its band, so an IndexLookup
//!   could silently miss remote rows -- the planner is bypassed
//!   entirely on the gather path).
//! - Columnar tables fan out to EVERY member instead of slot bands:
//!   segments commit where the txn closed, so any node may hold some
//!   of the table's segments. Each node answers one `ScanColumnar`
//!   with its locally visible rows and the coordinator concatenates
//!   (no dedup: a segment lives on exactly one node).
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
use super::{bands, routing, Band, Routing};
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
/// the per-owner band list (this node's band included). JOIN trees
/// are answered per-side by `materialize` (see `join_gathers` for the
/// EXPLAIN verdict), not by this function.
pub fn gatherable(shared: &Shared, tref: &TableRef) -> Option<Vec<Band>> {
    let TableRef::Table { .. } = tref else {
        return None; // join trees gather per-side in `materialize`
    };
    let r = routing(shared)?;
    (r.addrs.len() > 1).then(|| bands(&r))
}

/// EXPLAIN headline of the distributed plan ("Gather(bands=N)", or
/// "Gather(columnar, nodes=N)" when the FROM is a plain columnar
/// table, which fans out to every member instead of slot bands; a
/// JOIN tree whose sides fan out reads "Gather(join)").
pub fn headline(shared: &Shared, tref: &TableRef) -> Option<String> {
    if let TableRef::Table { name, .. } = tref {
        if let Ok(Some(schema)) = catalog::lookup(shared, name) {
            if schema.engine.is_columnar() {
                let r = routing(shared)?;
                return (r.addrs.len() > 1)
                    .then(|| format!("Gather(columnar, nodes={})", r.addrs.len()));
            }
        }
    }
    if matches!(tref, TableRef::Join { .. }) && join_gathers(shared, tref) {
        return Some("Gather(join)".to_string());
    }
    gatherable(shared, tref).map(|bs| format!("Gather(bands={})", bs.len()))
}

/// Whether any leaf table of a JOIN tree fans out in the current
/// topology (row tables per band, columnar tables to every member) --
/// the EXPLAIN verdict mirroring what `materialize` will do per-side.
fn join_gathers(shared: &Shared, tref: &TableRef) -> bool {
    match tref {
        TableRef::Table { name, .. } => match catalog::lookup(shared, name) {
            Ok(Some(s)) if s.engine.is_columnar() => {
                routing(shared).is_some_and(|r| r.addrs.len() > 1)
            }
            _ => gatherable(shared, tref).is_some(),
        },
        TableRef::Join { left, right, .. } => {
            join_gathers(shared, left) || join_gathers(shared, right)
        }
    }
}

/// Gather-aware FROM materialization for SELECTs. Single plain table
/// in a ready multi-node cluster -> band scatter-gather (columnar ->
/// every-member fan-out); JOIN trees -> each side through this same
/// logic, then the shared nested loop on the coordinator; everything
/// else -> the exact single-node `scan::materialize` path (no cluster
/// at all). The residual filter stays downstream in all paths; it is
/// only forwarded to the local planner in the fallback.
pub async fn materialize(
    shared: &Shared,
    tref: &TableRef,
    read_ts: u64,
    txn: Option<&Txn>,
    filter: Option<&Expr>,
) -> SqlResult<Source> {
    // JOIN trees: every side materializes gather-aware (row tables per
    // band, columnar to every member) and the shared nested loop runs
    // on the coordinator. A local-only side would silently drop other
    // nodes' rows, so joins never take the single-node fallback while
    // the cluster is ready.
    if let TableRef::Join { left, right, on } = tref {
        let l = Box::pin(materialize(shared, left, read_ts, txn, None)).await?;
        let r = Box::pin(materialize(shared, right, read_ts, txn, None)).await?;
        return scan::join_sources(l, r, on.as_ref());
    }
    // Columnar tables: segments commit where the txn closed, so a
    // ready multi-node cluster fans out to EVERY member (no slot
    // bands); single-node reads stay on the local segment scan. The
    // residual filter applies downstream, exactly like band gathers.
    if let TableRef::Table { name, alias } = tref {
        let schema = catalog::lookup(shared, name)
            .map_err(SqlError::from)?
            .ok_or_else(|| SqlError::no_such_table(name))?;
        if schema.engine.is_columnar() {
            let mut rows = match routing(shared) {
                Some(r) if r.addrs.len() > 1 => {
                    gather_columnar(shared, &r, &schema, read_ts).await?
                }
                _ => crate::sql::columnar::reader::scan_local(shared, &schema, read_ts, None)?,
            };
            if let Some(t) = txn {
                if let Some(ov) = t.appends.get(&schema.name) {
                    rows.extend(ov.iter().cloned());
                }
            }
            let mut scope = FromScope::default();
            scope.sides.push(scan::table_side(&schema, alias));
            return Ok(Source { scope, rows });
        }
    }
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

/// Union of every node's locally visible columnar rows at `read_ts`.
/// Self scans its store; every other member answers one `ScanColumnar`.
/// Concatenation (no dedup: segments live on exactly one node). First
/// failure aborts the whole read, same contract as `gather_rows`.
async fn gather_columnar(
    shared: &Shared,
    r: &Routing,
    schema: &TableSchema,
    read_ts: u64,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    let mut remote = Vec::new();
    for addr in &r.addrs {
        if *addr == shared.conf.bind {
            rows.extend(crate::sql::columnar::reader::scan_local(
                shared, schema, read_ts, None,
            )?);
        } else {
            remote.push(scan_columnar_remote(shared, schema, addr.clone(), read_ts));
        }
    }
    for (owner, part) in join_all(remote).await {
        match part {
            Ok(part_rows) => rows.extend(part_rows),
            Err(why) => {
                return Err(SqlError::new(
                    ErrorCode::NodeUnreachable,
                    format!("cluster node {owner} unreachable: {why}"),
                ))
            }
        }
    }
    Ok(rows)
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

/// One remote member's columnar scan: resolve its sql_rpc port through
/// the raft-replicated `sql_nodes` registry, exchange one
/// `ScanColumnar`. The Err side carries the REASON; the caller renders
/// the node error.
async fn scan_columnar_remote(
    shared: &Shared,
    schema: &TableSchema,
    owner: String,
    read_ts: u64,
) -> (String, Result<Vec<Vec<Value>>, String>) {
    let req = Req::ScanColumnar {
        table_id: schema.id,
        read_ts,
    };
    let res = match sql_rpc_of(shared, &owner) {
        None => Err("no sql_rpc registration".to_string()),
        Some(addr) => request_columnar(&addr, &req).await,
    };
    (owner, res)
}

/// One ScanColumnar exchange with a single immediate retry on transport
/// errors (connection blips); participant-side scan failures are
/// deterministic and never retried.
async fn request_columnar(sql_rpc: &str, req: &Req) -> Result<Vec<Vec<Value>>, String> {
    match client::request(sql_rpc, req).await {
        Ok(Resp::ColumnarRows { rows, error }) if error.is_empty() => Ok(rows),
        Ok(Resp::ColumnarRows { error, .. }) => Err(format!("scan failed: {error}")),
        Ok(other) => Err(format!("unexpected reply {other:?}")),
        Err(e) => match client::request(sql_rpc, req).await {
            Ok(Resp::ColumnarRows { rows, error }) if error.is_empty() => Ok(rows),
            Ok(Resp::ColumnarRows { error, .. }) => Err(format!("{e}; retry scan failed: {error}")),
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
