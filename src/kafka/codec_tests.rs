//! Unit tests for `kafka::codec` (feature-gated): every codec in both
//! producer shapes, the librdkafka-2.15.1 wire fixtures, the lz4
//! content-checksum quirk fallback, and the full compressed
//! `parse_batch` pipeline (attributes patch + CRC reseal).

use crate::kafka::codec::{decompress_batch, decompress_records};
use crate::kafka::record::{build_batch, parse_batch, BatchRecord};

/// Real librdkafka 2.15.1 produce captures (compression.type=gzip/
/// snappy/lz4, 10 records k0..k9 / "value-N-payload", batch count 10).
const RDKAFKA_GZIP: &str = concat!(
    "1f8b08000000000000034dc8390a85501004c0411a918f8878006fa0b82fc779",
    "e08f5ad044c1db9b343215566d666053de61bffe55539de1d98fb0596d1681ad",
    "bef50fb0d377fe63b0d7f7fe1370d00ffe7fe0a81ffda7e0a49ffc67e0ac9ffd",
    "e7e0a25ffc17e0aa5fbf7f01b93c42ccf0000000"
);
const RDKAFKA_SNAPPY: &str = concat!(
    "f0017c2e000000046b301e76616c75652d302d7061796c6f6164002e00000204",
    "6b311e091800312e18000c04046b320d1800322e18000c06046b330d1800332e",
    "18000c08046b340d1800342e18000c0a046b350d1800352e18000c0c046b360d",
    "1800362e18000c0e046b370d1800372e18000c10046b380d1800382e18000c12",
    "046b390d1824392d7061796c6f616400"
);
const RDKAFKA_LZ4: &str = concat!(
    "04224d1860408286000000f3102e000000046b301e76616c75652d302d706179",
    "6c6f6164002e000002046b311800183118004304046b32180018321800430604",
    "6b331800183318004308046b34180018341800430a046b35180018351800430c",
    "046b36180018361800430e046b371800183718004310046b3818001838180043",
    "12046b391800a0392d7061796c6f61640000000000"
);

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn gzip_bytes(data: &[u8], raw_deflate: bool) -> Vec<u8> {
    use std::io::Write;
    if raw_deflate {
        let mut w = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        w.write_all(data).unwrap();
        w.finish().unwrap()
    } else {
        let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        w.write_all(data).unwrap();
        w.finish().unwrap()
    }
}

/// xerial framing (the Java shape): magic + version + compat + blocks.
fn xerial_wrap(raw: &[u8], blocks: usize) -> Vec<u8> {
    let mut out = vec![0x82, b'S', b'N', b'A', b'P', b'P', b'Y', 0x00];
    out.extend_from_slice(&1i32.to_be_bytes()); // version
    out.extend_from_slice(&1i32.to_be_bytes()); // compat
    let mut enc = snap::raw::Encoder::new();
    for chunk in raw.chunks(raw.len().div_ceil(blocks).max(1)) {
        let c = enc.compress_vec(chunk).unwrap();
        out.extend_from_slice(&(c.len() as i32).to_be_bytes());
        out.extend_from_slice(&c);
    }
    out
}

fn lz4_frame(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut w = lz4_flex::frame::FrameEncoder::new(Vec::new());
    w.write_all(data).unwrap();
    w.finish().unwrap()
}

/// A full-record sample: tombstone value, null key, headers, deltas.
fn sample_batch() -> Vec<u8> {
    build_batch(
        7,
        5_000,
        &[
            BatchRecord {
                timestamp_delta: 0,
                key: Some(b"kk"),
                value: Some(b"vv"),
                headers: vec![],
            },
            BatchRecord {
                timestamp_delta: 9,
                key: None,
                value: None,
                headers: vec![("h1", Some(&b"x"[..])), ("h2", None)],
            },
        ],
    )
}

/// Re-seal `batch` as `kind`-compressed with `blob` as the records area
/// (attributes patch + batch_length + CRC32C over attributes..end).
fn reseal(mut batch: Vec<u8>, kind: u8, blob: &[u8]) -> Vec<u8> {
    let total = 61 + blob.len();
    batch.truncate(61);
    batch[8..12].copy_from_slice(&((total - 12) as i32).to_be_bytes());
    batch[21..23].copy_from_slice(&(kind as i16).to_be_bytes());
    batch.extend_from_slice(blob);
    // CRC covers attributes..batch end (the compressed blob included).
    let crc = crate::kafka::record::crc32c(&batch[21..]);
    batch[17..21].copy_from_slice(&crc.to_be_bytes());
    batch
}

