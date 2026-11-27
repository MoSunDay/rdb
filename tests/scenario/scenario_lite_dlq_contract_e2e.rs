//! Multi-store scenario #164 -- a payments team's dead-letter contract: the `payin164/q0` queue
//! feeds a MAXDELIVERY 2 group whose option matrix is pinned first (DLQ needs MAXDELIVERY; empty,
//! self, other-slot and bare targets are refused; default target `<stream>/dlq`), XCLAIM bumps the
//! delivery count (JUSTID never trips the cap), the claim past the cap moves the entry at its
//! ORIGINAL id into the DLQ with four trace fields (source stays, PEL row goes), XAUTOCLAIM lists
//! the dead id in its third segment only, the monitor shows `rdb_lite_messages{op="dlq"}` and
//! `rdb_lite_dlq_depth`, and a final XACK lands the committed watermark. Lite streams + monitor HTTP.
//! One REAL node, fronts none beyond RESP + monitor (always on).
//! Deviation: committed-id is the contiguous ACKED prefix (stays `0-0` while id1 is pending,
//! becomes id1 once id1 is transferred; src/lite/offset.rs:217-254); last-delivered-id is the
//! PERSISTED delivery watermark, moved in memory by `>` reads (read.rs:420) and landed by the next
//! committed advance -- the brief's "id2 after the id2 transfer" and "delivered head" are the real values.

use crate::common;
use crate::scenario_common;
use scenario_common::{front_world, int_of, ok, r, rb, rt, stream_ids, FrontSpec};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const S: &[u8] = b"payin164/q0";
const DLQ: &[u8] = b"payin164/q0/dlq";
const OTHER: &[u8] = b"otherdlq/q0";
const G: &[u8] = b"billing";
const IDS: [&str; 3] = ["1700000000000-1", "1700000000000-2", "1700000000000-3"];
const PAY: [&str; 3] = ["payin-alpha", "payin-beta", "payin-gamma"];
const DLQ_OP: &str = "rdb_lite_messages{op=\"dlq\"}";
const DEPTH: &str = "rdb_lite_dlq_depth";

