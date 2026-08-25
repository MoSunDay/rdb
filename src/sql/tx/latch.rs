//! Row-latch registry for locking reads (`SELECT ... FOR UPDATE /
//! FOR SHARE`).
//!
//! ## Why
//! Snapshot reads alone cannot express "read these rows and keep other
//! transactions away from them until I commit" -- a plain snapshot
//! SELECT pins nothing, and the write-write validation only fires when
//! BOTH sides write. Locking reads therefore take an in-memory latch
//! on every matched row's primary key, held by the owning txn's id
//! until COMMIT/ROLLBACK.
//!
//! ## Semantics (deliberate simplifications, kept deterministic)
//! - **Fail fast, never block**: a conflicting latch held by another
//!   live txn errors immediately with MySQL's 1205
//!   (`Lock wait timeout exceeded`) instead of sleeping through
//!   `innodb_lock_wait_timeout` -- tests and clients stay
//!   deterministic.
//! - **All-or-nothing acquisition**: a lock request either latches
//!   every key or none (no partial holdings to unwind).
//! - **Re-entrant**: re-acquiring a key the owner already holds is a
//!   no-op (FOR SHARE upgrading to FOR SHARE again, or a second FOR
//!   UPDATE in the same txn).
//! - **Shared vs exclusive**: `FOR SHARE` composes across txns;
//!   `FOR UPDATE` excludes everyone (including shared holders).
//! - **Node-local**: latches live in this process only, so cluster
//!   topologies veto locking reads instead of degrading silently.
//! - **ROLLBACK TO SAVEPOINT keeps the latches taken BEFORE the
//!   savepoint** (MySQL keeps row locks acquired before the savepoint)
//!   and releases the ones taken after it, together with the undone
//!   writes -- see `session::rollback_to`.
//!
//! Pure functions over a `LatchMap` (`*_in`); the process-wide
//! registry is one `Mutex<BTreeMap>` behind thin wrappers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::sql::exec::scan::Source;
use crate::sql::parse::ast::{Expr, LockRead};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::row;
use crate::state::Shared;

/// One latched row identity: `(table_id, encoded primary key)` -- the
/// same shape as [`crate::sql::tx::TxnKey`].
pub type LatchKey = (u32, Vec<u8>);

/// Owner ids come from the same counter as explicit-txn ids
/// (`session::begin`), so a txn and its latches identify each other by
/// one number. Ids start above 0; `Txn::default()` keeps 0 and never
/// touches the registry.
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// Allocate the next unique latch owner id.
pub fn next_owner_id() -> u64 {
    NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
}

/// State of one held latch: its holders and whether it is exclusive.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LatchEntry {
    holders: BTreeSet<u64>,
    exclusive: bool,
}

/// The latch registry's storage: key -> entry. `BTreeMap` so
/// `held_by_in` and savepoint markers iterate in a stable order.
pub type LatchMap = BTreeMap<LatchKey, LatchEntry>;

/// Acquire `keys` for `owner`, all-or-nothing. `exclusive` is
/// `FOR UPDATE` (any other holder conflicts); otherwise `FOR SHARE`
/// (coexists with other shared holders).
pub fn acquire_in(
    m: &mut LatchMap,
    owner: u64,
    keys: &[LatchKey],
    exclusive: bool,
) -> SqlResult<()> {
    // validate every key first: a losing request leaves no trace.
    for k in keys {
        if let Some(e) = m.get(k) {
            if e.holders.contains(&owner) {
                continue; // already ours (re-entrant)
            }
            if e.exclusive || exclusive {
                return Err(lock_wait(k));
            }
        }
    }
    for k in keys {
        let e = m.entry(k.clone()).or_default();
        if e.holders.is_empty() {
            e.exclusive = exclusive;
        }
        e.holders.insert(owner);
    }
    Ok(())
}

/// Release exactly `keys` if `owner` holds them (foreign keys are
/// never stolen). Entries whose holder set empties are removed.
pub fn release_keys_in(m: &mut LatchMap, owner: u64, keys: &[LatchKey]) {
    for k in keys {
        if let Some(e) = m.get_mut(k) {
            e.holders.remove(&owner);
            if e.holders.is_empty() {
                m.remove(k);
            }
        }
    }
}

/// Release every key `owner` holds; returns how many that was.
pub fn release_owner_in(m: &mut LatchMap, owner: u64) -> usize {
    let held: Vec<LatchKey> = held_by_in(m, owner);
    release_keys_in(m, owner, &held);
    held.len()
}

