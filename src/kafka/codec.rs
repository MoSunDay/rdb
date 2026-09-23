//! Produce-side decompression of compressed RecordBatch v2 payloads
//! (optional cargo feature `kafka-codecs`; the stock build keeps a
//! zero-new-dependency tree and rejects compressed batches with error
//! 76 UNSUPPORTED_COMPRESSION_TYPE).
//!
//! Scope decision (P5b): DEcompression only. Fetch always answers
//! uncompressed batches (attributes=0), which every Kafka client
//! accepts unconditionally -- no broker-side compression config.
//!
//! Kafka v2 compression wraps ONLY the records area (the bytes after
//! the 61-byte batch header); the header itself (base_offset,
//! first_timestamp, deltas base, CRC) stays plaintext. Inner record
//! offset/timestamp deltas stay relative to the OUTER header's bases;
//! the Lite write path drops timestamps anyway (arrival-clock ids).
//!
//! Codec quirks pinned by real librdkafka 2.15.1 captures (see
//! codec_tests.rs fixtures) + the Java-client lore:
//! - gzip: Java (GZIPOutputStream) and librdkafka both write a full
//!   gzip member; a bare raw-deflate stream is tolerated as fallback.
//! - snappy: librdkafka writes ONE raw snappy stream (uvarint size +
//!   compressed bytes, no framing); Java writes xerial framing
//!   (`\x82SNAPPY\x00` + version i32 + compat i32 + [len i32 + block]*).
//! - lz4: LZ4 Frame format. librdkafka sets no content checksum; Java
//!   (Kafka's LZ4FrameOutputStream) flags one -- and pre-KIP-110-era
//!   writers botched it. The strict `FrameDecoder` runs first; on
//!   checksum/structural failure a tolerant parser walks the frame
//!   manually, skipping header/block/content checksums.
//! - zstd: NOT built (would drag a C dependency); attributes=4 stays a
//!   76 error. Error texts avoid the word "magic" so
//!   `classify_parse_err` keeps mapping decode failures to
//!   CORRUPT_MESSAGE (2), not UNSUPPORTED_VERSION.

use crate::kafka::frame::Reader;
use crate::kafka::record::{parse_record, Record};

/// attributes bits 0-2 -> decompressed records area. `Err` text is
/// classify_parse_err input: "compressed ..." -> 76, anything else
/// (stream decode failure) -> CORRUPT_MESSAGE.
pub fn decompress_batch(kind: u8, buf: &[u8]) -> Result<Vec<u8>, String> {
    match kind {
        1 => gunzip(buf),
        2 => unsnappy(buf),
        3 => unlz4(buf),
        4 => Err("compressed batches unsupported (zstd not built)".into()),
        other => Err(format!("compressed batches unsupported (codec {other})")),
    }
}

/// Decompress the records area of a compressed batch and parse exactly
/// `count` bare records out of it (count comes from the outer header;
/// the inner blob is a headerless run of varint-length-prefixed
/// records).
pub fn decompress_records(kind: u8, blob: &[u8], count: usize) -> Result<Vec<Record>, String> {
    let raw = decompress_batch(kind, blob)?;
    let mut r = Reader::new(&raw);
    let mut out = Vec::with_capacity(count.min(8192));
    for _ in 0..count {
        out.push(parse_record(&mut r)?);
    }
    Ok(out)
}

fn gunzip(buf: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    if buf.is_empty() {
        return Ok(Vec::new()); // degenerate empty records area
    }
    let mut out = Vec::new();
    if flate2::read::GzDecoder::new(buf)
        .read_to_end(&mut out)
        .is_ok()
    {
        return Ok(out);
    }
    // Bare deflate stream (no gzip header/footer): tolerated fallback.
    let mut raw = Vec::new();
    flate2::read::DeflateDecoder::new(buf)
        .read_to_end(&mut raw)
        .map_err(|e| format!("gzip decode failed: {e}"))?;
    Ok(raw)
}

/// xerial framing header: 8-byte magic + version i32 + compat i32.
const XERIAL_MAGIC: [u8; 8] = [0x82, b'S', b'N', b'A', b'P', b'P', b'Y', 0x00];

