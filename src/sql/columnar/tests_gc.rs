//! Tests for the M5 columnar orphan/garbage sweep (`gc.rs`).

use std::time::Duration;

use rocksdb::WriteBatch;

use super::commit::PendingSegment;
use super::gc::{self, SweepStats, ORPHAN_MIN_AGE};
use super::meta::{self, SegmentMeta, SegmentState};
use super::{registry_of, writer};
use crate::sql::dist::participant::{self, Vote};
use crate::sql::storage::catalog;
use crate::sql::storage::schema::{ColumnDef, Engine, SqlType, TableSchema, Value};
use crate::state::testutil;
use crate::state::Shared;
use crate::store::ops;

const TABLE_ID: u32 = 9;
const SEGMENT_ID: u64 = 7;
const TABLE_NAME: &str = "c1";

fn shared() -> Shared {
    testutil::shared_with(testutil::test_config())
}

fn columnar_schema() -> TableSchema {
    TableSchema {
        id: TABLE_ID,
        name: TABLE_NAME.into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                sql_type: SqlType::Int,
                nullable: false,
            },
            ColumnDef {
                name: "v".into(),
                sql_type: SqlType::VarChar,
                nullable: true,
            },
        ],
        pk: "id".into(),
        engine: Engine::Columnar,
        indexes: vec![],
    }
}

/// Publish one schema into the stub raft catalog (what DDL would do).
fn seed(shared: &Shared, schema: &TableSchema) {
    shared.raft.write().unwrap().kv.insert(
        catalog::catalog_key(&schema.name),
        serde_json::to_string(schema).unwrap(),
    );
}

async fn commit_one(shared: &Shared) -> SegmentMeta {
    writer::commit_segment(
        shared,
        &columnar_schema(),
        SEGMENT_ID,
        100,
        &[vec![Value::Int(1), Value::Str("a".into())]],
    )
    .await
    .unwrap()
}

fn stored_meta(shared: &Shared) -> Option<SegmentMeta> {
    ops::get_physical(&shared.store, &meta::meta_key(TABLE_ID, SEGMENT_ID))
        .unwrap()
        .map(|v| meta::decode_meta(&v).unwrap())
}

/// Stage a Prepared meta + file + marker exactly like a 2PC vote.
fn stage_prepared(shared: &Shared, txn_id: &str) {
    seed(shared, &columnar_schema());
    let pending = PendingSegment {
        schema: columnar_schema(),
        segment_id: SEGMENT_ID,
        commit_ts: 200,
        rows: vec![vec![Value::Int(1), Value::Str("a".into())]],
    };
    let dir = writer::columnar_dir(&shared.conf);
    let vote = participant::vote(
        &shared.store,
        &shared.raft,
        &dir,
        txn_id,
        "coord",
        200,
        150,
        &[],
        &[pending],
    )
    .unwrap();
    assert!(matches!(vote, Vote::Yes));
}

#[test]
fn missing_dir_yields_zero_stats() {
    let shared = shared();
    assert_eq!(
        gc::sweep(&shared, Duration::ZERO).unwrap(),
        SweepStats::default()
    );
}

#[tokio::test]
async fn stray_files_swept_committed_segment_kept() {
    let shared = shared();
    seed(&shared, &columnar_schema());
    let kept = commit_one(&shared).await;
    let dir = writer::columnar_dir(&shared.conf);
    std::fs::write(dir.join("stray.col"), b"junk").unwrap();
    std::fs::write(dir.join("stray.tmp"), b"junk").unwrap();
    let stats = gc::sweep(&shared, Duration::ZERO).unwrap();
    assert_eq!(
        stats,
        SweepStats {
            metas_deleted: 0,
            files_deleted: 2
        }
    );
    assert!(dir.join(&kept.file).exists(), "committed file kept");
    assert!(stored_meta(&shared).is_some(), "committed meta kept");
    assert!(!dir.join("stray.col").exists());
    assert!(!dir.join("stray.tmp").exists());
}

#[tokio::test]
async fn young_stray_files_are_kept() {
    let shared = shared();
    seed(&shared, &columnar_schema());
    commit_one(&shared).await;
    let dir = writer::columnar_dir(&shared.conf);
    std::fs::write(dir.join("stray.col"), b"junk").unwrap();
    // Just written -> younger than ORPHAN_MIN_AGE: not an orphan yet.
    assert_eq!(
        gc::sweep(&shared, ORPHAN_MIN_AGE).unwrap(),
        SweepStats::default()
    );
    assert!(dir.join("stray.col").exists());
}

