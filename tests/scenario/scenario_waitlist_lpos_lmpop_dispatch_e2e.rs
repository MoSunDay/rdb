//! Multi-store scenario #151 -- a restaurant host stand keeps two waitlists as
//! lists and dispatches every seated guest through the RocksMQ HTTP front:
//! LPOS finds a party's place (first / second occurrence, from the tail,
//! COUNT arrays, MAXLEN-bounded scans whose positions stay head-based), and
//! LMPOP seats the head or the tail of the FIRST non-empty list (tail pops
//! answer tail-first, a COUNT past the length takes what is left, an
//! all-empty round is a nil array). Each pop is produced to one channel, a
//! consumer group reads the guests back in dispatch order until the acks
//! empty the PEL, command / route errors (401/404/405/400) are exact, and a
//! SQL seating log mirrors the dispatch. SURFACES: RESP (list) + RocksMQ
//! HTTP + MySQL. One REAL node, RocksMQ + MySQL (bearer configured).
//! Deviation (pinned): `/ack` of a well-formed id NOT pending is 200 `ok` (idempotent XACK, src/rocksmq/api.rs:216).

use crate::common;
use crate::scenario_common;
use common::mq::{b64, items_of, msgs_of, post, req};
use common::mysql::{aff, connect_root, ddl, ints, pairs, run};
use scenario_common::{front_world, r, FrontSpec};
use std::time::Duration;

const TAG: &str = "wait151";
/// The two waitlists, head -> tail, as RPUSH receives them.
const LIST1: [&str; 6] = ["anka", "bello", "cato", "anka", "dora", "emil"];
const LIST2: [&str; 4] = ["gus", "hana", "iris", "joel"];
/// Every seating of the five LMPOP rounds, in dispatch order (the oracle).
const SEATED: [&str; 10] = ["anka", "bello", "cato", "emil", "dora", "anka", "gus", "hana", "iris", "joel"];
const CHANNEL: &str = "wait151";
const GROUP: &str = "hoststand";
const WRONGTYPE: &str = "-WRONGTYPE Operation against a key holding the wrong kind of value";
const NOT_INT: &str = "-ERR value is not an integer or out of range";
const SYNTAX: &str = "-ERR syntax error";
const ARITY_LMPOP: &str = "-ERR wrong number of arguments for 'lmpop' command";
const CROSSSLOT: &str = "-ERR CROSSSLOT Keys in request don't hash to the same slot";

fn key(name: &str) -> String {
    format!("{{{TAG}}}{name}")
}

fn argv<'a>(args: &[&'a str]) -> Vec<&'a [u8]> {
    args.iter().map(|t| t.as_bytes()).collect()
}

/// Single-line reply text (`+OK`, `-ERR ...`, `:N`, `*-1`).
async fn cmd(addr: &str, args: &[&str]) -> String {
    common::lite::text(&r(addr, &argv(args)).await)
}

/// Full-drain reply text (arrays): quiet-gap reader, 25ms.
async fn drain(addr: &str, args: &[&str]) -> String {
    common::lite::text(
        &common::lite::cmd_full_reply_wait(addr, common::TOKEN, &argv(args), 25, 10_000).await,
    )
}

/// Wire-order cells of a full-drain reply: `:n` -> "n", `$len` -> the
/// payload that follows (array heads skipped).
fn cells(full: &str) -> Vec<String> {
    let toks: Vec<&str> = full.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if let Some(n) = toks[i].strip_prefix(':') {
            out.push(n.to_string());
        } else if toks[i].strip_prefix('$').is_some_and(|h| h.parse::<usize>().is_ok()) {
            out.push(toks.get(i + 1).unwrap_or(&"").to_string());
            i += 1;
        }
        i += 1;
    }
    out
}

/// One bearer-authenticated RocksMQ POST -> `(status, body)`.
async fn mq(mq_addr: &str, bearer: &str, target: &str, body: &str) -> (u16, String) {
    let (status, _, out) = req(mq_addr, "POST", target, body, &[("Authorization", bearer)]).await;
    (status, out)
}

/// `/produce_batch` of `guests` (one base64 body each) -> the message ids
/// in request order; every item must answer 200.
async fn dispatch(mq_addr: &str, bearer: &str, guests: &[String]) -> Vec<String> {
    let items: Vec<String> = guests
        .iter()
        .map(|g| format!("{{\"channel\":\"{CHANNEL}\",\"body\":\"{}\"}}", b64(g.as_bytes())))
        .collect();
    let (status, body) = mq(mq_addr, bearer, "/produce_batch", &format!("[{}]", items.join(","))).await;
    assert_eq!(status, 200, "produce_batch: {body}");
    let got = items_of(&body);
    assert!(got.iter().all(|(s, _)| *s == 200), "every guest produced: {body}");
    got.into_iter().map(|(_, id)| id).collect()
}

