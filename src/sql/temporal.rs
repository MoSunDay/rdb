//! Pure civil-date/time math for the SQL temporal types (DATE and
//! DATETIME). No chrono: the algorithms are Howard Hinnant's
//! `days_from_civil` / `civil_from_days` (proleptic Gregorian),
//! restricted to years 0001..=9999 like MySQL.
//!
//! Storage model: DATE is days since 1970-01-01 (i64), DATETIME is
//! microseconds since the epoch (i64). This module owns every string
//! form of those integers: parsing accepts the MySQL canonical
//! `YYYY-MM-DD` / `YYYY-MM-DD HH:MM:SS[.ffffff]` spellings (plus the
//! compact digit-only twins), formatting always emits the canonical
//! one.

use std::time::{SystemTime, UNIX_EPOCH};

/// Microseconds in one day.
pub const MICROS_PER_DAY: i64 = 86_400_000_000;
/// Seconds in one day.
pub const SECS_PER_DAY: i64 = 86_400;

/// Numeric value of an all-ASCII-digit slice (None on any other byte).
fn digits(s: &[u8]) -> Option<i64> {
    let mut n: i64 = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n * 10 + i64::from(b - b'0');
    }
    Some(n)
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Days in one month of a (proleptic Gregorian) year; 0 for a month
/// outside 1..=12.
fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from 1970-01-01 to the given civil date. None outside year
/// 0001..=9999 or for an invalid month/day (leap-year correct).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    // Howard Hinnant's days_from_civil; y is positive here, so plain
    // integer division is already the floor the algorithm assumes.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 }); // [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

/// Inverse of [`days_from_civil`]. None when the day count falls
/// outside year 0001..=9999.
pub fn civil_from_days(z: i64) -> Option<(i64, u32, u32)> {
    // Howard Hinnant's civil_from_days (era shifts keep the arithmetic
    // exact for negative day counts too). The epoch shift itself must
    // stay checked: raw i64 extremes (e.g. corrupted rows) would wrap.
    let z = z.checked_add(719_468)?;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap(); // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    if !(1..=9999).contains(&y) {
        return None;
    }
    Some((y, m, d))
}

/// Parse `YYYY-MM-DD` or `YYYYMMDD` into days since the epoch.
/// Strictly two-digit month/day (the MySQL canonical spellings).
pub fn parse_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let (y, m, d) = match b.len() {
        10 if b[4] == b'-' && b[7] == b'-' => {
            (digits(&b[..4])?, digits(&b[5..7])?, digits(&b[8..10])?)
        }
        8 => (digits(&b[..4])?, digits(&b[4..6])?, digits(&b[6..8])?),
        _ => return None,
    };
    days_from_civil(y, m as u32, d as u32)
}

/// Canonical `YYYY-MM-DD` of a day count. Out-of-range days have no
/// civil spelling; they render as the raw integer (debug aid -- the
/// write path never stores them).
pub fn format_date(days: i64) -> String {
    civil_from_days(days).map_or_else(
        || days.to_string(),
        |(y, m, d)| format!("{y:04}-{m:02}-{d:02}"),
    )
}

/// Parse `YYYY-MM-DD[ T]HH:MM:SS[.1-6 frac]`, `YYYYMMDDHHMMSS[.frac]`
/// or a bare date form (midnight) into microseconds since the epoch.
/// Fraction digits beyond six are rejected, not truncated.
pub fn parse_datetime(s: &str) -> Option<i64> {
    let (int_part, frac_micros) = match s.split_once('.') {
        // 1..=6 digits, zero-padded on the right to microseconds.
        Some((i, f)) => {
            let fb = f.as_bytes();
            if fb.is_empty() || fb.len() > 6 || digits(fb).is_none() {
                return None;
            }
            let mut padded = [b'0'; 6];
            padded[..fb.len()].copy_from_slice(fb);
            (i, digits(&padded)?)
        }
        None => (s, 0),
    };
    let b = int_part.as_bytes();
    let (days, secs) = match b.len() {
        8 => (parse_compact_digits(&b[..8])?, 0),
        14 => {
            let days = parse_compact_digits(&b[..8])?;
            (
                days,
                hhmmss(digits(&b[8..10])?, digits(&b[10..12])?, digits(&b[12..14])?)?,
            )
        }
        19 if b[10] == b' ' || b[10] == b'T' => {
            if b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
                return None;
            }
            let days = days_from_civil(
                digits(&b[..4])?,
                digits(&b[5..7])? as u32,
                digits(&b[8..10])? as u32,
            )?;
            (
                days,
                hhmmss(
                    digits(&b[11..13])?,
                    digits(&b[14..16])?,
                    digits(&b[17..19])?,
                )?,
            )
        }
        10 => (parse_date(int_part)?, 0),
        _ => return None,
    };
    Some(days * MICROS_PER_DAY + secs * 1_000_000 + frac_micros)
}

