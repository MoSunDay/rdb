//! Multi-store scenario #88 -- a lexicon index read backwards: six
//! words share one score so byte order rules, ZREVRANGEBYLEX walks the
//! whole dictionary newest-letter-first, windows an exclusive/inclusive
//! interior range with the REVERSED bound spellings (`(cherry` down to
//! `[apple`), and LIMIT caps the reverse walk; ZREMRANGEBYLEX then
//! prunes a whole PREFIX (`[a` to `(b` -- a different call shape from
//! #54's interior retirement), and the reverse window proves the
//! survivors intact. One REAL node, RESP only.

mod common;
mod scenario_common;

use scenario_common::{front_world, int_of, r, rf, FrontSpec};

/// Member payloads of a zset range reply, in reply order.
fn members(reply: &str) -> Vec<String> {
    let n = reply
        .strip_prefix('*')
        .and_then(|r| r.split("\r\n").next())
        .and_then(|d| d.parse::<usize>().ok())
        .unwrap_or(0);
    (0..n)
        .map(|i| scenario_common::bulk_at(reply.as_bytes(), i))
        .collect()
}

#[tokio::test]
async fn lexicon_reads_backwards_and_prunes_a_prefix() {
    let (_front, a) = front_world("lx88", &FrontSpec::default()).await;
    let lex = b"{lx88}lexicon";

    // a) Six words, one shared score: byte order is the only order.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", lex, b"0", b"apple", b"0", b"apricot", b"0", b"banana", b"0", b"cherry", b"0", b"citron", b"0", b"date"]).await),
        6
    );

    // b) The full reverse walk: newest letter first.
    let full = rf(&a, &[b"ZREVRANGEBYLEX", lex, b"+", b"-"]).await;
    assert_eq!(
        members(&full),
        vec!["date", "citron", "cherry", "banana", "apricot", "apple"],
        "{full}"
    );

    // c) Reversed window spellings: exclusive `(cherry` down to
    //    inclusive `[apple` = cherry..apple.
    let win = rf(&a, &[b"ZREVRANGEBYLEX", lex, b"(cherry", b"[apple"]).await;
    assert_eq!(members(&win), vec!["banana", "apricot", "apple"], "{win}");

    // d) LIMIT caps the reverse walk from its head.
    let top = rf(&a, &[b"ZREVRANGEBYLEX", lex, b"+", b"-", b"LIMIT", b"0", b"2"]).await;
    assert_eq!(members(&top), vec!["date", "citron"], "{top}");

    // e) Prefix prune: `[a` `(b` retires the whole 'a' words (:2), a
    //    different shape from #54's interior window.
    assert_eq!(
        int_of(&r(&a, &[b"ZREMRANGEBYLEX", lex, b"[a", b"(b"]).await),
        2,
        "apple+apricot pruned"
    );
    assert_eq!(int_of(&r(&a, &[b"ZCARD", lex]).await), 4);

    // f) The reverse window over the survivors: same bounds, the two
    //    'a' words gone from the tail.
    let after = rf(&a, &[b"ZREVRANGEBYLEX", lex, b"+", b"-"]).await;
    assert_eq!(members(&after), vec!["date", "citron", "cherry", "banana"], "{after}");
}