/// CRC-16/XMODEM (poly 0x1021, init 0) computed BITWISE -- independent of
/// the server's table-driven `hash::crc16`.
fn crc16(key: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in key {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// A stream's slot is the CRC16 of its PARENT part mod 16384 (`model::stream_prefix`).
fn slot_of(stream: &[u8]) -> u16 {
    let end = stream.iter().position(|&b| b == b'/').unwrap_or(stream.len());
    crc16(&stream[..end]) % 16384
}

/// Full reply of one command as text (quiet-gap reader, ~25-45ms).
async fn drain(a: &str, args: &[&[u8]]) -> String {
    common::lite::text(&common::lite::cmd_full_reply_wait(a, common::TOKEN, args, 20, 10_000).await)
}

/// RESP text of one `[id, [f1, v1, ...]]` entry frame, lengths computed here.
fn entry_frame(id: &str, fields: &[(&str, &str)]) -> String {
    let mut out = format!("*2\r\n${}\r\n{id}\r\n*{}\r\n", id.len(), fields.len() * 2);
    for (f, v) in fields {
        out.push_str(&format!("${}\r\n{f}\r\n${}\r\n{v}\r\n", f.len(), v.len()));
    }
    out
}

/// Smallest id strictly above `id` (seq + 1): the XAUTOCLAIM cursor rule.
fn succ(id: &str) -> String {
    let (ms, seq) = id.split_once('-').expect("ms-seq id");
    format!("{ms}-{}", seq.parse::<u64>().unwrap() + 1)
}

fn rows(list: &[(&str, &str, u64)]) -> Vec<(String, String, u64)> {
    list.iter().map(|(i, c, n)| (i.to_string(), c.to_string(), *n)).collect()
}

/// One labeled value of `billing`'s `XINFO GROUPS` row (the token after `label`).
async fn gf(a: &str, label: &str) -> String {
    let full = drain(a, &[b"XINFO", b"GROUPS", S]).await;
    let row = full
        .split("*13\r\n")
        .skip(1)
        .map(|row| row.split("\r\n").map(str::to_string).collect::<Vec<_>>())
        .find(|row| row.len() > 9 && row[1] == "billing")
        .unwrap_or_else(|| panic!("no XINFO GROUPS row for billing: {full}"));
    let at = row.iter().position(|t| t == label).unwrap_or_else(|| panic!("no {label} in {row:?}"));
    // A label is followed by its value's `$N` length (bulk) or by the
    // `:N` integer itself.
    let next = &row[at + 1];
    if next.starts_with('$') {
        row[at + 2].clone()
    } else {
        next.clone()
    }
}

/// (id, consumer, deliveries) of the group's PEL rows, sorted by id.
async fn pel(a: &str) -> Vec<(String, String, u64)> {
    let reply = drain(a, &[b"XPENDING", S, G, b"-", b"+", b"10"]).await;
    let mut out: Vec<(String, String, u64)> = common::lite::pel_rows(reply.as_bytes())
        .into_iter()
        .filter(|r| r.len() > 5)
        .map(|r| (r[1].clone(), r[3].clone(), r[5].trim_start_matches(':').parse().unwrap_or(0)))
        .collect();
    out.sort();
    out
}

/// Value of one exposition sample line (an absent series counts 0).
fn sample(body: &str, series: &str) -> f64 {
    body.lines()
        .find(|l| l.starts_with(&format!("{series} ")))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

async fn scrape(monitor: &str) -> String {
    let (status, body) = scenario_common::http(monitor, "GET", "/metrics", None).await;
    assert_eq!(status, 200, "/metrics: {body}");
    body
}

async fn xlen(a: &str, s: &[u8]) -> i64 {
    int_of(&r(a, &[b"XLEN", s]).await)
}

async fn xadd(a: &str, id: &str, v: &str) {
    let got = rb(a, &[b"XADD", S, id.as_bytes(), b"f", v.as_bytes()]).await;
    assert_eq!(got, id, "a full-name XADD echoes the id bulk: {got}");
}

#[tokio::test]
async fn lite_dlq_contract() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec::default();
    let (front, a) = front_world("dlq164", &spec).await;
    let (id1, id2, id3) = (IDS[0], IDS[1], IDS[2]);

    // a) Slot facts computed here, then the option matrix on the still-MISSING
    //    stream: every DLQ refusal fires before the stream-exists check.
    assert_ne!(slot_of(S), slot_of(OTHER), "the probe target must sit in another slot");
    assert_eq!(slot_of(DLQ), slot_of(S), "the default target shares the source slot");
    let matrix: [(&str, &[&[u8]], &str); 6] = [
        ("DLQ without MAXDELIVERY", &[b"DLQ", OTHER], "-ERR syntax error"),
        ("DLQ without a name", &[b"MAXDELIVERY", b"2", b"DLQ"], "-ERR syntax error"),
        ("empty DLQ name", &[b"MAXDELIVERY", b"2", b"DLQ", b""], "-ERR empty DLQ target name"),
        ("self target", &[b"MAXDELIVERY", b"2", b"DLQ", S], "-ERR DLQ target must not be the source stream"),
        (
            "other-slot target",
            &[b"MAXDELIVERY", b"2", b"DLQ", OTHER],
            "-ERR DLQ target must hash to the same slot as the source stream",
        ),
        ("bare parent target", &[b"MAXDELIVERY", b"2", b"DLQ", b"otherdlq"], "-ERR invalid DLQ stream name"),
    ];
    for (what, opts, want) in matrix {
        let mut args: Vec<&[u8]> = vec![b"XGROUP", b"CREATE", S, G, b"0-0"];
        args.extend_from_slice(opts);
        assert_eq!(rt(&a, &args).await, want, "{what}");
    }
    let no_stream = rt(&a, &[b"XGROUP", b"CREATE", S, G, b"0-0"]).await;
    assert!(
        no_stream.starts_with("-ERR The XGROUP subcommand requires the key to exist"),
        "CREATE without MKSTREAM on a missing stream: {no_stream}"
    );
    for (id, v) in IDS.iter().zip(PAY) {
        xadd(&a, id, v).await;
    }
    assert_eq!(xlen(&a, S).await, 3, "xlen");
    assert_eq!(drain(&a, &[b"XINFO", b"GROUPS", S]).await, "*0\r\n", "no group yet");
    ok(&r(&a, &[b"XGROUP", b"CREATE", S, G, b"0-0", b"MAXDELIVERY", b"2"]).await, "MAXDELIVERY 2 + default DLQ");
    assert_eq!(xlen(&a, DLQ).await, 0, "no DLQ stream before the first transfer");

    // b) Fan-out to c1: three PEL rows at delivery 1, watermarks untouched.
    let read = drain(&a, &[b"XREADGROUP", b"GROUP", G, b"c1", b"STREAMS", S, b">"]).await;
    assert_eq!(stream_ids(&read), IDS.map(String::from), "all three handed to c1: {read}");
    assert_eq!(pel(&a).await, rows(&[(id1, "c1", 1), (id2, "c1", 1), (id3, "c1", 1)]));
    let (last, committed) = (gf(&a, "last-delivered-id").await, gf(&a, "committed-id").await);
    assert_eq!(
        (last.as_str(), committed.as_str()),
        ("0-0", "0-0"),
        "`>` reads move neither watermark: both stay on the CREATE position"
    );

    // c) XCLAIM: a plain claim hands the entry over and bumps the count
    //    (1 -> 2, not past the cap of 2); JUSTID moves ownership only.
    let claim = drain(&a, &[b"XCLAIM", S, G, b"c2", b"0", id2.as_bytes()]).await;
    assert_eq!(
        claim,
        format!("*1\r\n{}", entry_frame(id2, &[("f", PAY[1])])),
        "plain claim reply: one entry frame: {claim}"
    );
    assert_eq!(pel(&a).await, rows(&[(id1, "c1", 1), (id2, "c2", 2), (id3, "c1", 1)]));
    let justid = drain(&a, &[b"XCLAIM", S, G, b"c3", b"0", id2.as_bytes(), b"JUSTID"]).await;
    assert_eq!(justid, format!("*1\r\n${}\r\n{id2}\r\n", id2.len()), "JUSTID reply = ids");
    assert_eq!(
        pel(&a).await,
        rows(&[(id1, "c1", 1), (id2, "c3", 2), (id3, "c1", 1)]),
        "JUSTID never trips the cap (count unchanged)"
    );

    // d) Hot block: the claim past the cap transfers id2 at its ORIGINAL id
    //    into the DLQ (payload + four trace fields), the PEL row goes, the
    //    source entry stays, and the committed watermark cannot cross id1.
    let t = scenario_common::perf::now();
    let before = scrape(&front.node.monitor).await;
    let dead = drain(&a, &[b"XCLAIM", S, G, b"c3", b"0", id2.as_bytes()]).await;
    assert_eq!(dead, "*0\r\n", "the dead-lettered id is omitted from the claim reply: {dead}");
    assert_eq!(pel(&a).await, rows(&[(id1, "c1", 1), (id3, "c1", 1)]), "PEL row gone");
    assert_eq!(xlen(&a, S).await, 3, "the source entry stays");
    assert_eq!(xlen(&a, DLQ).await, 1, "one dead letter");
    let dlq = drain(&a, &[b"XRANGE", DLQ, b"-", b"+"]).await;
    let letter2 = entry_frame(
        id2,
        &[
            ("f", PAY[1]),
            ("__dlq_group", "billing"),
            ("__dlq_consumer", "c3"),
            ("__dlq_times", "2"),
            ("__dlq_src", "payin164/q0"),
        ],
    );
    assert_eq!(
        dlq,
        format!("*1\r\n{letter2}"),
        "original id, payload kept, trace fields appended, pre-bump times: {dlq}"
    );
    assert_eq!(gf(&a, "committed-id").await, "0-0", "id1 still pending caps the watermark");

    // e) XAUTOCLAIM: COUNT 1 claims id1 (1 -> 2) and hands back the successor
    //    cursor; id3 is parked out of the idle gate by a JUSTID TIME hint in
    //    the future, so the second scan dead-letters id1 into segment three
    //    only and the claim segment stays empty.
    let ac1 = drain(&a, &[b"XAUTOCLAIM", S, G, b"c3", b"0", b"0-0", b"COUNT", b"1"]).await;
    let cursor = succ(id1);
    assert_eq!(
        ac1,
        format!("*3\r\n${}\r\n{cursor}\r\n*1\r\n{}*0\r\n", cursor.len(), entry_frame(id1, &[("f", PAY[0])])),
        "cursor = successor of the last scanned row, one claimed entry: {ac1}"
    );
    let future = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 + 3_600_000).to_string();
    let parked =
        drain(&a, &[b"XCLAIM", S, G, b"c3", b"0", id3.as_bytes(), b"JUSTID", b"TIME", future.as_bytes()]).await;
    assert_eq!(parked, format!("*1\r\n${}\r\n{id3}\r\n", id3.len()), "ownership move only");
    let ac2 = drain(&a, &[b"XAUTOCLAIM", S, G, b"c3", b"1", b"0-0", b"COUNT", b"1"]).await;
    assert_eq!(
        ac2,
        format!("*3\r\n$3\r\n0-0\r\n*0\r\n*1\r\n${}\r\n{id1}\r\n", id1.len()),
        "dead-lettered id in the third segment only: {ac2}"
    );
    assert_eq!(pel(&a).await, rows(&[(id3, "c3", 1)]), "only the parked survivor is left");
    let dlq = drain(&a, &[b"XRANGE", DLQ, b"-", b"+"]).await;
    assert_eq!(stream_ids(&dlq), [id1, id2].map(String::from), "both dead letters at their original ids: {dlq}");
    let letter1 = entry_frame(
        id1,
        &[
            ("f", PAY[0]),
            ("__dlq_group", "billing"),
            ("__dlq_consumer", "c3"),
            ("__dlq_times", "2"),
            ("__dlq_src", "payin164/q0"),
        ],
    );
    assert_eq!(dlq, format!("*2\r\n{letter1}{letter2}"), "XRANGE walks the DLQ in id order: {dlq}");
    assert_eq!(
        (gf(&a, "committed-id").await, gf(&a, "last-delivered-id").await),
        (id1.to_string(), id3.to_string()),
        "watermark = the transferred prefix below the survivor id3; the committed advance lands the delivery head"
    );
    // Monitor: one `dlq` count per transfer; the depth gauge follows the DLQ
    // len on the 200ms lite loop (cached groups only).
    let deadline = Instant::now() + Duration::from_secs(5);
    let after = loop {
        let body = scrape(&front.node.monitor).await;
        if sample(&body, DEPTH) == 2.0 {
            break body;
        }
        assert!(Instant::now() < deadline, "dlq depth gauge never reached 2: {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(sample(&after, DLQ_OP) - sample(&before, DLQ_OP), 2.0, "one dlq count per transfer");
    scenario_common::perf::within("dlq/transfers", t, Duration::from_secs(2));

    // f) XACK of the survivor lands the watermark on the stream position.
    assert_eq!(int_of(&r(&a, &[b"XACK", S, G, id3.as_bytes()]).await), 1, "the survivor acks");
    assert!(pel(&a).await.is_empty(), "PEL drained");
    let (committed, last) = (gf(&a, "committed-id").await, gf(&a, "last-delivered-id").await);
    assert_eq!(
        (committed.as_str(), last.as_str()),
        (id3, id3),
        "committed catches the stream head and lifts delivered with it"
    );
    scenario_common::perf::scenario("dlq", t0);
}
