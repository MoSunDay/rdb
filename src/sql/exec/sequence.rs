//! AUTO_INCREMENT id allocation and `LAST_INSERT_ID()`.
//!
//! The next-value counter is per-table raft-replicated state under
//! `sql_sequence/<table>` (see `storage/catalog.rs`) -- the SAME
//! control-plane path CREATE TABLE uses, so cluster allocation is
//! leader-only and linearizable like every other catalog write.
//!
//! Gaps are accepted (MySQL gaps too): each statement that auto-allocates
//! reserves a batch of [`RESERVE_BATCH`] ids ahead, so multi-row INSERTs
//! pay ONE raft round-trip per 64 ids instead of one per row; ids in the
//! reserved-but-unused tail are burned. Explicit values at or above the
//! counter raise it to `value + 1` (MySQL behavior). Rolled-back
//! transactions also burn their reserved ids -- same as MySQL.
//!
//! `LAST_INSERT_ID()` semantics: the per-connection truth lives on
//! [`crate::sql::exec::SqlSession::last_insert_id`] (set by the write
//! path); the expression evaluator cannot see the session yet (the
//! SELECT pipeline evaluates against pure row scopes), so
//! [`last_insert_id_value`] reads a process-wide mirror updated
//! alongside the session field. Deviation documented in COMPAT.md.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::schema::Value;
use crate::state::{self, Shared};

/// What one AUTO_INCREMENT assign round returns from the blocking
/// pool: the stamped rows, the session's first auto id, and the queued
/// counter bump (if any) to commit after the raft guard is dropped.
type AssignOutcome = (Vec<Vec<Value>>, Option<i64>, Option<catalog::QueuedApply>);

/// Ids reserved per raft round-trip when a statement auto-allocates.
/// Multi-row INSERTs of up to this size cost one replicated write.
pub const RESERVE_BATCH: i64 = 64;

/// Result of walking one statement's rows over the counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Walk {
    /// Next id a FOLLOWING statement would allocate (floor after this
    /// walk, including explicit-value bumps).
    pub next: i64,
    /// First auto-generated id of THIS statement (`None` when every row
    /// carried an explicit value).
    pub first_auto: Option<i64>,
}

/// Walk `rows` in order, assigning ids to the AUTO_INCREMENT slot `ai`
/// (full-width rows, values already coerced to the column type):
/// NULL or 0 -> next consecutive id (MySQL default mode auto-allocates
/// on 0 too); a positive explicit value is KEPT and, when at or above
/// the running floor, raises it to `value + 1` immediately -- later rows
/// in the same statement then continue above it, so an explicit value
/// never collides with an auto id of the same statement.
/// Non-positive explicit values other than 0 are kept verbatim and never
/// bump (MySQL does not auto-allocate on them).
pub fn assign(rows: &mut [Vec<Value>], ai: usize, floor: i64) -> Walk {
    let mut next = floor.max(1);
    let mut first_auto = None;
    for row in rows.iter_mut() {
        match row.get(ai).cloned().unwrap_or(Value::Null) {
            Value::Null | Value::Int(0) => {
                row[ai] = Value::Int(next);
                if first_auto.is_none() {
                    first_auto = Some(next);
                }
                next += 1;
            }
            Value::Int(n) if n >= next => next = n + 1,
            _ => {
                // Explicit value below the floor, or a non-integer that
                // coercion will have rejected earlier: no bump.
            }
        }
    }
    Walk { next, first_auto }
}

/// Persisted counter after a walk: statements that auto-allocated
/// reserve a batch ahead (one raft round-trip per 64 ids); pure-explicit
/// statements persist exactly `value + 1` like MySQL.
pub fn persist_target(floor: i64, walk: &Walk) -> i64 {
    match walk.first_auto {
        Some(_) => walk.next.max(floor.max(1) + RESERVE_BATCH),
        None => walk.next,
    }
}

