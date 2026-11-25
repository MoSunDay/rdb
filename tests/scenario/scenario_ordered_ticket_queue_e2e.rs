//! Multi-store scenario #134 -- a support-ticket queue consumed in
//! order per group: `XGROUP CREATE ... ORDERED INFLIGHT 2` makes the
//! in-flight window a hard cap (a `>` read delivers at most the open
//! slots, then nil until XACK frees one; a second consumer is fenced
//! out while the owner's lease is fresh); `XINFO STREAM FULL` shows the
//! entries, group PEL and roster, `XGROUP DESTROY` answers :1 then :0
//! (NOGROUP afterwards), and MySQL `ticket_audit` mirrors the lifecycle
//! (delivered / acked / orphaned counts checked against the FULL PEL).
//! Deviations: the roster entry declares `*6` but emits a trailing
//! `pel` pair outside it (src/lite/xinfo_full.rs:153); the deep view is
//! `XINFO STREAM <key> FULL [COUNT n]` (src/lite/info.rs:238-266).
//! RESP + MySQL. One REAL node, MySQL front.

use crate::common;
use crate::scenario_common;
use common::mysql::{aff, connect_root, ddl, ints, pairs};
use scenario_common::{front_world, int_of, r, rb, rt, stream_ids, FrontSpec};
use serde_json::Value as J;
use std::time::Duration;

const S: &[u8] = b"tickets/enterprise";
const G: &[u8] = b"agents";

/// Full-drain reply text of one array command (short quiet gap).
async fn full(a: &str, args: &[&[u8]]) -> String {
    let raw = common::lite::cmd_full_reply_wait(a, common::TOKEN, args, 30, 10_000).await;
    let text = common::lite::text(&raw);
    assert!(!text.starts_with('<'), "{}: {text:?}", String::from_utf8_lossy(args[0]));
    text
}

/// One `>` read asking for ten: the delivered ids (empty for nil).
async fn read_new(a: &str, consumer: &[u8]) -> Vec<String> {
    stream_ids(
        &full(
            a,
            &[b"XREADGROUP", b"GROUP", G, consumer, b"COUNT", b"10", b"STREAMS", S, b">"],
        )
        .await,
    )
}

/// RESP text -> JSON tree (arrays / bulk / int / nil), walking the
/// DECLARED array lengths strictly; the second item is every non-empty
/// token left after the top-level value (trailing data).
fn parse(reply: &str) -> (J, Vec<String>) {
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let (v, next) = resp_value(&toks, 0);
    let rest = toks[next..]
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();
    (v, rest)
}

fn tree(reply: &str) -> J {
    parse(reply).0
}

fn resp_value(toks: &[&str], i: usize) -> (J, usize) {
    let head = toks.get(i).copied().unwrap_or("");
    match head.as_bytes().first() {
        Some(b'*') => {
            let n: i64 = head[1..].parse().unwrap_or(-1);
            if n < 0 {
                return (J::Null, i + 1);
            }
            let (mut items, mut j) = (Vec::with_capacity(n as usize), i + 1);
            for _ in 0..n {
                let (v, next) = resp_value(toks, j);
                items.push(v);
                j = next;
            }
            (J::Array(items), j)
        }
        Some(b'$') => {
            let n: i64 = head[1..].parse().unwrap_or(-1);
            if n < 0 {
                return (J::Null, i + 1);
            }
            (J::String(toks.get(i + 1).copied().unwrap_or("").to_string()), i + 2)
        }
        Some(b':') => (head[1..].parse::<i64>().unwrap_or(0).into(), i + 1),
        Some(b'+') => (J::String(head[1..].to_string()), i + 1),
        _ => (J::String(head.to_string()), i + 1),
    }
}

/// Trailing consumer PEL (deviation, src/lite/xinfo_full.rs:153-161):
/// the roster entry declares `*6` (name / seen-time / pending) but a
/// fourth `pel` pair -- `pel`, `*n`, n `[id, idle, deliveries]` rows --
/// is emitted AFTER it, outside the declared array.
fn pel_tail(rest: &[String]) -> (usize, Vec<Vec<String>>) {
    assert_eq!(rest[0], "$3", "{rest:?}");
    assert_eq!(rest[1], "pel", "{rest:?}");
    let rows: Vec<Vec<String>> = rest[3..].chunks(5).map(<[String]>::to_vec).collect();
    (rest[2][1..].parse().unwrap(), rows)
}

fn pel_ids(rows: &[Vec<String>]) -> Vec<String> {
    rows.iter().map(|r| r[2].clone()).collect()
}

