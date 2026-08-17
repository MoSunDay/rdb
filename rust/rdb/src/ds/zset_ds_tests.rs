//! Unit tests for the zset layer (`ds::zset_ds`): pure encoding checks --
//! sortable-score monotonicity/roundtrip, score-record key ordering and
//! the decode-based window check. No RocksDB involved.

use super::zset_ds::{
    is_score_record, member_key, meta_key, score_key, score_key_from_suffix, score_sortable,
    sortable_score,
};

const P: &[u8] = b"70/";
const KEY: &[u8] = b"z";

/// Ascending numeric sample crossing every interesting boundary: both
/// infinities, the -0.0/+0.0 bit split, denormals and huge magnitudes.
const ORDERED: [f64; 8] = [
    f64::NEG_INFINITY,
    -1e300,
    -1.5,
    -0.0,
    0.0,
    1e-300,
    3.5,
    f64::INFINITY,
];

#[test]
fn sortable_is_monotonic_across_signs() {
    for pair in ORDERED.windows(2) {
        assert!(
            score_sortable(pair[0]) < score_sortable(pair[1]),
            "{:?} must sort before {:?}",
            pair[0],
            pair[1]
        );
    }
    // the two zeros keep their bit-distinct order: -0.0 before +0.0
    assert!(score_sortable(-0.0) < score_sortable(0.0));
    // negatives land below the sign-bit pivot, non-negatives above it
    assert!(score_sortable(-1.5) < 0x8000_0000_0000_0000);
    assert!(score_sortable(0.0) >= 0x8000_0000_0000_0000);
}

#[test]
fn sortable_roundtrips_bit_exactly() {
    for &v in &ORDERED {
        assert_eq!(sortable_score(score_sortable(v)).to_bits(), v.to_bits());
    }
}

#[test]
fn score_keys_order_by_score_then_member() {
    let sk = |s: f64, m: &[u8]| score_key(P, KEY, &score_sortable(s).to_be_bytes(), m);
    // same score: member byte order decides
    assert!(sk(1.0, b"a") < sk(1.0, b"b"));
    assert!(sk(1.0, b"") < sk(1.0, b"a"));
    // ascending scores order numerically through the sign boundary
    assert!(sk(-2.0, b"m") < sk(0.0, b"m"));
    assert!(sk(0.0, b"m") < sk(3.0, b"m"));
    assert!(sk(f64::NEG_INFINITY, b"m") < sk(f64::INFINITY, b"m"));
    // +inf (max sortable) still sorts below any LONGER suffix member
    // comparison handled by the decoder, and its key stays in-window
    assert!(is_score_record(&sk(f64::INFINITY, b"m"), P.len(), KEY).is_some());
}

#[test]
fn window_check_decodes_exactly() {
    let k = score_key(P, KEY, &score_sortable(1.0).to_be_bytes(), b"mem");
    let expected = [
        score_sortable(1.0).to_be_bytes().as_slice(),
        b"mem".as_slice(),
    ]
    .concat();
    assert_eq!(
        is_score_record(&k, P.len(), KEY).map(|s| s.to_vec()),
        Some(expected)
    );
    // foreign kinds, foreign user keys and short suffixes are rejected
    assert!(is_score_record(&member_key(P, KEY, b"mem"), P.len(), KEY).is_none());
    assert!(is_score_record(&k, P.len(), b"other").is_none());
    assert!(is_score_record(&meta_key(P, KEY), P.len(), KEY).is_none());
    // raw score record with an EMPTY suffix (< 8 bytes) is not in-window
    assert!(is_score_record(&score_key_from_suffix(P, KEY, b""), P.len(), KEY).is_none());
}