/// Read the floor, assign ids and persist the raised counter as ONE
/// serialized read-modify-write: the floor RMW is serialized by
/// `exec::ddl::CATALOG_MUX`, held across floor read -> queue ->
/// commit-await. The raft WRITE guard still only spans
/// read + assign + queue and never crosses an await (deadlock rule,
/// see `exec::ddl::catalog_apply`); the mux -- not the guard -- is
/// what guarantees two concurrent INSERTs never hand out the same
/// floor, because the floor read observes only FSM-APPLIED bumps and a
/// queued-but-unapplied bump stays invisible until its commit lands.
/// Returns the rewritten rows plus the first auto-generated id of the
/// statement (for `LAST_INSERT_ID()`).
pub async fn allocate(
    shared: &Shared,
    table: &str,
    mut rows: Vec<Vec<Value>>,
    ai: usize,
) -> SqlResult<(Vec<Vec<Value>>, Option<i64>)> {
    // Serialize the whole floor RMW (the plain tokio mutex is MADE to
    // be held across awaits); the raft write guard below stays
    // short-lived.
    let _catalog = crate::sql::exec::ddl::CATALOG_MUX.lock().await;
    let raft = Arc::clone(&shared.raft);
    let table = table.to_string();
    // Queue the counter bump under the raft write guard, await the
    // commit after the guard is dropped: the guard must never span an
    // await (see `exec::ddl::catalog_apply`).
    let (rows, first_auto, queued) =
        tokio::task::spawn_blocking(move || -> SqlResult<AssignOutcome> {
            let mut guard = raft.write().unwrap();
            let floor = catalog::sequence_next_state(&guard, &table);
            let walk = assign(&mut rows, ai, floor);
            let target = persist_target(floor, &walk);
            let mut queued = None;
            if target > floor {
                let mut txn = catalog::begin(&mut guard, "AUTO_INCREMENT allocation")
                    .map_err(SqlError::from)?;
                queued = Some(
                    txn.queue_put_kv(&catalog::sequence_key(&table), &target.to_string())
                        .map_err(SqlError::from)?,
                );
            }
            Ok((rows, walk.first_auto, queued))
        })
        .await
        .map_err(|e| SqlError::new(ErrorCode::Unknown, e.to_string()))??;
    if let Some(q) = queued {
        state::raft_apply_await(q.ticket)
            .await
            .map_err(SqlError::from)?;
    }
    Ok((rows, first_auto))
}

/// Process-wide mirror of the session's `last_insert_id`, read by
/// [`last_insert_id_value`] until expression evaluation can see the
/// session (see the module doc; COMPAT.md carries the deviation).
static LAST_INSERT_ID: AtomicI64 = AtomicI64::new(0);

/// Record `id` as the connection's `LAST_INSERT_ID()` (called by the
/// write path right after it sets `SqlSession::last_insert_id`).
pub fn note_last_insert_id(id: i64) {
    LAST_INSERT_ID.store(id, Ordering::Relaxed);
}

