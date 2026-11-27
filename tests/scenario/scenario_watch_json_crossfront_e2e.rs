//! Multi-store scenario #162 -- a newsroom publish desk commits story revisions
//! optimistically: WATCH the JSON draft + publish board, queue JSON.SET + FT.ADD +
//! XADD + HINCRBY in one MULTI; EXEC commits every face (`*N`) or aborts to the raw
//! `*-1` when a rival touched a watched key first (the second desk's Kafka Produce
//! v3 record, counted by its board bump, or another connection's JSON.NUMINCRBY).
//! The FT index only follows a committed transaction; a read keeps the watch, the own write
//! outside MULTI (or DISCARD) clears it; queue-time CROSSSLOT aborts wholesale; both fronts
//! read the SAME audit log (Kafka ListOffsets latest == RESP XLEN, the produced k/v in XRANGE).
//! Deviation: WATCH on the stream key itself is NOT exercised -- probed, it never fires (the
//! watch hashes under the full name's slot prefix, lite under the parent's, src/lite/append.rs:56).
//! RESP MULTI/WATCH/EXEC + JSON + FT.ADD/FT.SEARCH + Kafka (Produce v3, ListOffsets v5). One REAL node, fronts kafka (RESP always).
use crate::common;
use crate::kafka_front_common::topics::list_offsets_v5;
use crate::scenario_common;
use rdb::kafka::record::BatchRecord;
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{bulk_at, front_world, int_of, json_of, ok, r, rb, stream_ids, FrontSpec};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOPIC: &str = "w162audit";
/// The topic's queue: its name is the hash tag of every desk key.
const STREAM: &str = "w162audit/q0";
const STORY: &str = "{w162audit/q0}story";
const INDEX: &str = "{w162audit/q0}idx";
const BOARD: &str = "{w162audit/q0}board";
const ABORT: &str = "-EXECABORT Transaction discarded because of previous errors.";
const CROSSSLOT: &str = "-ERR CROSSSLOT Keys in request don't hash to the same slot";

/// Slot of `key` as the router computes it: crc16 of the hash tag.
fn slot_of(key: &str) -> u16 {
    let tag = key
        .split_once('{')
        .and_then(|(_, rest)| rest.split_once('}'))
        .filter(|(tag, _)| !tag.is_empty())
        .map_or(key, |(tag, _)| tag);
    rdb::hash::crc16(tag.as_bytes()) % 16384
}

/// The desk's draft at revision `rev`; the title carries the revision so
/// a stale draft is recognizable by its title alone.
fn draft(rev: i64) -> String {
    format!(
        "{{\"title\":\"harbor bridge reopens r{rev}\",\"body\":\"ferry timetable changes for the harbor district\",\"rev\":{rev}}}"
    )
}

/// AUTH'd persistent connection (MULTI/WATCH state is per connection).
async fn connect(addr: &str) -> TcpStream {
    let mut sock = TcpStream::connect(addr).await.expect("connect");
    sock.write_all(&common::lite::frame(&[b"AUTH", common::TOKEN.as_bytes()]))
        .await
        .expect("auth write");
    let (mut line, mut byte) = (Vec::new(), [0u8; 1]);
    while !line.ends_with(b"\r\n") {
        sock.read_exact(&mut byte).await.expect("auth reply");
        line.push(byte[0]);
    }
    assert_eq!(line, b"+OK\r\n", "AUTH");
    sock
}

/// Byte length of the complete RESP value at `pos` (nested arrays
/// included); `None` while it is still incomplete.
fn value_len(buf: &[u8], pos: usize) -> Option<usize> {
    let eol = buf[pos..].windows(2).position(|w| w == b"\r\n")? + pos;
    let count = || std::str::from_utf8(&buf[pos + 1..eol]).ok()?.parse::<i64>().ok();
    match buf[pos] {
        b'$' => match count()? {
            len if len >= 0 => Some(eol - pos + 2 + len as usize + 2),
            _ => Some(eol - pos + 2),
        },
        b'*' => {
            let mut at = eol + 2;
            for _ in 0..count()?.max(0) {
                at += value_len(buf, at)?;
            }
            Some(at - pos)
        }
        _ => Some(eol - pos + 2),
    }
}

/// The complete top-level RESP values of `buf` as text, each without its
/// final CRLF (`+OK`, `:1`, `$5\r\nhello`, `*2\r\n+OK\r\n:1`).
fn values(buf: &[u8]) -> Vec<String> {
    let (mut out, mut at) = (Vec::new(), 0);
    while let Some(len) = value_len(buf, at) {
        let text = common::lite::text(&buf[at..at + len]);
        out.push(text.strip_suffix("\r\n").unwrap_or(&text).to_string());
        at += len;
    }
    out
}

