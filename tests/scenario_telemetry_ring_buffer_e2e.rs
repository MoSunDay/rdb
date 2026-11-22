//! Multi-store scenario #76 -- a telemetry ring buffer through XADD's
//! INLINE trim: the trim options precede the id (`XADD s MAXLEN 3 * f
//! v`), all three MAXLEN spellings (`3`, `~ 3`, `= 3`) keep the
//! newest tail, MINID drops everything strictly older, NOMKSTREAM
//! refuses to mint a missing stream ($-1), and a REPEATED option lets
//! the last one win; XLEN and XINFO STREAM's length field agree after
//! every trim. One REAL node, RESP only.

mod common;
mod scenario_common;

use scenario_common::{front_world, int_of, r, rf, rt, FrontSpec};

/// The `:<int>` line directly after the named XINFO field line.
fn int_line(reply: &str, name: &str) -> Option<i64> {
    let mut it = reply.split("\r\n");
    while let Some(l) = it.next() {
        if l == name {
            return it.next().and_then(|v| v.strip_prefix(':')?.parse().ok());
        }
    }
    None
}

/// XLEN must equal XINFO STREAM's length field.
async fn agree(a: &str, key: &[u8]) {
    let xlen = int_of(&r(a, &[b"XLEN", key]).await);
    let info = rf(a, &[b"XINFO", b"STREAM", key]).await;
    assert_eq!(int_line(&info, "length"), Some(xlen), "xlen == xinfo length: {info}");
}

#[tokio::test]
async fn ring_buffer_trims_inline_on_every_xadd() {
    let (_front, a) = front_world("ring76", &FrontSpec::default()).await;
    let k: &[u8] = b"tele/ring";

    // a) Seed five samples with explicit ids (the reply is the id).
    for i in 1..=5 {
        let id = format!("1-{i}");
        assert!(rt(&a, &[b"XADD", k, id.as_bytes(), b"f", b"v"]).await.ends_with(&id), "{id} echoed");
    }
    agree(&a, k).await;

    // b) Inline MAXLEN before the id: the append lands and the buffer
    //    keeps only the newest THREE (1-4, 1-5, 1-6).
    assert_eq!(rt(&a, &[b"XADD", k, b"MAXLEN", b"3", b"1-6", b"f", b"v"]).await, "$3\r\n1-6");
    assert_eq!(int_of(&r(&a, &[b"XLEN", k]).await), 3);
    let live = rf(&a, &[b"XRANGE", k, b"-", b"+"]).await;
    assert!(live.contains("1-4") && live.contains("1-6") && !live.contains("1-1"), "{live}");
    agree(&a, k).await;

    // c) The approximate spelling `MAXLEN ~ 2`: identical semantics
    //    (victims are exact), keep the newest TWO.
    assert_eq!(rt(&a, &[b"XADD", k, b"MAXLEN", b"~", b"2", b"1-7", b"f", b"v"]).await, "$3\r\n1-7");
    assert_eq!(int_of(&r(&a, &[b"XLEN", k]).await), 2);
    let live = rf(&a, &[b"XRANGE", k, b"-", b"+"]).await;
    assert!(live.contains("1-6") && live.contains("1-7") && !live.contains("1-4"), "{live}");

    // d) The exact spelling `MAXLEN = 1`: one-slot buffer.
    assert_eq!(rt(&a, &[b"XADD", k, b"MAXLEN", b"=", b"1", b"1-8", b"f", b"v"]).await, "$3\r\n1-8");
    assert_eq!(int_of(&r(&a, &[b"XLEN", k]).await), 1);
    agree(&a, k).await;

    // e) MINID: drop everything STRICTLY below the id, keep the new
    //    sample; the survivor floor is 2-2.
    for i in 1..=3 {
        let id = format!("2-{i}");
        assert!(rt(&a, &[b"XADD", k, id.as_bytes(), b"f", b"v"]).await.ends_with(&id));
    }
    assert_eq!(rt(&a, &[b"XADD", k, b"MINID", b"2-2", b"2-4", b"f", b"v"]).await, "$3\r\n2-4");
    assert_eq!(int_of(&r(&a, &[b"XLEN", k]).await), 3, "1-8 and 2-1 dropped");
    let live = rf(&a, &[b"XRANGE", k, b"-", b"+"]).await;
    assert!(live.contains("2-2") && live.contains("2-4") && !live.contains("1-8") && !live.contains("2-1"), "{live}");
    agree(&a, k).await;

    // f) Repeated options: the LAST one wins (MAXLEN 10 then 2 keeps 2).
    let auto = rt(&a, &[b"XADD", k, b"MAXLEN", b"10", b"MAXLEN", b"2", b"*", b"f", b"v"]).await;
    assert!(auto.starts_with("$"), "auto id: {auto}");
    assert_eq!(int_of(&r(&a, &[b"XLEN", k]).await), 2, "last option wins");
    agree(&a, k).await;

    // g) NOMKSTREAM: a missing stream refuses to mint ($-1), the key
    //    never exists.
    assert_eq!(rt(&a, &[b"XADD", b"tele/nomk", b"NOMKSTREAM", b"*", b"f", b"v"]).await, "$-1");
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"tele/nomk"]).await), 0);
    assert_eq!(rt(&a, &[b"TYPE", b"tele/nomk"]).await, "+none");
}
