//! Kafka request-side wire codec for the bench's hand-rolled client:
//! classic (non-flexible) framing only, exactly the two request versions
//! the load loops need -- Produce v2 and Fetch v4.
//!
//! The bench crate is deliberately independent of the rdb lib (and of
//! any third-party crate), so the few server-side primitives living in
//! `src/kafka/frame.rs` are re-implemented here in minimal form, and the
//! CRC-32C table + RecordBatch v2 encoder mirror `src/kafka/record.rs`
//! (provenance: same reflected Castagnoli polynomial, same field order;
//! keeping this tiny copy beats taking a dependency across crates).
//! Response decoding lives in `kafka_reply`.

// ---- append writers (big-endian, classic framing) ------------------------

pub fn put_i16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_string(out: &mut Vec<u8>, s: &str) {
    put_i16(out, s.len() as i16);
    out.extend_from_slice(s.as_bytes());
}

/// Classic array length (int32 element count).
fn put_array_len(out: &mut Vec<u8>, n: usize) {
    put_i32(out, n as i32);
}

/// Unsigned LEB128 varint (Kafka record framing).
fn put_uvarint(out: &mut Vec<u8>, v: u64) {
    let mut v = v;
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Zigzag-mapped signed varint.
fn put_varint(out: &mut Vec<u8>, v: i64) {
    put_uvarint(out, ((v << 1) ^ (v >> 63)) as u64);
}

// ---- CRC-32C (copied from src/kafka/record.rs) ---------------------------

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

// ---- RecordBatch v2 ------------------------------------------------------

/// One bench record: non-null key + value, no headers, no timestamp
/// deltas (the load only varies the bytes, not the timing shape).
pub struct BenchRecord<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
}

/// Encode one RecordBatch v2 into `out` (cleared first): attributes 0,
/// lastOffsetDelta = n-1, producerId/epoch/sequence -1, CRC over the
/// body. Same layout as `src/kafka/record.rs::build_batch`.
pub fn build_batch(first_timestamp: i64, records: &[BenchRecord<'_>], out: &mut Vec<u8>) {
    let mut body = Vec::with_capacity(records.len() * 96 + 64);
    put_i16(&mut body, 0); // attributes: no compression
    put_i32(&mut body, records.len().saturating_sub(1) as i32); // last_offset_delta
    put_i64(&mut body, first_timestamp);
    put_i64(&mut body, first_timestamp); // max_timestamp (all deltas are 0)
    put_i64(&mut body, -1); // producer_id
    put_i16(&mut body, -1); // producer_epoch
    put_i32(&mut body, -1); // base_sequence
    put_i32(&mut body, records.len() as i32);
    for (delta, r) in records.iter().enumerate() {
        let mut rec = Vec::with_capacity(r.key.len() + r.value.len() + 16);
        rec.push(0u8); // record attributes
        put_varint(&mut rec, 0); // timestamp_delta
        put_varint(&mut rec, delta as i64); // offset_delta
        put_varint(&mut rec, r.key.len() as i64);
        rec.extend_from_slice(r.key);
        put_varint(&mut rec, r.value.len() as i64);
        rec.extend_from_slice(r.value);
        put_varint(&mut rec, 0); // header count
        put_varint(&mut body, rec.len() as i64);
        body.extend_from_slice(&rec);
    }
    let crc = crc32c(&body);
    out.clear();
    put_i64(out, 0); // base_offset (producer-side placeholder)
    put_i32(out, (4 + 1 + 4 + body.len()) as i32); // batch_length
    put_i32(out, 0); // partition_leader_epoch
    out.push(2); // magic
    out.extend_from_slice(&crc.to_be_bytes());
    out.extend_from_slice(&body);
}

/// Total records across the concatenated RecordBatch v2 frames of a
/// Fetch records blob. Only the fixed 61-byte header is decoded (it
/// ends with the int32 record count), never the record bodies.
pub fn count_records(blob: &[u8]) -> Result<u64, String> {
    let mut pos = 0usize;
    let mut total = 0u64;
    while pos < blob.len() {
        let rest = &blob[pos..];
        if rest.len() < 61 {
            return Err("truncated RecordBatch header".to_string());
        }
        if rest[16] != 2 {
            return Err(format!("unsupported RecordBatch magic {}", rest[16]));
        }
        let batch_len = i32::from_be_bytes(rest[8..12].try_into().unwrap());
        if batch_len < 0 {
            return Err(format!("negative batch_length {batch_len}"));
        }
        let end = 12 + batch_len as usize;
        if rest.len() < end {
            return Err(format!("truncated batch: {}/{end} bytes", rest.len()));
        }
        let count = i32::from_be_bytes(rest[57..61].try_into().unwrap());
        if count < 0 {
            return Err(format!("negative record count {count}"));
        }
        total += count as u64;
        pos += end;
    }
    Ok(total)
}

// ---- request assembly ----------------------------------------------------

const API_PRODUCE: i16 = 0;
const API_FETCH: i16 = 1;
const PRODUCE_V2: i16 = 2;
const FETCH_V4: i16 = 4;

/// Classic request header (api_key, api_version, correlation_id,
/// nullable client_id) + body -- the non-flexible shape both load
/// versions use.
fn request(api_key: i16, api_version: i16, corr: i32, client_id: &str, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 16 + client_id.len());
    put_i16(&mut out, api_key);
    put_i16(&mut out, api_version);
    put_i32(&mut out, corr);
    put_string(&mut out, client_id);
    out.extend_from_slice(body);
    out
}