/// Keys currently held by `owner`, in `(table_id, pk)` order.
pub fn held_by_in(m: &LatchMap, owner: u64) -> Vec<LatchKey> {
    m.iter()
        .filter(|(_, e)| e.holders.contains(&owner))
        .map(|(k, _)| k.clone())
        .collect()
}

/// The deterministic conflict error (MySQL 1205-style).
fn lock_wait(k: &LatchKey) -> SqlError {
    SqlError::new(
        ErrorCode::LockWaitTimeout,
        format!(
            "Lock wait timeout exceeded; try restarting transaction \
             (row latch on table {} pk {} held by another transaction)",
            k.0,
            k.1.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
    )
}

// ---- locking-read integration (SELECT ... FOR UPDATE / FOR SHARE) ----

/// Latch the primary keys of every row a locking read MATCHED (after
/// its WHERE): one `(table_id, pk)` per row-store side of the FROM
/// scope. Join fan-out dedupes; columnar sides have no pk to lock and
/// are skipped (they are append-only scans, never row-locked).
pub fn lock_matched(
    shared: &Shared,
    src: &Source,
    filter: Option<&Expr>,
    lock: LockRead,
    owner: u64,
) -> SqlResult<()> {
    let matched = crate::sql::exec::select::filter_rows(&src.rows, &src.scope, filter)?;
    let mut keys: BTreeSet<LatchKey> = BTreeSet::new();
    for side in &src.scope.sides {
        let schema = catalog::lookup(shared, &side.table)
            .map_err(SqlError::from)?
            .ok_or_else(|| SqlError::no_such_table(&side.table))?;
        if schema.engine.is_columnar() {
            continue;
        }
        let Some(pos) = side
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&schema.pk))
        else {
            continue;
        };
        let offset = side.offset + pos;
        for row in &matched {
            let pk = row::pk_encode(&row[offset]).map_err(SqlError::from)?;
            keys.insert((schema.id, pk));
        }
    }
    let keys: Vec<LatchKey> = keys.into_iter().collect();
    let exclusive = matches!(lock, LockRead::ForUpdate);
    acquire(owner, &keys, exclusive)
}

/// The cluster-mode veto for locking reads: latches are node-local, so
/// a query that would fan out through the gather path must fail loudly
/// instead of silently degrading to an unlocked snapshot read.
pub fn cluster_veto() -> SqlError {
    SqlError::unsupported(
        "SELECT ... FOR UPDATE / FOR SHARE in cluster mode \
         (row latches are node-local; the gather path cannot lock remote bands)",
    )
}

// ---- process-wide registry (one Mutex-guarded map) ----

static REGISTRY: OnceLock<Mutex<LatchMap>> = OnceLock::new();

fn registry() -> &'static Mutex<LatchMap> {
    REGISTRY.get_or_init(|| Mutex::new(LatchMap::new()))
}

/// [`acquire_in`] on the process-wide registry.
pub fn acquire(owner: u64, keys: &[LatchKey], exclusive: bool) -> SqlResult<()> {
    acquire_in(
        &mut registry().lock().expect("latch registry poisoned"),
        owner,
        keys,
        exclusive,
    )
}

/// [`release_keys_in`] on the process-wide registry.
pub fn release_keys(owner: u64, keys: &[LatchKey]) {
    release_keys_in(
        &mut registry().lock().expect("latch registry poisoned"),
        owner,
        keys,
    );
}

/// [`release_owner_in`] on the process-wide registry.
pub fn release_owner(owner: u64) -> usize {
    release_owner_in(
        &mut registry().lock().expect("latch registry poisoned"),
        owner,
    )
}

