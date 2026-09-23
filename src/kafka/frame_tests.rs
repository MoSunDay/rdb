use super::*;

#[test]
fn varints_roundtrip() {
    for v in [
        0i64,
        1,
        -1,
        63,
        64,
        -64,
        -65,
        300,
        -12345,
        i64::MAX,
        i64::MIN,
    ] {
        let mut buf = Vec::new();
        put_varint(&mut buf, v);
        assert_eq!(Reader::new(&buf).varint(), Some(v), "zigzag {v}");
    }
}

#[test]
fn uvarints_roundtrip() {
    for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
        let mut buf = Vec::new();
        put_uvarint(&mut buf, v);
        assert_eq!(Reader::new(&buf).uvarint(), Some(v), "uvarint {v}");
    }
}

#[test]
fn malformed_uvarint_is_none() {
    // Nine all-continuation bytes cannot decode into u64.
    let buf = [0xffu8; 9];
    assert_eq!(Reader::new(&buf).uvarint(), None);
}

#[test]
fn strings_classic_and_compact() {
    let mut buf = Vec::new();
    put_string(&mut buf, "hello");
    put_nullable_string(&mut buf, None);
    put_compact_string(&mut buf, "kafka");
    put_compact_nullable_string(&mut buf, None);
    let mut r = Reader::new(&buf);
    assert_eq!(r.string(), Some("hello".to_string()));
    assert_eq!(r.nullable_string(), Some(None));
    assert_eq!(r.compact_string(), Some("kafka".to_string()));
    assert_eq!(r.compact_nullable_string(), Some(None));
    assert_eq!(r.remaining(), 0);
    assert_eq!(r.string(), None, "truncated reads are None");
}

#[test]
fn arrays_classic_and_compact() {
    let mut buf = Vec::new();
    put_array_len(&mut buf, 3);
    put_null_array_len(&mut buf);
    put_compact_array_len(&mut buf, 3);
    let mut r = Reader::new(&buf);
    assert_eq!(r.array_len(), Some(Some(3)));
    assert_eq!(r.array_len(), Some(None));
    assert_eq!(r.compact_array_len(), Some(Some(3)));
}

#[test]
fn tagged_fields_roundtrip() {
    let mut buf = Vec::new();
    put_uvarint(&mut buf, 2); // 2 tags
    put_uvarint(&mut buf, 7); // tag 7
    put_uvarint(&mut buf, 3); // size 3
    buf.extend_from_slice(b"abc");
    put_uvarint(&mut buf, 300); // tag 300 (multibyte)
    put_uvarint(&mut buf, 0); // size 0
    let mut r = Reader::new(&buf);
    assert_eq!(r.skip_tagged_fields(), Some(()));
    assert_eq!(r.remaining(), 0);
    // Truncated tag body -> None, cursor stays consistent.
    let mut bad = Reader::new(&buf[..buf.len() - 1]);
    assert_eq!(bad.skip_tagged_fields(), None);
}

#[test]
fn header_v0_and_v2() {
    let mut plain = Vec::new();
    put_i16(&mut plain, 18);
    put_i16(&mut plain, 0);
    put_i32(&mut plain, 42);
    put_string(&mut plain, "client");
    let (h, body) = parse_req_header(&plain, false).unwrap();
    assert_eq!((h.api_key, h.api_version, h.correlation_id), (18, 0, 42));
    assert_eq!(h.client_id.as_deref(), Some("client"));
    assert_eq!(body.remaining(), 0);

    let mut flex = plain.clone();
    put_empty_tagged_fields(&mut flex);
    let (h2, body2) = parse_req_header(&flex, true).unwrap();
    assert_eq!(h2.correlation_id, 42);
    assert_eq!(body2.remaining(), 0);
    // Null client_id is legal.
    let mut nullid = Vec::new();
    put_i16(&mut nullid, 3);
    put_i16(&mut nullid, 0);
    put_i32(&mut nullid, 1);
    put_nullable_string(&mut nullid, None);
    let (h3, _) = parse_req_header(&nullid, false).unwrap();
    assert_eq!(h3.client_id, None);
}
