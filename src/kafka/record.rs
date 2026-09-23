//! RecordBatch v2 decoding + CRC32C (Castagnoli) -- the Kafka message
//! envelope. P0 decodes and CRC-verifies (unit-tested against a
//! hand-built batch); the produce/fetch wire-up lands in P1/P2.
//!
//! Batch layout (v2, big-endian):
//! baseOffset i64 | batchLength i32 (bytes after this field) |
//! partitionLeaderEpoch i32 | magic i8 (== 2) | crc u32 (Castagnoli over
//! everything from `attributes` to the batch end) | attributes i16 |
//! lastOffsetDelta i32 | firstTimestamp i64 | maxTimestamp i64 |
//! producerId i64 | producerEpoch i16 | baseSequence i32 | records i32
//! then varint-length-prefixed records (key/value/headers as varints).

/// Reflected Castagnoli polynomial (CRC-32C).
const CRC32C_POLY: u32 = 0x82F6_3B78;

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC32C_TABLE: [u32; 256] = build_table();

/// CRC-32C (iSCSI) of `data`; table-driven, no external crate.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = CRC32C_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Compression codec from the batch attributes bits 0-2 (P0: none only).
pub fn compression_of(attributes: i16) -> u8 {
    (attributes & 0x7) as u8
}

/// One decoded record (varint deltas already resolved to their values).
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub timestamp_delta: i64,
    pub offset_delta: i32,
    pub key: Option<Vec<u8>>,
    pub value: Option<Vec<u8>>,
    pub headers: Vec<(String, Option<Vec<u8>>)>,
}

/// One decoded RecordBatch v2 (CRC verified on parse).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordBatch {
    pub base_offset: i64,
    pub partition_leader_epoch: i32,
    pub attributes: i16,
    pub last_offset_delta: i32,
    pub first_timestamp: i64,
    pub max_timestamp: i64,
    pub producer_id: i64,
    pub producer_epoch: i16,
    pub base_sequence: i32,
    pub records: Vec<Record>,
}

/// Parse + CRC-verify one RecordBatch v2. `Err` text is log-facing.
pub fn parse_batch(buf: &[u8]) -> Result<RecordBatch, String> {
    let mut r = crate::kafka::frame::Reader::new(buf);
    let base_offset = r.i64().ok_or("truncated base_offset")?;
    let batch_length = r.i32().ok_or("truncated batch_length")?;
    if batch_length < 0 {
        return Err(format!("negative batch_length {batch_length}"));
    }
    // batchLength covers everything after itself: header 21B total.
    let total = batch_length as usize + 12;
    if buf.len() < total {
        return Err(format!("truncated batch: {}/{} bytes", buf.len(), total));
    }
    let partition_leader_epoch = r.i32().ok_or("truncated leader epoch")?;
    let magic = r.i8().ok_or("truncated magic")?;
    if magic != 2 {
        return Err(format!("unsupported magic {magic} (v2 only)"));
    }
    let crc = r.u32().ok_or("truncated crc")?;
    let crc_start = r.pos();
    let computed = crc32c(&buf[crc_start..total]);
    if computed != crc {
        return Err(format!(
            "crc mismatch: stored {crc:08x} computed {computed:08x}"
        ));
    }
    let attributes = r.i16().ok_or("truncated attributes")?;
    let last_offset_delta = r.i32().ok_or("truncated last_offset_delta")?;
    let first_timestamp = r.i64().ok_or("truncated first_timestamp")?;
    let max_timestamp = r.i64().ok_or("truncated max_timestamp")?;
    let producer_id = r.i64().ok_or("truncated producer_id")?;
    let producer_epoch = r.i16().ok_or("truncated producer_epoch")?;
    let base_sequence = r.i32().ok_or("truncated base_sequence")?;
    let count = r.i32().ok_or("truncated record count")?;
    if count < 0 {
        return Err(format!("negative record count {count}"));
    }
    let kind = compression_of(attributes);
    if kind != 0 {
        // Kafka v2 wraps ONLY the records area (bytes after this 61-byte
        // header) -- never the header itself. Inner record deltas stay
        // relative to the outer base_offset/first_timestamp (the Lite
        // write path drops timestamps anyway: arrival-clock entry ids).
        #[cfg(feature = "kafka-codecs")]
        {
            let records = crate::kafka::codec::decompress_records(
                kind,
                &buf[r.pos()..total],
                count as usize,
            )?;
            return Ok(RecordBatch {
                base_offset,
                partition_leader_epoch,
                attributes,
                last_offset_delta,
                first_timestamp,
                max_timestamp,
                producer_id,
                producer_epoch,
                base_sequence,
                records,
            });
        }
        #[cfg(not(feature = "kafka-codecs"))]
        {
            return Err(format!(
                "compressed batches unsupported (attributes {attributes})"
            ));
        }
    }
    let mut records = Vec::new();
    for _ in 0..count {
        records.push(parse_record(&mut r)?);
    }
    Ok(RecordBatch {
        base_offset,
        partition_leader_epoch,
        attributes,
        last_offset_delta,
        first_timestamp,
        max_timestamp,
        producer_id,
        producer_epoch,
        base_sequence,
        records,
    })
}