#[tokio::test]
async fn dropped_table_metas_and_files_swept() {
    let shared = shared();
    seed(&shared, &columnar_schema());
    let gone = commit_one(&shared).await;
    // Crash simulation: the catalog tombstone landed but
    // drop_table_segments' meta-delete batch never ran.
    // A witness table keeps the catalog view non-empty: only an EMPTY
    // view suspends the table-existence classification.
    let mut witness = columnar_schema();
    witness.id = TABLE_ID + 1;
    witness.name = "witness".into();
    seed(&shared, &witness);
    shared
        .raft
        .write()
        .unwrap()
        .kv
        .remove(&catalog::catalog_key(TABLE_NAME));
    let stats = gc::sweep(&shared, Duration::ZERO).unwrap();
    assert_eq!(
        stats,
        SweepStats {
            metas_deleted: 1,
            files_deleted: 1
        }
    );
    assert!(stored_meta(&shared).is_none(), "meta deleted");
    assert!(
        registry_of(&shared).segments(TABLE_ID).is_empty(),
        "registry emptied"
    );
    assert!(
        !writer::columnar_dir(&shared.conf).join(&gone.file).exists(),
        "file unlinked"
    );
}

#[test]
fn prepared_meta_referenced_by_marker_kept() {
    let shared = shared();
    stage_prepared(&shared, "t-keep");
    // Age ZERO would sweep any unreferenced Prepared meta; the
    // marker reference must win.
    assert_eq!(
        gc::sweep(&shared, Duration::ZERO).unwrap(),
        SweepStats::default()
    );
    let meta = stored_meta(&shared).expect("in-doubt Prepared meta kept");
    assert_eq!(meta.state, SegmentState::Prepared);
    assert!(writer::columnar_dir(&shared.conf).join(&meta.file).exists());
}

#[test]
fn prepared_meta_without_marker_swept() {
    let shared = shared();
    stage_prepared(&shared, "t-crash");
    // Crash before decide: only the marker key is lost.
    let mut batch = WriteBatch::default();
    batch.delete(participant::marker_key("t-crash"));
    ops::batch_write(&shared.store, batch).unwrap();
    let meta = stored_meta(&shared).unwrap();
    let stats = gc::sweep(&shared, Duration::ZERO).unwrap();
    assert_eq!(
        stats,
        SweepStats {
            metas_deleted: 1,
            files_deleted: 1
        }
    );
    assert!(
        stored_meta(&shared).is_none(),
        "stale Prepared meta deleted"
    );
    assert!(
        !writer::columnar_dir(&shared.conf).join(&meta.file).exists(),
        "stale Prepared file unlinked"
    );
}

#[tokio::test]
async fn registry_self_heals_live_metas() {
    let shared = shared();
    seed(&shared, &columnar_schema());
    let kept = commit_one(&shared).await;
    registry_of(&shared).drop_table(TABLE_ID);
    assert!(registry_of(&shared).segments(TABLE_ID).is_empty());
    assert_eq!(
        gc::sweep(&shared, ORPHAN_MIN_AGE).unwrap(),
        SweepStats::default()
    );
    assert_eq!(registry_of(&shared).segments(TABLE_ID), vec![kept]);
}

#[tokio::test]
async fn empty_catalog_view_keeps_every_meta() {
    let shared = shared();
    // No seed: the raft view is empty while the segment meta predates
    // the restart. Absence of view must not read as a DROP.
    let kept = commit_one(&shared).await;
    let dir = writer::columnar_dir(&shared.conf);
    std::fs::write(dir.join("stray.col"), b"junk").unwrap();
    let stats = gc::sweep(&shared, Duration::ZERO).unwrap();
    assert_eq!(
        stats,
        SweepStats {
            metas_deleted: 0,
            files_deleted: 1
        }
    );
    assert!(stored_meta(&shared).is_some(), "meta kept on empty view");
    assert!(dir.join(&kept.file).exists(), "file kept on empty view");
    assert!(!dir.join("stray.col").exists(), "stray file still swept");
}

#[tokio::test]
async fn unreadable_catalog_entry_keeps_metas() {
    let shared = shared();
    let mut witness = columnar_schema();
    witness.id = TABLE_ID + 1;
    witness.name = "witness".into();
    seed(&shared, &witness);
    commit_one(&shared).await;
    // Corrupt ONLY the target table's entry; the witness keeps the
    // view non-empty so this exercises the Err arm, not the guard.
    shared
        .raft
        .write()
        .unwrap()
        .kv
        .insert(catalog::catalog_key(TABLE_NAME), "{not json".to_string());
    assert_eq!(
        gc::sweep(&shared, Duration::ZERO).unwrap(),
        SweepStats::default()
    );
    assert!(
        stored_meta(&shared).is_some(),
        "meta kept on unreadable entry"
    );
}
