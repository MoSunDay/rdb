//! Unit tests for the derived-key codec (`ds::codec`): key
//! roundtrips (data/elem/expire-index), LEB128 envelope and count
//! edge cases, per-kind family delete ranges, and raw-vs-typed
//! classification of physical keys.

use super::codec::*;

const P: &[u8] = b"70/";

#[test]
fn data_key_roundtrip() {
    for kind in meta_kinds() {
        let k = data_key(P, *kind, b"mykey");
        let (got_kind, got_key, suffix) = decode_data_key(&k, P.len()).unwrap();
        assert_eq!(got_kind, *kind);
        assert_eq!(got_key, b"mykey");
        assert!(suffix.is_empty());
    }
}

#[test]
fn elem_key_roundtrip_keeps_suffix() {
    let k = elem_key(P, KIND_HASH_FLD, b"k", b"field\0x");
    let (kind, key, suffix) = decode_data_key(&k, P.len()).unwrap();
    assert_eq!(
        (kind, key.as_slice(), suffix),
        (KIND_HASH_FLD, &b"k"[..], &b"field\0x"[..])
    );
}

#[test]
fn decode_rejects_malformed_and_raw() {
    assert_eq!(decode_data_key(P, P.len()), None); // empty
    assert_eq!(decode_data_key(b"70/\x00abcd", P.len()), None); // raw kind
    assert_eq!(
        decode_data_key(
            &expire_index_key(P, 9, &data_key(P, KIND_HASH_META, b"k")),
            P.len()
        ),
        None
    );
    // truncated length field
    assert_eq!(decode_data_key(b"70/\x02\x00", P.len()), None);
    // length beyond the buffer
    assert_eq!(decode_data_key(b"70/\x02\x00\x00\x00\x09ab", P.len()), None);
    // zero-length user key is legal
    let empty_key = data_key(P, KIND_JSON, b"");
    let (kind, key, suffix) = decode_data_key(&empty_key, P.len()).unwrap();
    assert_eq!(
        (kind, key.as_slice(), suffix),
        (KIND_JSON, &b""[..], &b""[..])
    );
}

#[test]
fn envelope_leb128_edge_cases() {
    for expire in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
        let v = encode_envelope(expire, b"payload");
        let (got, payload) = decode_envelope(&v);
        assert_eq!(got, expire, "expire {expire}");
        assert_eq!(payload, b"payload");
    }
    assert_eq!(encode_envelope(0, b""), &[0]);
    assert_eq!(decode_envelope(&[0]), (0, &b""[..]));
    // continuation with no terminator -> empty payload, expire 0
    assert_eq!(decode_envelope(&[0x80]), (0, &b""[..]));
    // overlong 10-byte varint saturates
    let long = [0xffu8; 10];
    assert_eq!(decode_envelope(&long).0, u64::MAX);
}

#[test]
fn expire_index_key_roundtrip() {
    let dk = data_key(P, KIND_ZSET_META, b"z");
    let idx = expire_index_key(P, u64::MAX, &dk);
    let (expire, body) = decode_expire_index_key(&idx, P.len()).unwrap();
    assert_eq!(expire, u64::MAX);
    assert_eq!(body, &dk[P.len()..]);
    // non-index key rejected
    assert_eq!(decode_expire_index_key(&dk, P.len()), None);
}