/// The Rust-side seating oracle: `(first list, side, count)` rounds applied
/// to the two lists -- the first NON-EMPTY candidate in request order gives
/// `count` (clamped) guests from its head, or tail-first from its tail.
fn seating(rounds: [(usize, &str, usize); 5]) -> Vec<(usize, Vec<String>)> {
    let mut lists: Vec<Vec<&str>> = vec![LIST1.to_vec(), LIST2.to_vec()];
    let mut out = Vec::new();
    for (first, side, n) in rounds {
        let idx = [first, 1 - first].into_iter().find(|&i| !lists[i].is_empty()).unwrap_or(first);
        let take = n.min(lists[idx].len());
        let guests: Vec<String> = if lists[idx].is_empty() {
            Vec::new()
        } else if side == "LEFT" {
            lists[idx].drain(..take).map(str::to_string).collect()
        } else {
            let at = lists[idx].len() - take;
            lists[idx].drain(at..).rev().map(str::to_string).collect()
        };
        out.push((idx, guests));
    }
    out
}

/// Stream id `<ms>-<seq>` as a comparable pair.
fn id_rank(id: &str) -> (u64, u64) {
    let (ms, seq) = id.split_once('-').unwrap_or(("", ""));
    (ms.parse().unwrap_or(0), seq.parse().unwrap_or(0))
}

/// `/pending` body -> `(pending, min_id, max_id, consumers)` as JSON text
/// (`null` keeps its spelling), so an empty PEL is exact too.
fn pel(body: &str) -> (u64, String, String, String) {
    let v = scenario_common::json_of(body);
    (
        v["pending"].as_u64().unwrap_or(u64::MAX),
        v["min_id"].to_string(),
        v["max_id"].to_string(),
        v["consumers"].to_string(),
    )
}

