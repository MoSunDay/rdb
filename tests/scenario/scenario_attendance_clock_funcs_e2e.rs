//! Multi-store scenario #136 -- the factory attendance clock: the SQL
//! clock family (`NOW`/`CURDATE`/`CURTIME`) against the Rust wall clock,
//! field extraction / `DATE_SUB` on fixed stamps, the
//! `UNIX_TIMESTAMP`/`FROM_UNIXTIME` round trip feeding an `attendance`
//! table (shift seconds, a midnight-crossing DATEDIFF, `%W %p`), cross-
//! checked with a RESP `att:open` epoch key under `SET .. EX` + TTL/PTTL.
//! RESP + MySQL. One REAL node, MySQL front.
//! Deviations: no `TIMESTAMPDIFF` (shift length = `UNIX_TIMESTAMP`
//! difference; DATETIME arithmetic refused, src/sql/exec/expr.rs:361; the
//! unit word parses as a column: 1054); no `DAYOFWEEK`/`DAYNAME`/`LAST_DAY`/
//! `MAKEDATE`/`STR_TO_DATE`/`TIME()` (1235, src/sql/exec/func/mod.rs:51);
//! no RESP `TIME`/`LASTSAVE`/`EXPIRETIME` (src/command/mod.rs:420).

use crate::common;
use crate::scenario_common;
use common::mysql::{
    aff, connect_root, ddl, ints, one, pairs, rows, server_error, ER_BAD_FIELD_ERROR, ER_NOT_SUPPORTED_YET,
};
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, r, rb, rt, FrontSpec};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Doors-open moment shared by both faces: 2026-11-25 12:00:00 UTC,
/// a Wednesday. Fixed stamp for the extraction matrix.
const DOORS: i64 = 1_795_608_000;
const T: &str = "2026-11-25 16:50:07";
const DAY_US: i64 = 86_400_000_000;

/// Days since 1970-01-01 of a civil date (Howard Hinnant's
/// days_from_civil) -- the independent oracle for every epoch here.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn fmt_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The server's whole-second DATETIME text (`YYYY-MM-DD HH:MM:SS`).
fn fmt_epoch(secs: i64) -> String {
    let (days, t) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    format!("{} {:02}:{:02}:{:02}", fmt_date(days), t / 3_600, (t / 60) % 60, t % 60)
}

/// Weekday name of an epoch (1970-01-01 was a Thursday; Sunday = 0).
fn weekday(secs: i64) -> &'static str {
    const NAMES: [&str; 7] = [
        "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
    ];
    NAMES[(secs.div_euclid(86_400) + 4).rem_euclid(7) as usize]
}

/// `(y, m, d, h, mi, s)` of the server's `YYYY-MM-DD HH:MM:SS[.ffffff]`
/// text; any other length (a 3-digit fsp fraction, say) is a failure.
fn fields(s: &str) -> (i64, i64, i64, i64, i64, i64) {
    assert!(matches!(s.len(), 19 | 26), "DATETIME text shape: {s:?}");
    let n = |a: usize, z: usize| {
        s.get(a..z)
            .and_then(|p| p.parse::<i64>().ok())
            .unwrap_or_else(|| panic!("non-numeric field in {s:?}"))
    };
    (n(0, 4), n(5, 7), n(8, 10), n(11, 13), n(14, 16), n(17, 19))
}

