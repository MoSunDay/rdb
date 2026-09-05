//! SETBIT / GETBIT / BITCOUNT handlers.

use super::*;

#[test]
fn setbit_getbit_roundtrip_across_byte_boundaries() {
    let (_guard, shared) = shared_for("127.0.0.1:43101");
    // Bit 0 is the MSB of byte 0 (Redis): 0x80 first, 0x01 last.
    assert_eq!(sbit(&shared, &[b"k", b"0", b"1"]), b":0\r\n");
    assert_eq!(stored(&shared, b"k"), b"\x80");
    assert_eq!(gbit(&shared, &[b"k", b"0"]), b":1\r\n");
    assert_eq!(gbit(&shared, &[b"k", b"7"]), b":0\r\n");
    assert_eq!(sbit(&shared, &[b"k", b"7", b"1"]), b":0\r\n");
    assert_eq!(stored(&shared, b"k"), b"\x81");
    // Crossing into byte 1 and byte 2 (offset 17 -> mask 0x40).
    assert_eq!(sbit(&shared, &[b"k", b"8", b"1"]), b":0\r\n");
    assert_eq!(stored(&shared, b"k"), b"\x81\x80");
    assert_eq!(gbit(&shared, &[b"k", b"8"]), b":1\r\n");
    assert_eq!(gbit(&shared, &[b"k", b"9"]), b":0\r\n");
    assert_eq!(sbit(&shared, &[b"k", b"17", b"1"]), b":0\r\n");
    assert_eq!(stored(&shared, b"k"), b"\x81\x80\x40");
    // Clearing returns the old bit.
    assert_eq!(sbit(&shared, &[b"k", b"7", b"0"]), b":1\r\n");
    assert_eq!(stored(&shared, b"k"), b"\x80\x80\x40");
    // Out-of-string and missing-key reads are zero bits.
    assert_eq!(gbit(&shared, &[b"k", b"1000"]), b":0\r\n");
    assert_eq!(gbit(&shared, &[b"absent", b"3"]), b":0\r\n");
    // Sparse offsets materialize offset/8 + 1 zero-filled bytes.
    assert_eq!(sbit(&shared, &[b"big", b"100000", b"1"]), b":0\r\n");
    assert_eq!(stored(&shared, b"big").len(), 12501);
    assert_eq!(gbit(&shared, &[b"big", b"100000"]), b":1\r\n");
    assert_eq!(gbit(&shared, &[b"big", b"99999"]), b":0\r\n");
    // Clearing past the end still extends (Redis >= 7).
    seed(&shared, b"s1", b"hello", None);
    assert_eq!(sbit(&shared, &[b"s1", b"100", b"0"]), b":0\r\n");
    assert_eq!(stored(&shared, b"s1").len(), 13);
    assert_eq!(
        stored(&shared, b"s1"),
        b"hello\x00\x00\x00\x00\x00\x00\x00\x00"
    );
}

#[test]
fn setbit_getbit_errors_and_arity() {
    let (_guard, shared) = shared_for("127.0.0.1:43102");
    let off = b"-ERR bit offset is not an integer or out of range\r\n";
    let bit = b"-ERR bit is not an integer or out of range\r\n";
    // Offset: non-integer, negative, and past the 2^32-1 cap.
    for arg in [b"zz".as_slice(), b"-1", b"4294967296"] {
        assert_eq!(sbit(&shared, &[b"k", arg, b"1"]), off, "{arg:?}");
        assert_eq!(gbit(&shared, &[b"k", arg]), off, "{arg:?}");
    }
    assert_eq!(sbit(&shared, &[b"k", b"4294967295", b"0"]), b":0\r\n");
    // Value must parse and be 0 or 1.
    for arg in [b"2".as_slice(), b"-1", b"zz"] {
        assert_eq!(sbit(&shared, &[b"k", b"7", arg]), bit, "{arg:?}");
    }
    assert_eq!(
        sbit(&shared, &[b"k", b"0"]),
        b"-ERR wrong number of arguments for 'setbit' command\r\n"
    );
    assert_eq!(
        gbit(&shared, &[b"k"]),
        b"-ERR wrong number of arguments for 'getbit' command\r\n"
    );
}

