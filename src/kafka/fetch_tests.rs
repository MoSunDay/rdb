//! Unit tests for `kafka::fetch`: the entry-pair inverse map, the
//! version ladder (v0/v4/v10 response shapes), offset semantics
//! (EOF empty batch / OUT_OF_RANGE / unknown partition), the byte
//! budget truncation, and long-poll (deadline expiry + wake by notify).

use std::sync::Arc;
use std::time::{Duration, Instant};

use rocksdb::WriteBatch;

use crate::hash;
use crate::kafka::errors;
use crate::kafka::fetch::handle_fetch;
use crate::kafka::fetch_records::to_key_value;
use crate::kafka::frame::{put_array_len, put_i32, put_i64, put_string, Reader};
use crate::kafka::record::parse_batch;
use crate::lite::model;
use crate::state::testutil;
use crate::state::Shared;
use crate::{ds::wait, store::ops};

/// Skip responses-array head + topic name + partitions-array head + the
/// partition/error/hwm scalars (v0 shape), leaving the reader at records.
fn skip_partition_head(r: &mut Reader<'_>) {
    r.array_len();
    r.string();
    r.array_len();
    r.i32();
    r.i16();
    r.i64();
}

fn pairs(v: &[(&[u8], &[u8])]) -> Vec<(Vec<u8>, Vec<u8>)> {
    v.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()
}

/// Write `n` k/v entries into partition 0 (the `q0` queue) of `topic`.
fn seed(shared: &Shared, topic: &[u8], n: u64) -> Vec<u8> {
    let stream = [topic, b"/q0"].concat();
    let prefix = hash::slot_with_prefix(topic).1;
    let mut wb = WriteBatch::default();
    let mut meta = model::MetaPayload { created_ms: 1, ..Default::default() };
    for i in 0..n {
        let id = model::EntryId { ms: 100 + i, seq: 0 };
        let k = format!("k{i}").into_bytes();
        let v = format!("v{i}").into_bytes();
        wb.put(
            model::entry_key(&prefix, &stream, id),
            model::encode_entry(&[(b"k".as_slice(), k.as_slice()), (b"v".as_slice(), v.as_slice())]),
        );
        meta.len += 1;
        meta.last_ms = id.ms;
        meta.last_seq = id.seq;
    }
    wb.put(&model::meta_key(&prefix, &stream), model::encode_meta_at(&meta, 0));
    ops::batch_write(&shared.store, wb).unwrap();
    prefix
}

/// v0/v4/v10 fetch request for partition 0 of `topic`.
fn req(version: i16, topic: &str, offset: i64, pmax: i32, max_wait: i32, min_bytes: i32) -> Vec<u8> {
    let mut r = Vec::new();
    put_i32(&mut r, -1); // replica_id (any consumer: -1)
    put_i32(&mut r, max_wait);
    put_i32(&mut r, min_bytes);
    if version >= 3 {
        put_i32(&mut r, 1 << 20); // max_bytes (generous)
    }
    if version >= 4 {
        r.push(0); // isolation_level
    }
    if version >= 7 {
        put_i32(&mut r, 0); // session_id
        put_i32(&mut r, -1); // session_epoch
    }
    put_array_len(&mut r, 1);
    put_string(&mut r, topic);
    put_array_len(&mut r, 1);
    put_i32(&mut r, 0); // partition
    if version >= 9 {
        put_i32(&mut r, -1); // current_leader_epoch
    }
    put_i64(&mut r, offset);
    if version >= 5 {
        put_i64(&mut r, -1); // log_start_offset
    }
    put_i32(&mut r, pmax);
    if version >= 7 {
        put_array_len(&mut r, 0); // forgotten_topics_data (AFTER topics)
    }
    r
}

/// Response body of a one-topic/one-partition fetch plus its first row
/// (partition, error, high watermark); the caller wraps a Reader over
/// the body to read the version-dependent fields + records.
async fn one_partition(sh: &Shared, version: i16, request: &[u8]) -> (Vec<u8>, i32, i16, i64) {
    let mut r = Reader::new(request);
    let (body, _parked) = handle_fetch(&mut r, version, sh).await.unwrap();
    let mut row = Reader::new(&body);
    assert_eq!(row.array_len(), Some(Some(1)));
    row.string(); // topic echoed
    assert_eq!(row.array_len(), Some(Some(1)));
    let partition = row.i32().unwrap();
    let error = row.i16().unwrap();
    let hwm = row.i64().unwrap();
    (body, partition, error, hwm)
}

