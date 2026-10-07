//! RENAME/TRUNCATE vs the background MVCC GC, over one REAL rdb
//! MySQL-protocol process with the GC period shortened via
//! `RDB_SQL_GC_PERIOD_MS` (see `storage::gc::gc_period`): a RENAME must
//! never park the still-live table id in the dropped set (the pre-fix
//! rename emitted a Drop of the old name at the SAME id, so every GC
//! round gradually deleted the renamed table's rows), and a TRUNCATE
//! must retire the OLD id for real (the pre-fix same-name Put of the
//! fresh id overwrote the name-keyed tombstone, so the old id never
//! entered the dropped set and its bytes leaked forever). The
//! functional asserts run after >5 GC rounds, so a regression fails
//! loudly instead of leaking silently.

mod common;

use common::mysql::{aff, col, ddl, ints, s, world};
use std::time::Duration;

#[tokio::test]
async fn rename_keeps_rows_and_truncate_old_id_is_reclaimed() {
    // BEFORE the node spawn: the child inherits the parent env and the
    // GC loop reads the var at startup. The shortened period is
    // semantics-safe for any concurrently-spawned node reading the
    // same var: GC only deletes watermark-invisible row versions and
    // dropped-table data -- never anything a live snapshot can read.
    std::env::set_var("RDB_SQL_GC_PERIOD_MS", "200");
    let (mut node, mut c) = world("rename-gc").await;
    ddl(
        &mut c,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(32))",
    )
    .await;
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO t (id, v) VALUES (1, 'a'), (2, 'b'), (3, 'c')"
        )
        .await,
        3
    );
    ddl(&mut c, "RENAME TABLE t TO t2").await;

    // ~7 GC rounds at 200ms: enough for the pre-fix bug to have eaten
    // every row of the renamed table (a dropped id loses ALL versions,
    // regardless of the watermark).
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM t2").await,
        vec![3],
        "rename must not hand the live id to the GC\n{}",
        node.ctx()
    );
    assert_eq!(
        ints(&mut c, "SELECT id FROM t2 ORDER BY id").await,
        vec![1, 2, 3]
    );
    assert_eq!(
        col(&mut c, "SELECT v FROM t2 ORDER BY id").await,
        vec![s("a"), s("b"), s("c")],
        "values intact after the GC rounds\n{}",
        node.ctx()
    );

    // TRUNCATE: fresh id under the SAME name. The old id retires via
    // the `sql_dropped/<id>` side entry, which the same-name Put can
    // no longer overwrite, so the background sweep reclaims its rows
    // and index entries instead of leaking them.
    ddl(&mut c, "TRUNCATE t2").await;
    assert_eq!(
        aff(&mut c, "INSERT INTO t2 (id, v) VALUES (1, 'z')").await,
        1
    );
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM t2").await,
        vec![1],
        "post-truncate table holds exactly the fresh row\n{}",
        node.ctx()
    );
    assert_eq!(
        col(&mut c, "SELECT v FROM t2").await,
        vec![s("z")],
        "pre-truncate data is gone\n{}",
        node.ctx()
    );
    node.kill_now();
}