#[test]
fn setbit_preserves_ttl() {
    let (_guard, shared) = shared_for("127.0.0.1:43103");
    seed(&shared, b"t", b"abc", Some(b"100"));
    let before = expire_of(&shared, b"t");
    assert!(before > crate::ds::expire::now_ms());
    assert_eq!(sbit(&shared, &[b"t", b"3", b"1"]), b":0\r\n");
    let after = expire_of(&shared, b"t");
    assert!(
        after >= before.saturating_sub(2000) && after > 0,
        "TTL survived"
    );
}

#[test]
fn wrongtype_rejections() {
    let (_guard, shared) = shared_for("127.0.0.1:43104");
    call(
        &shared,
        |ctx| Box::pin(hash_cmd::hset(ctx)),
        &[b"{g}h", b"f", b"v"],
    );
    let wt = format!("-{}\r\n", hash_cmd::WRONGTYPE).into_bytes();
    assert_eq!(sbit(&shared, &[b"{g}h", b"0", b"1"]), wt);
    assert_eq!(gbit(&shared, &[b"{g}h", b"0"]), wt);
    assert_eq!(bcount(&shared, &[b"{g}h"]), wt);
    assert_eq!(bcount(&shared, &[b"{g}h", b"0", b"1"]), wt);
    assert_eq!(bpos(&shared, &[b"{g}h", b"0"]), wt);
    assert_eq!(bop(&shared, &[b"and", b"{g}d", b"{g}h"]), wt);
    assert!(!present(&shared, b"{g}d"), "no dest write after error");
}

#[test]
fn bitcount_full_negative_and_bit_mode() {
    let (_guard, shared) = shared_for("127.0.0.1:43105");
    seed(&shared, b"b", b"\xff\xf0\x00", None);
    assert_eq!(bcount(&shared, &[b"b"]), b":12\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"0", b"-1"]), b":12\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"0", b"0"]), b":8\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"1", b"1"]), b":4\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"-2", b"-1"]), b":4\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"-100", b"100"]), b":12\r\n");
    // Inverted / past-the-end ranges clamp to empty.
    assert_eq!(bcount(&shared, &[b"b", b"3", b"1"]), b":0\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"5", b"10"]), b":0\r\n");
    // BIT unit (case-insensitive tokens).
    assert_eq!(bcount(&shared, &[b"b", b"0", b"7", b"BIT"]), b":8\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"8", b"15", b"bit"]), b":4\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"8", b"-1", b"BIT"]), b":4\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"-8", b"-1", b"BIT"]), b":0\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"0", b"100", b"BIT"]), b":12\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"9", b"8", b"BIT"]), b":0\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"0", b"5", b"BIT"]), b":6\r\n");
    assert_eq!(bcount(&shared, &[b"b", b"0", b"-1", b"BYTE"]), b":12\r\n");
    // Missing keys reply 0 before any range parsing.
    assert_eq!(bcount(&shared, &[b"missing"]), b":0\r\n");
    assert_eq!(bcount(&shared, &[b"missing", b"0", b"junk"]), b":0\r\n");
    assert_eq!(
        bcount(&shared, &[b"missing", b"0", b"1", b"LIMIT", b"0", b"1"]),
        b":0\r\n"
    );
}

#[test]
fn bitcount_syntax_and_integer_errors() {
    let (_guard, shared) = shared_for("127.0.0.1:43106");
    seed(&shared, b"b", b"\xff", None);
    let syn = b"-ERR syntax error\r\n";
    assert_eq!(bcount(&shared, &[b"b", b"0"]), syn, "start-only rejected");
    assert_eq!(
        bcount(&shared, &[b"b", b"0", b"1", b"LIMIT", b"0", b"1"]),
        syn
    );
    assert_eq!(bcount(&shared, &[b"b", b"0", b"1", b"XY"]), syn);
    assert_eq!(
        bcount(&shared, &[b"b", b"0", b"x"]),
        b"-ERR value is not an integer or out of range\r\n"
    );
    assert_eq!(
        bcount(&shared, &[]),
        b"-ERR wrong number of arguments for 'bitcount' command\r\n"
    );
}
