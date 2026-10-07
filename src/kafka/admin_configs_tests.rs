//! Unit/wire tests for the two constant-answer admin APIs of the P3
//! backfill: the DescribeConfigs(32) stub (TOPIC resource -> minimal
//! static set, unknown types -> INVALID_REQUEST) and the
//! OffsetForLeaderEpoch(23) constant answer (error 0, leader_epoch
//! -1, end_offset = the log end offset).

use crate::kafka::admin_configs::{handle_describe_configs, handle_offset_for_leader_epoch};
use crate::kafka::frame::put_i8;

use rocksdb::WriteBatch;

use crate::hash;
use crate::kafka::errors;
use crate::kafka::frame::{put_array_len, put_bool, put_i32, put_string, Reader};
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

#[test]
fn describe_configs_stub_entries() {
    let sh = sh();
    seed_stream(&sh, b"tc/p0", 1);
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_i8(&mut req, 2); // TOPIC
    put_string(&mut req, "tc");
    put_array_len(&mut req, 0); // all keys
    put_bool(&mut req, true); // include_synonyms (v1)
    let body = handle_describe_configs(&mut Reader::new(&req), 1, &sh).unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(r.i32(), Some(0), "throttle first");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i16(), Some(errors::NONE));
    assert!(r.nullable_string().unwrap().is_none());
    assert_eq!(r.i8(), Some(2));
    assert_eq!(r.string().as_deref(), Some("tc"));
    assert_eq!(r.array_len(), Some(Some(4)), "minimal set");
    let mut names = Vec::new();
    for _ in 0..4 {
        names.push(r.string().unwrap());
        assert!(r.nullable_string().unwrap().is_some());
        assert_eq!(r.boolean(), Some(false), "read_only");
        assert_eq!(r.i8(), Some(5), "config_source = DEFAULT_CONFIG");
        assert_eq!(r.boolean(), Some(false), "is_sensitive");
        assert_eq!(r.array_len(), Some(Some(0)), "no synonyms");
    }
    assert_eq!(
        names,
        vec![
            "cleanup.policy".to_string(),
            "retention.ms".to_string(),
            "retention.bytes".to_string(),
            "min.insync.replicas".to_string()
        ]
    );
    assert_eq!(r.remaining(), 0);
}

#[test]
fn describe_configs_errors_and_v0_v3_shapes() {
    let sh = sh();
    seed_stream(&sh, b"tc/p0", 1);
    // Unknown resource type -> INVALID_REQUEST (42).
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_i8(&mut req, 4); // BROKER
    put_string(&mut req, "1");
    put_array_len(&mut req, 0);
    let body = handle_describe_configs(&mut Reader::new(&req), 0, &sh).unwrap();
    let mut r = Reader::new(&body);
    r.i32();
    r.array_len();
    assert_eq!(r.i16(), Some(errors::INVALID_REQUEST));
    // v0: is_default instead of config_source, no synonyms.
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_i8(&mut req, 2);
    put_string(&mut req, "tc");
    put_array_len(&mut req, 0);
    let body = handle_describe_configs(&mut Reader::new(&req), 0, &sh).unwrap();
    let mut r = Reader::new(&body);
    r.i32();
    r.array_len();
    r.i16();
    r.nullable_string();
    r.i8();
    r.string();
    assert_eq!(r.array_len(), Some(Some(4)));
    for _ in 0..4 {
        r.string();
        r.nullable_string();
        r.boolean();
        assert_eq!(r.boolean(), Some(true), "v0 is_default");
        r.boolean(); // is_sensitive
    }
    assert_eq!(r.remaining(), 0);
    // v3: config_type + documentation trail each entry.
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_i8(&mut req, 2);
    put_string(&mut req, "tc");
    put_array_len(&mut req, 0);
    put_bool(&mut req, false); // include_synonyms
    put_bool(&mut req, false); // include_documentation
    let body = handle_describe_configs(&mut Reader::new(&req), 3, &sh).unwrap();
    let mut r = Reader::new(&body);
    r.i32();
    r.array_len();
    r.i16();
    r.nullable_string();
    r.i8();
    r.string();
    assert_eq!(r.array_len(), Some(Some(4)));
    for _ in 0..4 {
        r.string();
        r.nullable_string();
        r.boolean();
        r.i8(); // config_source
        r.boolean(); // is_sensitive
        r.array_len(); // synonyms
        assert_eq!(r.i8(), Some(0), "config_type UNKNOWN");
        assert!(r.nullable_string().unwrap().is_none(), "no documentation");
    }
    assert_eq!(r.remaining(), 0);
    // Unknown topic -> 3.
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_i8(&mut req, 2);
    put_string(&mut req, "gone");
    put_array_len(&mut req, 0);
    put_bool(&mut req, false); // include_synonyms (v1)
    let body = handle_describe_configs(&mut Reader::new(&req), 1, &sh).unwrap();
    let mut r = Reader::new(&body);
    r.i32();
    r.array_len();
    assert_eq!(r.i16(), Some(errors::UNKNOWN_TOPIC_OR_PARTITION));
}

