//! Multi-store scenario #70 -- an ad audience builder: tag sets
//! SINTERSTORE/SDIFFSTORE into overlap and exclusion audiences,
//! SRANDMEMBER draws a fixed-size panel (positive = distinct capped,
//! negative = independent repeats), SSCAN pages one tag, SCAN pages
//! the whole exported-audience keyspace by MATCH pattern with cursor
//! resume, and UNLINK retires the batch in one command. One REAL
//! node, RESP only.

use crate::scenario_common;
use std::collections::BTreeSet;

use scenario_common::{front_world, int_of, r, rf, rt, FrontSpec};

/// Collect every `$<len>\r\n<payload>\r\n` bulk of a reply, in order.
fn bulks(reply: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < reply.len() {
        if reply[i] != b'$' {
            i += 1;
            continue;
        }
        let Some(hdr_end) = reply[i..].windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let hdr_end = hdr_end + i;
        let Ok(len) = String::from_utf8_lossy(&reply[i + 1..hdr_end]).parse::<usize>() else {
            i += 1; // "$-1" nil marker: skip
            continue;
        };
        out.push(String::from_utf8_lossy(&reply[hdr_end + 2..hdr_end + 2 + len]).to_string());
        i = hdr_end + 2 + len + 2;
    }
    out
}

#[tokio::test]
async fn audience_sets_scan_pages_and_retire() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec::default();
    let (_front, a) = front_world("aud70", &spec).await;

    // a) Tag membership: vip and new overlap on u3/u4; the stored
    //    intersection and difference precompute the two audiences.
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{a7}vip", b"u1", b"u2", b"u3", b"u4"]).await),
        4
    );
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{a7}new", b"u3", b"u4", b"u5"]).await),
        3
    );
    assert_eq!(
        int_of(&r(&a, &[b"SINTERSTORE", b"{a7}both", b"{a7}vip", b"{a7}new"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"SDIFFSTORE", b"{a7}only", b"{a7}vip", b"{a7}new"]).await),
        2
    );
    assert_eq!(int_of(&r(&a, &[b"SISMEMBER", b"{a7}both", b"u3"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"SISMEMBER", b"{a7}only", b"u1"]).await), 1);

    // b) Panel draws: a positive count answers DISTINCT members capped
    //    by cardinality; a negative count answers exactly |n|
    //    independent picks (repeats allowed); every draw is a member.
    let panel = rf(&a, &[b"SRANDMEMBER", b"{a7}vip", b"2"]).await;
    let picks = bulks(panel.as_bytes());
    assert_eq!(picks.len(), 2, "{panel}");
    for p in &picks {
        assert_eq!(
            int_of(&r(&a, &[b"SISMEMBER", b"{a7}vip", p.as_bytes()]).await),
            1,
            "drawn {p} must be a member"
        );
    }
    assert_eq!(
        picks.len(),
        BTreeSet::from_iter(picks.iter().cloned()).len(),
        "positive count is distinct"
    );
    let five = rf(&a, &[b"SRANDMEMBER", b"{a7}vip", b"9"]).await;
    assert_eq!(
        bulks(five.as_bytes()).len(),
        4,
        "capped at the cardinality: {five}"
    );
    let rep = rf(&a, &[b"SRANDMEMBER", b"{a7}vip", b"-6"]).await;
    assert_eq!(
        bulks(rep.as_bytes()).len(),
        6,
        "negative count repeats: {rep}"
    );
    assert_eq!(rt(&a, &[b"SRANDMEMBER", b"{a7}ghost"]).await, "$-1");

    // c) SSCAN pages the vip tag with MATCH; the cursor resumes past
    //    the last served member and ends at "0".
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cursor = "0".to_string();
    loop {
        let page = rf(
            &a,
            &[
                b"SSCAN",
                b"{a7}vip",
                cursor.as_bytes(),
                b"COUNT",
                b"2",
                b"MATCH",
                b"u*",
            ],
        )
        .await;
        let parts = bulks(page.as_bytes());
        cursor = parts[0].clone();
        seen.extend(parts[1..].iter().cloned());
        if cursor == "0" {
            break;
        }
    }
    assert_eq!(seen.len(), 4, "full page walk: {seen:?}");

    // d) Export six audience keys (hash-tagged: the batch UNLINK below
    //    is a multi-key command and must clear the CROSSSLOT gate), then
    //    SCAN the whole keyspace by MATCH pattern with COUNT 2 pages:
    //    every key exactly once, cursor ends at "0".
    for i in 1..=6 {
        assert_eq!(
            rt(&a, &[b"SET", format!("{{a7}}exp:{i}").as_bytes(), b"x"]).await,
            "+OK"
        );
    }
    let mut keys: BTreeSet<String> = BTreeSet::new();
    let mut cursor = "0".to_string();
    let mut pages = 0;
    loop {
        let page = rf(
            &a,
            &[
                b"SCAN",
                cursor.as_bytes(),
                b"MATCH",
                b"*exp:*",
                b"COUNT",
                b"2",
            ],
        )
        .await;
        let parts = bulks(page.as_bytes());
        cursor = parts[0].clone();
        keys.extend(parts[1..].iter().cloned());
        pages += 1;
        if cursor == "0" {
            break;
        }
    }
    assert_eq!(keys.len(), 6, "every export exactly once: {keys:?}");
    assert!(pages >= 2, "COUNT 2 forces pagination: {pages}");

    // e) UNLINK retires the batch in one command; KEYS agrees.
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"UNLINK",
                    b"{a7}exp:1",
                    b"{a7}exp:2",
                    b"{a7}exp:3",
                    b"{a7}exp:4",
                    b"{a7}exp:5",
                    b"{a7}exp:6"
                ]
            )
            .await
        ),
        6
    );
    assert_eq!(rt(&a, &[b"KEYS", b"*exp:*"]).await, "*0");
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"{a7}exp:3"]).await), 0);
}