fn unsnappy(buf: &[u8]) -> Result<Vec<u8>, String> {
    let mut dec = snap::raw::Decoder::new();
    if buf.len() < 16 || &buf[..8] != XERIAL_MAGIC {
        // Raw single stream (the librdkafka shape).
        return dec
            .decompress_vec(buf)
            .map_err(|e| format!("snappy decode failed: {e}"));
    }
    // xerial framed (the Java shape): version/compat then [len i32 + raw
    // snappy block]*, each block a standalone stream.
    let mut out = Vec::new();
    let mut o = 16usize;
    while o < buf.len() {
        let len = be32(buf, o).ok_or("snappy xerial block length truncated")? as usize;
        o += 4;
        let block = buf
            .get(o..o.checked_add(len).ok_or("snappy xerial length overflow")?)
            .ok_or("snappy xerial block truncated")?;
        o += len;
        let chunk = dec
            .decompress_vec(block)
            .map_err(|e| format!("snappy block decode failed: {e}"))?;
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn unlz4(buf: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::new();
    if lz4_flex::frame::FrameDecoder::new(buf)
        .read_to_end(&mut out)
        .is_ok()
    {
        return Ok(out);
    }
    unlz4_blocks(buf)
}

/// Tolerant LZ4 frame walk for the Kafka content-checksum quirk (Java
/// producers flag a checksum the strict decoder rejects). Structure is
/// still honored (magic, flags, block boundaries); checksums --
/// header, per-block, content -- are skipped, never verified.
fn unlz4_blocks(buf: &[u8]) -> Result<Vec<u8>, String> {
    if buf.get(0..4) != Some(&[0x04, 0x22, 0x4d, 0x18][..]) {
        return Err("lz4 frame signature mismatch".into());
    }
    if buf.len() < 7 {
        return Err("lz4 frame header truncated".into());
    }
    let flg = buf[4];
    if flg >> 6 != 1 {
        return Err(format!("lz4 frame version {} unsupported", flg >> 6));
    }
    if flg & 0x02 != 0 {
        return Err("lz4 reserved flag set".into());
    }
    if flg & 0x20 == 0 {
        // Block-linked frames need cross-block history; every Kafka
        // writer emits independent blocks, so refuse instead of
        // silently corrupting.
        return Err("lz4 linked blocks unsupported".into());
    }
    let block_checksums = flg & 0x10 != 0;
    let block_max = match (buf[5] >> 4) & 0x07 {
        4 => 64 * 1024,
        5 => 256 * 1024,
        6 => 1024 * 1024,
        7 => 4 * 1024 * 1024,
        v => return Err(format!("lz4 invalid block-max code {v}")),
    };
    // Field order: magic FLG BD [content_size i64] [dict_id i32] HC.
    let mut o = 6usize;
    if flg & 0x08 != 0 {
        o += 8; // content size (hint only)
    }
    if flg & 0x01 != 0 {
        return Err("lz4 dictionary frames unsupported".into());
    }
    o += 1; // header checksum byte: skipped
    let mut out = Vec::new();
    loop {
        let bsize = u32::from_le_bytes(
            buf.get(o..o + 4)
                .and_then(|b| b.try_into().ok())
                .ok_or("lz4 block header truncated")?,
        );
        o += 4;
        if bsize == 0 {
            break; // EndMark
        }
        let raw_block = bsize & 0x8000_0000 != 0;
        let bsize = (bsize & 0x7fff_ffff) as usize;
        let data = buf
            .get(o..o.checked_add(bsize).ok_or("lz4 block length overflow")?)
            .ok_or("lz4 block truncated")?;
        o += bsize;
        if raw_block {
            out.extend_from_slice(data);
        } else {
            let chunk = lz4_flex::block::decompress(data, block_max)
                .map_err(|e| format!("lz4 block decode failed: {e}"))?;
            out.extend_from_slice(&chunk);
        }
        if block_checksums {
            o += 4; // per-block checksum: skipped
        }
    }
    // Trailing content checksum (if flagged) deliberately skipped.
    Ok(out)
}

fn be32(buf: &[u8], o: usize) -> Option<u32> {
    buf.get(o..o + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes)
}