#[test]
fn family_delete_ranges_confine_to_one_key() {
    let ranges = family_delete_ranges(P, HASH_FAMILY, b"k");
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0].0, data_key(P, KIND_HASH_META, b"k"));
    assert_eq!(ranges[1].0, data_key(P, KIND_HASH_FLD, b"k"));
    // k's field inside its range; the next key's field outside.
    let fld_k = elem_key(P, KIND_HASH_FLD, b"k", b"f");
    let fld_l = elem_key(P, KIND_HASH_FLD, b"l", b"f");
    let inside = |k: &[u8]| {
        ranges
            .iter()
            .any(|(lo, up)| lo.as_slice() <= k && k < up.as_slice())
    };
    assert!(inside(&fld_k));
    assert!(!inside(&fld_l));
    // REGRESSION (single-span bug): a LONGER key of a family kind used
    // to fall inside the family span; per-kind ranges exclude it.
    assert!(!inside(&data_key(P, KIND_HASH_META, b"kz")));
    assert!(!inside(&elem_key(P, KIND_HASH_FLD, b"kz", b"f")));
    // Other kinds for the same key are outside too.
    assert!(!inside(&data_key(P, KIND_STRING_TTL, b"k")));
    assert!(!inside(&data_key(P, KIND_LIST_META, b"k")));
}

#[test]
fn family_delete_ranges_upper_is_never_empty_for_typed_roots() {
    // key_upper_bound only returns None when EVERY byte is 0xff; a
    // typed root always contains an incrementable kind/len byte, so
    // the "to end" case is unreachable here (ops::delete_range still
    // treats an empty upper defensively).
    let ranges = family_delete_ranges(b"\xff", JSON_FAMILY, b"\xff\xff");
    assert_eq!(ranges.len(), 1);
    let (lower, upper) = &ranges[0];
    assert!(!lower.is_empty() && !upper.is_empty());
    assert!(lower < upper);
    let ranges = family_delete_ranges(P, JSON_FAMILY, b"\xff\xff");
    // carry across the 0xff user key lands on the length byte: 0x02 -> 0x03
    assert_eq!(ranges[0].1, b"70/\x10\x00\x00\x00\x03".to_vec());
    assert!(ranges[0].0 < ranges[0].1);
}

#[test]
fn classify_raw_vs_typed() {
    assert_eq!(classify(b"abc"), Classification::Raw);
    assert_eq!(classify(b""), Classification::Raw);
    assert_eq!(classify(&[0x13]), Classification::Raw); // unassigned, >= 0x13
    assert_eq!(classify(&[0x20]), Classification::Raw);
    assert_eq!(
        classify(&[KIND_HASH_META, 0, 0, 0, 1]),
        Classification::Typed(KIND_HASH_META)
    );
    assert_eq!(
        classify(&[KIND_VECTORSET_ELEM]),
        Classification::Typed(KIND_VECTORSET_ELEM)
    );
    assert_eq!(
        classify(&[KIND_EXPIRE_INDEX]),
        Classification::Typed(KIND_EXPIRE_INDEX)
    );
    // documented misread: raw string starting with a control byte
    assert_eq!(
        classify(&[0x01, b'x']),
        Classification::Typed(KIND_STRING_TTL)
    );
}

#[test]
fn string_key_is_legacy_layout() {
    assert_eq!(string_key(b"70/", b"k"), b"70/k".to_vec());
}

#[test]
fn count_varint_roundtrip_and_saturation() {
    for n in [0u64, 1, 127, 128, 300, 16_383, u32::MAX as u64, u64::MAX] {
        let enc = encode_count(n);
        assert_eq!(decode_count(&enc), n, "roundtrip {n}");
    }
    // truncated varint: value so far (no terminating byte)
    assert_eq!(decode_count(&[0x80]), 0);
    // overlong stream saturates instead of failing
    assert_eq!(decode_count(&[0xff; 12]), u64::MAX);
    // trailing junk after a terminated varint is ignored
    assert_eq!(decode_count(&[0x05, 0x00, 0x00]), 5);
}

#[test]
fn family_of_spans() {
    assert_eq!(family_of(KIND_STRING), None);
    assert_eq!(family_of(KIND_EXPIRE_INDEX), None);
    assert_eq!(family_of(KIND_HASH_FLD), Some(HASH_FAMILY));
    assert_eq!(family_of(KIND_STREAM_PEND), Some(STREAM_FAMILY));
    for kind in meta_kinds() {
        let (first, last) = family_of(*kind).unwrap();
        assert!(first <= *kind && *kind <= last);
    }
}