#[test]
fn describe_configs_key_filter() {
    let sh = sh();
    seed_stream(&sh, b"tk/p0", 1);
    // Explicit configuration_keys: only the named entries return.
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_i8(&mut req, 2);
    put_string(&mut req, "tk");
    put_array_len(&mut req, 1);
    put_string(&mut req, "retention.ms");
    put_bool(&mut req, false); // include_synonyms (v1)
    let body = handle_describe_configs(&mut Reader::new(&req), 1, &sh).unwrap();
    let mut r = Reader::new(&body);
    r.i32();
    r.array_len();
    r.i16();
    r.nullable_string();
    r.i8();
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)), "filtered to one entry");
    assert_eq!(r.string().as_deref(), Some("retention.ms"));
    assert_eq!(r.nullable_string().unwrap().as_deref(), Some("604800000"));
}

#[test]
fn offset_for_leader_epoch_constant_answer() {
    let sh = sh();
    seed_stream(&sh, b"te/p0", 3);
    seed_stream(&sh, b"te/p1", 1);
    // v2 (the librdkafka window): replica-free, throttle-led, epochs.
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_string(&mut req, "te");
    put_array_len(&mut req, 3);
    for (part, probe) in [(0, 5), (1, 0), (7, 0)] {
        put_i32(&mut req, part);
        put_i32(&mut req, -1); // current_leader_epoch
        put_i32(&mut req, probe); // leader_epoch
    }
    let body = handle_offset_for_leader_epoch(&mut Reader::new(&req), 2, &sh).unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(r.i32(), Some(0), "throttle on v2+");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some("te"));
    assert_eq!(r.array_len(), Some(Some(3)));
    let got: Vec<(i16, i32, i32, i64)> = (0..3)
        .map(|_| {
            (
                r.i16().unwrap(),
                r.i32().unwrap(),
                r.i32().unwrap(),
                r.i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(got[0], (errors::NONE, 0, -1, 3), "end = log end offset");
    assert_eq!(got[1], (errors::NONE, 1, -1, 1));
    assert_eq!(got[2], (errors::UNKNOWN_TOPIC_OR_PARTITION, 7, -1, -1));
    assert_eq!(r.remaining(), 0);
    // v0: no throttle, no leader_epoch field.
    let mut req = Vec::new();
    put_array_len(&mut req, 1);
    put_string(&mut req, "te");
    put_array_len(&mut req, 1);
    put_i32(&mut req, 0);
    put_i32(&mut req, 0); // leader_epoch only on v0
    let body = handle_offset_for_leader_epoch(&mut Reader::new(&req), 0, &sh).unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i16(), Some(errors::NONE));
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i64(), Some(3));
    assert_eq!(r.remaining(), 0);
    // v3: replica_id leads the request.
    let mut req = Vec::new();
    put_i32(&mut req, -1); // replica_id
    put_array_len(&mut req, 1);
    put_string(&mut req, "te");
    put_array_len(&mut req, 1);
    put_i32(&mut req, 0);
    put_i32(&mut req, -1);
    put_i32(&mut req, 0);
    let body = handle_offset_for_leader_epoch(&mut Reader::new(&req), 3, &sh).unwrap();
    let mut r = Reader::new(&body);
    r.i32();
    r.array_len();
    r.string();
    r.array_len();
    assert_eq!(r.i16(), Some(errors::NONE));
}
