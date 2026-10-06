//! Lite DLQ trigger-surface e2e (process-level, real binary over RESP):
//! the two MAXDELIVERY surfaces lite_dlq_e2e.rs leaves uncovered --
//! (1) `>` re-delivery whose next delivery crosses the cap (a rewind via
//! XGROUP SETID re-offers pending ids through XREADGROUP `>`, the crash
//! redelivery path) and (2) XAUTOCLAIM: repeated delivery claims push the
//! row over the limit while JUSTID (an ownership move, not a delivery)
//! must never trigger the transfer. Sweep-surface and XCLAIM-surface
//! coverage lives in lite_redeliver_e2e.rs / lite_dlq_e2e.rs.

mod common;

use common::lite::{boot, pending_rows, resp_full, resp_text};

/// XGROUP CREATE <s> <g> 0-0 MKSTREAM <extra..>; asserts +OK.
async fn gcreate(a: &str, s: &[u8], extra: &[&[u8]]) {
    let mut args: Vec<&[u8]> = vec![b"xgroup", b"create", s, b"g", b"0-0", b"MKSTREAM"];
    args.extend_from_slice(extra);
    assert_eq!(resp_text(a, &args).await, "+OK", "xgroup create {s:?}");
}

async fn xadd(a: &str, s: &[u8], id: &str) {
    let r = resp_text(a, &[b"xadd", s, id.as_bytes(), b"f", b"v"]).await;
    assert!(r.contains(id), "xadd {id}: {r}");
}

/// XREADGROUP GROUP g <c> STREAMS <s> `>` full reply.
async fn read_new(a: &str, s: &[u8], c: &[u8]) -> String {
    resp_full(a, &[b"xreadgroup", b"group", b"g", c, b"streams", s, b">"]).await
}

/// XGROUP SETID <s> g 0-0 (the public rewind that makes `>` re-offer
/// already-delivered ids, mirroring a restart to the committed watermark).
async fn rewind(a: &str, s: &[u8]) {
    assert_eq!(
        resp_text(a, &[b"xgroup", b"setid", s, b"g", b"0-0"]).await,
        "+OK",
        "xgroup setid {s:?}"
    );
}

async fn dlq_len(a: &str, s: &[u8]) -> String {
    let dlq = format!("{}/dlq", String::from_utf8_lossy(s));
    resp_text(a, &[b"xlen", dlq.as_bytes()]).await
}

/// Expected-PEL literal for assert_eq against [`pending_rows`].
fn rows(v: &[(&str, &str, u64)]) -> Vec<(String, String, u64)> {
    v.iter()
        .map(|(i, c, n)| (i.to_string(), c.to_string(), *n))
        .collect()
}

/// Empty XPENDING summary (all rows gone from the group's PEL).
const PEL0: &str = "*3\r\n:0\r\n$-1\r\n$-1\r\n";

/// Surface 1: a `>` round that re-offers a pending id (XGROUP SETID
/// rewind, the crash-redelivery shape) keeps serving while the next
/// delivery still fits the cap, then transfers -- mid-batch, around a
/// legal sibling -- instead of re-delivering the poison forever.
#[tokio::test]
async fn gt_redelivery_over_limit_dead_letters() {
    let (_node, a) = boot("gt").await;
    gcreate(&a, b"t/q0", &[b"MAXDELIVERY", b"2"]).await;
    xadd(&a, b"t/q0", "1-1").await;
    xadd(&a, b"t/q0", "1-2").await;
    // `>` delivery #1 for both; the healthy sibling is acked at once.
    assert!(read_new(&a, b"t/q0", b"c1").await.contains("1-2"));
    assert_eq!(resp_text(&a, &[b"xack", b"t/q0", b"g", b"1-1"]).await, ":1");
    // Rewind #1: `>` re-offers both -- 1-1 has no PEL row (fresh again,
    // count restarts at 1) while 1-2 carries its count over: 1 -> 2 is
    // still legal (REACHING the cap delivers), so both are served.
    rewind(&a, b"t/q0").await;
    let second = read_new(&a, b"t/q0", b"c2").await;
    assert!(
        second.contains("1-1") && second.contains("1-2"),
        "legal re-deliveries must serve: {second}"
    );
    assert_eq!(
        pending_rows(&a, b"t/q0", b"g", 10).await,
        rows(&[("1-1", "c2", 1), ("1-2", "c2", 2)]),
        "rewound `>` carries the PEL count over"
    );
    // Rewind #2: 1-1 is served again (1 -> 2 fits) but 1-2's NEXT
    // delivery (2 -> 3 > 2) dead-letters mid-batch: a partial round
    // still replies with the kept entries, never the poison one.
    rewind(&a, b"t/q0").await;
    let third = read_new(&a, b"t/q0", b"c3").await;
    assert!(
        third.contains("1-1") && !third.contains("1-2"),
        "over-limit `>` re-delivery must transfer, not serve: {third}"
    );
    assert_eq!(
        pending_rows(&a, b"t/q0", b"g", 10).await,
        rows(&[("1-1", "c3", 2)]),
        "PEL row drained by the transfer"
    );
    assert_eq!(dlq_len(&a, b"t/q0").await, ":1");
    let dlq = resp_full(&a, &[b"xrange", b"t/q0/dlq", b"-", b"+"]).await;
    assert!(
        dlq.contains("1-2") && dlq.contains("__dlq_times") && dlq.contains("c3"),
        "DLQ entry carries id + trace fields: {dlq}"
    );
    // Not redelivered forever: without another rewind `>` has nothing
    // past the delivered watermark, and the poison never resurfaces.
    assert_eq!(read_new(&a, b"t/q0", b"c4").await, "*-1\r\n", "nil reply");
    assert_eq!(dlq_len(&a, b"t/q0").await, ":1", "no second DLQ copy");
}

