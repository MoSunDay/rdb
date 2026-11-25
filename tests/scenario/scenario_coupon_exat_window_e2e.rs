//! Multi-store scenario #133 -- coupon windows: a campaign arms coupons
//! with ABSOLUTE expiry stamps (`SET ... EXAT/PXAT`), reads them back
//! through TTL/PTTL, pins the KEEPTTL precedence rules and the past-stamp
//! behavior, mirrors the windows in a SQL ledger whose `expires_at`
//! classifies exactly the coupons RESP reports alive / expired, and a
//! delayed reminder staged on the lite MQ lands for the coupon that is
//! still alive. SURFACES: RESP (strings + delay stream) + MySQL. One
//! REAL node, MySQL + lite delay sweep (100ms).
//!
//! Deviation from the brief (pinned; both match Redis 7): `SET k v EX 50
//! KEEPTTL` is REFUSED with `-ERR syntax error` and the key keeps its
//! value AND its previous TTL -- KEEPTTL together with any TTL option is
//! a syntax error, src/command/string_opts.rs:110-112. A PAST EXAT/PXAT
//! stamp answers +OK and the key is due immediately (EXISTS 0, TTL -2),
//! src/command/string.rs:161-162 + string_opts.rs:14-16.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, int, one, rows as sql_rows, run, s};
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, r, rb, stream_ids, until_contains, FrontSpec};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TAG: &str = "exat133";
const STREAM: &str = "coupon/reminders";
const SWEEP_MS: u64 = 100;

fn spec() -> FrontSpec {
    FrontSpec {
        mysql: true,
        extra_yaml: format!("lite:\n  delay_sweep_ms: {SWEEP_MS}\n"),
        ..Default::default()
    }
}

fn now_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// Bytes of a `&str` command line.
fn argv<'a>(args: &[&'a str]) -> Vec<&'a [u8]> {
    args.iter().map(|t| t.as_bytes()).collect()
}

/// Single-line reply text (`+OK`, `-ERR ...`, `:N`).
async fn cmd(addr: &str, args: &[&str]) -> String {
    common::lite::text(&r(addr, &argv(args)).await)
}

async fn set(addr: &str, key: &str, value: &str, opts: &[&str]) -> String {
    let mut args = vec!["SET", key, value];
    args.extend_from_slice(opts);
    cmd(addr, &args).await
}

async fn get(addr: &str, key: &str) -> String {
    rb(addr, &argv(&["GET", key])).await
}

async fn ttl(addr: &str, key: &str) -> i64 {
    int_of(&r(addr, &argv(&["TTL", key])).await)
}

async fn pttl(addr: &str, key: &str) -> i64 {
    int_of(&r(addr, &argv(&["PTTL", key])).await)
}

async fn exists(addr: &str, key: &str) -> i64 {
    int_of(&r(addr, &argv(&["EXISTS", key])).await)
}

/// Wire-order cells of a reply: `:n` -> "n", `$len` -> the payload that
/// follows (null -> ""), array heads skipped.
fn cells(full: &str) -> Vec<String> {
    let toks: Vec<&str> = full.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if let Some(n) = t.strip_prefix(':') {
            out.push(n.to_string());
        } else if let Some(len) = t.strip_prefix('$') {
            if let Ok(n) = len.parse::<usize>() {
                out.push(if n == 0 { String::new() } else { toks.get(i + 1).unwrap_or(&"").to_string() });
                i += 1;
            }
        }
        i += 1;
    }
    out
}

/// Text cells of a one-column result (text protocol: `MVal::Bytes`).
fn texts(cells: Vec<MVal>) -> Vec<String> {
    cells.iter().map(|v| match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        other => panic!("expected text cell, got {other:?}"),
    }).collect()
}

