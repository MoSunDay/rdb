//! Unit tests for `kafka::produce`: the record->field-pair map, the
//! RecordBatch parse-error -> Kafka error-code classification, and the
//! full v2 request round against an in-process store (base_offset,
//! field pairs landing, meta len, acks=0 no-response).

use rocksdb::WriteBatch;

use crate::hash;
use crate::kafka::errors;
use crate::kafka::frame::{put_array_len, put_i16, put_i32, put_string, Reader};
use crate::kafka::produce::{classify_parse_err, handle_produce, record_fields};
use crate::kafka::record::Record;
use crate::lite::model;
use crate::store::ops;

fn rec(key: Option<&[u8]>, value: Option<&[u8]>, headers: Vec<(&str, Option<&[u8]>)>) -> Record {
    Record {
        timestamp_delta: 0,
        offset_delta: 0,
        key: key.map(|k| k.to_vec()),
        value: value.map(|v| v.to_vec()),
        headers: headers
            .into_iter()
            .map(|(n, v)| (n.to_string(), v.map(|b| b.to_vec())))
            .collect(),
    }
}

#[test]
fn field_pair_map() {
    let kv = record_fields(&rec(Some(b"k"), Some(b"v"), vec![]));
    assert_eq!(kv, vec![(b"k".to_vec(), b"k".to_vec()), (b"v".to_vec(), b"v".to_vec())]);
    // key null, no headers: 1 pair
    assert_eq!(
        record_fields(&rec(None, Some(b"v"), vec![])),
        vec![(b"v".to_vec(), b"v".to_vec())]
    );
    // empty value (NOT a tombstone): ("v", b"")
    assert_eq!(
        record_fields(&rec(None, Some(b""), vec![])),
        vec![(b"v".to_vec(), Vec::new())]
    );
    // tombstone: ("__null__", b"")
    assert_eq!(
        record_fields(&rec(None, None, vec![])),
        vec![(b"__null__".to_vec(), Vec::new())]
    );
    // tombstone with a key: k pair + __null__ slot
    assert_eq!(
        record_fields(&rec(Some(b"k"), None, vec![])),
        vec![(b"k".to_vec(), b"k".to_vec()), (b"__null__".to_vec(), Vec::new())]
    );
    // headers: k + v + h (2 pairs when the key is null)
    let h = record_fields(&rec(
        Some(b"k"),
        Some(b"v"),
        vec![("h1", Some(b"x")), ("h2", None)],
    ));
    assert_eq!(h.len(), 3);
    assert_eq!(h[2].0, b"h".to_vec());
    assert_eq!(
        String::from_utf8_lossy(&h[2].1),
        r#"[{"n":"h1","v":"78"},{"n":"h2","v":null}]"#
    );
    let h2 = record_fields(&rec(None, Some(b"v"), vec![("only", None)]));
    assert_eq!(h2.len(), 2);
    assert_eq!(String::from_utf8_lossy(&h2[1].1), r#"[{"n":"only","v":null}]"#);
}

#[test]
fn parse_error_classification() {
    assert_eq!(classify_parse_err("unsupported magic 1 (v2 only)".into()), errors::UNSUPPORTED_VERSION);
    assert_eq!(
        classify_parse_err("compressed batches unsupported (attributes 3)".into()),
        errors::UNSUPPORTED_COMPRESSION_TYPE
    );
    assert_eq!(classify_parse_err("crc mismatch: ..".into()), errors::CORRUPT_MESSAGE);
    assert_eq!(classify_parse_err("truncated batch".into()), errors::CORRUPT_MESSAGE);
}

#[tokio::test]
async fn produce_appends_and_answers_base_offset() {
    use crate::lite::entries::scan_entries;
    use crate::lite::model::MIN_ID;
    let sh = crate::state::testutil::shared_with(crate::state::testutil::test_config());
    // RESP-born topic: partition 0 resolves to the q0 queue.
    let mut wb = WriteBatch::default();
    let prefix = hash::slot_with_prefix(b"tp").1;
    let mkey = model::meta_key(&prefix, b"tp/q0");
    let mp = model::MetaPayload { created_ms: 1, len: 1, last_ms: 7, last_seq: 0, ..Default::default() };
    wb.put(&mkey, model::encode_meta_at(&mp, 0));
    wb.put(
        model::entry_key(&prefix, b"tp/q0", model::EntryId { ms: 7, seq: 0 }),
        model::encode_entry(&[(b"f", b"v")]),
    );
    ops::batch_write(&sh.store, wb).unwrap();

    let batch = crate::kafka::record::build_batch(
        0,
        1_000,
        &[
            crate::kafka::record::BatchRecord { timestamp_delta: 0, key: Some(b"k1"), value: Some(b"v1"), headers: vec![] },
            crate::kafka::record::BatchRecord { timestamp_delta: 5, key: None, value: Some(b"v2"), headers: vec![] },
        ],
    );
    let mut req = Vec::new();
    put_i16(&mut req, 1); // acks
    put_i32(&mut req, 1000);
    put_array_len(&mut req, 1);
    put_string(&mut req, "tp");
    put_array_len(&mut req, 1);
    put_i32(&mut req, 0);
    req.extend_from_slice(&(batch.len() as i32).to_be_bytes());
    req.extend_from_slice(&batch);
    let body = handle_produce(&mut Reader::new(&req), 2, &sh)
        .await
        .unwrap()
        .expect("acks=1 answers");
    let mut r = Reader::new(&body);
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string(), Some("tp".into()));
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i16(), Some(0));
    assert_eq!(r.i64(), Some(1), "base_offset = pre-append len");
    assert_eq!(r.i64(), Some(-1), "v2 log_append_time");
    assert_eq!(r.i32(), Some(0), "v1+ throttle trails the array");
    assert_eq!(r.remaining(), 0);

    // Two entries landed with the mapped field pairs; meta len -> 3.
    let entries = scan_entries(&sh.store, &prefix, b"tp/q0", MIN_ID, 10).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[1].fields, vec![(b"k".to_vec(), b"k1".to_vec()), (b"v".to_vec(), b"v1".to_vec())]);
    assert_eq!(entries[2].fields, vec![(b"v".to_vec(), b"v2".to_vec())]);
    assert_eq!(
        model::read_meta(&sh.store, &prefix, b"tp/q0", None).unwrap().live().unwrap().len,
        3
    );

    // acks=0: no response frame at all.
    let mut req0 = req.clone();
    req0[1] = 0; // acks = 0 (low byte of the i16)
    assert!(handle_produce(&mut Reader::new(&req0), 2, &sh).await.unwrap().is_none());
}