/// Varint fields: length, attributes i8, timestampDelta, offsetDelta,
/// nullable key, nullable value, headers. `pub(crate)`: the compressed
/// batch path (kafka-codecs feature) reuses this parser over the
/// decompressed headerless records run.
pub(crate) fn parse_record(r: &mut crate::kafka::frame::Reader<'_>) -> Result<Record, String> {
    let _length = r.varint().ok_or("truncated record length")?;
    let _attributes = r.i8().ok_or("truncated record attributes")?;
    let timestamp_delta = r.varint().ok_or("truncated timestamp_delta")?;
    let offset_delta = r.varint().ok_or("truncated offset_delta")? as i32;
    let key = varint_bytes(r)
        .map(|o| o.map(|b| b.to_vec()))
        .ok_or("truncated record key")?;
    let value = varint_bytes(r)
        .map(|o| o.map(|b| b.to_vec()))
        .ok_or("truncated record value")?;
    let header_count = r.varint().ok_or("truncated header count")?;
    let mut headers = Vec::new();
    for _ in 0..header_count {
        let name = r
            .varint()
            .and_then(|len| {
                if len < 0 {
                    None
                } else {
                    Some(String::from_utf8_lossy(r.take(len as usize)?).into_owned())
                }
            })
            .ok_or("truncated header key")?;
        let val = varint_bytes(r)
            .map(|o| o.map(|b| b.to_vec()))
            .ok_or("truncated header value")?;
        headers.push((name, val));
    }
    Ok(Record {
        timestamp_delta,
        offset_delta,
        key,
        value,
        headers,
    })
}

/// varint length + bytes; a negative length is null (record key/value
/// and header values are nullable, header keys are not).
fn varint_bytes<'a>(r: &mut crate::kafka::frame::Reader<'a>) -> Option<Option<&'a [u8]>> {
    let len = r.varint()?;
    if len < 0 {
        return Some(None);
    }
    Some(Some(r.take(len as usize)?))
}

// ---- encoder (producer-side helper) ---------------------------------------

/// One record specification for [`build_batch`]: the exact mirror of
/// what [`Record`] decodes. Test/e2e request builder, not used by the
/// broker write path itself.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchRecord<'a> {
    pub timestamp_delta: i64,
    pub key: Option<&'a [u8]>,
    pub value: Option<&'a [u8]>,
    pub headers: Vec<(&'a str, Option<&'a [u8]>)>,
}