/// Value that follows the `name` label in a flat field/value array.
fn field<'a>(arr: &'a J, name: &str) -> &'a J {
    list(arr)
        .windows(2)
        .find(|w| w[0] == name)
        .map(|w| &w[1])
        .unwrap_or_else(|| panic!("no field {name}: {arr}"))
}

fn list(v: &J) -> &Vec<J> {
    v.as_array().unwrap_or_else(|| panic!("not an array: {v}"))
}

fn num(v: &J) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("not a number: {v}"))
}

fn txt(v: &J) -> String {
    v.as_str()
        .unwrap_or_else(|| panic!("not a string: {v}"))
        .to_string()
}

/// The id (first element) of every `[id, ..]` row, in reply order.
fn ids_of(rows: &[J]) -> Vec<String> {
    rows.iter().map(|row| txt(&row[0])).collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `XGROUP CREATE <S> <g> 0-0 <opts..>` argument vector.
fn create<'a>(g: &'a [u8], opts: &[&'a [u8]]) -> Vec<&'a [u8]> {
    let mut args: Vec<&[u8]> = vec![b"XGROUP", b"CREATE", S, g, b"0-0"];
    args.extend_from_slice(opts);
    args
}

/// Audit mirror: `delivered` rows for freshly handed ids, `acked`
/// flips for acked ones -- one row per delivery, as the desk does it.
async fn audit(c: &mut mysql_async::Conn, ids: &[String], consumer: &str, ack: bool) {
    for id in ids {
        let sql = if ack {
            format!("UPDATE ticket_audit SET state = 'acked' WHERE ticket_id = '{id}' AND state = 'delivered'")
        } else {
            format!("INSERT INTO ticket_audit (ticket_id, consumer, state) VALUES ('{id}', '{consumer}', 'delivered')")
        };
        assert_eq!(aff(c, &sql).await, 1, "{} {id}", if ack { "ack" } else { "deliver" });
    }
}

#[tokio::test]
async fn ordered_groups_inspect_destroy_and_audit() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("ord134", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE ticket_audit (ticket_id VARCHAR(32) PRIMARY KEY, \
         consumer VARCHAR(32), state VARCHAR(16))",
    )
    .await;

    // a) Six tickets land on the enterprise queue in XADD order; no
    //    group exists yet.
    let mut ids = Vec::new();
    for i in 1..=6 {
        let prio = if i % 2 == 0 { b"P2" } else { b"P1" };
        let id = rb(&a, &[b"XADD", S, b"*", b"evt", b"ticket", b"prio", prio]).await;
        assert!(!id.is_empty(), "XADD receipt #{i}");
        ids.push(id);
    }
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "ascending ids: {ids:?}");
    assert_eq!(int_of(&r(&a, &[b"XLEN", S]).await), 6);
    assert_eq!(full(&a, &[b"XINFO", b"GROUPS", S]).await, "*0\r\n", "no group yet");

    // b) The agents group: ORDERED with an in-flight window of 2. The
    //    cap exists only for ordered groups; a zero cap and the
    //    unordered spelling are refused; the name is unique.
    assert_eq!(rt(&a, &create(G, &[b"ORDERED", b"INFLIGHT", b"2"])).await, "+OK");
    assert_eq!(rt(&a, &create(b"loose", &[b"INFLIGHT", b"2"])).await, "-ERR syntax error");
    assert_eq!(
        rt(&a, &create(b"loose", &[b"ORDERED", b"INFLIGHT", b"0"])).await,
        "-ERR value is not an integer or out of range"
    );
    assert_eq!(
        rt(&a, &[b"XGROUP", b"CREATE", S, G, b"$", b"ORDERED"]).await,
        "-BUSYGROUP Consumer Group name already exists"
    );
    let groups = tree(&full(&a, &[b"XINFO", b"GROUPS", S]).await);
    assert_eq!(list(&groups).len(), 1, "{groups}");
    let g = &groups[0];
    assert_eq!(txt(&g[0]), "agents", "bare group name leads the row");
    assert_eq!(list(g).len(), 13, "name + 6 labeled fields");
    assert_eq!(txt(field(g, "last-delivered-id")), "0-0");
    assert_eq!(txt(field(g, "committed-id")), "0-0");
    assert_eq!(num(field(g, "ordered")), 1);
    assert_eq!(num(field(g, "inflight")), 2);
    assert_eq!(field(g, "owner"), &J::Null, "no owner before a read");
    assert_eq!(num(field(g, "epoch")), 0);

    // c) The window is the cap: c1 asks for ten, gets the first two
    //    in stream order; a second ask gets nil until an ack frees a slot.
    assert_eq!(
        read_new(&a, b"c1").await,
        vec![ids[0].clone(), ids[1].clone()],
        "window 2 rations the first read"
    );
    assert_eq!(read_new(&a, b"c1").await, Vec::<String>::new(), "window full -> nil");

    // d) XINFO STREAM FULL: header, the six entries oldest-first, the
    //    group's two-row PEL and c1's roster entry; COUNT 1 truncates
    //    the entry and PEL lists but not the header / roster counts.
    let (doc, rest) = parse(&full(&a, &[b"XINFO", b"STREAM", S, b"FULL"]).await);
    assert_eq!(num(field(&doc, "length")), 6);
    assert_eq!(txt(field(&doc, "last-generated-id")), ids[5]);
    assert_eq!(ids_of(list(field(&doc, "entries"))), ids, "entries oldest-first");
    let groups = list(field(&doc, "groups"));
    assert_eq!(groups.len(), 1);
    let g = &groups[0];
    assert_eq!(txt(field(g, "name")), "agents");
    assert_eq!(txt(field(g, "last-delivered-id")), ids[1]);
    let pending = list(field(g, "pending"));
    assert_eq!(ids_of(pending), vec![ids[0].clone(), ids[1].clone()]);
    for row in pending {
        assert_eq!((txt(&row[1]), num(&row[3])), ("c1".to_string(), 1), "{row}");
    }
    let consumers = list(field(g, "consumers"));
    assert_eq!(consumers.len(), 1, "{consumers:?}");
    assert_eq!(txt(field(&consumers[0], "name")), "c1");
    assert_eq!(num(field(&consumers[0], "pending")), 2);
    let seen = num(field(&consumers[0], "seen-time"));
    assert!((seen - now_ms()).abs() < 120_000, "seen-time is an epoch ms: {seen}");
    // Deviation (src/lite/xinfo_full.rs:153-161): the roster entry is
    // declared `*6` (name / seen-time / pending) but a fourth `pel`
    // pair is emitted after it, OUTSIDE the declared array -- it reads
    // as trailing data after the top-level reply. Asserted as-is.
    let (n, tail) = pel_tail(&rest);
    assert_eq!((n, tail.len()), (2, 2), "two consumer PEL rows: {rest:?}");
    assert_eq!(pel_ids(&tail), ids_of(pending), "consumer PEL == group PEL");
    assert!(tail.iter().all(|r| r[4] == ":1"), "first delivery: {tail:?}");
    let (trunc, rest) = parse(&full(&a, &[b"XINFO", b"STREAM", S, b"FULL", b"COUNT", b"1"]).await);
    assert_eq!(num(field(&trunc, "length")), 6, "COUNT caps lists, not the header");
    assert_eq!(
        ids_of(list(field(&trunc, "entries"))),
        vec![ids[5].clone()],
        "newest entry only"
    );
    let tg = &list(field(&trunc, "groups"))[0];
    assert_eq!(ids_of(list(field(tg, "pending"))), vec![ids[0].clone()], "PEL capped too");
    let tc = &list(field(tg, "consumers"))[0];
    assert_eq!(num(field(tc, "pending")), 2, "roster count stays exact");
    let (n, tail) = pel_tail(&rest);
    assert_eq!((n, tail.len()), (1, 1), "consumer PEL capped: {rest:?}");
    assert_eq!(tail[0][2], ids[0], "oldest consumer PEL row kept");
    audit(&mut c, &[ids[0].clone(), ids[1].clone()], "c1", false).await;

    // e) One slot open: a second consumer is fenced out (the owner's
    //    lease is fresh) and c1 takes the next ticket; the window is
    //    full again for everyone.
    assert_eq!(int_of(&r(&a, &[b"XACK", S, G, ids[0].as_bytes()]).await), 1);
    audit(&mut c, &[ids[0].clone()], "c1", true).await;
    assert_eq!(read_new(&a, b"c2").await, Vec::<String>::new(), "c2 fenced out");
    assert_eq!(read_new(&a, b"c1").await, vec![ids[2].clone()], "one slot -> one ticket");
    audit(&mut c, &[ids[2].clone()], "c1", false).await;
    assert_eq!(read_new(&a, b"c1").await, Vec::<String>::new(), "full again");
    let groups = tree(&full(&a, &[b"XINFO", b"GROUPS", S]).await);
    assert_eq!(txt(field(&groups[0], "owner")), "c1", "{groups}");
    assert_eq!(num(field(&groups[0], "epoch")), 1);

    // f) Acks free slots (a re-ack is 0): the next two tickets flow in
    //    order; one is acked, one stays pending.
    for id in [ids[1].clone(), ids[2].clone()] {
        assert_eq!(int_of(&r(&a, &[b"XACK", S, G, id.as_bytes()]).await), 1, "ack {id}");
    }
    audit(&mut c, &[ids[1].clone(), ids[2].clone()], "c1", true).await;
    assert_eq!(int_of(&r(&a, &[b"XACK", S, G, ids[2].as_bytes()]).await), 0, "double ack");
    assert_eq!(
        read_new(&a, b"c1").await,
        vec![ids[3].clone(), ids[4].clone()],
        "two slots -> two tickets"
    );
    audit(&mut c, &[ids[3].clone(), ids[4].clone()], "c1", false).await;
    assert_eq!(int_of(&r(&a, &[b"XACK", S, G, ids[3].as_bytes()]).await), 1);
    audit(&mut c, &[ids[3].clone()], "c1", true).await;

    // g) Ops inspect + audit cross-check: the PEL / roster counts read
    //    from XINFO FULL equal the SQL state counts (5 delivered,
    //    4 acked, 1 open; the newest ticket was never handed out).
    let t = scenario_common::perf::now();
    let (doc, rest) = parse(&full(&a, &[b"XINFO", b"STREAM", S, b"FULL"]).await);
    let g = &list(field(&doc, "groups"))[0];
    let pending = ids_of(list(field(g, "pending")));
    assert_eq!(pending, vec![ids[4].clone()], "{g}");
    assert_eq!(txt(field(g, "last-delivered-id")), ids[4]);
    let consumers = list(field(g, "consumers"));
    assert_eq!(consumers.len(), 1, "c2 never delivered, so it is not on the roster");
    let roster: i64 = consumers.iter().map(|c| num(field(c, "pending"))).sum();
    assert_eq!(roster, pending.len() as i64, "roster sums to the PEL");
    assert_eq!(txt(field(&consumers[0], "name")), "c1");
    let (_, tail) = pel_tail(&rest);
    assert_eq!(pel_ids(&tail), pending, "trailing consumer PEL");
    let delivered = ids.iter().position(|id| *id == pending[0]).unwrap() as i64 + 1;
    let open = pending.len() as i64;
    let states = pairs(
        &mut c,
        "SELECT COUNT(*), state FROM ticket_audit GROUP BY state ORDER BY state",
    )
    .await;
    assert_eq!(
        states,
        vec![(delivered - open, "acked".to_string()), (open, "delivered".to_string())]
    );
    let per_consumer = pairs(
        &mut c,
        "SELECT COUNT(*), consumer FROM ticket_audit WHERE state = 'delivered' GROUP BY consumer",
    )
    .await;
    assert_eq!(per_consumer, vec![(open, "c1".to_string())], "per-consumer open count");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM ticket_audit").await, vec![delivered]);
    scenario_common::perf::within("ord134/xinfo-full", t, Duration::from_secs(2));

    // h) Retire the group: :1 then :0; reads fail with NOGROUP, XACK
    //    is :0, XINFO shows no group while the entries stay; the audit
    //    orphans exactly the undelivered PEL rows.
    assert_eq!(int_of(&r(&a, &[b"XGROUP", b"DESTROY", S, G]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"XGROUP", b"DESTROY", S, G]).await), 0);
    let read: &[&[u8]] = &[b"XREADGROUP", b"GROUP", G, b"c1", b"STREAMS", S, b">"];
    assert_eq!(
        rt(&a, read).await,
        "-NOGROUP No such key 'tickets/enterprise' or consumer group 'agents'"
    );
    assert_eq!(int_of(&r(&a, &[b"XACK", S, G, ids[4].as_bytes()]).await), 0, "ack without group");
    assert_eq!(full(&a, &[b"XINFO", b"GROUPS", S]).await, "*0\r\n");
    let after = tree(&full(&a, &[b"XINFO", b"STREAM", S, b"FULL"]).await);
    assert_eq!(num(field(&after, "length")), 6, "entries survive the group");
    assert_eq!(list(field(&after, "groups")).len(), 0);
    assert_eq!(int_of(&r(&a, &[b"XLEN", S]).await), 6);
    let orphaned = aff(&mut c, "UPDATE ticket_audit SET state = 'orphaned' WHERE state = 'delivered'")
        .await as i64;
    assert_eq!(orphaned, open, "orphaned == PEL rows");
    let states = pairs(
        &mut c,
        "SELECT COUNT(*), state FROM ticket_audit GROUP BY state ORDER BY state",
    )
    .await;
    assert_eq!(
        states,
        vec![(delivered - open, "acked".to_string()), (open, "orphaned".to_string())]
    );
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM ticket_audit").await, vec![delivered]);
    assert_eq!(delivered + 1, 6, "the newest ticket was never delivered");
    scenario_common::perf::scenario("ord134", t0);
}