/// P5b (kafka-codecs): a gzip-wrapped batch rides the ordinary append
/// path -- records decode in order (offset delta order = append order),
/// base_offset answers the pre-append len, entries land as field pairs.
#[cfg(feature = "kafka-codecs")]
#[tokio::test]
async fn compressed_produce_appends_through_the_lite_path() {
    use crate::lite::entries::scan_entries;
    use crate::lite::model::MIN_ID;
    let sh = crate::state::testutil::shared_with(crate::state::testutil::test_config());
    let wb = WriteBatch::default();
    ops::batch_write(&sh.store, wb).unwrap();
    let prefix = hash::slot_with_prefix(b"tp").1;

    // One RESP-born entry seeds partition 0 (the q0 queue) at len 1.
    let mut wb = WriteBatch::default();
    let mkey = model::meta_key(&prefix, b"tp/q0");
    let mp = model::MetaPayload { created_ms: 1, len: 1, last_ms: 7, last_seq: 0, ..Default::default() };
    wb.put(&mkey, model::encode_meta_at(&mp, 0));
    wb.put(
        model::entry_key(&prefix, b"tp/q0", model::EntryId { ms: 7, seq: 0 }),
        model::encode_entry(&[(b"f", b"v")]),
    );
    ops::batch_write(&sh.store, wb).unwrap();

    // Plain 3-record batch, resealed as gzip (attributes=1, CRC re-sealed
    // over the compressed records area).
    let plain = crate::kafka::record::build_batch(
        0,
        1_000,
        &[
            crate::kafka::record::BatchRecord { timestamp_delta: 0, key: Some(b"c0"), value: Some(b"v0"), headers: vec![] },
            crate::kafka::record::BatchRecord { timestamp_delta: 3, key: Some(b"c1"), value: Some(b"v1"), headers: vec![] },
            crate::kafka::record::BatchRecord { timestamp_delta: 6, key: Some(b"c2"), value: Some(b"v2"), headers: vec![] },
        ],
    );
    let area = plain[61..].to_vec();
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&area).unwrap();
    let blob = gz.finish().unwrap();
    let mut batch = plain[..61].to_vec();
    batch[8..12].copy_from_slice(&((49 + blob.len()) as i32).to_be_bytes());
    batch[21..23].copy_from_slice(&1i16.to_be_bytes()); // attributes: gzip
    batch.extend_from_slice(&blob);
    let crc = crate::kafka::record::crc32c(&batch[21..]);
    batch[17..21].copy_from_slice(&crc.to_be_bytes());

    let mut req = Vec::new();
    put_i16(&mut req, 1); // acks
    put_i32(&mut req, 100);
    put_array_len(&mut req, 1);
    put_string(&mut req, "tp");
    put_array_len(&mut req, 1);
    put_i32(&mut req, 0);
    put_i32(&mut req, batch.len() as i32);
    req.extend_from_slice(&batch);
    let body = handle_produce(&mut Reader::new(&req), 2, &sh)
        .await
        .unwrap()
        .expect("acks=1 answers");
    let mut r = Reader::new(&body);
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string(), Some("tp".into()));
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i16(), Some(0), "compressed batch accepted");
    assert_eq!(r.i64(), Some(1), "base_offset = pre-append len (1 seed)");

    // All three records landed in delta order; meta len -> 4.
    let entries = scan_entries(&sh.store, &prefix, b"tp/q0", MIN_ID, 10).unwrap();
    assert_eq!(entries.len(), 4);
    for (i, e) in entries.iter().enumerate().skip(1) {
        assert_eq!(
            e.fields,
            vec![
                (b"k".to_vec(), format!("c{}", i - 1).into_bytes()),
                (b"v".to_vec(), format!("v{}", i - 1).into_bytes())
            ]
        );
    }
    assert_eq!(
        model::read_meta(&sh.store, &prefix, b"tp/q0", None).unwrap().live().unwrap().len,
        4
    );
}