/// Encode a LEGAL RecordBatch v2: attributes 0 (never compressed), CRC
/// filled over `attributes..end`, ids -1/-1/-1 (no idempotence). An
/// empty `records` list is allowed (clients probe with empty batches).
pub fn build_batch(base_offset: i64, first_timestamp: i64, records: &[BatchRecord<'_>]) -> Vec<u8> {
    use crate::kafka::frame::{put_i16, put_i32, put_i64, put_i8, put_varint};
    let mut body = Vec::new();
    put_i16(&mut body, 0); // attributes: no compression
    put_i32(&mut body, records.len().saturating_sub(1) as i32); // last_offset_delta
    put_i64(&mut body, first_timestamp);
    let max_ts = first_timestamp + records.iter().map(|r| r.timestamp_delta).max().unwrap_or(0);
    put_i64(&mut body, max_ts);
    put_i64(&mut body, -1); // producer_id
    put_i16(&mut body, -1); // producer_epoch
    put_i32(&mut body, -1); // base_sequence
    put_i32(&mut body, records.len() as i32);
    for (delta, r) in records.iter().enumerate() {
        let mut rec = Vec::new();
        put_i8(&mut rec, 0); // record attributes
        put_varint(&mut rec, r.timestamp_delta);
        put_varint(&mut rec, delta as i64); // offset_delta
        put_varint_bytes(&mut rec, r.key);
        put_varint_bytes(&mut rec, r.value);
        put_varint(&mut rec, r.headers.len() as i64);
        for (name, val) in &r.headers {
            put_varint(&mut rec, name.len() as i64);
            rec.extend_from_slice(name.as_bytes());
            put_varint_bytes(&mut rec, val.as_deref());
        }
        put_varint(&mut body, rec.len() as i64);
        body.extend_from_slice(&rec);
    }
    let crc = crc32c(&body);
    let mut out = Vec::with_capacity(12 + 4 + 1 + 4 + body.len());
    put_i64(&mut out, base_offset);
    put_i32(&mut out, (4 + 1 + 4 + body.len()) as i32); // batch_length
    put_i32(&mut out, 0); // partition_leader_epoch
    put_i8(&mut out, 2); // magic
    out.extend_from_slice(&crc.to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// varint length + bytes / -1 for null (encoder twin of `varint_bytes`).
fn put_varint_bytes(out: &mut Vec<u8>, b: Option<&[u8]>) {
    use crate::kafka::frame::put_varint;
    match b {
        Some(b) => {
            put_varint(out, b.len() as i64);
            out.extend_from_slice(b);
        }
        None => put_varint(out, -1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_reference_vectors() {
        // RFC 3720 / iSCSI check value.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"a"), 0xC1D0_4330);
        assert_eq!(crc32c(b"abcdefgh"), 0x0A94_21B7);
    }

    /// Simple string/string convenience over [`build_batch`].
    fn simple(records: &[(&str, &str)]) -> Vec<u8> {
        build_batch(
            7,
            1000,
            &records
                .iter()
                .map(|(k, v)| BatchRecord {
                    timestamp_delta: 0,
                    key: Some(k.as_bytes()),
                    value: Some(v.as_bytes()),
                    headers: vec![],
                })
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn parses_and_verifies_a_v2_batch() {
        let buf = simple(&[("k0", "hello"), ("k1", "world")]);
        let batch = parse_batch(&buf).expect("batch parses");
        assert_eq!(batch.base_offset, 7);
        assert_eq!(batch.partition_leader_epoch, 0);
        assert_eq!(batch.records.len(), 2);
        assert_eq!(batch.records[0].key.as_deref(), Some(&b"k0"[..]));
        assert_eq!(batch.records[1].value.as_deref(), Some(&b"world"[..]));
        assert_eq!(batch.records[1].offset_delta, 1);
    }

    #[test]
    fn rejects_corrupt_crc_and_bad_magic() {
        let mut buf = simple(&[("k", "v")]);
        buf[17] ^= 1; // CRC byte 0 (after baseOffset+length+epoch+magic)
        assert!(parse_batch(&buf).unwrap_err().contains("crc mismatch"));
        let mut bad_magic = simple(&[]);
        bad_magic[16] = 1; // magic byte position: 8+4+4 = 16
        assert!(parse_batch(&bad_magic).unwrap_err().contains("magic"));
        let truncated = &simple(&[("k", "v")])[..10];
        assert!(parse_batch(truncated).unwrap_err().contains("truncated"));
    }

    #[test]
    fn build_batch_roundtrip_full_shape() {
        // Multi record, null key, tombstone value, headers incl. null.
        let buf = build_batch(
            42,
            5_000,
            &[
                BatchRecord {
                    timestamp_delta: 0,
                    key: Some(b"kk"),
                    value: Some(b"vv"),
                    headers: vec![],
                },
                BatchRecord {
                    timestamp_delta: 7,
                    key: None,
                    value: None,
                    headers: vec![("h1", Some(&b"x"[..])), ("h2", None)],
                },
            ],
        );
        let b = parse_batch(&buf).expect("roundtrip parses");
        assert_eq!(b.base_offset, 42);
        assert_eq!(b.last_offset_delta, 1);
        assert_eq!(b.first_timestamp, 5_000);
        assert_eq!(b.max_timestamp, 5_007);
        assert_eq!(b.records.len(), 2);
        assert_eq!(b.records[0].key.as_deref(), Some(&b"kk"[..]));
        assert_eq!(b.records[1].key, None);
        assert_eq!(b.records[1].value, None, "tombstone survives");
        assert_eq!(
            b.records[1].headers,
            vec![
                ("h1".to_string(), Some(b"x".to_vec())),
                ("h2".to_string(), None)
            ]
        );
        // An empty batch is legal on the wire.
        let empty = build_batch(0, 1, &[]);
        assert_eq!(parse_batch(&empty).unwrap().records.len(), 0);
    }
}