#[test]
fn gzip_both_shapes_roundtrip() {
    let area = &sample_batch()[61..];
    let full = gzip_bytes(area, false);
    assert_eq!(decompress_batch(1, &full).unwrap(), area);
    let bare = gzip_bytes(area, true);
    assert_eq!(decompress_batch(1, &bare).unwrap(), area, "raw deflate fallback");
    // Not gzip at all: CORRUPT_MESSAGE-class text (no "magic"/"compressed").
    let e = decompress_batch(1, b"not-a-gzip-stream").unwrap_err();
    assert!(e.contains("gzip decode failed"), "{e}");
}

#[test]
fn snappy_raw_and_xerial_roundtrip() {
    let area = &sample_batch()[61..];
    // Raw single stream (the librdkafka shape).
    let raw = snap::raw::Encoder::new().compress_vec(area).unwrap();
    assert_eq!(decompress_batch(2, &raw).unwrap(), area);
    // xerial, one block and three blocks (the Java shape).
    assert_eq!(decompress_batch(2, &xerial_wrap(area, 1)).unwrap(), area);
    assert_eq!(decompress_batch(2, &xerial_wrap(area, 3)).unwrap(), area);
    assert!(decompress_batch(2, b"\x82SNAPP").is_err(), "short blob");
}

#[test]
fn lz4_frame_and_checksum_quirk() {
    let area = &sample_batch()[61..];
    let frame = lz4_frame(area);
    assert_eq!(decompress_batch(3, &frame).unwrap(), area);
    // Quirk frame: content-checksum FLAG flipped on (header checksum
    // now mismatches too) + 4 garbage trailing bytes standing in for a
    // bogus content checksum. Strict FrameDecoder refuses; the tolerant
    // walk must still yield the exact records.
    let mut quirk = frame.clone();
    quirk[4] |= 0x04; // FLG content-checksum bit
    quirk.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(decompress_batch(3, &quirk).unwrap(), area, "quirk fallback");
    assert!(decompress_batch(3, b"nonsense").is_err(), "not a frame");
}

#[test]
fn librdkafka_wire_fixtures_decode() {
    for (kind, hex) in [(1u8, RDKAFKA_GZIP), (2, RDKAFKA_SNAPPY), (3, RDKAFKA_LZ4)] {
        let blob = unhex(hex);
        let recs = decompress_records(kind, &blob, 10).unwrap();
        assert_eq!(recs.len(), 10, "kind {kind}");
        for (i, r) in recs.iter().enumerate() {
            assert_eq!(r.key.as_deref(), Some(format!("k{i}").as_bytes()));
            assert_eq!(
                r.value.as_deref(),
                Some(format!("value-{i}-payload").as_bytes())
            );
            assert_eq!(r.offset_delta, i as i32, "inner deltas are 0..=9");
            assert_eq!(r.timestamp_delta, 0);
            assert!(r.headers.is_empty());
        }
    }
}

#[test]
fn zstd_and_unknown_kinds_rejected() {
    for kind in [4u8, 5, 6, 7] {
        let e = decompress_batch(kind, b"x").unwrap_err();
        assert!(e.contains("compressed batches unsupported"), "kind {kind}: {e}");
    }
}

#[test]
fn compressed_parse_batch_pipeline() {
    let plain = sample_batch();
    let area = plain[61..].to_vec();
    let expected = parse_batch(&plain).unwrap().records;
    for (kind, blob) in [
        (1u8, gzip_bytes(&area, false)),
        (2, snap::raw::Encoder::new().compress_vec(&area).unwrap()),
        (3, lz4_frame(&area)),
    ] {
        let batch = reseal(plain.clone(), kind, &blob);
        let parsed = parse_batch(&batch)
            .unwrap_or_else(|e| panic!("kind {kind}: {e}"));
        assert_eq!(parsed.attributes & 0x7, kind as i16);
        assert_eq!(parsed.base_offset, 7);
        assert_eq!(parsed.first_timestamp, 5_000);
        assert_eq!(parsed.last_offset_delta, 1);
        assert_eq!(parsed.records, expected, "kind {kind} fidelity");
    }
    // Decompression failure (gzip flag over plain bytes): deflate is a
    // tolerant codec, so the fallback may "decode" the records area
    // into garbage that trips the record parser instead of the gzip
    // error -- either way the classification is CORRUPT_MESSAGE (2).
    let bad = reseal(plain.clone(), 1, &area);
    let e = parse_batch(&bad).unwrap_err();
    assert_eq!(crate::kafka::produce::classify_parse_err(e), 2);
    // zstd stays 76 even with the codecs feature.
    let zstd = reseal(plain.clone(), 4, &area);
    let e = parse_batch(&zstd).unwrap_err();
    assert_eq!(crate::kafka::produce::classify_parse_err(e), 76);
}
