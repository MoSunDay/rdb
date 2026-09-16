//! Write-frontier reservation of the autocommit DML paths (INSERT /
//! UPDATE / DELETE): one shared helper that folds the statement's
//! physical write set into ts-authority probes and reserves the
//! frontier above the statement's read point.
//!
//! The reserve must run BEFORE the commit plan is built: the plan's ts
//! range (2PC) or the local allocation stamps above the read point,
//! and the `strict` predicate (any remote owner) decides whether a
//! degraded GAP fallback is allowed at all -- a distributed commit
//! fails fast (1213, retryable) when the ts authority is unreachable
//! instead of stamping GAP versions, while purely local write sets
//! keep the degraded fallback (a same-node refill re-anchors those).

use crate::sql::dist;
use crate::sql::index::IndexOps;
use crate::sql::parse::error::SqlResult;
use crate::state::Shared;

/// Probe set + frontier reservation for one autocommit batch:
/// `row_keys` are the physical pk keys the batch stamps (tombstones of
/// pk-moving UPDATEs included), `idx` the index-entry keys landing in
/// the same batch, `versions` the ts count the batch will consume.
pub(crate) async fn reserve<'a>(
    shared: &Shared,
    table_id: u32,
    read_ts: u64,
    versions: u64,
    row_keys: impl Iterator<Item = &'a [u8]>,
    idx: &IndexOps,
) -> SqlResult<()> {
    let mut probes: Vec<Vec<u8>> = row_keys.map(|pk| dist::row_probe(table_id, pk)).collect();
    probes.extend(idx.iter().map(|(k, _)| k.clone()));
    let strict = dist::any_remote_owner(shared, &probes);
    shared
        .sql_ts
        .reserve_write_frontier(read_ts, versions, strict)
        .await
}
