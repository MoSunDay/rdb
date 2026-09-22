//! S3-compatible object storage front for rdb.
//!
//! A local-filesystem object store exposed over a hand-rolled HTTP/1.1
//! server (the `es/http.rs` / `rocksmq` school; no hyper, no new
//! crates). The layout maps the StarRocks tablet -> object-storage
//! hierarchy onto plain directories:
//!
//! ```text
//! StarRocks: warehouse/db/table/partition/tablet/schema_hash/rowset/segment
//! rdb:       <bucket>/rocksdb/<node bind>/ckpt_<unix_ms>/<sst files>
//! ```
//!
//! Buckets are directories under the store root, object keys are
//! slash-separated relative paths, and every object carries a sibling
//! `<obj>.s3meta.json` sidecar (etag/content-type). Real files land on
//! the local FS; the S3 REST surface (ListBuckets/ListObjectsV2/GET/
//! PUT/DELETE/HEAD, ranged GET) is what `checkpoint.rs` publishes
//! RocksDB checkpoints into and what aws cli / curl can read back.
//!
//! Layout of this module:
//! - `object`: the FS-backed object store (pure std::fs, testable);
//! - `xml`: S3 XML response rendering;
//! - `http`: the HTTP/1.1 transport + routing;
//! - `checkpoint`: the periodic RocksDB checkpoint publisher.

pub mod checkpoint;
pub mod http;
pub mod object;
pub mod xml;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// (year, month, day) from days since 1970-01-01 -- Howard Hinnant's
/// `civil_from_days`, valid for the whole proleptic Gregorian range.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// `2026-09-22T12:34:56.789Z` -- the S3 LastModified shape
/// (millisecond precision, always UTC).
pub(crate) fn iso8601_millis(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1_000);
    let ms = unix_ms.rem_euclid(1_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        sod / 3_600,
        (sod % 3_600) / 60,
        sod % 60
    )
}

/// `Tue, 22 Sep 2026 12:34:56 GMT` -- the RFC 7231 HTTP-date used for
/// the `Date` and `Last-Modified` headers.
pub(crate) fn http_date(unix_secs: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = unix_secs.div_euclid(86_400);
    let sod = unix_secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    // 1970-01-01 was a Thursday (index 4 with Sunday = 0).
    let dow = (days + 4).rem_euclid(7) as usize;
    format!(
        "{}, {:02} {} {y:04} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[dow],
        d,
        MONTHS[(mo - 1) as usize],
        sod / 3_600,
        (sod % 3_600) / 60,
        sod % 60
    )
}

/// Wall-clock milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `s3-<16 hex>` request id: md5 of (now, counter) -- unique within a
/// process and across restarts without pulling a uuid crate in.
pub(crate) fn request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    use md5::{Digest, Md5};
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Md5::new();
    hasher.update(now_ms().to_le_bytes());
    hasher.update(n.to_le_bytes());
    format!("s3-{}", &hex::encode(hasher.finalize())[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(20_618), (2026, 6, 14)); // leap-year span
    }

    #[test]
    fn date_rendering_anchors() {
        assert_eq!(iso8601_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601_millis(1_789_450_496_789), "2026-09-15T05:34:56.789Z");
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_789_450_496), "Tue, 15 Sep 2026 05:34:56 GMT");
    }
}