/// `LAST_INSERT_ID()`: the value of the last auto-allocated id (0 when
/// none); `LAST_INSERT_ID(n)` SETS it to `n` and returns `n` (MySQL).
pub fn last_insert_id_value(args: &[Value]) -> SqlResult<Value> {
    match args {
        [] => Ok(Value::Int(LAST_INSERT_ID.load(Ordering::Relaxed))),
        [Value::Int(n)] => {
            LAST_INSERT_ID.store(*n, Ordering::Relaxed);
            Ok(Value::Int(*n))
        }
        _ => Err(SqlError::new(
            ErrorCode::NotSupported,
            "LAST_INSERT_ID takes no arguments or one integer",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(specs: &[(i64, &str)]) -> Vec<Vec<Value>> {
        specs
            .iter()
            .map(|&(id, v)| vec![Value::Int(id), Value::Str(v.into())])
            .collect()
    }

    fn ids(rows: &[Vec<Value>]) -> Vec<i64> {
        rows.iter()
            .map(|r| match r[0] {
                Value::Int(i) => i,
                _ => panic!("not an int"),
            })
            .collect()
    }

    #[test]
    fn null_and_zero_auto_allocate_consecutively() {
        // 0 = i64 sentinel for a NULL slot (pre-coercion rows carry
        // Value::Null; use the real forms here).
        let mut r = vec![
            vec![Value::Null, Value::Str("a".into())],
            vec![Value::Int(0), Value::Str("b".into())],
            vec![Value::Null, Value::Str("c".into())],
        ];
        let w = assign(&mut r, 0, 1);
        assert_eq!(ids(&r), vec![1, 2, 3], "consecutive across the statement");
        assert_eq!(w.first_auto, Some(1));
        assert_eq!(w.next, 4);
    }

    #[test]
    fn explicit_values_bump_the_floor() {
        // explicit 100 with autos after it: autos continue above 101
        let mut r = rows(&[(100, "a"), (0, "b"), (0, "c")]);
        let w = assign(&mut r, 0, 1);
        assert_eq!(ids(&r), vec![100, 101, 102]);
        assert_eq!(w.first_auto, Some(101));
        assert_eq!(w.next, 103);

        // explicit below the floor never bumps
        let mut r = rows(&[(5, "a"), (0, "b")]);
        let w = assign(&mut r, 0, 50);
        assert_eq!(ids(&r), vec![5, 50]);
        assert_eq!(w.next, 51, "floor 50 untouched by explicit 5");

        // non-positive explicit values are kept and never auto-allocated
        let mut r = rows(&[(-3, "a"), (0, "b")]);
        let w = assign(&mut r, 0, 7);
        assert_eq!(ids(&r), vec![-3, 7]);
        assert_eq!(w.next, 8);
    }

    #[test]
    fn persist_target_reserves_batches() {
        // auto-allocating statements reserve a full batch ahead
        let w = Walk {
            next: 4,
            first_auto: Some(1),
        };
        assert_eq!(persist_target(1, &w), 65);

        // big statements reserve exactly what they consumed
        let w = Walk {
            next: 101,
            first_auto: Some(1),
        };
        assert_eq!(persist_target(1, &w), 101);

        // pure-explicit statements persist value+1 exactly (no padding)
        let w = Walk {
            next: 101,
            first_auto: None,
        };
        assert_eq!(persist_target(1, &w), 101);
    }

    #[test]
    fn last_insert_id_value_set_and_get() {
        // Single test owns the mirror: unit tests run in parallel in one
        // process, so only this test may touch it.
        assert!(matches!(
            last_insert_id_value(&[Value::Int(41)]),
            Ok(Value::Int(41))
        ));
        assert!(matches!(last_insert_id_value(&[]), Ok(Value::Int(41))));
        assert!(last_insert_id_value(&[Value::Str("x".into())]).is_err());
        assert!(last_insert_id_value(&[Value::Int(1), Value::Int(2)]).is_err());
    }

    mod insert_path {
        //! INSERT-allocation integration over the stub engine (same
        //! shape as exec/write_tests.rs): proves the write path calls
        //! [`allocate`], rewrites rows and sets the session value.

        use super::super::*;
        use crate::sql::exec::{ddl, write, SqlSession};
        use crate::sql::parse::parse_statement;
        use crate::sql::storage::catalog;
        use crate::state::testutil;

        async fn world() -> crate::state::Shared {
            let shared = testutil::shared_with(testutil::test_config());
            ddl::run(
                &shared,
                parse_statement(
                    "CREATE TABLE ai \
                     (id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(64) NULL)",
                )
                .unwrap(),
            )
            .await
            .unwrap();
            shared
        }

        async fn insert(shared: &crate::state::Shared, sess: &mut SqlSession, sql: &str) {
            write::insert(shared, sess, parse_statement(sql).unwrap())
                .await
                .unwrap();
        }

        async fn ids(shared: &crate::state::Shared) -> Vec<i64> {
            let rows = crate::sql::exec::scan::visible_rows(
                &shared.store,
                &catalog::lookup(shared, "ai").unwrap().unwrap(),
                shared.sql_ts.now(),
            )
            .unwrap();
            let mut out: Vec<i64> = rows
                .iter()
                .map(|r| match r[0] {
                    Value::Int(i) => i,
                    _ => panic!("non-int id"),
                })
                .collect();
            out.sort_unstable();
            out
        }

        #[tokio::test]
        async fn insert_allocates_for_missing_null_and_zero() {
            let shared = world().await;
            let mut sess = SqlSession::default();

            // column omitted entirely -> auto
            insert(&shared, &mut sess, "INSERT INTO ai (v) VALUES ('a')").await;
            assert_eq!(sess.last_insert_id, 1, "first generated id of stmt 1");
            // explicit NULL -> auto; explicit 0 -> auto (MySQL default mode)
            insert(
                &shared,
                &mut sess,
                "INSERT INTO ai (id, v) VALUES (NULL, 'b')",
            )
            .await;
            assert_eq!(sess.last_insert_id, 65);
            insert(&shared, &mut sess, "INSERT INTO ai (id, v) VALUES (0, 'c')").await;
            assert_eq!(sess.last_insert_id, 129);
            // one statement each -> batch reservation burns the tails
            assert_eq!(ids(&shared).await, vec![1, 65, 129]);

            // multi-row: consecutive WITHIN the statement, LAST_INSERT_ID
            // = first of them
            insert(
                &shared,
                &mut sess,
                "INSERT INTO ai (v) VALUES ('d'), ('e'), ('f')",
            )
            .await;
            assert_eq!(ids(&shared).await, vec![1, 65, 129, 193, 194, 195]);
            assert_eq!(sess.last_insert_id, 193);
        }

        #[tokio::test]
        async fn explicit_value_bumps_persisted_counter() {
            let shared = world().await;
            let mut sess = SqlSession::default();
            insert(&shared, &mut sess, "INSERT INTO ai (v) VALUES ('a')").await;
            insert(
                &shared,
                &mut sess,
                "INSERT INTO ai (id, v) VALUES (100, 'b')",
            )
            .await;
            assert_eq!(
                catalog::sequence_next(&shared, "ai"),
                101,
                "counter bumped to value+1"
            );
            assert_eq!(sess.last_insert_id, 1, "explicit rows do not touch it");
            insert(&shared, &mut sess, "INSERT INTO ai (v) VALUES ('c')").await;
            assert_eq!(ids(&shared).await, vec![1, 100, 101]);
            assert_eq!(sess.last_insert_id, 101);
        }

        /// The batch reservation actually persists through the stub raft
        /// path: after one 1-row auto INSERT the counter sits at 65, so
        /// the next statement takes 65 (63 burned ids -- accepted gaps).
        #[tokio::test]
        async fn batch_reservation_persists_through_raft() {
            let shared = world().await;
            let mut sess = SqlSession::default();
            insert(&shared, &mut sess, "INSERT INTO ai (v) VALUES ('a')").await;
            assert_eq!(catalog::sequence_next(&shared, "ai"), 65);
            insert(&shared, &mut sess, "INSERT INTO ai (v) VALUES ('b')").await;
            assert_eq!(ids(&shared).await, vec![1, 65]);
            assert_eq!(sess.last_insert_id, 65);
        }

        /// Stub raft with a REAL queue->apply window: entries sent on
        /// `apply_tx` park for 50ms in a background loop before landing
        /// in the FSM view, so a floor read in `allocate` observes only
        /// FSM-APPLIED bumps -- exactly the production shape, where the
        /// bump becomes visible only after the queued entry commits.
        async fn world_with_delayed_apply() -> crate::state::Shared {
            let shared = testutil::shared_with(testutil::test_config());
            let raft = Arc::clone(&shared.raft);
            let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::state::ApplyReq>(64);
            tokio::spawn(async move {
                while let Some(req) = rx.recv().await {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    {
                        let mut guard = raft.write().unwrap();
                        guard
                            .kv
                            .insert(req.entry.key.clone(), req.entry.value.clone());
                        guard.apply_count += 1;
                    }
                    let _ = req.reply.send(Ok(()));
                }
            });
            shared.raft.write().unwrap().apply_tx = Some(tx);
            shared
        }

        /// Deterministic repro of the queue->apply window race: the
        /// floor read in `allocate` sees only FSM-APPLIED bumps, so a
        /// concurrent INSERT racing a queued-but-unapplied bump re-reads
        /// the OLD floor and hands out overlapping ids -- the row
        /// store's PK upsert then silently overwrites the earlier row
        /// (data corruption, not a visible error). The 50ms apply delay
        /// widens the window to certainty: 4 tasks x (3 single-row + 2
        /// two-row INSERTs) = 28 ids must all stay distinct. Sentinel
        /// for the CATALOG_MUX serialization fix.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn concurrent_inserts_never_share_ids_when_apply_lags() {
            let shared = Arc::new(world_with_delayed_apply().await);
            ddl::run(
                &shared,
                parse_statement(
                    "CREATE TABLE ai \
                     (id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(64) NULL)",
                )
                .unwrap(),
            )
            .await
            .unwrap();
            let mut tasks = Vec::new();
            for t in 0..4u8 {
                let shared = Arc::clone(&shared);
                tasks.push(tokio::spawn(async move {
                    let mut sess = SqlSession::default();
                    for i in 0..3 {
                        insert(
                            &shared,
                            &mut sess,
                            &format!("INSERT INTO ai (v) VALUES ('{t}-{i}')"),
                        )
                        .await;
                    }
                    for _ in 0..2 {
                        insert(
                            &shared,
                            &mut sess,
                            &format!("INSERT INTO ai (v) VALUES ('{t}-a'), ('{t}-b')"),
                        )
                        .await;
                    }
                }));
            }
            for task in tasks {
                task.await.unwrap();
            }
            let got = ids(&shared).await;
            assert_eq!(got.len(), 28, "one visible row per INSERTed row: {got:?}");
            let distinct: std::collections::HashSet<_> = got.iter().collect();
            assert_eq!(
                distinct.len(),
                28,
                "duplicate ids mean the PK upsert silently overwrote a row: {got:?}"
            );
        }
    }
}