#[test]
fn inverse_field_map() {
    assert_eq!(to_key_value(&pairs(&[])), (None, None));
    // value-only + null-null tombstone (1-pair shapes)
    assert_eq!(to_key_value(&pairs(&[(b"v", b"x")])), (None, Some(b"x".to_vec())));
    assert_eq!(to_key_value(&pairs(&[(b"__null__", b"")])), (None, None));
    // 2-pair shapes: k+v and k+null-value
    assert_eq!(
        to_key_value(&pairs(&[(b"k", b"K"), (b"v", b"V")])),
        (Some(b"K".to_vec()), Some(b"V".to_vec()))
    );
    assert_eq!(
        to_key_value(&pairs(&[(b"k", b"K"), (b"__null__", b"")])),
        (Some(b"K".to_vec()), None)
    );
    // 3 pairs (or any "h") collapse to the generic JSON envelope.
    let (k, v) = to_key_value(&pairs(&[(b"a", b"\x01"), (b"b", b""), (b"c", b"zz")]));
    assert_eq!(k, None);
    let v = String::from_utf8(v.unwrap()).unwrap();
    assert_eq!(v, r#"{"fields":[["a","01"],["b",""],["c","7a7a"]]}"#);
    let (k2, _) = to_key_value(&pairs(&[(b"h", b"[{}]"), (b"v", b"V")]));
    assert_eq!(k2, None, "a header pair forces the envelope");
}

#[tokio::test]
async fn fetch_v0_reads_records_with_base_offset() {
    let sh = testutil::shared_with(testutil::test_config());
    seed(&sh, b"ft", 3);
    let request = req(0, "ft", 1, 1 << 20, 0, 1);
    let (body, partition, error, hwm) = one_partition(&sh, 0, &request).await;
    assert_eq!((partition, error, hwm), (0, errors::NONE, 3));
    let mut r = Reader::new(&body);
    skip_partition_head(&mut r);
    let records = r.bytes().unwrap().unwrap().to_vec();
    assert_eq!(r.remaining(), 0);
    let batch = parse_batch(&records).unwrap();
    assert_eq!(batch.base_offset, 1, "base_offset = fetch offset");
    assert_eq!(batch.records.len(), 2);
    assert_eq!(batch.records[0].key.as_deref(), Some(b"k1".as_slice()));
    assert_eq!(batch.records[0].value.as_deref(), Some(b"v1".as_slice()));
    assert_eq!(batch.records[1].key.as_deref(), Some(b"k2".as_slice()));
    // timestamp deltas relative to the first entry's arrival ms.
    assert_eq!(batch.first_timestamp, 101);
    assert_eq!(batch.records[1].timestamp_delta, 1);
}

#[tokio::test]
async fn fetch_v10_version_ladder() {
    let sh = testutil::shared_with(testutil::test_config());
    seed(&sh, b"ft", 1);
    let request = req(10, "ft", 0, 1 << 20, 0, 1);
    let mut r = Reader::new(&request);
    let (body, _parked) = handle_fetch(&mut r, 10, &sh).await.unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(r.i32(), Some(0), "throttle_time_ms");
    assert_eq!(r.i16(), Some(0), "top-level error (v7+, before session_id)");
    assert_eq!(r.i32(), Some(0), "session_id");
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i16(), Some(0));
    assert_eq!(r.i64(), Some(1), "hwm");
    assert_eq!(r.i64(), Some(1), "last_stable_offset");
    assert_eq!(r.i64(), Some(0), "log_start_offset");
    assert_eq!(r.array_len(), Some(None), "aborted_transactions null");
    assert!(r.bytes().unwrap().unwrap().len() > 60, "records");
    assert_eq!(r.remaining(), 0, "no preferred_read_replica in v10");
    // v4: throttle present, session_id/error absent, lso + aborted.
    let request4 = req(4, "ft", 0, 1 << 20, 0, 1);
    let mut r4 = Reader::new(&request4);
    let (b4, _parked) = handle_fetch(&mut r4, 4, &sh).await.unwrap();
    let mut r = Reader::new(&b4);
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    r.array_len();
    r.i32();
    r.i16();
    r.i64();
    assert_eq!(r.i64(), Some(1), "v4 last_stable_offset");
    assert_eq!(r.array_len(), Some(None), "v4 aborted null");
    assert!(r.bytes().is_some());
}

