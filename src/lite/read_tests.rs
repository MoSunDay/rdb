//! Unit tests for `lite::read`: STREAMS-list splitting and the entry-id
//! successor. Handler-level (store-backed) read tests live with the
//! command layer; these exercise the pure parsing helpers the handler
//! drives. Mirrors the `kafka/fetch_tests.rs` layout: a sibling module
//! wired as `#[cfg(test)] mod read_tests;` from `lite/mod.rs`.

use super::model;
use super::read::{split_streams_tail, succ_id};

#[test]
fn split_streams_tail_pairs_each_name_with_one_id() {
    let arg = |s: &str| s.as_bytes().to_vec();
    // Two streams, two ids: the id half starts after the name half.
    let args = vec![arg("orders/q0"), arg("orders/q1"), arg("$"), arg("5-0")];
    assert_eq!(split_streams_tail(&args, 0), Some((2, 2)));
    let with_opts = vec![arg("COUNT"), arg("10"), arg("a/b"), arg("0-0")];
    assert_eq!(split_streams_tail(&with_opts, 2), Some((3, 1)));
    // Odd tail (a stream lost its id) and an empty tail are both
    // "Unbalanced": the caller cannot pair names to ids.
    assert_eq!(split_streams_tail(&args[..3], 0), None);
    assert_eq!(split_streams_tail(&args[..0], 0), None);
}

#[test]
fn succ_id_steps_seq_then_ms() {
    let id = |ms: u64, seq: u64| model::EntryId { ms, seq };
    // Intra-millisecond: seq bumps; seq exhausted rolls into the
    // next millisecond; the id ceiling has no successor.
    assert_eq!(succ_id(id(5, 1)), Some(id(5, 2)));
    assert_eq!(succ_id(id(5, u64::MAX)), Some(id(6, 0)));
    assert_eq!(succ_id(id(u64::MAX, u64::MAX)), None);
}
