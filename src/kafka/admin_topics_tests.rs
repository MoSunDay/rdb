//! Unit/wire tests for the P3 topic-admin surface: CreateTopics /
//! DeleteTopics / CreatePartitions round trips per version (classic
//! bodies, error codes, validate_only), the family-delete fold (meta,
//! entries, ledger 0x20), DescribeConfigs entries, the
//! OffsetForLeaderEpoch constant answer, and the produce auto-create
//! switch.

use rocksdb::WriteBatch;

use crate::hash;
use crate::kafka::admin_topics::{
    handle_create_partitions, handle_create_topics, handle_delete_topics,
};
use crate::kafka::catalog;
use crate::kafka::errors;
use crate::kafka::frame::{put_array_len, put_bool, put_i16, put_i32, put_string, Reader};
use crate::kafka::ledger;
use crate::kafka::produce::handle_produce;
use crate::kafka::record::{build_batch, BatchRecord};
use crate::lite::model;
use crate::state::testutil::{shared_with, test_config};
use crate::state::Shared;
use crate::store::ops;

fn sh() -> Shared {
    shared_with(test_config())
}

/// Seed one stream family the way RESP/XADD would: meta + entries.
fn seed_stream(sh: &Shared, stream: &[u8], n: u64) {
    let parent = &stream[..stream.iter().position(|&b| b == b'/').unwrap()];
    let prefix = hash::slot_with_prefix(parent).1;
    let meta = model::MetaPayload {
        created_ms: 1,
        len: n,
        ..Default::default()
    };
    let mut wb = WriteBatch::default();
    wb.put(
        model::meta_key(&prefix, stream),
        model::encode_meta_at(&meta, 0),
    );
    for i in 0..n {
        wb.put(
            model::entry_key(&prefix, stream, model::EntryId { ms: 10 + i, seq: 0 }),
            model::encode_entry(&[(b"f", b"v")]),
        );
    }
    ops::batch_write(&sh.store, wb).unwrap();
}

/// Count every physical record of one stream name across the stream
/// family kinds (0x0C-0x0F) and the committed-offset ledger (0x20):
/// the family-delete assertion.
fn family_keys(sh: &Shared, stream: &[u8]) -> usize {
    use crate::ds::codec::{self, OFFSET_FAMILY, STREAM_FAMILY};
    let parent = &stream[..stream.iter().position(|&b| b == b'/').unwrap()];
    let prefix = hash::slot_with_prefix(parent).1;
    let ranges = codec::family_delete_ranges(&prefix, STREAM_FAMILY, stream);
    let mut count = 0usize;
    for (lower, upper) in
        ranges
            .into_iter()
            .chain(codec::family_delete_ranges(&prefix, OFFSET_FAMILY, stream))
    {
        let _ = ops::for_each_from(&sh.store, &lower, false, &mut |k, _| {
            if k < upper.as_slice() {
                count += 1;
            }
            true
        });
    }
    count
}

/// CreateTopics request body for one topic.
fn create_req(
    name: &str,
    parts: i32,
    rf: i16,
    assignments: usize,
    version: i16,
    validate_only: bool,
) -> Vec<u8> {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_string(&mut b, name);
    put_i32(&mut b, parts);
    b.extend_from_slice(&rf.to_be_bytes());
    put_array_len(&mut b, assignments);
    for i in 0..assignments {
        put_i32(&mut b, i as i32);
        put_array_len(&mut b, 1);
        put_i32(&mut b, 1); // broker id 1
    }
    put_array_len(&mut b, 0); // configs
    put_i32(&mut b, 1_000); // timeout
    if version >= 1 {
        put_bool(&mut b, validate_only);
    }
    b
}

/// Run CreateTopics for `name` and return (error, error_message-is-null).
async fn create_round(sh: &Shared, name: &str, version: i16, req: &[u8]) -> (i16, bool) {
    let body = handle_create_topics(&mut Reader::new(req), version, sh)
        .await
        .unwrap();
    let mut r = Reader::new(&body);
    if version >= 2 {
        assert_eq!(r.i32(), Some(0), "throttle first on v2+");
    }
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some(name));
    let err = r.i16().unwrap();
    let msg_null = if version >= 1 {
        r.nullable_string().unwrap().is_none()
    } else {
        true
    };
    assert_eq!(r.remaining(), 0, "body fully consumed");
    (err, msg_null)
}

