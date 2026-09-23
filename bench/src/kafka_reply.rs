//! Kafka response decoding for the bench client: Produce v2 / Fetch v4
//! replies, reduced to exactly the fields the load loops assert on
//! (correlation-id echo, error code, base_offset, high watermark,
//! records blob). Classic framing only -- the response header is the
//! 4-byte correlation id, no tagged fields.

/// Read-only cursor over a response body; every method returns `None`
/// on truncation so parsing stays total (no panics on hostile bytes).
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.buf.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(out)
    }

    fn i16(&mut self) -> Option<i16> {
        self.take(2)
            .map(|b| i16::from_be_bytes(b.try_into().unwrap()))
    }

    fn i32(&mut self) -> Option<i32> {
        self.take(4)
            .map(|b| i32::from_be_bytes(b.try_into().unwrap()))
    }

    fn i64(&mut self) -> Option<i64> {
        self.take(8)
            .map(|b| i64::from_be_bytes(b.try_into().unwrap()))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.i16()?;
        if len < 0 {
            return None;
        }
        String::from_utf8(self.take(len as usize)?.to_vec()).ok()
    }

    /// Classic array count; `Some(None)` = null array (-1).
    fn array_len(&mut self) -> Option<Option<usize>> {
        let len = self.i32()?;
        if len < 0 {
            return Some(None);
        }
        Some(Some(len as usize))
    }

    /// Classic bytes (int32 length); `Some(None)` = null (-1).
    fn bytes(&mut self) -> Option<Option<&'a [u8]>> {
        let len = self.i32()?;
        if len < 0 {
            return Some(None);
        }
        Some(Some(self.take(len as usize)?))
    }
}

/// Strip the response header after checking the echoed correlation id;
/// returns the body slice.
fn body_after_header(payload: &[u8], expected_corr: i32) -> Result<&[u8], String> {
    if payload.len() < 4 {
        return Err("short response (no correlation id)".to_string());
    }
    let corr = i32::from_be_bytes(payload[..4].try_into().unwrap());
    if corr != expected_corr {
        return Err(format!(
            "correlation id mismatch: sent {expected_corr}, got {corr}"
        ));
    }
    Ok(&payload[4..])
}

/// Walk the one topic/partition array pair the bench always requests.
fn single_partition(r: &mut Reader<'_>, api: &str) -> Result<(), String> {
    let n_topics = r
        .array_len()
        .ok_or_else(|| format!("malformed {api} reply"))?
        .ok_or_else(|| format!("null {api} topics array"))?;
    if n_topics != 1 {
        return Err(format!("{api} reply: {n_topics} topics, expected 1"));
    }
    r.string()
        .ok_or_else(|| format!("malformed {api} topic echo"))?;
    let n_parts = r
        .array_len()
        .ok_or_else(|| format!("malformed {api} reply"))?
        .ok_or_else(|| format!("null {api} partitions array"))?;
    if n_parts != 1 {
        return Err(format!("{api} reply: {n_parts} partitions, expected 1"));
    }
    r.i32()
        .ok_or_else(|| format!("malformed {api} partition"))?;
    Ok(())
}

/// Produce v2 reply reduced to the two fields the bench asserts on.
#[derive(Debug)]
pub struct ProduceOut {
    pub error: i16,
    pub base_offset: i64,
}

pub fn parse_produce(payload: &[u8], expected_corr: i32) -> Result<ProduceOut, String> {
    let body = body_after_header(payload, expected_corr)?;
    let mut r = Reader::new(body);
    single_partition(&mut r, "produce")?;
    let error = r.i16().ok_or("malformed produce error_code")?;
    let base_offset = r.i64().ok_or("malformed produce base_offset")?;
    // log_append_time (v2) + throttle_time_ms (v1+) trail unread.
    Ok(ProduceOut { error, base_offset })
}

/// Fetch v4 reply reduced to error / high watermark / records blob.
pub struct FetchOut {
    pub error: i16,
    pub hwm: i64,
    pub records: Vec<u8>,
}

pub fn parse_fetch(payload: &[u8], expected_corr: i32) -> Result<FetchOut, String> {
    let body = body_after_header(payload, expected_corr)?;
    let mut r = Reader::new(body);
    r.i32().ok_or("malformed fetch throttle_time_ms")?;
    single_partition(&mut r, "fetch")?;
    let error = r.i16().ok_or("malformed fetch error_code")?;
    let hwm = r.i64().ok_or("malformed fetch high_watermark")?;
    r.i64().ok_or("malformed fetch last_stable_offset")?;
    let aborted = r
        .array_len()
        .ok_or("malformed fetch aborted_transactions")?
        .unwrap_or(0);
    for _ in 0..aborted {
        // AbortedTransaction = producerId i64 + firstOffset i64 (rdb
        // always answers null; skipping keeps the parser total).
        r.i64().ok_or("malformed aborted transaction")?;
        r.i64().ok_or("malformed aborted transaction")?;
    }
    let records = r
        .bytes()
        .ok_or("malformed fetch records")?
        .map(|b| b.to_vec());
    Ok(FetchOut {
        error,
        hwm,
        records: records.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kafka_wire::{build_batch, put_i16, put_i32, put_i64, put_string, BenchRecord};

    #[test]
    fn produce_reply_parses_and_checks_corr() {
        let mut body = Vec::new();
        put_i32(&mut body, 1); // topics array len
        put_string(&mut body, "bench1");
        put_i32(&mut body, 1); // partitions array len
        put_i32(&mut body, 0); // partition
        put_i16(&mut body, 0); // error NONE
        put_i64(&mut body, 41); // base_offset
        put_i64(&mut body, -1); // log_append_time (v2)
        put_i32(&mut body, 0); // throttle_time_ms (v1+)
        let mut payload = 7i32.to_be_bytes().to_vec();
        payload.extend_from_slice(&body);
        let out = parse_produce(&payload, 7).unwrap();
        assert_eq!((out.error, out.base_offset), (0, 41));
        assert!(parse_produce(&payload, 8).unwrap_err().contains("mismatch"));
        let mut short = payload.clone();
        short.truncate(10);
        assert!(parse_produce(&short, 7).is_err(), "truncated body errors");
    }

    #[test]
    fn fetch_reply_parses_records_blob() {
        let mut batch = Vec::new();
        build_batch(
            0,
            &[
                BenchRecord {
                    key: b"k",
                    value: b"vvvv",
                },
                BenchRecord {
                    key: b"k",
                    value: b"vvvv",
                },
            ],
            &mut batch,
        );
        let mut body = Vec::new();
        put_i32(&mut body, 0); // throttle_time_ms
        put_i32(&mut body, 1); // topics array len
        put_string(&mut body, "bench1");
        put_i32(&mut body, 1); // partitions array len
        put_i32(&mut body, 0); // partition
        put_i16(&mut body, 0); // error
        put_i64(&mut body, 9); // high_watermark
        put_i64(&mut body, 9); // last_stable_offset
        put_i32(&mut body, -1); // aborted_transactions: null
        put_i32(&mut body, batch.len() as i32);
        body.extend_from_slice(&batch);
        let mut payload = 3i32.to_be_bytes().to_vec();
        payload.extend_from_slice(&body);
        let out = parse_fetch(&payload, 3).unwrap();
        assert_eq!((out.error, out.hwm), (0, 9));
        assert_eq!(crate::kafka_wire::count_records(&out.records).unwrap(), 2);
    }
}