/// Whole-second epoch of a `YYYY-MM-DD HH:MM:SS` civil stamp.
fn epoch(y: i64, m: u32, d: u32, h: i64, mi: i64, s: i64) -> i64 {
    days_from_civil(y, m, d) * 86_400 + h * 3_600 + mi * 60 + s
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Text cell of a single-value SELECT (`<NULL>` for SQL NULL).
async fn cell(c: &mut mysql_async::Conn, sql: &str) -> String {
    match one(c, sql).await {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
        MVal::Int(i) => i.to_string(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn mi(v: &MVal) -> i64 {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).parse().unwrap(),
        MVal::Int(i) => *i,
        other => panic!("expected int cell, got {other:?}"),
    }
}

/// `(int, int, int)` rows of a three-column all-integer SELECT.
fn triples_i(reply: Vec<Vec<MVal>>) -> Vec<(i64, i64, i64)> {
    reply.iter().map(|r| (mi(&r[0]), mi(&r[1]), mi(&r[2]))).collect()
}

/// Every cell of a single-row, all-integer SELECT (in column order).
fn cells_i(reply: Vec<Vec<MVal>>) -> Vec<i64> {
    assert_eq!(reply.len(), 1, "one row: {reply:?}");
    reply[0].iter().map(mi).collect()
}

/// `pairs` with the text column parsed as an integer.
fn pair_i(v: Vec<(i64, String)>) -> Vec<(i64, i64)> {
    v.into_iter().map(|(a, b)| (a, b.parse().unwrap())).collect()
}

#[tokio::test]
async fn clock_functions_match_resp_and_sql() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("clock136", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE attendance (id INT PRIMARY KEY, worker VARCHAR(32), site VARCHAR(16), \
         checkin DATETIME, checkout DATETIME, doors_at DATETIME)",
    )
    .await;

    // a) The NOW family reads the same UTC clock the test does: every
    //    spelling lands inside the Rust microsecond bracket around its
    //    call, and the fsp argument parses but is ignored (NOW(3) prints
    //    the same 19/26-char shape -- `fields` rejects a 3-digit one).
    let lo = now_us() / 1_000_000;
    for spelling in ["NOW()", "CURRENT_TIMESTAMP()", "SYSDATE()", "LOCALTIME()", "LOCALTIMESTAMP()", "NOW(3)"] {
        let got = cell(&mut c, &format!("SELECT {spelling}")).await;
        let (y, mo, d, h, mi, s) = fields(&got);
        let secs = epoch(y, mo as u32, d as u32, h, mi, s);
        assert!((lo..=now_us() / 1_000_000).contains(&secs), "{spelling} = {got} vs Rust {lo}");
    }

    // b) CURDATE is today's UTC civil date (either side of a possible
    //    midnight flip); CURTIME is the Rust time of day within 5s and
    //    equals DATE_FORMAT(NOW(), '%H:%i:%s'); DATE(NOW()) == CURDATE().
    let today0 = fmt_date(now_us().div_euclid(DAY_US));
    let cur = cell(&mut c, "SELECT CURDATE()").await;
    let today1 = fmt_date(now_us().div_euclid(DAY_US));
    assert!(cur == today0 || cur == today1, "CURDATE() {cur} vs UTC {today0}..{today1}");
    let ct = cell(&mut c, "SELECT CURTIME()").await;
    let (h, m, s) = (ct[0..2].parse::<i64>().unwrap(), ct[3..5].parse::<i64>().unwrap(), ct[6..8].parse::<i64>().unwrap());
    let drift = (h * 3_600 + m * 60 + s - now_us() / 1_000_000 % 86_400).rem_euclid(86_400);
    assert!(drift <= 5 || drift >= 86_395, "CURTIME() {ct} vs Rust clock");
    assert_eq!(ct.len(), 8, "CURTIME() is HH:MM:SS text: {ct}");
    assert_eq!(cells_i(rows(&mut c, "SELECT DATE(NOW()) = CURDATE(), CURDATE() = CURRENT_DATE()").await), vec![1, 1]);
    assert_eq!(cells_i(rows(&mut c, "SELECT CURTIME() = DATE_FORMAT(NOW(), '%H:%i:%s')").await), vec![1], "CURTIME is the formatted clock");
    assert_eq!(cell(&mut c, &format!("SELECT DATE('{T}')")).await, "2026-11-25", "DATE keeps the bare-date shape");

    // c) Extraction on the fixed stamp: the oracle is the civil text
    //    itself; a bare date is midnight; compact ints parse; junk is NULL.
    let t = scenario_common::perf::now();
    let (y, mo, d, h, mi, s) = fields(T);
    assert_eq!(
        cells_i(rows(&mut c, &format!("SELECT YEAR('{T}'), MONTH('{T}'), DAY('{T}'), DAYOFMONTH('{T}'), HOUR('{T}'), MINUTE('{T}'), SECOND('{T}')")).await),
        vec![y, mo, d, d, h, mi, s]
    );
    assert_eq!(
        cells_i(rows(&mut c, "SELECT YEAR('2026-11-25'), HOUR('2026-11-25'), MINUTE('2026-11-25'), SECOND('2026-11-25')").await),
        vec![2026, 0, 0, 0],
        "bare date reads as midnight"
    );
    assert_eq!(cells_i(rows(&mut c, "SELECT YEAR(20261125), HOUR(20261125165007)").await), vec![2026, 16], "compact int spellings");
    assert_eq!(cell(&mut c, "SELECT YEAR('not-a-date')").await, "<NULL>", "unparseable -> NULL");
    assert_eq!(cell(&mut c, "SELECT DATE('not-a-date')").await, "<NULL>");

    // d) DATE_SUB / SUBDATE on fixed stamps against Rust epoch math:
    //    time units keep the DATETIME shape (also at midnight), a bare
    //    date stays a date, a negative interval adds, the plain-days
    //    form works, and UNIX_TIMESTAMP agrees with the result.
    let noon = epoch(2026, 11, 25, 12, 0, 0);
    assert_eq!(noon, DOORS, "oracle sanity: the doors-open constant");
    assert_eq!(cell(&mut c, "SELECT DATE_SUB('2026-11-25 12:00:00', INTERVAL 90 MINUTE)").await, fmt_epoch(DOORS - 5_400));
    assert_eq!(cell(&mut c, "SELECT DATE_SUB('2026-11-25 12:00:00', INTERVAL 2 HOUR)").await, fmt_epoch(DOORS - 7_200));
    assert_eq!(cell(&mut c, "SELECT DATE_SUB('2026-11-25 12:00:00', INTERVAL 12 HOUR)").await, "2026-11-25 00:00:00", "midnight keeps the DATETIME shape");
    assert_eq!(cell(&mut c, "SELECT DATE_SUB('2026-11-25 12:00:00', INTERVAL 3 DAY)").await, fmt_epoch(DOORS - 3 * 86_400));
    assert_eq!(cell(&mut c, "SELECT SUBDATE('2026-11-25', INTERVAL 1 DAY)").await, fmt_date(days_from_civil(2026, 11, 25) - 1), "bare date in, bare date out");
    assert_eq!(cell(&mut c, "SELECT SUBDATE('2026-11-25 12:00:00', 1)").await, fmt_epoch(DOORS - 86_400), "plain-days form");
    assert_eq!(cell(&mut c, "SELECT DATE_SUB('2026-11-25 12:00:00', INTERVAL -90 MINUTE)").await, fmt_epoch(DOORS + 5_400), "negative interval adds");
    assert_eq!(cell(&mut c, "SELECT UNIX_TIMESTAMP(DATE_SUB('2026-11-25 12:00:00', INTERVAL 90 MINUTE))").await, (DOORS - 5_400).to_string());
    assert_eq!(cell(&mut c, "SELECT DATE_SUB(NULL, INTERVAL 1 DAY)").await, "<NULL>");

    // e) UNIX_TIMESTAMP / FROM_UNIXTIME: exact on fixed stamps (text,
    //    bare date = UTC midnight, compact int), a round trip, the
    //    no-arg form inside the Rust bracket, junk -> NULL.
    assert_eq!(
        cells_i(rows(&mut c, "SELECT UNIX_TIMESTAMP('2026-11-25 12:00:00'), UNIX_TIMESTAMP('2026-11-25'), UNIX_TIMESTAMP(20261125120000)").await),
        vec![DOORS, DOORS - 43_200, DOORS]
    );
    assert_eq!(cell(&mut c, &format!("SELECT FROM_UNIXTIME({DOORS})")).await, "2026-11-25 12:00:00");
    assert_eq!(cell(&mut c, "SELECT FROM_UNIXTIME(0)").await, "1970-01-01 00:00:00");
    assert_eq!(cells_i(rows(&mut c, &format!("SELECT UNIX_TIMESTAMP(FROM_UNIXTIME({DOORS})), UNIX_TIMESTAMP(FROM_UNIXTIME(0))")).await), vec![DOORS, 0], "round trip");
    let lo = now_us() / 1_000_000;
    let u: i64 = cell(&mut c, "SELECT UNIX_TIMESTAMP()").await.parse().unwrap();
    assert!((lo..=now_us() / 1_000_000).contains(&u), "UNIX_TIMESTAMP() {u} vs Rust {lo}");
    assert_eq!(cell(&mut c, "SELECT UNIX_TIMESTAMP('nope')").await, "<NULL>");
    scenario_common::perf::within("clock136/clock-matrix", t, Duration::from_secs(3));

    // f) The attendance sheet: stamps inserted through FROM_UNIXTIME or
    //    as text come back as the same epochs; the shift length is the
    //    UNIX_TIMESTAMP difference (TIMESTAMPDIFF is not implemented and
    //    DATETIME - DATETIME is refused outright), the night shift
    //    crosses one date line, and the clock fields render from the column.
    let (in1, out1) = (epoch(2026, 11, 25, 8, 15, 0), epoch(2026, 11, 25, 16, 50, 0));
    let (in2, out2) = (epoch(2026, 11, 25, 22, 0, 0), epoch(2026, 11, 26, 6, 0, 0));
    let (in3, out3) = (epoch(2026, 11, 24, 8, 15, 0), epoch(2026, 11, 24, 16, 50, 0));
    let doors2 = epoch(2026, 11, 25, 21, 30, 0);
    let doors3 = epoch(2026, 11, 24, 12, 0, 0);
    assert_eq!(
        aff(&mut c, &format!("INSERT INTO attendance VALUES \
            (1, 'ana', 'berlin', FROM_UNIXTIME({in1}), FROM_UNIXTIME({out1}), FROM_UNIXTIME({DOORS})), \
            (2, 'bruno', 'lisbon', FROM_UNIXTIME({in2}), FROM_UNIXTIME({out2}), '2026-11-25 21:30:00'), \
            (3, 'carla', 'berlin', '2026-11-24 08:15:00', '2026-11-24 16:50:00', '2026-11-24 12:00:00')")).await,
        3
    );
    assert_eq!(
        triples_i(rows(&mut c, "SELECT id, UNIX_TIMESTAMP(checkin), UNIX_TIMESTAMP(checkout) FROM attendance ORDER BY id").await),
        vec![(1, in1, out1), (2, in2, out2), (3, in3, out3)],
        "stored stamps round-trip through the epoch"
    );
    assert_eq!(
        pair_i(pairs(&mut c, "SELECT id, UNIX_TIMESTAMP(doors_at) FROM attendance ORDER BY id").await),
        vec![(1, DOORS), (2, doors2), (3, doors3)],
        "doors_at: epoch- and text-inserted rows agree"
    );
    // Deviation (src/sql/exec/func/mod.rs:51 has no TIMESTAMPDIFF; the
    // parser never sees the call -- its unit word is read as a column).
    let e = server_error(&mut c, "SELECT TIMESTAMPDIFF(MINUTE, checkin, checkout) FROM attendance WHERE id = 1").await;
    assert_eq!((e.code, e.message.contains("unknown column 'MINUTE'")), (ER_BAD_FIELD_ERROR, true), "{}", e.message);
    let e = server_error(&mut c, "SELECT checkout - checkin FROM attendance WHERE id = 1").await;
    assert_eq!((e.code, e.message.contains("do not support arithmetic")), (ER_NOT_SUPPORTED_YET, true), "{}", e.message);
    assert_eq!(
        pair_i(pairs(&mut c, "SELECT id, UNIX_TIMESTAMP(checkout) - UNIX_TIMESTAMP(checkin) FROM attendance ORDER BY id").await),
        vec![(1, out1 - in1), (2, out2 - in2), (3, out3 - in3)],
        "shift length in seconds"
    );
    assert_eq!(out1 - in1, 30_900, "8h35m day shift");
    // Integer `/` is integer division here: minutes = seconds / 60.
    assert_eq!(pair_i(pairs(&mut c, "SELECT id, (UNIX_TIMESTAMP(checkout) - UNIX_TIMESTAMP(checkin)) / 60 FROM attendance ORDER BY id").await), vec![(1, 515), (2, 480), (3, 515)], "shift minutes");
    assert_eq!(ints(&mut c, "SELECT DATEDIFF(checkout, checkin) FROM attendance ORDER BY id").await, vec![0, 1, 0], "the night shift crosses one date line");
    assert_eq!(cell(&mut c, "SELECT DATE_FORMAT(checkin, '%W %p') FROM attendance WHERE id = 1").await, format!("{} AM", weekday(in1)));
    assert_eq!(cell(&mut c, "SELECT DATE_FORMAT(checkout, '%W %p') FROM attendance WHERE id = 1").await, format!("{} PM", weekday(out1)));
    assert_eq!(cells_i(rows(&mut c, "SELECT HOUR(doors_at), MINUTE(doors_at), SECOND(doors_at) FROM attendance WHERE id = 1").await), vec![12, 0, 0]);
    // FROM_UNIXTIME takes an integer only (int_arg, src/sql/exec/func/
    // string.rs:217): a DATETIME column is a loud 1235, not a coercion.
    let e = server_error(&mut c, "SELECT FROM_UNIXTIME(doors_at) FROM attendance WHERE id = 1").await;
    assert_eq!((e.code, e.message.contains("integer argument")), (ER_NOT_SUPPORTED_YET, true), "{}", e.message);
    assert_eq!(cell(&mut c, "SELECT FROM_UNIXTIME(UNIX_TIMESTAMP(doors_at)) FROM attendance WHERE id = 1").await, "2026-11-25 12:00:00", "epoch column -> text");
    assert_eq!(ints(&mut c, "SELECT id FROM attendance WHERE checkin < '2026-11-25 12:00:00' AND checkout > '2026-11-25 12:00:00' ORDER BY id").await, vec![1], "on shift at doors-open");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM attendance WHERE checkin >= '2026-11-25'").await, vec![2], "date-only bound is midnight");
    for (sql, name) in [
        ("SELECT DAYNAME(checkin) FROM attendance WHERE id = 1", "DAYNAME"),
        ("SELECT DAYOFWEEK(checkin) FROM attendance WHERE id = 1", "DAYOFWEEK"),
        ("SELECT LAST_DAY(checkin) FROM attendance WHERE id = 1", "LAST_DAY"),
        ("SELECT MAKEDATE(2026, 329)", "MAKEDATE"),
        ("SELECT STR_TO_DATE('2026-11-25', '%Y-%m-%d')", "STR_TO_DATE"),
        ("SELECT TIME(checkin) FROM attendance WHERE id = 1", "TIME"),
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!((e.code, e.message.as_str()), (ER_NOT_SUPPORTED_YET, format!("unknown function {}", name.to_lowercase()).as_str()), "{sql}");
    }

    // g) RESP face: the same doors-open epoch as a string under
    //    SET .. EX; TTL/PTTL count down from the window, EXPIRE extends
    //    it, missing -> -2, no expiry -> -1; TIME/LASTSAVE/EXPIRETIME
    //    are unknown here. Cross-face: SQL's UNIX_TIMESTAMP(doors_at)
    //    equals the RESP GET.
    let open = DOORS.to_string();
    assert_eq!(rt(&a, &[b"SET", b"att:open", open.as_bytes(), b"EX", b"120"]).await, "+OK");
    assert_eq!(rb(&a, &[b"GET", b"att:open"]).await, open, "RESP carries the same epoch");
    let ttl = int_of(&r(&a, &[b"TTL", b"att:open"]).await);
    assert!((115..=120).contains(&ttl), "TTL {ttl}");
    let pttl = int_of(&r(&a, &[b"PTTL", b"att:open"]).await);
    assert!((115_000..=120_000).contains(&pttl), "PTTL {pttl}");
    assert_eq!(int_of(&r(&a, &[b"EXPIRE", b"att:open", b"300"]).await), 1, "EXPIRE extends the window");
    let ttl = int_of(&r(&a, &[b"TTL", b"att:open"]).await);
    assert!((295..=300).contains(&ttl), "TTL after EXPIRE {ttl}");
    assert_eq!(int_of(&r(&a, &[b"TTL", b"att:missing"]).await), -2, "missing key");
    assert_eq!(rt(&a, &[b"SET", b"att:plain", b"x"]).await, "+OK");
    assert_eq!(int_of(&r(&a, &[b"TTL", b"att:plain"]).await), -1, "no expiry");
    for cmd in ["TIME", "LASTSAVE", "EXPIRETIME"] {
        assert_eq!(rt(&a, &[cmd.as_bytes(), b"att:open"]).await, format!("-ERR unknown command '{cmd}'"), "no RESP {cmd}");
    }
    let resp_epoch: i64 = rb(&a, &[b"GET", b"att:open"]).await.parse().unwrap();
    assert_eq!(
        ints(&mut c, "SELECT UNIX_TIMESTAMP(doors_at) FROM attendance WHERE id = 1").await,
        vec![resp_epoch],
        "doors-open epoch: SQL == RESP"
    );
    assert_eq!(ints(&mut c, "SELECT id FROM attendance WHERE UNIX_TIMESTAMP(doors_at) = 1795608000").await, vec![1], "the RESP literal names the SQL row");
    scenario_common::perf::scenario("clock136", t0);
}
