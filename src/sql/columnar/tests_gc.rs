//! Tests for the M5 columnar orphan/garbage sweep (`gc.rs`).

use std::time::Duration;

use rocksdb::WriteBatch;

use super::commit::PendingSegment;
use super::gc::{self, SweepStats, ORPHAN_MIN_AGE};
use super::meta::{self, SegmentMeta, SegmentState};
use super::{registry_of, writer};
use crate::sql::dist::participant::{self, Vote};
use crate::sql::storage::catalog;
use crate::sql::storage::schema::{ColumnDef, Engine, KeyModel, SqlType, TableSchema, Value};
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
        pk: vec!["id".to_string()],
        auto_increment: None,
        engine: Engine::Columnar,
        indexes: vec![],
        key_model: KeyModel::MySql,
        distribution: None,
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
    // Crash simulation: the real DROP landed both catalog writes --
    // the id-less name tombstone AND the sql_dropped/<id> marker --
    // but drop_table_segments' meta-delete batch never ran. A witness
    // table keeps the view populated: only an unloaded view (both the
    // live and dropped sets empty) suspends classification.
    let mut witness = columnar_schema();
    witness.id = TABLE_ID + 1;
    witness.name = "witness".into();
    seed(&shared, &witness);
    {
        let mut raft = shared.raft.write().unwrap();
        raft.kv
            .insert(catalog::catalog_key(TABLE_NAME), String::new());
        raft.kv
            .insert(catalog::dropped_id_key(TABLE_ID), TABLE_NAME.to_string());
    }
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
    // Corrupt ONLY the target table's entry: the id then appears in
    // neither the live set (unparsable schema) nor the dropped set
    // (a non-decimal value is no legacy tombstone), so it is
    // "unknown, not gone" and kept; the witness keeps the view
    // populated so this is a genuine unknown, not the empty-view
    // restart guard.
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

#[tokio::test]
async fn renamed_table_metas_survive_sweep() {
    let shared = shared();
    seed(&shared, &columnar_schema());
    let kept = commit_one(&shared).await;
    // RENAME replayed through the stub raft catalog: the old name
    // becomes an id-less tombstone and the SAME schema (same id 9)
    // reappears under the new name -- RENAME writes no
    // sql_dropped/<id> marker, the id stays live. The meta still says
    // table_name "c1"; classification must key on table_id, not the
    // possibly-stale name, or the rename silently wipes the table.
    let mut renamed = columnar_schema();
    renamed.name = "c2".into();
    {
        let mut raft = shared.raft.write().unwrap();
        raft.kv
            .insert(catalog::catalog_key(TABLE_NAME), String::new());
        raft.kv.insert(
            catalog::catalog_key(&renamed.name),
            serde_json::to_string(&renamed).unwrap(),
        );
    }
    assert_eq!(
        gc::sweep(&shared, Duration::ZERO).unwrap(),
        SweepStats::default()
    );
    assert!(stored_meta(&shared).is_some(), "renamed table's meta kept");
    assert!(
        writer::columnar_dir(&shared.conf).join(&kept.file).exists(),
        "renamed table's file kept (still referenced)"
    );
}

#[tokio::test]
async fn truncated_table_old_id_metas_swept() {
    let shared = shared();
    seed(&shared, &columnar_schema());
    let gone = commit_one(&shared).await;
    // TRUNCATE replayed: same name, fresh id (10) under
    // sql_catalog/c1, plus the sql_dropped/<9> marker for the retired
    // id. This is the NON-LEADER path -- only the leader runs
    // drop_table_segments eagerly, so on everyone else GC alone must
    // reclaim the old id's metas (they would otherwise leak forever,
    // the name resolving to the new table).
    let mut fresh = columnar_schema();
    fresh.id = TABLE_ID + 1;
    {
        let mut raft = shared.raft.write().unwrap();
        raft.kv.insert(
            catalog::catalog_key(TABLE_NAME),
            serde_json::to_string(&fresh).unwrap(),
        );
        raft.kv
            .insert(catalog::dropped_id_key(TABLE_ID), TABLE_NAME.to_string());
    }
    let stats = gc::sweep(&shared, Duration::ZERO).unwrap();
    assert_eq!(
        stats,
        SweepStats {
            metas_deleted: 1,
            files_deleted: 1
        }
    );
    assert!(stored_meta(&shared).is_none(), "old-id meta deleted");
    assert!(
        registry_of(&shared).segments(TABLE_ID).is_empty(),
        "registry emptied for the old id"
    );
    assert!(
        !writer::columnar_dir(&shared.conf).join(&gone.file).exists(),
        "old-id file unlinked"
    );
}

#[tokio::test]
async fn unknown_table_id_metas_kept() {
    let shared = shared();
    // No catalog entry at all for the meta's table (neither live nor
    // dropped): the view is populated via the witness, so this is a
    // genuine unknown id -- never-issued or pre-side-entry debris,
    // plus the restart window in general (only an id PRESENT in the
    // dropped set proves a DROP). Unknown is kept, never deleted.
    let mut witness = columnar_schema();
    witness.id = TABLE_ID + 1;
    witness.name = "witness".into();
    seed(&shared, &witness);
    let kept = commit_one(&shared).await;
    assert_eq!(
        gc::sweep(&shared, Duration::ZERO).unwrap(),
        SweepStats::default()
    );
    assert!(stored_meta(&shared).is_some(), "unknown-id meta kept");
    assert!(
        writer::columnar_dir(&shared.conf).join(&kept.file).exists(),
        "unknown-id file kept"
    );
}