/// Surface 2a: repeated delivery XAUTOCLAIMs (min-idle 0, so every call
/// is a delivery) walk the count up legally until the claim that would
/// cross the cap transfers instead -- the id rides the reply's
/// deleted-ids segment, the three-segment Redis>=7 shape stays intact.
#[tokio::test]
async fn xautoclaim_over_limit_dead_letters() {
    let (_node, a) = boot("ac").await;
    gcreate(&a, b"t/q1", &[b"MAXDELIVERY", b"2"]).await;
    xadd(&a, b"t/q1", "1-1").await;
    assert!(read_new(&a, b"t/q1", b"c1").await.contains("1-1"));
    // Delivery claim #2 (1 -> 2 = cap): still served, entry frame back.
    let legal = resp_full(
        &a,
        &[
            b"xautoclaim",
            b"t/q1",
            b"g",
            b"c2",
            b"0",
            b"0-0",
            b"COUNT",
            b"10",
        ],
    )
    .await;
    assert!(legal.contains("1-1"), "claim at the cap delivers: {legal}");
    assert_eq!(
        pending_rows(&a, b"t/q1", b"g", 10).await,
        rows(&[("1-1", "c2", 2)])
    );
    // Claim #3 (2 -> 3 > cap): the transfer round -- no entry frame,
    // the id appears in the DELETED segment (cursor 0-0: scan complete).
    let over = resp_full(
        &a,
        &[
            b"xautoclaim",
            b"t/q1",
            b"g",
            b"c3",
            b"0",
            b"0-0",
            b"COUNT",
            b"10",
        ],
    )
    .await;
    assert_eq!(
        over, "*3\r\n$3\r\n0-0\r\n*0\r\n*1\r\n$3\r\n1-1\r\n",
        "over-limit autoclaim: [cursor, no entries, deleted=[1-1]]"
    );
    assert_eq!(
        resp_full(&a, &[b"xpending", b"t/q1", b"g"]).await,
        PEL0,
        "PEL drained by the transfer"
    );
    assert_eq!(dlq_len(&a, b"t/q1").await, ":1");
}

/// Surface 2b: JUSTID is an ownership move, not a delivery -- repeated
/// JUSTID autoclaims never bump the count and so never transfer, even
/// against a row already at the cap; the row stays fully claimable and
/// the very next DELIVERY claim settles it.
#[tokio::test]
async fn xautoclaim_justid_never_transfers() {
    let (_node, a) = boot("ji").await;
    gcreate(&a, b"t/q2", &[b"MAXDELIVERY", b"2"]).await;
    xadd(&a, b"t/q2", "1-1").await;
    assert!(read_new(&a, b"t/q2", b"c1").await.contains("1-1"));
    // Walk to the cap with a delivery claim (times 2 = MAXDELIVERY).
    let to_cap = resp_full(
        &a,
        &[
            b"xautoclaim",
            b"t/q2",
            b"g",
            b"c2",
            b"0",
            b"0-0",
            b"COUNT",
            b"10",
        ],
    )
    .await;
    assert!(
        to_cap.contains("1-1"),
        "cap-reaching claim delivers: {to_cap}"
    );
    // JUSTID against the at-cap row, repeatedly: ownership moves, the
    // count stays 2, no DLQ side effect -- the transfer gate is
    // delivery-claims only.
    for holder in ["c3", "c4", "c5"] {
        let r = resp_full(
            &a,
            &[
                b"xautoclaim",
                b"t/q2",
                b"g",
                holder.as_bytes(),
                b"0",
                b"0-0",
                b"COUNT",
                b"10",
                b"JUSTID",
            ],
        )
        .await;
        assert_eq!(
            r, "*3\r\n$3\r\n0-0\r\n*1\r\n$3\r\n1-1\r\n*0\r\n",
            "JUSTID replies ids only (no entry frames, no deleted ids)"
        );
        assert_eq!(
            pending_rows(&a, b"t/q2", b"g", 10).await,
            rows(&[("1-1", holder, 2)]),
            "JUSTID moves ownership without a delivery bump"
        );
        assert_eq!(dlq_len(&a, b"t/q2").await, ":0", "JUSTID never transfers");
    }
    // The row is intact: the next DELIVERY claim (2 -> 3 > 2) transfers.
    let settles = resp_full(
        &a,
        &[
            b"xautoclaim",
            b"t/q2",
            b"g",
            b"c6",
            b"0",
            b"0-0",
            b"COUNT",
            b"10",
        ],
    )
    .await;
    assert_eq!(
        settles, "*3\r\n$3\r\n0-0\r\n*0\r\n*1\r\n$3\r\n1-1\r\n",
        "delivery claim settles the at-cap row: [cursor, no entries, deleted=[1-1]]"
    );
    assert_eq!(dlq_len(&a, b"t/q2").await, ":1");
    assert_eq!(resp_full(&a, &[b"xpending", b"t/q2", b"g"]).await, PEL0);
}
