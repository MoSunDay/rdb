//! Participant-side 2PC behavior for columnar segments: vote stages a
//! Prepared meta + file, decide(commit) flips it Live, decide(abort)
//! removes meta, file and registry entry; catalog disagreements veto.

use super::commit::PendingSegment;
use super::meta::{self, decode_meta, meta_key, SegmentState};
use super::{registry_of, writer};
use crate::sql::dist::participant::{self, Vote};
use crate::sql::storage::catalog;
use crate::sql::storage::schema::{ColumnDef, Engine, KeyModel, SqlType, TableSchema, Value};
use crate::state::testutil;
use crate::store::ops;

const TABLE_ID: u32 = 4;
const SEGMENT_ID: u64 = 42;

fn shared() -> crate::state::Shared {
    testutil::shared_with(testutil::test_config())
}

fn columnar_schema() -> TableSchema {
    TableSchema {
        id: TABLE_ID,
        name: "c1".into(),
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
        auto_increment: None,
        engine: Engine::Columnar,
        indexes: vec![],
        key_model: KeyModel::MySql,
        distribution: None,
    }
}

/// Publish one schema into the stub raft catalog (what DDL would do).
fn seed(shared: &crate::state::Shared, schema: &TableSchema) {
    shared.raft.write().unwrap().kv.insert(
        catalog::catalog_key(&schema.name),
        serde_json::to_string(schema).unwrap(),
    );
}

fn pending(schema: TableSchema, commit_ts: u64) -> PendingSegment {
    PendingSegment {
        schema,
        segment_id: SEGMENT_ID,
        commit_ts,
        rows: vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Int(2), Value::Null],
        ],
    }
}

fn stored_meta(shared: &crate::state::Shared) -> Option<super::meta::SegmentMeta> {
    ops::get_physical(&shared.store, &meta_key(TABLE_ID, SEGMENT_ID))
        .unwrap()
        .map(|v| decode_meta(&v).unwrap())
}

#[test]
fn vote_stages_prepared_meta_file_and_marker() {
    let shared = shared();
    seed(&shared, &columnar_schema());
    let dir = writer::columnar_dir(&shared.conf);
    let seg = pending(columnar_schema(), 100);
    let vote = participant::vote(
        &shared.store,
        &shared.raft,
        &dir,
        "t1",
        "coord",
        100,
        50,
        &[],
        &[seg],
    )
    .unwrap();
    assert_eq!(vote, Vote::Yes);
    // segment file published before the marker
    assert!(
        dir.join(writer::segment_file_name(TABLE_ID, SEGMENT_ID))
            .exists(),
        "segment file missing"
    );
    // store carries the meta, still Prepared
    let meta = stored_meta(&shared).expect("meta staged");
    assert_eq!(meta.state, SegmentState::Prepared);
    assert_eq!(meta.commit_ts, 100);
    assert_eq!(meta.num_rows, 2);
    // the marker remembers the meta key so decide can find it
    let marker = participant::read_marker(&shared.store, "t1").expect("marker");
    assert_eq!(marker.commit_ts, 100);
    assert_eq!(marker.read_ts, 50);
    assert_eq!(marker.coordinator, "coord");
    assert!(marker.keys.contains(&meta_key(TABLE_ID, SEGMENT_ID)));
}

#[test]
fn decide_commit_flips_meta_live_and_registers() {
    let shared = shared();
    seed(&shared, &columnar_schema());
    let dir = writer::columnar_dir(&shared.conf);
    let registry = registry_of(&shared);
    assert!(registry.all().is_empty());
    participant::vote(
        &shared.store,
        &shared.raft,
        &dir,
        "t1",
        "coord",
        100,
        50,
        &[],
        &[pending(columnar_schema(), 100)],
    )
    .unwrap();
    let hi = participant::decide(&shared.store, &dir, &registry, "t1", true, &[]).unwrap();
    assert_eq!(hi, 100, "marker commit_ts is the visibility point");
    let meta = stored_meta(&shared).expect("meta still present");
    assert_eq!(meta.state, SegmentState::Live);
    let segs = registry.segments(TABLE_ID);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].segment_id, SEGMENT_ID);
    assert_eq!(segs[0].state, SegmentState::Live);
    assert!(participant::read_marker(&shared.store, "t1").is_none());
    // idempotent replay: marker gone, decide is a no-op that returns 0
    let hi = participant::decide(&shared.store, &dir, &registry, "t1", true, &[]).unwrap();
    assert_eq!(hi, 0);
    assert_eq!(stored_meta(&shared).unwrap().state, SegmentState::Live);
}

#[test]
fn decide_abort_deletes_meta_file_and_registry_entry() {
    let shared = shared();
    seed(&shared, &columnar_schema());
    let dir = writer::columnar_dir(&shared.conf);
    let registry = registry_of(&shared);
    participant::vote(
        &shared.store,
        &shared.raft,
        &dir,
        "t2",
        "coord",
        200,
        60,
        &[],
        &[pending(columnar_schema(), 200)],
    )
    .unwrap();
    // simulate a commit-flip already registered, then abort must undo it
    registry.insert(&stored_meta(&shared).unwrap());
    participant::decide(&shared.store, &dir, &registry, "t2", false, &[]).unwrap();
    assert!(stored_meta(&shared).is_none(), "meta key deleted");
    assert!(
        !dir.join(writer::segment_file_name(TABLE_ID, SEGMENT_ID))
            .exists(),
        "segment file removed"
    );
    assert!(registry.all().is_empty(), "registry entry removed");
    assert!(participant::read_marker(&shared.store, "t2").is_none());
}

#[test]
fn vote_vetoes_unknown_or_row_engine_schema() {
    let shared = shared();
    let dir = writer::columnar_dir(&shared.conf);
    // unknown table: catalog disagrees -> schema mismatch veto
    let vote = participant::vote(
        &shared.store,
        &shared.raft,
        &dir,
        "t3",
        "coord",
        300,
        70,
        &[],
        &[pending(columnar_schema(), 300)],
    )
    .unwrap();
    match vote {
        Vote::No(reason) => assert!(reason.starts_with("schema mismatch"), "{reason}"),
        other => panic!("expected veto, got {other:?}"),
    }
    // same name but row engine (or differing id): veto again
    let row_engine = TableSchema {
        engine: Engine::Row,
        ..columnar_schema()
    };
    seed(&shared, &row_engine);
    let vote = participant::vote(
        &shared.store,
        &shared.raft,
        &dir,
        "t4",
        "coord",
        301,
        70,
        &[],
        &[pending(columnar_schema(), 301)],
    )
    .unwrap();
    assert!(matches!(vote, Vote::No(r) if r.starts_with("schema mismatch")));
    // nothing staged by the vetoes: no marker, no meta, no file
    assert!(participant::read_marker(&shared.store, "t3").is_none());
    assert!(participant::read_marker(&shared.store, "t4").is_none());
    assert!(stored_meta(&shared).is_none());
    if dir.exists() {
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(left.is_empty(), "veto leaked files: {left:?}");
    }
    let _ = meta::segment_slot(TABLE_ID, SEGMENT_ID); // placement unchanged
}
