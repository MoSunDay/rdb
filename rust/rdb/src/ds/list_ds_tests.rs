//! Unit tests for the list layer (`ds::list_ds`): pure encoding/math
//! checks -- suffix ordering, meta payload roundtrip, logical-position
//! resolution and pop targeting. No RocksDB involved.

use super::codec;
use super::list_ds::*;

const P: &[u8] = b"70/";

#[test]
fn suffixes_order_l_descending_r_ascending() {
    // L: l = 0 sorts AFTER l = 1 physically (front of the list first)
    assert!(l_key(P, b"k", 0) > l_key(P, b"k", 1));
    assert!(l_key(P, b"k", 5) < l_key(P, b"k", 4));
    assert!(l_key(P, b"k", u64::MAX) < l_key(P, b"k", u64::MAX - 1));
    // R: ascending physical = ascending r
    assert!(r_key(P, b"k", 0) < r_key(P, b"k", 1));
    assert!(r_key(P, b"k", u64::MAX - 1) < r_key(P, b"k", u64::MAX));
    // both entry keys carry exactly one 8-byte suffix after the header
    let body = P.len() + 1 + 4 + b"k".len();
    assert_eq!(l_key(P, b"k", 7).len(), body + SUFFIX_LEN);
    assert_eq!(r_key(P, b"k", 7).len(), body + SUFFIX_LEN);
}

#[test]
fn meta_payload_roundtrip() {
    let meta = ListMeta {
        expire_ms: 42,
        l_count: 3,
        l_next: 10,
        r_count: 5,
        r_next: 7,
    };
    let payload = encode_meta_payload(&meta);
    assert_eq!(decode_meta_payload(42, &payload), meta);
    // the payload composes unchanged inside a TTL envelope
    let enveloped = codec::encode_envelope(42, &payload);
    let (expire, rest) = codec::decode_envelope(&enveloped);
    assert_eq!((expire, rest.to_vec()), (42, payload));
    // blank and saturated counters survive too
    let blank = ListMeta {
        expire_ms: 0,
        l_count: 0,
        l_next: 0,
        r_count: 0,
        r_next: 0,
    };
    assert_eq!(decode_meta_payload(0, &encode_meta_payload(&blank)), blank);
    let big = ListMeta {
        expire_ms: 0,
        l_count: u64::MAX,
        l_next: u64::MAX,
        r_count: 0,
        r_next: 1,
    };
    assert_eq!(decode_meta_payload(0, &encode_meta_payload(&big)), big);
}

#[test]
fn position_of_resolves_negative_and_bounds() {
    // len = 5
    let meta = ListMeta {
        expire_ms: 0,
        l_count: 2,
        l_next: 5,
        r_count: 3,
        r_next: 4,
    };
    assert_eq!(position_of(&meta, 0), Some(0));
    assert_eq!(position_of(&meta, 4), Some(4));
    assert_eq!(position_of(&meta, -1), Some(4)); // last
    assert_eq!(position_of(&meta, -5), Some(0)); // first
    assert_eq!(position_of(&meta, 5), None); // past the end
    assert_eq!(position_of(&meta, -6), None); // before the start
    let empty = ListMeta {
        expire_ms: 0,
        l_count: 0,
        l_next: 0,
        r_count: 0,
        r_next: 0,
    };
    assert_eq!(position_of(&empty, 0), None);
    assert_eq!(position_of(&empty, -1), None);
}

#[test]
fn locate_spans_the_lr_boundary() {
    // l_base = 3, r_base = 1; logical [l4, l3, r1, r2, r3]
    let meta = ListMeta {
        expire_ms: 0,
        l_count: 2,
        l_next: 5,
        r_count: 3,
        r_next: 4,
    };
    assert_eq!(locate(&meta, 0), (true, 4));
    assert_eq!(locate(&meta, meta.l_count - 1), (true, 3)); // last L slot
    assert_eq!(locate(&meta, meta.l_count), (false, 1)); // first R slot
    assert_eq!(locate(&meta, 4), (false, 3));
}

#[test]
fn pop_targets_prefer_their_own_side() {
    let pure_l = ListMeta {
        expire_ms: 0,
        l_count: 3,
        l_next: 9,
        r_count: 0,
        r_next: 2,
    };
    let pure_r = ListMeta {
        expire_ms: 0,
        l_count: 0,
        l_next: 4,
        r_count: 3,
        r_next: 7,
    };
    let mixed = ListMeta {
        expire_ms: 0,
        l_count: 2,
        l_next: 5,
        r_count: 3,
        r_next: 4,
    };
    let empty = ListMeta {
        expire_ms: 0,
        l_count: 0,
        l_next: 0,
        r_count: 0,
        r_next: 0,
    };
    assert_eq!(pop_left_target(&pure_l), (true, 8)); // l_next - 1
    assert_eq!(pop_right_target(&pure_l), (true, 6)); // falls to l_base
    assert_eq!(pop_left_target(&pure_r), (false, 4)); // falls to r_base
    assert_eq!(pop_right_target(&pure_r), (false, 6)); // r_next - 1
    assert_eq!(pop_left_target(&mixed), (true, 4));
    assert_eq!(pop_right_target(&mixed), (false, 3));
    assert_eq!(pop_left_target(&empty), (false, 0));
    assert_eq!(pop_right_target(&empty), (true, 0));
}