#[tokio::test]
async fn exat_pxat_coupons_expire_on_time() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let (front, a) = front_world(TAG, &spec()).await;
    let mut c = connect_root(&front.node).await;

    // a) Absolute stamps: future EXAT/PXAT arms a TTL, a past stamp is
    //    +OK and due immediately, KEEPTTL keeps the previous TTL (and
    //    refuses to combine with a TTL option), a plain SET clears it.
    let t = scenario_common::perf::now();
    assert_eq!(set(&a, "c:live", "v", &["EXAT", &(now_s() + 3).to_string()]).await, "+OK");
    let live_at = scenario_common::perf::now();
    let live_ttl = ttl(&a, "c:live").await;
    assert!((1..=3).contains(&live_ttl), "TTL after EXAT now+3: {live_ttl}");
    assert_eq!(set(&a, "c:ms", "v", &["PXAT", &(now_ms() + 3_000).to_string()]).await, "+OK");
    let ms_at = scenario_common::perf::now();
    let ms_pttl = pttl(&a, "c:ms").await;
    assert!((1..=3_000).contains(&ms_pttl), "PTTL after PXAT now+3000: {ms_pttl}");
    assert!((1..=3).contains(&ttl(&a, "c:ms").await));

    assert_eq!(set(&a, "c:gone", "v", &["EXAT", &(now_s() - 10).to_string()]).await, "+OK", "past EXAT is accepted");
    assert_eq!(exists(&a, "c:gone").await, 0, "past EXAT: due immediately");
    assert_eq!(get(&a, "c:gone").await, "", "past EXAT: reads nil");
    assert_eq!(ttl(&a, "c:gone").await, -2);
    assert_eq!(set(&a, "c:gone-ms", "v", &["PXAT", &(now_ms() - 5_000).to_string()]).await, "+OK");
    assert_eq!((exists(&a, "c:gone-ms").await, pttl(&a, "c:gone-ms").await), (0, -2));
    assert_eq!(set(&a, "c:gone", "w", &["XX"]).await, "$-1", "XX sees no live key");
    assert_eq!(set(&a, "c:gone", "w", &["NX"]).await, "+OK", "NX sees it absent");

    assert_eq!(set(&a, "c:keep", "v", &["EX", "100"]).await, "+OK");
    assert_eq!(set(&a, "c:keep", "v2", &["KEEPTTL"]).await, "+OK");
    assert_eq!(get(&a, "c:keep").await, "v2");
    let keep_ttl = ttl(&a, "c:keep").await;
    assert!((95..=100).contains(&keep_ttl), "KEEPTTL carries the deadline: {keep_ttl}");
    assert_eq!(set(&a, "c:keep", "v3", &["EX", "50", "KEEPTTL"]).await, "-ERR syntax error", "KEEPTTL + EX is a syntax error");
    assert_eq!(set(&a, "c:keep", "v3", &["KEEPTTL", "PXAT", &(now_ms() + 50_000).to_string()]).await, "-ERR syntax error");
    assert_eq!(get(&a, "c:keep").await, "v2", "refused SET leaves the value");
    let again = ttl(&a, "c:keep").await;
    assert!((keep_ttl - 1..=keep_ttl).contains(&again), "and the TTL: {again} vs {keep_ttl}");

    assert_eq!(set(&a, "c:norm", "v", &["EX", "100"]).await, "+OK");
    assert_eq!(ttl(&a, "c:norm").await, 100);
    assert_eq!(set(&a, "c:norm", "v2", &[]).await, "+OK");
    assert_eq!(ttl(&a, "c:norm").await, -1, "plain SET clears the TTL");
    assert_eq!(set(&a, "c:fresh", "v", &["KEEPTTL"]).await, "+OK");
    assert_eq!(ttl(&a, "c:fresh").await, -1, "KEEPTTL on a fresh key stays persistent");
    for bad in ["EXAT 0", "EXAT -5", "PXAT 0", "PXAT -1"] {
        assert_eq!(set(&a, "c:bad", "v", &bad.split(' ').collect::<Vec<_>>()).await, "-ERR invalid expire time in 'set' command", "{bad}");
    }
    assert_eq!(set(&a, "c:bad", "v", &["EXAT", "abc"]).await, "-ERR value is not an integer or out of range");
    assert_eq!(exists(&a, "c:bad").await, 0, "refused SETs never wrote");
    scenario_common::perf::within("exat133/expiry-probe", t, Duration::from_secs(2));

    // b) The ledger coupons: two alive (one EXAT, one PXAT), one past.
    let ledger: [(String, i64, i64); 3] = [
        ("SAVE10".to_string(), now_s() + 600, 10),
        ("SAVE20".to_string(), (now_ms() + 900_000) / 1_000, 20),
        ("EXPIRED".to_string(), now_s() - 10, 5),
    ];
    for (i, (code, at, _)) in ledger.iter().enumerate() {
        let key = format!("coupon:{code}");
        let opts = if i == 1 { vec!["PXAT".to_string(), (at * 1_000).to_string()] } else { vec!["EXAT".to_string(), at.to_string()] };
        let o: Vec<&str> = opts.iter().map(String::as_str).collect();
        assert_eq!(set(&a, &key, &format!("off-{code}"), &o).await, "+OK", "{code}");
    }
    ddl(&mut c, "CREATE TABLE coupon_ledger (code VARCHAR(32) PRIMARY KEY, expires_at DATETIME, amount INT)").await;
    let values: Vec<String> = ledger.iter().map(|(code, at, amount)| format!("('{code}', FROM_UNIXTIME({at}), {amount})")).collect();
    run(&mut c, &format!("INSERT INTO coupon_ledger (code, expires_at, amount) VALUES {}", values.join(", "))).await;
    // RESP classifies the coupons; the ledger's expires_at vs NOW() agrees.
    let mut alive = Vec::new();
    let mut dead = Vec::new();
    for (code, at, _) in &ledger {
        let t = ttl(&a, &format!("coupon:{code}")).await;
        if t > 0 {
            alive.push(code.clone());
            assert!(t as i64 <= at - now_s() + 1, "TTL {t} within the stamp window for {code}");
        } else {
            assert_eq!(t, -2, "{code} expired: {t}");
            dead.push(code.clone());
        }
    }
    alive.sort();
    dead.sort();
    assert_eq!(alive, vec!["SAVE10", "SAVE20"]);
    assert_eq!(dead, vec!["EXPIRED"]);
    let sql_alive = texts(common::mysql::col(&mut c, "SELECT code FROM coupon_ledger WHERE expires_at > NOW() ORDER BY code").await);
    assert_eq!(sql_alive, alive, "SQL alive == RESP alive");
    let sql_dead = texts(common::mysql::col(&mut c, "SELECT code FROM coupon_ledger WHERE expires_at <= NOW() ORDER BY code").await);
    assert_eq!(sql_dead, dead, "SQL expired == RESP expired");
    let roundtrip = common::mysql::rows(&mut c, "SELECT code, UNIX_TIMESTAMP(expires_at) FROM coupon_ledger ORDER BY code").await;
    assert_eq!(roundtrip, vec![vec![s("EXPIRED"), int(ledger[2].1)], vec![s("SAVE10"), int(ledger[0].1)], vec![s("SAVE20"), int(ledger[1].1)]], "expires_at round-trips the EXAT stamps");
    let remaining = common::mysql::pairs(&mut c, "SELECT amount, code FROM coupon_ledger WHERE expires_at > NOW() ORDER BY expires_at").await;
    assert_eq!(remaining, vec![(10, "SAVE10".to_string()), (20, "SAVE20".to_string())], "ledger order by expiry");
    let (t10, t20) = (ttl(&a, "coupon:SAVE10").await, ttl(&a, "coupon:SAVE20").await);
    assert!(t10 < t20, "RESP TTL order matches the ledger's expires_at order: {t10} < {t20}");
    let sql_left = common::mysql::ints(&mut c, "SELECT UNIX_TIMESTAMP(expires_at) - UNIX_TIMESTAMP() FROM coupon_ledger WHERE code = 'SAVE10'").await;
    assert_eq!(sql_left.len(), 1);
    assert!((sql_left[0] - t10).abs() <= 2, "SQL remaining {} ~ RESP TTL {t10}", sql_left[0]);
    assert_eq!(one(&mut c, "SELECT code FROM coupon_ledger WHERE amount = 5").await, s("EXPIRED"));

    // c) Delayed reminder for the live coupon: staged (invisible), then
    //    exchanged by the sweep after its due time.
    let staged = rb(&a, &argv(&["XADD", STREAM, "*", "DELAY", "700", "code", "SAVE10"])).await;
    assert!(staged.contains('-'), "staged id: {staged}");
    assert_eq!(int_of(&r(&a, &argv(&["XLEN", STREAM])).await), 0, "staged, not yet visible");
    let t_stage = scenario_common::perf::now();
    until_contains(&a, &argv(&["XLEN", STREAM]), ":1", "reminder exchanged", 8).await;
    scenario_common::perf::at_least("exat133/reminder-parked", t_stage, Duration::from_millis(600));
    let xrange = common::lite::text(&common::lite::cmd_full_reply_wait(&a, common::TOKEN, &argv(&["XRANGE", STREAM, "-", "+"]), 25, 10_000).await);
    let ids = stream_ids(&xrange);
    assert_eq!(ids.len(), 1, "one reminder: {xrange}");
    assert_eq!(cells(&xrange)[1..].to_vec(), vec!["code", "SAVE10"], "the reminder names the live coupon");
    ddl(&mut c, "CREATE TABLE coupon_ledger_reminders (code VARCHAR(32) PRIMARY KEY, stream_id VARCHAR(32))").await;
    run(&mut c, &format!("INSERT INTO coupon_ledger_reminders VALUES ('SAVE10', '{}')", ids[0])).await;
    assert_eq!(sql_rows(&mut c, "SELECT r.code, r.stream_id FROM coupon_ledger_reminders r JOIN coupon_ledger l ON l.code = r.code WHERE l.expires_at > NOW()").await, vec![vec![s("SAVE10"), s(&ids[0])]], "reminder row joins the live ledger coupon");

    // d) The 3s coupon expires on its stamp: not early, and inside 6s.
    until_contains(&a, &argv(&["TTL", "c:live"]), ":-2", "c:live expired", 6).await;
    scenario_common::perf::at_least("exat133/live-not-early", live_at, Duration::from_millis(1_900));
    assert_eq!((exists(&a, "c:live").await, get(&a, "c:live").await), (0, String::new()));
    // c:ms was armed for a FULL 3000ms (PXAT), so it outlives c:live whose
    // EXAT stamp is cut to whole seconds (2.0-3.0s of life): its own wait.
    until_contains(&a, &argv(&["PTTL", "c:ms"]), ":-2", "c:ms expired", 4).await;
    scenario_common::perf::at_least("exat133/ms-not-early", ms_at, Duration::from_millis(2_900));
    assert_eq!((pttl(&a, "c:ms").await, exists(&a, "c:ms").await), (-2, 0));
    assert_eq!(ttl(&a, "c:norm").await, -1, "the persistent coupon outlives them");
    assert!(ttl(&a, "coupon:SAVE10").await > 0, "the ledger coupon is still alive");
    scenario_common::perf::scenario("exat133", t0);
}