/// Seconds-of-day of an HH:MM:SS triple (None outside 00:00:00..
/// 23:59:59; no leap-second 60).
fn hhmmss(h: i64, m: i64, s: i64) -> Option<i64> {
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) || !(0..=59).contains(&s) {
        return None;
    }
    Some(h * 3600 + m * 60 + s)
}

/// Canonical `YYYY-MM-DD HH:MM:SS` of a microsecond count, with the
/// `.{ffffff}` fraction appended only when the micros-of-second part
/// is non-zero.
pub fn format_datetime(us: i64) -> String {
    let days = us.div_euclid(MICROS_PER_DAY);
    let rem = us.rem_euclid(MICROS_PER_DAY);
    let (secs, micros) = (rem / 1_000_000, rem % 1_000_000);
    civil_from_days(days).map_or_else(
        || us.to_string(),
        |(y, m, d)| {
            let base = format!(
                "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
                secs / 3600,
                (secs / 60) % 60,
                secs % 60
            );
            if micros == 0 {
                base
            } else {
                format!("{base}.{micros:06}")
            }
        },
    )
}

/// `YYYYMMDD` of a day count (None outside the civil range).
pub fn compact_date(days: i64) -> Option<i64> {
    let (y, m, d) = civil_from_days(days)?;
    Some(y * 10_000 + i64::from(m) * 100 + i64::from(d))
}

/// 8-digit `YYYYMMDD` byte slice -> days (shared by the string and
/// integer compact parsers).
fn parse_compact_digits(b: &[u8]) -> Option<i64> {
    if b.len() != 8 {
        return None;
    }
    let (y, m, d) = (digits(&b[..4])?, digits(&b[4..6])?, digits(&b[6..8])?);
    days_from_civil(y, m as u32, d as u32)
}

/// Days of a `YYYYMMDD` integer (validated as a real date).
pub fn parse_compact_date(i: i64) -> Option<i64> {
    parse_compact_digits(&i.to_string().into_bytes())
}

/// `YYYYMMDDHHMMSS` of a microsecond count (fraction truncated).
pub fn compact_datetime(us: i64) -> Option<i64> {
    let date = compact_date(us.div_euclid(MICROS_PER_DAY))?;
    let rem = us.rem_euclid(MICROS_PER_DAY) / 1_000_000;
    let hms = (rem / 3600) * 10_000 + ((rem / 60) % 60) * 100 + rem % 60;
    Some(date * 1_000_000 + hms)
}

/// Microseconds of a `YYYYMMDDHHMMSS` integer (validated date and
/// time-of-day).
pub fn parse_compact_datetime(i: i64) -> Option<i64> {
    if !(10_000_101_000_000..=99_991_231_235_959).contains(&i) {
        return None;
    }
    let days = parse_compact_date(i / 1_000_000)?;
    let secs = hhmmss((i / 10_000) % 100, (i / 100) % 100, i % 100)?;
    Some(days * MICROS_PER_DAY + secs * 1_000_000)
}

/// Wall-clock microseconds since the epoch (UTC; a pre-epoch clock
/// clamps to 0 -- cluster timestamps are monotonic anyway).
pub fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Today's date as days since the epoch (UTC).
pub fn today_days() -> i64 {
    now_micros().div_euclid(MICROS_PER_DAY)
}

#[cfg(test)]
#[path = "temporal_tests.rs"]
mod temporal_tests;
