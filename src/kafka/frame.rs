//! Kafka wire primitives: request/response headers, varints (unsigned
//! LEB128 + zigzag), classic vs compact (flexible) strings/bytes/arrays,
//! and tagged-field skipping/writing.
//!
//! Decoding is cursor-based and total: a truncated or malformed payload
//! surfaces as `None`, never a panic. Encoding is a set of `put_*`
//! append functions -- responses are plain byte buffers, no state.

/// Read-only cursor over one framed payload. `Copy` so the caller can
/// freeze a position (e.g. re-derive the CRC window of a RecordBatch).
#[derive(Clone, Copy)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    /// Bytes consumed so far.
    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.buf.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(out)
    }

    /// Big-endian signed int of `N` bytes (sign-extended from the first
    /// byte so the truncated widths read like their full forms).
    fn int<const N: usize>(&mut self) -> Option<i64> {
        let b = self.take(N)?;
        let mut v: i64 = if b[0] & 0x80 != 0 { -1 } else { 0 };
        for byte in b {
            v = (v << 8) | *byte as i64;
        }
        Some(v)
    }

    pub fn i8(&mut self) -> Option<i8> {
        self.int::<1>().map(|v| v as i8)
    }

    pub fn i16(&mut self) -> Option<i16> {
        self.int::<2>().map(|v| v as i16)
    }

    pub fn i32(&mut self) -> Option<i32> {
        self.int::<4>().map(|v| v as i32)
    }

    pub fn i64(&mut self) -> Option<i64> {
        self.int::<8>()
    }

    pub fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_be_bytes(b.try_into().ok()?))
    }

    /// Kafka BOOLEAN: any nonzero byte is true (the spec pins 0/1 but
    /// lenient reads survive hostile peers).
    pub fn boolean(&mut self) -> Option<bool> {
        Some(self.take(1)?[0] != 0)
    }

    /// Unsigned LEB128 varint (9+ bytes is malformed: the value is 64-bit).
    pub fn uvarint(&mut self) -> Option<u64> {
        let mut out: u64 = 0;
        let mut shift = 0u32;
        loop {
            let b = self.take(1)?[0];
            if shift >= 64 {
                return None;
            }
            out |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(out);
            }
            shift += 7;
        }
    }

    /// Zigzag-mapped signed varint (protobuf encoding).
    pub fn varint(&mut self) -> Option<i64> {
        let u = self.uvarint()?;
        Some(((u >> 1) as i64) ^ -((u & 1) as i64))
    }

    fn utf8(&mut self, len: i64) -> Option<Option<String>> {
        if len < 0 {
            return Some(None);
        }
        let b = self.take(len as usize)?;
        Some(Some(String::from_utf8(b.to_vec()).ok()?))
    }

    /// Classic string (int16 length); `Some(None)` = null (-1).
    pub fn nullable_string(&mut self) -> Option<Option<String>> {
        let len = self.i16()? as i64;
        self.utf8(len)
    }

    /// Classic string; a -1 length is malformed here (use the nullable
    /// variant where the schema allows null).
    pub fn string(&mut self) -> Option<String> {
        self.nullable_string().flatten()
    }

    /// Compact string (flexible): uvarint(len+1); 0 = null.
    pub fn compact_nullable_string(&mut self) -> Option<Option<String>> {
        let n = self.uvarint()? as i64;
        self.utf8(n - 1)
    }

    pub fn compact_string(&mut self) -> Option<String> {
        self.compact_nullable_string().flatten()
    }

    /// Classic bytes (int32 length); `Some(None)` = null (-1).
    pub fn bytes(&mut self) -> Option<Option<&'a [u8]>> {
        let len = self.i32()?;
        if len < 0 {
            return Some(None);
        }
        Some(Some(self.take(len as usize)?))
    }

    /// Compact bytes: uvarint(len+1); 0 = null.
    pub fn compact_bytes(&mut self) -> Option<Option<&'a [u8]>> {
        let n = self.uvarint()?;
        if n == 0 {
            return Some(None);
        }
        Some(Some(self.take((n - 1) as usize)?))
    }

    /// Classic array count (int32); `Some(None)` = null array (-1).
    pub fn array_len(&mut self) -> Option<Option<usize>> {
        let len = self.i32()?;
        if len < 0 {
            return Some(None);
        }
        Some(Some(len as usize))
    }

    /// Compact array count: uvarint(count+1); 0 = null.
    pub fn compact_array_len(&mut self) -> Option<Option<usize>> {
        let n = self.uvarint()?;
        if n == 0 {
            return Some(None);
        }
        Some(Some((n - 1) as usize))
    }

    /// Skip a whole tagged-fields section (unknown tags are ignored by
    /// design; this front never reads tag values).
    pub fn skip_tagged_fields(&mut self) -> Option<()> {
        let count = self.uvarint()?;
        for _ in 0..count {
            self.uvarint()?; // tag id
            let size = self.uvarint()? as usize;
            self.take(size)?;
        }
        Some(())
    }
}