#[tokio::test]
async fn create_topics_versions_and_layout() {
    let sh = sh();
    // v4 (the librdkafka pick): success, null error_message.
    assert_eq!(
        create_round(&sh, "t1", 4, &create_req("t1", 2, 1, 0, 4, false)).await,
        (errors::NONE, true)
    );
    assert_eq!(
        catalog::partitions_of(&sh.store, b"t1").unwrap(),
        vec![0, 1]
    );
    // v0: no throttle, no error_message field; duplicate -> 36 (the
    // helper reads no message on v0, so msg_null stays true).
    assert_eq!(
        create_round(&sh, "t1", 0, &create_req("t1", 2, 1, 0, 0, false)).await,
        (errors::TOPIC_ALREADY_EXISTS, true)
    );
    // v1: error_message present, non-null on error.
    let (code, msg_null) = create_round(&sh, "t1", 1, &create_req("t1", 2, 1, 0, 1, false)).await;
    assert_eq!(code, errors::TOPIC_ALREADY_EXISTS);
    assert!(!msg_null);
}

#[tokio::test]
async fn create_topics_validation_matrix() {
    let sh = sh();
    assert_eq!(
        create_round(
            &sh,
            "bad name",
            4,
            &create_req("bad name", 1, 1, 0, 4, false)
        )
        .await,
        (errors::INVALID_TOPIC_EXCEPTION, false)
    );
    assert_eq!(
        create_round(&sh, "t2", 4, &create_req("t2", 1, 2, 0, 4, false)).await,
        (errors::INVALID_REPLICATION_FACTOR, false)
    );
    assert_eq!(
        create_round(&sh, "t2", 4, &create_req("t2", 1, -1, 0, 4, false)).await,
        (errors::NONE, true),
        "rf -1 = unset default"
    );
    assert_eq!(
        create_round(&sh, "t3", 4, &create_req("t3", 1, 1, 1, 4, false)).await,
        (errors::INVALID_REPLICA_ASSIGNMENT, false)
    );
    assert_eq!(
        create_round(&sh, "t4", 4, &create_req("t4", 0, 1, 0, 4, false)).await,
        (errors::INVALID_PARTITIONS, false)
    );
    // num_partitions -1 -> default 1 partition.
    assert_eq!(
        create_round(&sh, "t5", 4, &create_req("t5", -1, -1, 0, 4, false)).await,
        (errors::NONE, true)
    );
    assert_eq!(catalog::partitions_of(&sh.store, b"t5").unwrap(), vec![0]);
    // validate_only: full validation, zero mutation.
    assert_eq!(
        create_round(&sh, "t6", 4, &create_req("t6", 3, 1, 0, 4, true)).await,
        (errors::NONE, true)
    );
    assert!(catalog::partitions_of(&sh.store, b"t6").unwrap().is_empty());
    assert_eq!(
        create_round(&sh, "t5", 4, &create_req("t5", 3, 1, 0, 4, true)).await,
        (errors::TOPIC_ALREADY_EXISTS, false)
    );
}

#[tokio::test]
async fn delete_topics_folds_the_whole_family() {
    let sh = sh();
    seed_stream(&sh, b"td/p0", 2);
    seed_stream(&sh, b"td/p1", 1);
    seed_stream(&sh, b"td/p0/dlq", 1); // nested DLQ stream
    let prefix = hash::slot_with_prefix(b"td").1;
    // put_rows only stages into a batch: commit both rows (one per
    // partition) in a single write.
    let mut wb = WriteBatch::default();
    ledger::put_rows(
        &mut wb,
        &[
            ledger::LedgerRow {
                stream: b"td/p0".to_vec(),
                group: b"g1".to_vec(),
                prefix: prefix.clone(),
                committed_ordinal: 1,
                generation: 1,
                leader: "m".into(),
            },
            ledger::LedgerRow {
                stream: b"td/p1".to_vec(),
                group: b"g2".to_vec(),
                prefix: prefix.clone(),
                committed_ordinal: 0,
                generation: 1,
                leader: "m".into(),
            },
        ],
    );
    ops::batch_write(&sh.store, wb).unwrap();
    assert!(ledger::has_rows(&sh.store, &prefix, b"td/p0").unwrap());
    assert!(family_keys(&sh, b"td/p0") >= 3);
    // DeleteTopics v1 (throttle first, name+error rows).
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_string(&mut req, "td");
    put_i32(&mut req, 1_000);
    let body = handle_delete_topics(&mut Reader::new(&req), 1, &sh)
        .await
        .unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(r.i32(), Some(0), "throttle first on v1+");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some("td"));
    assert_eq!(r.i16(), Some(errors::NONE));
    assert_eq!(r.remaining(), 0);
    // The family is GONE: partitions, entries, nested DLQ, ledger.
    assert!(catalog::partitions_of(&sh.store, b"td").unwrap().is_empty());
    assert_eq!(family_keys(&sh, b"td/p0"), 0);
    assert_eq!(family_keys(&sh, b"td/p1"), 0);
    assert_eq!(family_keys(&sh, b"td/p0/dlq"), 0);
    assert!(!ledger::has_rows(&sh.store, &prefix, b"td/p0").unwrap());
    // Unknown topic answers 3; v0 carries no throttle.
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_string(&mut req, "nope");
    put_i32(&mut req, 1_000);
    let body = handle_delete_topics(&mut Reader::new(&req), 0, &sh)
        .await
        .unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some("nope"));
    assert_eq!(r.i16(), Some(errors::UNKNOWN_TOPIC_OR_PARTITION));
    assert_eq!(r.remaining(), 0);
}

