//! Compressed Produce over the wire against the REAL binary
//! (feature `kafka-codecs`; the whole file is cfg'd out of default
//! builds -- the stock binary keeps rejecting compression with 76).
//!
//! Each codec (gzip/snappy/lz4) rides a hand-built v2 RecordBatch whose
//! records area is compressed and whose attributes/CRC are re-sealed;
//! the ordinary append path must land every record (RESP XRANGE pins
//! the Lite field pairs) and answer rising base_offsets. zstd stays
//! UNSUPPORTED_COMPRESSION_TYPE (76) even with the feature.
//!
//! The real-SDK shape of these bytes (librdkafka 2.15.1) is pinned by
//! the librdkafka fixtures in src/kafka/codec_tests.rs and by the
//! compression section of scrtips/e2e_scenarios/scenario_kafka_sdk.sh.

#![cfg(feature = "kafka-codecs")]

mod common;
mod kafka_front_common;

use common::contains_bytes;
use kafka_front_common::{kafka_req, kafka_round, resp_one_shot, spawn_kafka_node, wait_accepting};
use rdb::kafka::frame::{put_array_len, put_i16, put_i32, put_string, Reader};
use rdb::kafka::record::{build_batch, crc32c, BatchRecord};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

const API_PRODUCE: i16 = 0;
const ERR_NONE: i16 = 0;
const ERR_CORRUPT: i16 = 2;
const ERR_UNSUPPORTED_COMPRESSION: i16 = 76;

/// Compress the records area the way each producer family does:
/// gzip via a full gzip member, snappy as one raw stream, lz4 frame.
fn compress(kind: u8, area: &[u8]) -> Vec<u8> {
    use std::io::Write;
    match kind {
        1 => {
            let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            w.write_all(area).unwrap();
            w.finish().unwrap()
        }
        2 => snap::raw::Encoder::new().compress_vec(area).unwrap(),
        3 => {
            let mut w = lz4_flex::frame::FrameEncoder::new(Vec::new());
            w.write_all(area).unwrap();
            w.finish().unwrap()
        }
        _ => area.to_vec(),
    }
}

/// Re-seal a plain build_batch as kind-compressed (attributes patch,
/// batch_length, CRC32C over attributes..end).
fn reseal(plain: &[u8], kind: u8, blob: &[u8]) -> Vec<u8> {
    let mut batch = plain[..61].to_vec();
    batch[8..12].copy_from_slice(&((49 + blob.len()) as i32).to_be_bytes());
    batch[21..23].copy_from_slice(&(kind as i16).to_be_bytes());
    batch.extend_from_slice(blob);
    let crc = crc32c(&batch[21..]);
    batch[17..21].copy_from_slice(&crc.to_be_bytes());
    batch
}

fn sample_batch(tag: &str) -> Vec<u8> {
    let keys: Vec<String> = (0..5).map(|i| format!("{tag}-{i}")).collect();
    let vals: Vec<String> = (0..5).map(|i| format!("{tag}-value-{i}")).collect();
    build_batch(
        0,
        1_000,
        &(0..5)
            .map(|i| BatchRecord {
                timestamp_delta: i,
                key: Some(keys[i as usize].as_bytes()),
                value: Some(vals[i as usize].as_bytes()),
                headers: vec![],
            })
            .collect::<Vec<_>>(),
    )
}