#[tokio::test]
async fn waitlist_dispatches_through_rocksmq() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        rocksmq: true,
        extra_yaml: format!("rocksmq_token: {}\n", common::mq::TOKEN),
        ..Default::default()
    };
    let (front, a) = front_world(TAG, &spec).await;
    let mq_addr = front.rocksmq.clone();
    let bearer = format!("Bearer {}", common::mq::TOKEN);
    let (w1, w2, closed) = (key("waitlist1"), key("waitlist2"), key("closed"));

    // a) Two waitlists exactly as RPUSH receives them, and a non-list key.
    for (list, name) in [(&LIST1[..], w1.as_str()), (&LIST2[..], w2.as_str())] {
        let mut owned: Vec<String> = vec!["RPUSH".into(), name.to_string()];
        owned.extend(list.iter().map(|g| g.to_string()));
        let args: Vec<&[u8]> = owned.iter().map(|s| s.as_bytes()).collect();
        assert_eq!(common::lite::text(&r(&a, &args).await), format!(":{}", list.len()), "RPUSH {name}");
        assert_eq!(cells(&drain(&a, &["LRANGE", name, "0", "-1"]).await), list, "head -> tail");
    }
    assert_eq!(cmd(&a, &["SET", &closed, "monday"]).await, "+OK");

    // b) LPOS: positions are head-based, RANK picks the occurrence.
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka"]).await, ":0", "first occurrence");
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "RANK", "2"]).await, ":3", "second occurrence");
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "RANK", "-1"]).await, ":3", "negative RANK scans from the tail");
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "RANK", "-2"]).await, ":0");
    assert_eq!(cmd(&a, &["LPOS", &w1, "dora"]).await, ":4");
    assert_eq!(cmd(&a, &["LPOS", &w1, "zoe"]).await, "$-1", "not on the list");
    assert_eq!(cells(&drain(&a, &["LPOS", &w1, "anka", "COUNT", "0"]).await), ["0", "3"], "COUNT 0 = every match");
    assert_eq!(cells(&drain(&a, &["LPOS", &w1, "anka", "COUNT", "1"]).await), ["0"], "COUNT caps the array");
    assert_eq!(cells(&drain(&a, &["LPOS", &w1, "anka", "RANK", "2", "COUNT", "5"]).await), ["3"], "RANK skips, COUNT caps");
    assert_eq!(cells(&drain(&a, &["LPOS", &w1, "zoe", "COUNT", "5"]).await), Vec::<String>::new(), "no match with COUNT = empty array");
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "MAXLEN", "2"]).await, ":0");
    assert_eq!(cmd(&a, &["LPOS", &w1, "emil", "MAXLEN", "2"]).await, "$-1", "MAXLEN bounds the scan to the head");
    assert_eq!(cmd(&a, &["LPOS", &w1, "emil", "MAXLEN", "3", "RANK", "-1"]).await, ":5", "a tail-bounded scan still reports head-based positions");
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "MAXLEN", "2", "RANK", "-1"]).await, "$-1", "negative RANK + MAXLEN keeps the LAST maxlen entries");

    // c) LPOS argument errors, then a wrong-typed and a missing key.
    assert_eq!(cmd(&a, &["LPOS", &w1]).await, "-ERR wrong number of arguments for 'lpos' command");
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "RANK", "0"]).await, "-ERR RANK can't be zero");
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "COUNT", "-1"]).await, "-ERR COUNT can't be negative");
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "MAXLEN", "-1"]).await, "-ERR MAXLEN can't be negative");
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "RANK", "x"]).await, NOT_INT);
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "BOGUS", "1"]).await, SYNTAX);
    assert_eq!(cmd(&a, &["LPOS", &w1, "anka", "RANK"]).await, SYNTAX, "an option without its value");
    assert_eq!(cmd(&a, &["LPOS", &closed, "anka"]).await, WRONGTYPE);
    assert_eq!(cmd(&a, &["LPOS", &key("nolist"), "anka"]).await, "$-1", "a missing list has no positions");

    // d) LMPOP argument errors: none of them seats anybody.
    assert_eq!(cmd(&a, &["LMPOP"]).await, ARITY_LMPOP);
    assert_eq!(cmd(&a, &["LMPOP", "2", &w1]).await, ARITY_LMPOP);
    for bad in ["0", "-1", "x"] {
        assert_eq!(cmd(&a, &["LMPOP", bad, &w1, "LEFT"]).await, "-ERR numkeys should be greater than 0", "numkeys {bad}");
    }
    assert_eq!(cmd(&a, &["LMPOP", "4", &w1, &w2, "LEFT"]).await, "-ERR Number of keys can't be greater than number of args", "numkeys past the remaining args");
    assert_eq!(cmd(&a, &["LMPOP", "3", &w1, &w2, "LEFT"]).await, CROSSSLOT, "numkeys 3 takes LEFT as the third key: the same-slot gate runs before the side is parsed");
    assert_eq!(cmd(&a, &["LMPOP", "2", &w1, &w2, "SIDEWAYS"]).await, SYNTAX);
    assert_eq!(cmd(&a, &["LMPOP", "2", &w1, &w2, "LEFT", "COUNT"]).await, SYNTAX, "COUNT without its value");
    assert_eq!(cmd(&a, &["LMPOP", "2", &w1, &w2, "LEFT", "BOGUS", "1"]).await, SYNTAX);
    for bad in ["0", "-3", "x"] {
        assert_eq!(cmd(&a, &["LMPOP", "2", &w1, &w2, "LEFT", "COUNT", bad]).await, "-ERR value is out of range, must be positive", "COUNT {bad}");
    }
    assert_eq!(cells(&drain(&a, &["LRANGE", &w1, "0", "-1"]).await), LIST1, "the refused LMPOPs seated nobody");
    assert_eq!(cells(&drain(&a, &["LRANGE", &w2, "0", "-1"]).await), LIST2);

    // e) Hot block -- five seating rounds (LMPOP + produce_batch each)
    //    and two consumer-group deliveries (n = 4, then the rest).
    let rounds: [(usize, &str, usize); 5] = [(0, "LEFT", 3), (0, "RIGHT", 2), (0, "LEFT", 5), (0, "LEFT", 3), (1, "RIGHT", 5)];
    let want = seating(rounds);
    assert_eq!(want.iter().flat_map(|(_, g)| g).collect::<Vec<_>>(), SEATED, "oracle: the story's seating order");
    let lists = [w1.as_str(), w2.as_str()];
    let t = scenario_common::perf::now();
    let mut ids: Vec<String> = Vec::new();
    let mut seated: Vec<Vec<String>> = Vec::new();
    for (first, side, n) in rounds {
        let count = n.to_string();
        let got = cells(&drain(&a, &["LMPOP", "2", lists[first], lists[1 - first], side, "COUNT", &count]).await);
        ids.extend(dispatch(&mq_addr, &bearer, got.get(1..).unwrap_or(&[])).await);
        seated.push(got);
    }
    let delivery = |n: u8| format!("/consume?channel={CHANNEL}&group={GROUP}&n={n}");
    let (s1, b1) = mq(&mq_addr, &bearer, &delivery(4), "").await;
    let (s2, b2) = mq(&mq_addr, &bearer, &delivery(10), "").await;
    scenario_common::perf::within("wait151/dispatch-block", t, Duration::from_secs(2));
    for (i, (got, (idx, guests))) in seated.iter().zip(&want).enumerate() {
        assert_eq!(got.first().map(String::as_str), Some(lists[*idx]), "round {} names the list it seated from", i + 1);
        assert_eq!(&got[1..], guests, "round {} guests", i + 1);
    }
    assert_eq!(seated[1][1..], ["emil", "dora"], "a RIGHT pop answers tail first");
    assert_eq!(seated[2][1..], ["anka"], "COUNT 5 takes what is left");
    assert_eq!((seated[3].first().map(String::as_str), seated[4].first().map(String::as_str)), (Some(w2.as_str()), Some(w2.as_str())), "the drained list is skipped");
    for tail in [&["LEFT"][..], &["LEFT", "COUNT", "1"], &["RIGHT", "COUNT", "3"]] {
        let mut args = vec!["LMPOP", "2", lists[0], lists[1]];
        args.extend_from_slice(tail);
        assert_eq!(cmd(&a, &args).await, "*-1", "both lists drained: nil array");
    }
    assert_eq!((cmd(&a, &["EXISTS", &w1]).await, cmd(&a, &["EXISTS", &w2]).await), (":0".to_string(), ":0".to_string()), "a drained list is deleted");

    // f) The consumer group replays the seating order.
    assert_eq!((s1, s2), (200, 200), "group deliveries: {b1} {b2}");
    let (got1, got2) = (msgs_of(&b1), msgs_of(&b2));
    assert_eq!((got1.len(), got2.len()), (4, 6), "n = 4 caps the first delivery; n = 10 takes the rest");
    let consumed: Vec<String> = got1.iter().chain(&got2).map(|(_, body)| scenario_common::codec::b64d(body)).collect();
    assert_eq!(consumed, SEATED, "the group replays the seating order");
    let stream_ids: Vec<String> = got1.iter().chain(&got2).map(|(id, _)| id.clone()).collect();
    assert_eq!(stream_ids, ids, "stream order == produce_batch order");
    assert!(ids.windows(2).all(|w| id_rank(&w[0]) < id_rank(&w[1])), "ids ascend: {ids:?}");

    // g) The SQL seating log, one row per dispatch in dispatch order.
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE seating (seq INT PRIMARY KEY, guest VARCHAR(16) NOT NULL, list_no TINYINT NOT NULL, status VARCHAR(8) NOT NULL)").await;
    let mut logged: Vec<String> = Vec::new();
    for got in &seated {
        let list_no = if got[0] == w1 { 1 } else { 2 };
        for g in &got[1..] {
            logged.push(format!("({}, '{g}', {list_no}, 'waiting')", logged.len() + 1));
        }
    }
    run(&mut c, &format!("INSERT INTO seating (seq, guest, list_no, status) VALUES {}", logged.join(","))).await;
    let booked: Vec<(i64, String)> = SEATED.iter().enumerate().map(|(i, g)| (i as i64 + 1, g.to_string())).collect();
    assert_eq!(pairs(&mut c, "SELECT seq, guest FROM seating ORDER BY seq").await, booked, "the seating log is the dispatch order");
    assert_eq!(
        (ints(&mut c, "SELECT COUNT(*) FROM seating WHERE list_no = 1").await, ints(&mut c, "SELECT COUNT(*) FROM seating WHERE list_no = 2").await),
        (vec![LIST1.len() as i64], vec![LIST2.len() as i64]),
        "every guest of both lists was seated once"
    );

    // h) The PEL: ten deliveries under the one HTTP consumer; four acks
    //    leave the unacked guests == the SQL rows still waiting.
    let pending = format!("/pending?channel={CHANNEL}&group={GROUP}");
    let (status, body) = mq(&mq_addr, &bearer, &pending, "").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        pel(&body),
        (10, format!("\"{}\"", ids[0]), format!("\"{}\"", ids[9]), "[{\"name\":\"http\",\"pending\":10}]".to_string()),
        "ten pending between the first and the last id: {body}"
    );
    for id in &ids[..4] {
        assert_eq!(mq(&mq_addr, &bearer, &format!("/ack?channel={CHANNEL}&group={GROUP}&id={id}"), "").await, (200, "ok".to_string()), "ack {id}");
    }
    let (status, body) = mq(&mq_addr, &bearer, &pending, "").await;
    assert_eq!((status, pel(&body)), (200, (6, format!("\"{}\"", ids[4]), format!("\"{}\"", ids[9]), "[{\"name\":\"http\",\"pending\":6}]".to_string())), "six still pending: {body}");
    assert_eq!(aff(&mut c, "UPDATE seating SET status = 'seated' WHERE seq <= 4").await, 4);
    assert_eq!(pairs(&mut c, "SELECT seq, guest FROM seating WHERE status = 'waiting' ORDER BY seq").await, booked[4..].to_vec(), "waiting rows == unacked guests");
    let acks: Vec<String> = ids[4..].iter().map(|id| format!("{{\"channel\":\"{CHANNEL}\",\"group\":\"{GROUP}\",\"id\":\"{id}\"}}")).collect();
    let (status, body) = mq(&mq_addr, &bearer, "/ack_batch", &format!("[{}]", acks.join(","))).await;
    assert_eq!((status, items_of(&body)), (200, vec![(200, "ok".to_string()); 6]), "ack_batch: {body}");
    let (status, body) = mq(&mq_addr, &bearer, &pending, "").await;
    assert_eq!((status, pel(&body)), (200, (0, "null".to_string(), "null".to_string(), "[]".to_string())), "an empty PEL: {body}");
    run(&mut c, "UPDATE seating SET status = 'seated' WHERE status = 'waiting'").await;
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM seating WHERE status = 'waiting'").await, vec![0]);
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM seating WHERE status = 'seated'").await, vec![10], "every logged guest is seated");

    // i) The route contract around the bearer gate.
    assert_eq!(post(&mq_addr, &format!("/produce?channel={CHANNEL}"), "walk-in").await, (401, "unauthorized".to_string()), "no bearer");
    let (status, _, _) = req(&mq_addr, "POST", &format!("/produce?channel={CHANNEL}"), "walk-in", &[("Authorization", &format!("Basic {}", common::mq::TOKEN))]).await;
    assert_eq!(status, 401, "wrong scheme");
    let (status, body) = mq(&mq_addr, &bearer, &delivery(10), "").await;
    assert_eq!((status, body.as_str()), (200, "{\"msgs\":[]}"), "the refused produces wrote nothing; nothing is pending");
    let (status, head, body) = req(&mq_addr, "GET", &delivery(1), "", &[("Authorization", bearer.as_str())]).await;
    assert_eq!((status, body.as_str()), (405, "method not allowed"), "known path, wrong verb");
    assert!(head.to_ascii_lowercase().contains("allow: post"), "the 405 names the verb it takes: {head}");
    assert_eq!(mq(&mq_addr, &bearer, "/nope", "").await, (404, "not found".to_string()));
    assert_eq!(mq(&mq_addr, &bearer, &delivery(0), "").await, (400, "'n' must be 1..=100, got 0".to_string()));
    assert_eq!(mq(&mq_addr, &bearer, &delivery(101), "").await, (400, "'n' must be 1..=100, got 101".to_string()));
    let over: Vec<String> = (0..101).map(|_| format!("{{\"channel\":\"{CHANNEL}\",\"body\":\"{}\"}}", b64(b"x"))).collect();
    assert_eq!(mq(&mq_addr, &bearer, "/produce_batch", &format!("[{}]", over.join(","))).await, (400, "batch accepts at most 100 items".to_string()));
    for target in [format!("/ack?channel={CHANNEL}&group=nogroup&id=1-1"), format!("/pending?channel={CHANNEL}&group=nogroup")] {
        assert_eq!(mq(&mq_addr, &bearer, &target, "").await, (404, "no such consumer group".to_string()), "{target}");
    }
    assert_eq!(mq(&mq_addr, &bearer, &format!("/ack?channel={CHANNEL}&group={GROUP}&id=not-an-id"), "").await, (400, "invalid message id".to_string()));
    assert_eq!(
        mq(&mq_addr, &bearer, &format!("/ack?channel={CHANNEL}&group={GROUP}&id=1-1"), "").await,
        (200, "ok".to_string()),
        "DEVIATION: a well-formed id outside the PEL is 200 ok, not 404"
    );
    scenario_common::perf::scenario("wait151", t0);
}