#[tokio::test]
async fn create_partitions_grows_only() {
    let sh = sh();
    assert_eq!(
        create_round(&sh, "tg", 4, &create_req("tg", 2, 1, 0, 4, false)).await,
        (errors::NONE, true)
    );
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_string(&mut req, "tg");
    put_i32(&mut req, 4); // new total count
    put_array_len(&mut req, 0); // assignments (null/empty)
    put_i32(&mut req, 1_000); // timeout
    put_bool(&mut req, false); // validate_only
    let body = handle_create_partitions(&mut Reader::new(&req), 1, &sh)
        .await
        .unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(r.i32(), Some(0), "throttle always present");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some("tg"));
    assert_eq!(r.i16(), Some(errors::NONE));
    assert!(r.nullable_string().unwrap().is_none());
    assert_eq!(r.remaining(), 0);
    assert_eq!(
        catalog::partitions_of(&sh.store, b"tg").unwrap(),
        vec![0, 1, 2, 3]
    );
    // Shrink -> 37; assignments -> 39; unknown -> 3.
    for (count, assigns, want) in [
        (2usize, 0usize, errors::INVALID_PARTITIONS),
        (4, 1, errors::INVALID_REPLICA_ASSIGNMENT),
    ] {
        let mut req = Vec::new();
        put_array_len(&mut req, 1);
        put_string(&mut req, "tg");
        put_i32(&mut req, count as i32);
        put_array_len(&mut req, assigns);
        if assigns > 0 {
            put_array_len(&mut req, 1);
            put_i32(&mut req, 1);
        }
        put_i32(&mut req, 1_000);
        put_bool(&mut req, false);
        let body = handle_create_partitions(&mut Reader::new(&req), 1, &sh)
            .await
            .unwrap();
        let mut r = Reader::new(&body);
        r.i32();
        r.array_len();
        r.string();
        assert_eq!(r.i16(), Some(want));
    }
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_string(&mut req, "missing");
    put_i32(&mut req, 4);
    put_array_len(&mut req, 0);
    put_i32(&mut req, 1_000);
    put_bool(&mut req, false);
    let body = handle_create_partitions(&mut Reader::new(&req), 1, &sh)
        .await
        .unwrap();
    let mut r = Reader::new(&body);
    r.i32();
    r.array_len();
    r.string();
    assert_eq!(r.i16(), Some(errors::UNKNOWN_TOPIC_OR_PARTITION));
}

/// Produce one record to (topic, partition); returns the partition
/// error code (auto-create tests).
async fn produce_err(sh: &Shared, topic: &str, part: i32) -> i16 {
    let batch = build_batch(
        0,
        1_000,
        &[BatchRecord {
            timestamp_delta: 0,
            key: None,
            value: Some(b"v"),
            headers: vec![],
        }],
    );
    let mut req = Vec::new();
    put_i16(&mut req, 1); // acks
    put_i32(&mut req, 1_000);
    put_array_len(&mut req, 1);
    put_string(&mut req, topic);
    put_array_len(&mut req, 1);
    put_i32(&mut req, part);
    put_i32(&mut req, batch.len() as i32);
    req.extend_from_slice(&batch);
    let body = handle_produce(&mut Reader::new(&req), 2, sh)
        .await
        .unwrap()
        .expect("acks=1 answers");
    let mut r = Reader::new(&body);
    r.array_len();
    r.string();
    r.array_len();
    r.i32();
    r.i16().unwrap()
}

#[tokio::test]
async fn produce_auto_create_switch() {
    // Default off: byte-for-byte UNKNOWN_TOPIC_OR_PARTITION.
    let sh = sh();
    assert_eq!(
        produce_err(&sh, "ta", 0).await,
        errors::UNKNOWN_TOPIC_OR_PARTITION
    );
    assert!(catalog::partitions_of(&sh.store, b"ta").unwrap().is_empty());
    // On: the topic materializes with one default partition and the
    // produce proceeds.
    let mut conf = test_config();
    conf.kafka_auto_create_topics = true;
    let sh2 = shared_with(conf);
    assert_eq!(produce_err(&sh2, "ta", 0).await, errors::NONE);
    assert_eq!(catalog::partitions_of(&sh2.store, b"ta").unwrap(), vec![0]);
    // A produce to a partition beyond the default still misses.
    assert_eq!(
        produce_err(&sh2, "ta", 3).await,
        errors::UNKNOWN_TOPIC_OR_PARTITION
    );
}