#[tokio::test]
async fn compressed_produce_roundtrips_through_the_real_binary() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-codec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    resp_one_shot(&resp, &[b"XADD", b"kc/q0", b"*", b"seed", b"1"]).await;
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka");

    let mut corr = 0i32;
    for (kind, tag) in [(1u8, "gzip"), (2u8, "snappy"), (3u8, "lz4")] {
        let plain = sample_batch(tag);
        let batch = reseal(&plain, kind, &compress(kind, &plain[61..]));
        corr += 1;
        let mut body = Vec::new();
        put_i16(&mut body, 1); // acks
        put_i32(&mut body, 5_000);
        put_array_len(&mut body, 1);
        put_string(&mut body, "kc");
        put_array_len(&mut body, 1);
        put_i32(&mut body, 0);
        put_i32(&mut body, batch.len() as i32);
        body.extend_from_slice(&batch);
        let payload = kafka_round(&mut sock, &kafka_req(API_PRODUCE, 2, corr, false, &body)).await;
        let mut r = Reader::new(&payload);
        assert_eq!(r.i32(), Some(corr), "corr echo");
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.string().as_deref(), Some("kc"));
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.i32(), Some(0), "{tag}: partition");
        assert_eq!(r.i16(), Some(ERR_NONE), "{tag}: accepted");
        // 1 seed + 5 per earlier codec batch.
        let expect_base = 1 + 5 * (kind as i64 - 1);
        assert_eq!(r.i64(), Some(expect_base), "{tag}: base_offset");
    }

    // zstd stays 76 even with kafka-codecs on.
    let plain = sample_batch("zstd");
    let batch = reseal(&plain, 4, &plain[61..]);
    corr += 1;
    let mut body = Vec::new();
    put_i16(&mut body, 1);
    put_i32(&mut body, 5_000);
    put_array_len(&mut body, 1);
    put_string(&mut body, "kc");
    put_array_len(&mut body, 1);
    put_i32(&mut body, 0);
    put_i32(&mut body, batch.len() as i32);
    body.extend_from_slice(&batch);
    let payload = kafka_round(&mut sock, &kafka_req(API_PRODUCE, 2, corr, false, &body)).await;
    let mut r = Reader::new(&payload);
    r.i32();
    r.array_len();
    r.string();
    r.array_len();
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i16(), Some(ERR_UNSUPPORTED_COMPRESSION), "zstd -> 76");

    // Garbage under a gzip flag: CORRUPT_MESSAGE (2), not 76.
    let mut garbage = vec![0xffu8; 32];
    garbage.extend_from_slice(b"not gzip at all");
    let batch = reseal(&plain, 1, &garbage);
    corr += 1;
    let mut body = Vec::new();
    put_i16(&mut body, 1);
    put_i32(&mut body, 5_000);
    put_array_len(&mut body, 1);
    put_string(&mut body, "kc");
    put_array_len(&mut body, 1);
    put_i32(&mut body, 0);
    put_i32(&mut body, batch.len() as i32);
    body.extend_from_slice(&batch);
    let payload = kafka_round(&mut sock, &kafka_req(API_PRODUCE, 2, corr, false, &body)).await;
    let mut r = Reader::new(&payload);
    r.i32();
    r.array_len();
    r.string();
    r.array_len();
    assert_eq!(r.i32(), Some(0));
    assert_eq!(
        r.i16(),
        Some(ERR_CORRUPT),
        "garbage gzip -> CORRUPT_MESSAGE"
    );

    // All 15 compressed-produced records landed on the Lite queue in
    // order (seed + 5 x 3).
    let xlen = resp_one_shot(&resp, &[b"XLEN", b"kc/q0"]).await;
    assert!(contains_bytes(&xlen, b":16\r\n"), "XLEN 16: {xlen:?}");
    for (tag, _kind) in [("gzip", 1u8), ("snappy", 2), ("lz4", 3)] {
        for i in 0..5 {
            let key = format!("{tag}-{i}");
            let val = format!("{tag}-value-{i}");
            let xrange = resp_one_shot(&resp, &[b"XRANGE", b"kc/q0", b"-", b"+"]).await;
            assert!(
                contains_bytes(
                    &xrange,
                    format!("$1\r\nk\r\n${}\r\n{}\r\n", key.len(), key).as_bytes()
                ),
                "{tag} key {key} missing"
            );
            assert!(
                contains_bytes(
                    &xrange,
                    format!("$1\r\nv\r\n${}\r\n{}\r\n", val.len(), val).as_bytes()
                ),
                "{tag} value {val} missing"
            );
        }
    }
    sock.shutdown().await.ok();
}