/// One Produce v2 request: acks, timeout, one topic / partition 0 with
/// one RecordBatch payload.
pub fn produce_request(
    corr: i32,
    client_id: &str,
    topic: &str,
    acks: i16,
    timeout_ms: i32,
    batch: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(batch.len() + 64);
    put_i16(&mut body, acks);
    put_i32(&mut body, timeout_ms);
    put_array_len(&mut body, 1);
    put_string(&mut body, topic);
    put_array_len(&mut body, 1);
    put_i32(&mut body, 0); // partition 0
    put_i32(&mut body, batch.len() as i32);
    body.extend_from_slice(batch);
    request(API_PRODUCE, PRODUCE_V2, corr, client_id, &body)
}

/// One Fetch v4 request: no replica, long-poll budgets, one topic /
/// partition 0 from `offset`.
pub fn fetch_request(
    corr: i32,
    client_id: &str,
    topic: &str,
    offset: i64,
    max_wait_ms: i32,
    min_bytes: i32,
    max_bytes: i32,
    partition_max_bytes: i32,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(96);
    put_i32(&mut body, -1); // replica_id: none
    put_i32(&mut body, max_wait_ms);
    put_i32(&mut body, min_bytes);
    put_i32(&mut body, max_bytes); // v3+
    body.push(0u8); // isolation_level (v4+): read_uncommitted
    put_array_len(&mut body, 1);
    put_string(&mut body, topic);
    put_array_len(&mut body, 1);
    put_i32(&mut body, 0); // partition 0
    put_i64(&mut body, offset);
    put_i32(&mut body, partition_max_bytes);
    request(API_FETCH, FETCH_V4, corr, client_id, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(n: usize) -> Vec<BenchRecord<'static>> {
        (0..n)
            .map(|i| BenchRecord {
                key: Box::leak(format!("{i:016x}").into_boxed_str()).as_bytes(),
                value: b"vvvv",
            })
            .collect()
    }

    #[test]
    fn crc32c_matches_known_vector() {
        // CRC-32C check value of "123456789" (iSCSI/RFC 3720).
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn batch_layout_is_countable() {
        let mut buf = Vec::new();
        build_batch(1_000, &records(3), &mut buf);
        assert_eq!(buf[16], 2, "magic");
        let batch_len = i32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
        assert_eq!(buf.len(), 12 + batch_len, "batch_length covers the tail");
        assert_eq!(
            i32::from_be_bytes(buf[57..61].try_into().unwrap()),
            3,
            "record count field"
        );
        assert_eq!(count_records(&buf).unwrap(), 3);
        // Two concatenated batches (a multi-batch fetch blob) sum up.
        let mut blob = buf.clone();
        build_batch(2_000, &records(2), &mut buf);
        blob.extend_from_slice(&buf);
        assert_eq!(count_records(&blob).unwrap(), 5);
    }

    #[test]
    fn count_records_rejects_truncation_and_bad_magic() {
        let mut buf = Vec::new();
        build_batch(0, &records(1), &mut buf);
        assert!(count_records(&buf[..20]).unwrap_err().contains("truncated"));
        let mut bad = buf.clone();
        bad[16] = 1;
        assert!(count_records(&bad).unwrap_err().contains("magic"));
    }

    #[test]
    fn requests_carry_classic_header_and_versions() {
        let mut batch = Vec::new();
        build_batch(0, &records(1), &mut batch);
        let req = produce_request(5, "cid", "t1", 1, 500, &batch);
        assert_eq!(&req[..2], &0i16.to_be_bytes(), "api key Produce");
        assert_eq!(&req[2..4], &2i16.to_be_bytes(), "version v2");
        assert_eq!(&req[4..8], &5i32.to_be_bytes(), "corr id");
        let fetch = fetch_request(6, "cid", "t1", 7, 500, 1, 100, 50);
        assert_eq!(&fetch[..2], &1i16.to_be_bytes(), "api key Fetch");
        assert_eq!(&fetch[2..4], &4i16.to_be_bytes(), "version v4");
    }
}