#[tokio::test]
async fn fetch_eof_out_of_range_and_unknown() {
    let sh = testutil::shared_with(testutil::test_config());
    seed(&sh, b"ft", 2);
    // EOF poll: offset == len -> empty record set, error NONE.
    let eof = req(0, "ft", 2, 1 << 20, 0, 1);
    let (body, _, error, hwm) = one_partition(&sh, 0, &eof).await;
    assert_eq!((error, hwm), (errors::NONE, 2));
    let mut r = Reader::new(&body);
    skip_partition_head(&mut r);
    assert_eq!(r.bytes().unwrap().unwrap().len(), 0, "EOF = len-0 records");
    // Past the end: OFFSET_OUT_OF_RANGE.
    let past = req(0, "ft", 5, 1 << 20, 0, 1);
    let (_, _, error, _) = one_partition(&sh, 0, &past).await;
    assert_eq!(error, errors::OFFSET_OUT_OF_RANGE);
    // Unknown topic: UNKNOWN_TOPIC_OR_PARTITION, hwm -1.
    let missing = req(0, "nope", 0, 1 << 20, 0, 1);
    let (_, _, error, hwm) = one_partition(&sh, 0, &missing).await;
    assert_eq!((error, hwm), (errors::UNKNOWN_TOPIC_OR_PARTITION, -1));
}

#[tokio::test]
async fn fetch_budget_truncates_but_keeps_one_record() {
    let sh = testutil::shared_with(testutil::test_config());
    seed(&sh, b"ft", 5);
    // A tiny budget: exactly one record survives (never zero).
    let tiny = req(0, "ft", 0, 1, 0, 1);
    let (body, _, error, _) = one_partition(&sh, 0, &tiny).await;
    assert_eq!(error, errors::NONE);
    let mut r = Reader::new(&body);
    skip_partition_head(&mut r);
    let recs = r.bytes().unwrap().unwrap();
    assert_eq!(parse_batch(recs).unwrap().records.len(), 1);
}

#[tokio::test]
async fn long_poll_expires_at_deadline_when_nothing_arrives() {
    let sh = testutil::shared_with(testutil::test_config());
    seed(&sh, b"ft", 1);
    let started = Instant::now();
    // min_bytes above what the one remaining (absent) record can give:
    // the handler must park out the whole max_wait.
    let parked = req(0, "ft", 1, 1 << 20, 150, 1 << 20);
    let (body, _, error, hwm) = one_partition(&sh, 0, &parked).await;
    assert!(started.elapsed() >= Duration::from_millis(140));
    assert_eq!((error, hwm), (errors::NONE, 1));
    let mut r = Reader::new(&body);
    skip_partition_head(&mut r);
    assert_eq!(r.bytes().unwrap().unwrap().len(), 0);
}

#[tokio::test]
async fn long_poll_wakes_on_notify() {
    let sh = Arc::new(testutil::shared_with(testutil::test_config()));
    seed(&sh, b"ft", 0);
    let prefix = hash::slot_with_prefix(b"ft").1;
    let mkey = model::meta_key(&prefix, b"ft/q0");
    let writer = {
        let sh = Arc::clone(&sh);
        let mkey = mkey.clone();
        let prefix = prefix.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let mut wb = WriteBatch::default();
            let id = model::EntryId { ms: 900, seq: 0 };
            wb.put(
                model::entry_key(&prefix, b"ft/q0", id),
                model::encode_entry(&[(b"v".as_slice(), b"woke".as_slice())]),
            );
            let meta = model::MetaPayload {
                created_ms: 1,
                len: 1,
                last_ms: 900,
                ..Default::default()
            };
            wb.put(&mkey, model::encode_meta_at(&meta, 0));
            ops::batch_write(&sh.store, wb).unwrap();
            wait::notify(&sh.wait_hub, &mkey);
        })
    };
    let started = Instant::now();
    let parked = req(0, "ft", 0, 1 << 20, 10_000, 1);
    let (body, _, error, hwm) = one_partition(&sh, 0, &parked).await;
    writer.await.unwrap();
    // Woken long before the 10s deadline.
    assert!(started.elapsed() < Duration::from_millis(5_000));
    assert_eq!((error, hwm), (errors::NONE, 1));
    let mut r = Reader::new(&body);
    skip_partition_head(&mut r);
    let recs = r.bytes().unwrap().unwrap().to_vec();
    assert_eq!(parse_batch(&recs).unwrap().records.len(), 1);
}
