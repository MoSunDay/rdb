//! Multi-store scenario #63 -- a plant sensor wall: one bitmap per
//! lane records alarm slots (SETBIT/GETBIT), BITPOS finds the first
//! tripped sensor with and without a byte offset, BITCOUNT sizes the
//! window, SSCAN pages the sensor registry with cursor resume, and
//! SINTERSTORE/SDIFFSTORE precompute the lane rollups SMOVE then
//! quarantines into. One REAL node, RESP only.

use crate::scenario_common;
use std::collections::BTreeSet;

use scenario_common::{front_world, int_of, r, rf, FrontSpec};

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
async fn bitmaps_first_alarm_and_lane_rollups() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec::default();
    let (_front, a) = front_world("sensor63", &spec).await;

    // a) Lane A trips sensors 1, 3 and 9; GETBIT reads them back.
    for bit in [1u8, 3, 9] {
        assert_eq!(
            int_of(
                &r(
                    &a,
                    &[b"SETBIT", b"{sn}laneA", bit.to_string().as_bytes(), b"1"]
                )
                .await
            ),
            0,
            "SETBIT answers the previous bit"
        );
    }
    assert_eq!(int_of(&r(&a, &[b"GETBIT", b"{sn}laneA", b"3"]).await), 1);
    assert_eq!(
        int_of(&r(&a, &[b"GETBIT", b"{sn}laneA", b"4"]).await),
        0,
        "never tripped"
    );

    // b) BITPOS walks to the first alarm; a byte start skips lane A's
    //    whole first byte (bits 0-7) and lands on 9; a clean lane
    //    answers -1.
    assert_eq!(int_of(&r(&a, &[b"BITPOS", b"{sn}laneA", b"1"]).await), 1);
    assert_eq!(
        int_of(&r(&a, &[b"BITPOS", b"{sn}laneA", b"1", b"1"]).await),
        9,
        "resume past byte 0"
    );
    assert_eq!(
        int_of(&r(&a, &[b"BITPOS", b"{sn}laneZ", b"1"]).await),
        -1,
        "clean lane"
    );

    // c) Window sizing: BITCOUNT over all bytes and over one byte.
    assert_eq!(int_of(&r(&a, &[b"BITCOUNT", b"{sn}laneA"]).await), 3);
    assert_eq!(
        int_of(&r(&a, &[b"BITCOUNT", b"{sn}laneA", b"0", b"0"]).await),
        2,
        "bits 1 and 3"
    );

    // d) The sensor registry pages through SSCAN with COUNT 2: the
    //    cursor walks every member exactly once and ends at "0".
    assert_eq!(
        int_of(
            &r(
                &a,
                &[b"SADD", b"{sn}registry", b"s1", b"s2", b"s3", b"s4", b"s5"]
            )
            .await
        ),
        5
    );
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cursor = "0".to_string();
    let mut pages = 0;
    loop {
        let page = rf(
            &a,
            &[b"SSCAN", b"{sn}registry", cursor.as_bytes(), b"COUNT", b"2"],
        )
        .await;
        let parts = bulks(page.as_bytes());
        cursor = parts[0].clone();
        seen.extend(parts[1..].iter().cloned());
        pages += 1;
        if cursor == "0" {
            break;
        }
    }
    assert_eq!(seen.len(), 5, "every sensor exactly once: {seen:?}");
    assert!(pages >= 2, "COUNT 2 forces real pagination, pages={pages}");

    // e) Lane rollups precompute: intersection and difference land in
    //    stored sets; SMOVE quarantines a flapping sensor.
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{sn}down", b"s1", b"s3"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{sn}watch", b"s1", b"s2", b"s3"]).await),
        3
    );
    assert_eq!(
        int_of(&r(&a, &[b"SINTERSTORE", b"{sn}ack", b"{sn}down", b"{sn}watch"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"SDIFFSTORE", b"{sn}todo", b"{sn}watch", b"{sn}down"]).await),
        1
    );
    assert_eq!(int_of(&r(&a, &[b"SISMEMBER", b"{sn}todo", b"s2"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"SISMEMBER", b"{sn}todo", b"s1"]).await), 0);
    assert_eq!(
        int_of(&r(&a, &[b"SMOVE", b"{sn}watch", b"{sn}quarantine", b"s3"]).await),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"SMOVE", b"{sn}watch", b"{sn}quarantine", b"s3"]).await),
        0,
        "already moved"
    );
    assert_eq!(int_of(&r(&a, &[b"SCARD", b"{sn}watch"]).await), 2);
}