// ---- writers -------------------------------------------------------------

pub fn put_i8(out: &mut Vec<u8>, v: i8) {
    out.push(v as u8);
}

pub fn put_i16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_bool(out: &mut Vec<u8>, v: bool) {
    out.push(v as u8);
}

pub fn put_uvarint(out: &mut Vec<u8>, v: u64) {
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

pub fn put_varint(out: &mut Vec<u8>, v: i64) {
    put_uvarint(out, ((v << 1) ^ (v >> 63)) as u64);
}

pub fn put_string(out: &mut Vec<u8>, s: &str) {
    put_i16(out, s.len() as i16);
    out.extend_from_slice(s.as_bytes());
}

pub fn put_nullable_string(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        Some(s) => put_string(out, s),
        None => put_i16(out, -1),
    }
}

pub fn put_compact_string(out: &mut Vec<u8>, s: &str) {
    put_uvarint(out, s.len() as u64 + 1);
    out.extend_from_slice(s.as_bytes());
}

/// Classic bytes (int32 length + payload).
pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_i32(out, b.len() as i32);
    out.extend_from_slice(b);
}

/// Classic nullable bytes; `None` = -1 length.
pub fn put_nullable_bytes(out: &mut Vec<u8>, b: Option<&[u8]>) {
    match b {
        Some(b) => put_bytes(out, b),
        None => put_i32(out, -1),
    }
}

pub fn put_compact_nullable_string(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        Some(s) => put_compact_string(out, s),
        None => put_uvarint(out, 0),
    }
}

/// Compact bytes (uvarint length + 1 + payload; used by flexible
/// versions, e.g. SyncGroup v3+ assignments).
pub fn put_compact_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_uvarint(out, b.len() as u64 + 1);
    out.extend_from_slice(b);
}

pub fn put_array_len(out: &mut Vec<u8>, n: usize) {
    put_i32(out, n as i32);
}

pub fn put_null_array_len(out: &mut Vec<u8>) {
    put_i32(out, -1);
}

pub fn put_compact_array_len(out: &mut Vec<u8>, n: usize) {
    put_uvarint(out, n as u64 + 1);
}

/// Tagged-fields section with zero tags (this front emits none).
pub fn put_empty_tagged_fields(out: &mut Vec<u8>) {
    out.push(0);
}

// ---- request/response headers --------------------------------------------

/// Request header v0-v2 (the tag tail is read when `flexible`).
#[derive(Debug, Clone)]
pub struct ReqHeader {
    pub api_key: i16,
    pub api_version: i16,
    pub correlation_id: i32,
    pub client_id: Option<String>,
}

/// Parse the request header; the returned `Reader` is positioned at the
/// body. `flexible` comes from the api-version registry (the header's
/// own shape depends on the requested api version, not the body's).
pub fn parse_req_header(buf: &[u8], flexible: bool) -> Option<(ReqHeader, Reader<'_>)> {
    let mut r = Reader::new(buf);
    let api_key = r.i16()?;
    let api_version = r.i16()?;
    let correlation_id = r.i32()?;
    let client_id = r.nullable_string()?;
    if flexible {
        r.skip_tagged_fields()?;
    }
    Some((
        ReqHeader {
            api_key,
            api_version,
            correlation_id,
            client_id,
        },
        r,
    ))
}

/// Response header: correlation id (+ empty tagged fields on flexible
/// versions).
pub fn put_resp_header(out: &mut Vec<u8>, correlation_id: i32, flexible: bool) {
    put_i32(out, correlation_id);
    if flexible {
        put_empty_tagged_fields(out);
    }
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
