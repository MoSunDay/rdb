//! Unit tests for [`crate::sql::temporal`] (sibling file so
//! temporal.rs stays under the 400-line budget for new files).

use super::*;

#[test]
fn epoch_is_zero_days() {
    assert_eq!(days_from_civil(1970, 1, 1), Some(0));
    assert_eq!(civil_from_days(0), Some((1970, 1, 1)));
    assert_eq!(parse_date("1970-01-01"), Some(0));
    assert_eq!(parse_date("19700101"), Some(0));
    assert_eq!(format_date(0), "1970-01-01");
    assert_eq!(parse_datetime("1970-01-01"), Some(0));
    assert_eq!(parse_datetime("19700101"), Some(0));
    assert_eq!(format_datetime(0), "1970-01-01 00:00:00");
    assert_eq!(SECS_PER_DAY * 1_000_000, MICROS_PER_DAY);
}

#[test]
fn day_before_epoch_is_minus_one() {
    assert_eq!(parse_date("1969-12-31"), Some(-1));
    assert_eq!(format_date(-1), "1969-12-31");
    assert_eq!(parse_datetime("1969-12-31"), Some(-MICROS_PER_DAY));
    assert_eq!(format_datetime(-MICROS_PER_DAY), "1969-12-31 00:00:00");
}

#[test]
fn leap_years_are_correct() {
    assert!(days_from_civil(2000, 2, 29).is_some());
    assert_eq!(days_from_civil(1900, 2, 29), None);
    assert!(days_from_civil(2024, 2, 29).is_some());
    assert_eq!(days_from_civil(2024, 2, 30), None);
    assert_eq!(days_from_civil(0, 1, 1), None);
    assert_eq!(days_from_civil(10_000, 1, 1), None);
}

#[test]
fn civil_round_trip_over_the_whole_range() {
    let first = days_from_civil(1, 1, 1).unwrap();
    let last = days_from_civil(9999, 12, 31).unwrap();
    // Sampled (every 97th day, co-prime with month lengths, plus
    // both endpoints) keeps the loop fast while still crossing
    // every month length and century boundary.
    let mut d = first;
    while d <= last {
        let (y, m, dd) = civil_from_days(d).unwrap();
        assert_eq!(days_from_civil(y, m, dd), Some(d), "{y}-{m}-{dd}");
        d += 97;
    }
    assert_eq!(civil_from_days(first - 1), None);
    assert_eq!(civil_from_days(last + 1), None);
}

#[test]
fn date_parse_rejects_non_canonical_forms() {
    for bad in [
        "",
        "2024-02-29 ",
        " 2024-02-29",
        "2024-2-29",
        "2024-02-9",
        "2024/02/29",
        "2024-00-15",
        "2024-13-15",
        "2024-01-00",
        "2024-01-32",
        "2024-02-30",
        "0000-01-01",
        "10000-01-01",
        "2024022",
        "202402290",
        "2024022x",
        "2024-01-15T00:00:00",
    ] {
        assert_eq!(parse_date(bad), None, "{bad:?}");
    }
}

#[test]
fn date_parse_format_round_trip() {
    for s in [
        "0001-01-01",
        "1899-12-31",
        "1969-12-31",
        "1970-01-01",
        "2000-02-29",
        "2024-02-29",
        "9999-12-31",
    ] {
        let days = parse_date(s).unwrap();
        assert_eq!(format_date(days), s);
        assert_eq!(parse_date(&s.replace('-', "")), Some(days));
    }
}

#[test]
fn datetime_accepts_canonical_forms() {
    let d = |s: &str| parse_datetime(s).unwrap();
    let day = parse_date("2024-02-29").unwrap();
    assert_eq!(d("2024-02-29"), day * MICROS_PER_DAY);
    assert_eq!(d("20240229"), day * MICROS_PER_DAY);
    assert_eq!(d("2024-02-29T13:45:59"), d("2024-02-29 13:45:59"));
    assert_eq!(
        d("2024-02-29 13:45:59"),
        day * MICROS_PER_DAY + (13 * 3600 + 45 * 60 + 59) * 1_000_000
    );
    assert_eq!(d("20240229134559"), d("2024-02-29 13:45:59"));
    // Fractions are micros, 1..=6 digits, right-padded.
    assert_eq!(d("2024-02-29 00:00:00.5"), d("2024-02-29 00:00:00.500000"));
    assert_eq!(d("2024-02-29 00:00:00.000001") % 1_000_000, 1);
    assert!(d("1969-12-31 23:59:59") < 0);
}

