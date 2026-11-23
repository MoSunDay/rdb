//! Multi-store scenario #49 -- a chat message edit log: the original
//! text seeds a list, RPUSHX appends edits only once the ledger
//! exists, LINSERT injects corrections around a pivot message, LSET
//! rewrites a slot in place, LREM retracts every copy of a bad word,
//! and RPOPLPUSH rotates the final tail into the archive list. One
//! REAL node, RESP only, same-slot hash-tagged pair for the rotate.

use crate::scenario_common;
use scenario_common::{bulk_at, front_world, int_of, r, rb, rf, FrontSpec};

#[tokio::test]
async fn chat_edits_append_insert_rewrite_and_retract() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec::default();
    let (_front, a) = front_world("chat49", &spec).await;

    // a) The ledger: seed then append-only via RPUSHX; LPUSHX on a
    //    MISSING key refuses (:0) -- the draft channel stays empty.
    assert_eq!(
        int_of(&r(&a, &[b"RPUSH", b"{ch}log:1", b"hello world"]).await),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"RPUSHX", b"{ch}log:1", b"hello* (typo)"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"LPUSHX", b"chat:draft:1", b"ghost"]).await),
        0,
        "no ghost draft"
    );
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"chat:draft:1"]).await), 0);
    assert_eq!(
        int_of(&r(&a, &[b"RPUSH", b"{ch}log:1", b"hello world"]).await),
        3,
        "dupe rides too"
    );

    // b) Corrections: LINSERT wraps the typo with markers; a missing
    //    pivot refuses with -1.
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"LINSERT",
                    b"{ch}log:1",
                    b"BEFORE",
                    b"hello* (typo)",
                    b"[edit]"
                ]
            )
            .await
        ),
        4
    );
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"LINSERT",
                    b"{ch}log:1",
                    b"AFTER",
                    b"hello* (typo)",
                    b"[/edit]"
                ]
            )
            .await
        ),
        5
    );
    assert_eq!(
        int_of(&r(&a, &[b"LINSERT", b"{ch}log:1", b"BEFORE", b"nope", b"x"]).await),
        -1
    );

    // c) In-place rewrite: LSET swaps the typo slot for the fix.
    assert!(r(&a, &[b"LSET", b"{ch}log:1", b"2", b"hello, world!"])
        .await
        .starts_with(b"+OK"));
    assert_eq!(
        rb(&a, &[b"LINDEX", b"{ch}log:1", b"2"]).await,
        "hello, world!"
    );

    // d) Retraction: LREM 0 strips EVERY copy of the stale phrase.
    assert_eq!(
        int_of(&r(&a, &[b"LREM", b"{ch}log:1", b"0", b"hello world"]).await),
        2
    );
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"{ch}log:1"]).await), 3);
    let log = rf(&a, &[b"LRANGE", b"{ch}log:1", b"0", b"-1"]).await;
    let lines: Vec<String> = (0..3).map(|i| bulk_at(log.as_bytes(), i)).collect();
    assert_eq!(lines, vec!["[edit]", "hello, world!", "[/edit]"]);
    // LPOS lands on the fix.
    assert_eq!(
        int_of(&r(&a, &[b"LPOS", b"{ch}log:1", b"hello, world!"]).await),
        1
    );

    // e) Archive rotation: RPOPLPUSH walks the tail into the archive
    //    (same-slot tagged pair), returning the moved element.
    assert_eq!(
        rb(&a, &[b"RPOPLPUSH", b"{ch}log:1", b"{ch}arch:1"]).await,
        "[/edit]"
    );
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"{ch}log:1"]).await), 2);
    assert_eq!(rb(&a, &[b"LINDEX", b"{ch}arch:1", b"0"]).await, "[/edit]");
}