/// Write `cmds` back to back and read exactly one reply per command
/// (no quiet gap: the reply count decides).
async fn round(sock: &mut TcpStream, cmds: &[&[&[u8]]]) -> Vec<String> {
    for cmd in cmds {
        sock.write_all(&common::lite::frame(cmd)).await.expect("write");
    }
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
    loop {
        let done = values(&buf);
        if done.len() >= cmds.len() {
            return done;
        }
        match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {
                panic!("short reply for {cmds:?}: {}", common::lite::text(&buf))
            }
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// `round` over text arguments.
async fn say(sock: &mut TcpStream, cmds: &[&[&str]]) -> Vec<String> {
    let framed: Vec<Vec<&[u8]>> = cmds
        .iter()
        .map(|c| c.iter().map(|s| s.as_bytes()).collect())
        .collect();
    let borrowed: Vec<&[&[u8]]> = framed.iter().map(Vec::as_slice).collect();
    round(sock, &borrowed).await
}

/// Lines of a flat EXEC reply minus the `$N` bulk headers.
fn cells(exec: &str) -> Vec<&str> {
    exec.split("\r\n")
        .filter(|l| !l.is_empty() && !(l.starts_with('$') && l[1..].parse::<usize>().is_ok()))
        .collect()
}

/// Payload of a bulk reply value (`$N\r\n<payload>\r\n`).
fn payload(value: &str) -> &str {
    value
        .split_once("\r\n")
        .map_or("", |(_, p)| p.strip_suffix("\r\n").unwrap_or(p))
}

/// Full-drain reply text with a short quiet gap (the fixture's `rf`
/// parks 250ms per call).
async fn drain(a: &str, args: &[&str]) -> String {
    let argv: Vec<&[u8]> = args.iter().map(|t| t.as_bytes()).collect();
    common::lite::text(&common::lite::cmd_full_reply_wait(a, common::TOKEN, &argv, 20, 10_000).await)
}

/// The rival desk's edit: bump `$.rev` on another connection.
async fn rival_bump(a: &str) -> String {
    rb(a, &[b"JSON.NUMINCRBY", STORY.as_bytes(), b"$.rev", b"1"]).await
}

/// The draft as the JSON face holds it.
async fn stored(a: &str) -> serde_json::Value {
    json_of(&rb(a, &[b"JSON.GET", STORY.as_bytes()]).await)
}

#[tokio::test]
async fn watch_json_crossfront() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        kafka: true,
        ..Default::default()
    };
    let (front, a) = front_world("watchjson", &spec).await;
    let (kafka, resp) = (front.kafka.clone(), a.clone());

    // a) The audit stream's slot is the hash tag of every desk key
    //    (recomputed in Rust; the single-slot MULTI bodies below are the
    //    server-side proof); the topic's queue is seeded so Kafka Produce
    //    has a partition to append to.
    let slot = slot_of(STREAM);
    for key in [STORY, INDEX, BOARD] {
        assert_eq!(slot_of(key), slot, "slot of {key}");
    }
    assert_eq!(rb(&a, &[b"XADD", STREAM.as_bytes(), b"1-1", b"seed", b"boot"]).await, "1-1", "seed");
    ok(
        &r(&a, &[b"FT.CREATE", INDEX.as_bytes(), b"SCHEMA", b"title", b"TEXT", b"body", b"TEXT"]).await,
        "ft.create",
    );
    assert_eq!(int_of(&r(&a, &[b"HSET", BOARD.as_bytes(), b"publishes", b"0"]).await), 1, "board");
    let mut ed = connect(&a).await;
    let (d1, d2) = (draft(1), draft(2));

    // b) Hot block: the clean publish, the second desk's publish (Kafka
    //    Produce + board bump) aborting the stale transaction + the retry,
    //    and the JSON-rival abort + retry (five EXECs, one Produce).
    let t = scenario_common::perf::now();
    let t1 = say(&mut ed, &[&["WATCH", STORY, BOARD], &["MULTI"], &["JSON.SET", STORY, "$", &d1], &["FT.ADD", INDEX, "story", &d1], &["XADD", STREAM, "*", "event", "publish", "rev", "1"], &["HINCRBY", BOARD, "publishes", "1"], &["EXEC"]]).await;
    assert_eq!(&t1[..6], ["+OK", "+OK", "+QUEUED", "+QUEUED", "+QUEUED", "+QUEUED"], "T1 queue: {t1:?}");
    let c1 = cells(&t1[6]);
    assert_eq!((c1.len(), c1[0], c1[1], c1[2], c1[4]), (5, "*4", "+OK", ":1", ":1"), "T1 exec: {t1:?}");
    assert_eq!(say(&mut ed, &[&["WATCH", BOARD]]).await, vec!["+OK"], "armed on the board");
    let record = [BatchRecord {
        timestamp_delta: 0,
        key: Some(b"desk-b"),
        value: Some(br#"{"event":"retract","rev":1}"#),
        headers: vec![],
    }];
    assert_eq!(kafka_produce_v3(&kafka, 1, TOPIC, &record).await, 2, "base offset = seed + T1 audit entry");
    let rival = int_of(&r(&a, &[b"HINCRBY", BOARD.as_bytes(), b"publishes", b"1"]).await);
    assert_eq!(rival, 2, "the second desk counted its publish");
    let audit = ["XADD", STREAM, "*", "event", "publish", "rev", "2"];
    let bump_board = ["HINCRBY", BOARD, "publishes", "1"];
    let t2 = say(&mut ed, &[&["MULTI"], &audit, &bump_board, &["EXEC"]]).await;
    assert_eq!(t2, vec!["+OK", "+QUEUED", "+QUEUED", "*-1"], "the rival publish broke the watch: {t2:?}");
    let t2r = say(&mut ed, &[&["WATCH", BOARD], &["MULTI"], &audit, &bump_board, &["EXEC"]]).await;
    assert_eq!(&t2r[..4], ["+OK", "+OK", "+QUEUED", "+QUEUED"], "retry queue: {t2r:?}");
    let c2 = cells(&t2r[4]);
    assert_eq!((c2.len(), c2[0], c2[2]), (3, "*2", ":3"), "retry exec: {t2r:?}");
    assert_eq!(say(&mut ed, &[&["WATCH", STORY]]).await, vec!["+OK"]);
    assert_eq!(rival_bump(&resp).await, "2", "rival bump");
    let t3 = say(&mut ed, &[&["MULTI"], &["JSON.SET", STORY, "$", &d2], &["FT.ADD", INDEX, "story", &d2], &["EXEC"]]).await;
    assert_eq!(t3, vec!["+OK", "+QUEUED", "+QUEUED", "*-1"], "the rival edit broke the watch: {t3:?}");
    let reread = json_of(payload(&say(&mut ed, &[&["JSON.GET", STORY]]).await[0]))["rev"].as_i64().expect("rev");
    let fresh = draft(reread + 1);
    let t3r = say(&mut ed, &[&["WATCH", STORY], &["MULTI"], &["JSON.SET", STORY, "$", fresh.as_str()], &["FT.ADD", INDEX, "story", fresh.as_str()], &["EXEC"]]).await;
    assert_eq!(t3r, vec!["+OK", "+OK", "+QUEUED", "+QUEUED", "*2\r\n+OK\r\n:1"], "retry exec: {t3r:?}");
    scenario_common::perf::within("watchjson/5-tx+1-produce", t, Duration::from_secs(2));

    // c) Both fronts read the SAME audit log: the produced key/value in
    //    RESP XRANGE, the EXEC-reported ids among the stream ids, Kafka
    //    ListOffsets latest == XLEN == seed + 2 audits + 1 record; the
    //    board counted T1, the rival and the retry (not the aborted one).
    let xlen = int_of(&r(&a, &[b"XLEN", STREAM.as_bytes()]).await);
    let xrange = drain(&a, &["XRANGE", STREAM, "-", "+"]).await;
    assert_eq!(xlen, 4, "stream depth");
    assert!(xrange.contains("desk-b") && xrange.contains("retract"), "produced record visible in RESP: {xrange}");
    let ids = stream_ids(&xrange);
    assert_eq!(ids.len(), 4, "four entries by id: {xrange}");
    assert!(ids.contains(&c1[3].to_string()) && ids.contains(&c2[1].to_string()), "EXEC ids are the stream's: {ids:?}");
    let mut ksock = TcpStream::connect(&kafka).await.expect("kafka front");
    assert_eq!(list_offsets_v5(&mut ksock, 1, TOPIC, 0, -1).await, (0, -1, xlen, -1), "Kafka latest == RESP XLEN");
    assert_eq!(rb(&a, &[b"HGET", BOARD.as_bytes(), b"publishes"]).await, "3", "board");
    let found = drain(&a, &["FT.SEARCH", INDEX, "harbor", "WITHSCORES"]).await;
    let lagging = json_of(&bulk_at(found.as_bytes(), 2));
    assert_eq!((lagging["rev"].as_i64(), lagging["title"].as_str()), (Some(3), Some("harbor bridge reopens r3")), "index follows the committed retry: {lagging}");

    // d) A READ keeps the watch: the same rival bump still aborts the stale
    //    draft (title r3 stays, the number is the rival's 4).
    let t4 = say(&mut ed, &[&["WATCH", STORY], &["JSON.GET", STORY]]).await;
    assert_eq!(t4[0], "+OK", "watch: {t4:?}");
    let read4 = json_of(payload(&t4[1]))["rev"].as_i64().expect("rev");
    assert_eq!(rival_bump(&resp).await, "4", "rival bump");
    let t4x = say(&mut ed, &[&["MULTI"], &["JSON.SET", STORY, "$", draft(read4 + 1).as_str()], &["EXEC"]]).await;
    assert_eq!(t4x, vec!["+OK", "+QUEUED", "*-1"], "read kept the watch: {t4x:?}");
    let after4 = stored(&a).await;
    assert_eq!((after4["title"].as_str(), after4["rev"].as_i64()), (Some("harbor bridge reopens r3"), Some(4)), "stale draft NOT applied");

    // e) The connection's OWN write outside MULTI clears the watches: the
    //    same bump now lets the stale draft through (last writer wins).
    let t5 = say(&mut ed, &[&["WATCH", STORY], &["JSON.GET", STORY], &["HSET", BOARD, "note", "recheck"]]).await;
    let read5 = json_of(payload(&t5[1]))["rev"].as_i64().expect("rev");
    assert_eq!(t5[2], ":1", "own write: {t5:?}");
    assert_eq!(rival_bump(&resp).await, "5", "rival bump");
    let t5x = say(&mut ed, &[&["MULTI"], &["JSON.SET", STORY, "$", draft(read5 + 1).as_str()], &["EXEC"]]).await;
    assert_eq!(t5x, vec!["+OK", "+QUEUED", "*1\r\n+OK"], "watch cleared by the own write: {t5x:?}");
    assert_eq!(stored(&a).await["title"], "harbor bridge reopens r5", "the stale draft went through");

    // f) DISCARD unwatches and applies nothing: the queued r6 draft is
    //    dropped, the rival's bump lands, the next EXEC commits it.
    let t6 = say(&mut ed, &[&["WATCH", STORY], &["MULTI"], &["JSON.SET", STORY, "$", draft(6).as_str()], &["DISCARD"]]).await;
    assert_eq!(t6, vec!["+OK", "+OK", "+QUEUED", "+OK"], "discard: {t6:?}");
    assert_eq!(stored(&a).await["title"], "harbor bridge reopens r5", "DISCARD applied nothing");
    assert_eq!(rival_bump(&resp).await, "6", "rival bump");
    let t6x = say(&mut ed, &[&["MULTI"], &["JSON.SET", STORY, "$", draft(6).as_str()], &["FT.ADD", INDEX, "story", draft(6).as_str()], &["EXEC"]]).await;
    assert_eq!(t6x, vec!["+OK", "+QUEUED", "+QUEUED", "*2\r\n+OK\r\n:1"], "post-DISCARD exec: {t6x:?}");
    let hit = json_of(&bulk_at(drain(&a, &["FT.SEARCH", INDEX, "harbor", "WITHSCORES"]).await.as_bytes(), 2));
    assert_eq!((hit["rev"].as_i64(), hit == stored(&a).await), (Some(6), true), "FT payload == JSON document: {hit}");

    // g) Session error pins, then a queue-time CROSSSLOT: the second
    //    queued key hashes elsewhere -> dirty -> EXECABORT, nothing written.
    let pins = say(&mut ed, &[&["EXEC"], &["DISCARD"], &["UNWATCH"], &["MULTI"], &["MULTI"], &["WATCH", STORY], &["EXEC"]]).await;
    assert_eq!(pins, vec!["-ERR EXEC without MULTI", "-ERR DISCARD without MULTI", "+OK", "+OK", "-ERR MULTI calls can not be nested", "-ERR WATCH inside MULTI is not allowed", ABORT], "pins: {pins:?}");
    assert_ne!(slot_of("{x162}other"), slot_of("{y162}stream"), "distinct slots by construction");
    let x = say(&mut ed, &[&["MULTI"], &["JSON.SET", "{x162}other", "$", &d1], &["XADD", "{y162}stream", "1-1", "f", "v"], &["EXEC"]]).await;
    assert_eq!(x, vec!["+OK", "+QUEUED", CROSSSLOT, ABORT], "crossslot: {x:?}");
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"{x162}other"]).await), 0, "nothing was written");
    scenario_common::perf::scenario("watchjson", t0);
}