#[test]
fn datetime_parse_rejects_non_canonical_forms() {
    for bad in [
        "",
        "2024-02-29 13:45",
        "2024-02-2913:45:59",
        "2024-02-29 24:00:00",
        "2024-02-29 13:60:59",
        "2024-02-29 13:45:60",
        "2024-02-29 13:45:59.",
        "2024-02-29 13:45:59.1234567",
        "2024-02-29 13:45:59.12.34",
        "2024022913455",
        "202402291345599",
        "2024-13-01 00:00:00",
        "garbage",
    ] {
        assert_eq!(parse_datetime(bad), None, "{bad:?}");
    }
}

#[test]
fn datetime_format_trims_zero_fraction() {
    let us = parse_datetime("2024-01-02 03:04:05.100000").unwrap();
    assert_eq!(format_datetime(us), "2024-01-02 03:04:05.100000");
    assert_eq!(
        format_datetime(parse_datetime("2024-01-02 03:04:05.5").unwrap()),
        "2024-01-02 03:04:05.500000"
    );
    assert_eq!(
        format_datetime(parse_datetime("2024-01-02 03:04:05.000001").unwrap()),
        "2024-01-02 03:04:05.000001"
    );
    // An explicit zero fraction formats back without one.
    assert_eq!(
        format_datetime(parse_datetime("2024-01-02 03:04:05.000000").unwrap()),
        "2024-01-02 03:04:05"
    );
}

#[test]
fn compact_round_trips() {
    let days = parse_date("2024-02-29").unwrap();
    assert_eq!(compact_date(days), Some(20_240_229));
    assert_eq!(parse_compact_date(20_240_229), Some(days));
    assert_eq!(compact_date(0), Some(19_700_101));
    assert_eq!(parse_compact_date(19_700_101), Some(0));
    // Invalid compacts: bad month/day, wrong digit count, negative
    // (7-digit 9999999 is not a YYYYMMDD).
    for bad in [
        20_240_230, 20_240_200, 20_240_000, 20_241_301, 19_700_132, 19_700_229, 9_999_999, -1,
    ] {
        assert_eq!(parse_compact_date(bad), None, "{bad}");
    }
    let us = parse_datetime("2024-02-29 13:45:59.999999").unwrap();
    assert_eq!(compact_datetime(us), Some(20_240_229_134_559));
    // The compact form cannot carry a fraction: parsing it back yields
    // the whole second (and compact_datetime truncates it away).
    let whole = parse_datetime("2024-02-29 13:45:59").unwrap();
    assert_eq!(parse_compact_datetime(20_240_229_134_559), Some(whole));
    assert_eq!(compact_datetime(us), compact_datetime(whole));
    // 13-digit / bad time-of-day compacts are rejected.
    for bad in [
        2_024_022_913_455,
        20_240_229_245_959,
        20_240_229_136_059,
        20_240_229_134_560,
        0,
    ] {
        assert_eq!(parse_compact_datetime(bad), None, "{bad}");
    }
}

#[test]
fn out_of_range_days_render_as_raw_integers() {
    // Best-effort debug rendering for values the write path never
    // stores: the raw integer instead of a wrong civil date.
    let hi = days_from_civil(9999, 12, 31).unwrap() + 1;
    assert_eq!(format_date(hi), hi.to_string());
    assert_eq!(
        format_datetime(hi * MICROS_PER_DAY),
        (hi * MICROS_PER_DAY).to_string()
    );
}

#[test]
fn wall_clock_helpers_stay_in_range() {
    // Sanity clamp against a broken clock: "today" must be a real
    // civil date this era.
    assert!(parse_date("2000-01-01").unwrap() <= today_days());
    assert!(today_days() <= parse_date("9999-12-31").unwrap());
    assert!(now_micros() >= 0);
}