/// [`held_by_in`] on the process-wide registry.
pub fn held_by(owner: u64) -> Vec<LatchKey> {
    held_by_in(&registry().lock().expect("latch registry poisoned"), owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(t: u32, pk: &[u8]) -> LatchKey {
        (t, pk.to_vec())
    }

    #[test]
    fn owner_ids_are_unique_and_start_above_zero() {
        let a = next_owner_id();
        let b = next_owner_id();
        assert!(a > 0 && b > 0 && a != b);
    }

    #[test]
    fn acquire_is_all_or_nothing_and_reentrant() {
        let mut m = LatchMap::default();
        acquire_in(&mut m, 7, &[key(1, b"a"), key(1, b"b")], true).unwrap();
        // re-acquiring one's own latch is a no-op, not a conflict.
        acquire_in(&mut m, 7, &[key(1, b"b"), key(1, b"c")], true).unwrap();
        assert_eq!(held_by_in(&m, 7).len(), 3);
        // acquiring {b,c} for 8 must fail WITHOUT having taken "c":
        // all-or-nothing.
        acquire_in(&mut m, 8, &[key(1, b"b"), key(1, b"c")], true).unwrap_err();
        assert_eq!(held_by_in(&m, 8), Vec::new(), "loser acquired nothing");
    }

    #[test]
    fn conflicting_latch_is_1205_lock_wait_timeout() {
        let mut m = LatchMap::default();
        acquire_in(&mut m, 1, &[key(2, b"x")], true).unwrap();
        let e = acquire_in(&mut m, 2, &[key(2, b"x")], true).unwrap_err();
        assert_eq!(e.code, crate::sql::parse::error::ErrorCode::LockWaitTimeout);
        assert!(e.msg.contains("Lock wait timeout exceeded"), "{e}");
    }

    #[test]
    fn shared_latches_compose_exclusive_excludes() {
        let mut m = LatchMap::default();
        acquire_in(&mut m, 1, &[key(3, b"s")], false).unwrap();
        // a second FOR SHARE holder is fine (MySQL shared locks).
        acquire_in(&mut m, 2, &[key(3, b"s")], false).unwrap();
        // but FOR UPDATE against any holder conflicts, both directions.
        acquire_in(&mut m, 3, &[key(3, b"s")], true).unwrap_err();
        acquire_in(&mut m, 3, &[key(3, b"e")], true).unwrap();
        acquire_in(&mut m, 1, &[key(3, b"e")], false).unwrap_err();
    }

    #[test]
    fn release_owner_and_release_keys_clear_entries() {
        let mut m = LatchMap::default();
        acquire_in(&mut m, 1, &[key(4, b"a"), key(4, b"b")], true).unwrap();
        release_keys_in(&mut m, 1, &[key(4, b"a")]);
        assert_eq!(held_by_in(&m, 1), vec![key(4, b"b")]);
        // foreign keys are not stolen by another owner's release.
        release_keys_in(&mut m, 2, &[key(4, b"b")]);
        assert_eq!(held_by_in(&m, 1).len(), 1);
        assert_eq!(release_owner_in(&mut m, 1), 1);
        assert!(m.is_empty(), "empty entries are removed, not leaked");
        assert!(held_by_in(&m, 1).is_empty());
    }

    /// Cluster topologies veto locking reads before any gather fan-out:
    /// row latches are node-local, a remote band cannot be locked. The
    /// single-node path must get PAST the veto (failing later for its
    /// own reasons, e.g. a missing table).
    #[tokio::test]
    async fn cluster_topology_vetoes_locking_reads_single_node_proceeds() {
        use crate::sql::exec::select;
        use crate::sql::parse::parse_statement;
        let shared = crate::state::testutil::shared_with(crate::state::testutil::test_config());
        *shared.topology.write().unwrap() = crate::topology::refresh("a:1, b:2, c:3");
        let crate::sql::parse::ast::Statement::Select(q) =
            parse_statement("SELECT id FROM t WHERE id = 1 FOR UPDATE").unwrap()
        else {
            panic!("select");
        };
        let err = select::run(&shared, &crate::sql::exec::SqlSession::default(), q)
            .await
            .expect_err("cluster veto");
        assert!(
            err.msg.contains("cluster mode") && err.msg.contains("FOR UPDATE"),
            "veto message: {err}"
        );

        // same query on a single-node topology runs the local path (no
        // such table -> the veto was not what stopped it).
        let shared = crate::state::testutil::shared_with(crate::state::testutil::test_config());
        let crate::sql::parse::ast::Statement::Select(q) =
            parse_statement("SELECT id FROM t WHERE id = 1 FOR UPDATE").unwrap()
        else {
            panic!("select");
        };
        let err = select::run(&shared, &crate::sql::exec::SqlSession::default(), q)
            .await
            .expect_err("no veto on single node");
        assert_eq!(
            err.code,
            crate::sql::parse::error::ErrorCode::NoSuchTable,
            "{err}"
        );
    }

    #[test]
    fn shared_entry_fully_released_when_last_holder_leaves() {
        let mut m = LatchMap::default();
        acquire_in(&mut m, 1, &[key(5, b"s")], false).unwrap();
        acquire_in(&mut m, 2, &[key(5, b"s")], false).unwrap();
        release_owner_in(&mut m, 1);
        // holder 2 keeps the shared latch alive...
        acquire_in(&mut m, 3, &[key(5, b"s")], true).unwrap_err();
        release_owner_in(&mut m, 2);
        // ...and the last release frees it for an exclusive taker.
        acquire_in(&mut m, 3, &[key(5, b"s")], true).unwrap();
    }
}
