//! BITPOS / BITOP handlers.

use super::*;

#[test]
fn bitpos_documented_rules() {
    let (_guard, shared) = shared_for("127.0.0.1:43107");
    seed(&shared, b"b", b"\xff\xf0\x00", None);
    // Rule (b): bit=0, no range, first zero of ff f0 00.
    assert_eq!(bpos(&shared, &[b"b", b"0"]), b":12\r\n");
    assert_eq!(bpos(&shared, &[b"b", b"1"]), b":0\r\n");
    assert_eq!(bpos(&shared, &[b"b", b"0", b"0", b"-1"]), b":12\r\n");
    // Rule (d): explicit end confines the search — an all-ones window
    // has no zero bit even when it reaches the string end.
    assert_eq!(bpos(&shared, &[b"b", b"0", b"0", b"0"]), b":-1\r\n");
    assert_eq!(bpos(&shared, &[b"b", b"0", b"1", b"1"]), b":12\r\n");
    assert_eq!(bpos(&shared, &[b"b", b"1", b"2", b"2"]), b":-1\r\n");
    assert_eq!(bpos(&shared, &[b"b", b"1", b"-1", b"-1"]), b":-1\r\n");
    // Start-only form: `end` defaults to the string end.
    assert_eq!(bpos(&shared, &[b"b", b"0", b"2"]), b":16\r\n");
    assert_eq!(bpos(&shared, &[b"b", b"1", b"1"]), b":8\r\n");
    // All-ones strings: total bit count without an end, -1 with one.
    seed(&shared, b"ones", b"\xff\xff", None);
    assert_eq!(bpos(&shared, &[b"ones", b"0"]), b":16\r\n");
    assert_eq!(bpos(&shared, &[b"ones", b"1"]), b":0\r\n");
    assert_eq!(bpos(&shared, &[b"ones", b"0", b"0", b"1"]), b":-1\r\n");
    assert_eq!(bpos(&shared, &[b"ones", b"0", b"0", b"-1"]), b":-1\r\n");
    assert_eq!(bpos(&shared, &[b"ones", b"0", b"0"]), b":16\r\n");
    assert_eq!(bpos(&shared, &[b"ones", b"0", b"1"]), b":16\r\n");
    // Rules (a)/(c): missing keys answer before any range parsing.
    assert_eq!(bpos(&shared, &[b"miss", b"0"]), b":0\r\n");
    assert_eq!(bpos(&shared, &[b"miss", b"1"]), b":-1\r\n");
    assert_eq!(bpos(&shared, &[b"miss", b"0", b"0", b"10"]), b":0\r\n");
    assert_eq!(bpos(&shared, &[b"miss", b"1", b"5", b"9"]), b":-1\r\n");
    assert_eq!(
        bpos(&shared, &[b"miss", b"0", b"garbage", b"args"]),
        b":0\r\n"
    );
    // Existing empty strings have no bits at all.
    seed(&shared, b"empty", b"", None);
    assert_eq!(bpos(&shared, &[b"empty", b"0"]), b":-1\r\n");
    assert_eq!(bpos(&shared, &[b"empty", b"1"]), b":-1\r\n");
    // BIT-unit windows (edge masking inside the boundary bytes).
    assert_eq!(bpos(&shared, &[b"b", b"0", b"0", b"7", b"BIT"]), b":-1\r\n");
    assert_eq!(
        bpos(&shared, &[b"b", b"0", b"8", b"15", b"BIT"]),
        b":12\r\n"
    );
    assert_eq!(bpos(&shared, &[b"b", b"1", b"0", b"7", b"BIT"]), b":0\r\n");
    assert_eq!(
        bpos(&shared, &[b"b", b"0", b"12", b"12", b"BIT"]),
        b":12\r\n"
    );
    assert_eq!(
        bpos(&shared, &[b"b", b"0", b"6", b"23", b"BIT"]),
        b":12\r\n"
    );
    assert_eq!(
        bpos(&shared, &[b"b", b"1", b"24", b"30", b"BIT"]),
        b":-1\r\n"
    );
    assert_eq!(
        bpos(&shared, &[b"ones", b"0", b"0", b"15", b"BIT"]),
        b":-1\r\n"
    );
}

#[test]
fn bitpos_errors() {
    let (_guard, shared) = shared_for("127.0.0.1:43108");
    seed(&shared, b"b", b"\xff", None);
    assert_eq!(
        bpos(&shared, &[b"b", b"2"]),
        b"-ERR The bit argument must be 1 or 0.\r\n"
    );
    assert_eq!(
        bpos(&shared, &[b"b", b"x"]),
        b"-ERR value is not an integer or out of range\r\n"
    );
    assert_eq!(
        bpos(&shared, &[b"b", b"0", b"a", b"b"]),
        b"-ERR value is not an integer or out of range\r\n"
    );
    assert_eq!(
        bpos(&shared, &[b"b", b"0", b"1", b"2", b"3"]),
        b"-ERR syntax error\r\n"
    );
    assert_eq!(
        bpos(&shared, &[b"b", b"0", b"1", b"BITX"]),
        b"-ERR value is not an integer or out of range\r\n"
    );
    // In the 5-argc shape the 4th arg is `end`, so a BYTE token there is
    // an integer error (BITPOS's unit token needs all 6 args).
    assert_eq!(
        bpos(&shared, &[b"b", b"0", b"1", b"BYTE"]),
        b"-ERR value is not an integer or out of range\r\n"
    );
    assert_eq!(
        bpos(&shared, &[b"b"]),
        b"-ERR wrong number of arguments for 'bitpos' command\r\n"
    );
}

#[test]
fn bitop_algebra_with_different_lengths() {
    let (_guard, shared) = shared_for("127.0.0.1:43109");
    seed(&shared, b"{g}a", b"\xff\x0f", None);
    seed(&shared, b"{g}s", b"\xf0\xf0", None);
    seed(&shared, b"{g}t", b"\xf0", None);
    for (op, want, len) in [
        ("and", b"\xf0\x00".as_slice(), b":2\r\n".as_slice()),
        ("or", b"\xff\xff", b":2\r\n"),
        ("xor", b"\x0f\xff", b":2\r\n"),
    ] {
        assert_eq!(
            bop(&shared, &[op.as_bytes(), b"{g}d", b"{g}a", b"{g}s"]),
            len
        );
        assert_eq!(stored(&shared, b"{g}d"), want, "{op}");
    }
    assert_eq!(bop(&shared, &[b"NOT", b"{g}d", b"{g}a"]), b":2\r\n");
    assert_eq!(stored(&shared, b"{g}d"), b"\x00\xf0");
    // Mixed lengths pad the shorter source with zero bytes.
    assert_eq!(
        bop(&shared, &[b"and", b"{g}d", b"{g}a", b"{g}t"]),
        b":2\r\n"
    );
    assert_eq!(stored(&shared, b"{g}d"), b"\xf0\x00");
    assert_eq!(bop(&shared, &[b"or", b"{g}d", b"{g}a", b"{g}t"]), b":2\r\n");
    assert_eq!(stored(&shared, b"{g}d"), b"\xff\x0f");
    assert_eq!(
        bop(&shared, &[b"xor", b"{g}d", b"{g}a", b"{g}t"]),
        b":2\r\n"
    );
    assert_eq!(stored(&shared, b"{g}d"), b"\x0f\x0f");
    // All-empty result deletes the destination.
    seed(&shared, b"{g}d", b"stale", None);
    assert_eq!(
        bop(&shared, &[b"AND", b"{g}d", b"{g}no1", b"{g}no2"]),
        b":0\r\n"
    );
    assert!(!present(&shared, b"{g}d"));
}

#[test]
fn bitop_dest_overwrites_hash_and_clears_ttl() {
    let (_guard, shared) = shared_for("127.0.0.1:43110");
    seed(&shared, b"{g}src", b"\x0f", None);
    // Destination is a hash: BITOP overwrites it with a string (real
    // Redis setKey semantics — WRONGTYPE applies to sources only).
    call(
        &shared,
        |ctx| Box::pin(hash_cmd::hset(ctx)),
        &[b"{g}dh", b"f", b"v"],
    );
    assert_eq!(bop(&shared, &[b"not", b"{g}dh", b"{g}src"]), b":1\r\n");
    assert_eq!(stored(&shared, b"{g}dh"), b"\xf0");
    // Destination TTL is dropped.
    seed(&shared, b"{g}dt", b"zz", Some(b"100"));
    assert!(expire_of(&shared, b"{g}dt") > 0);
    assert_eq!(bop(&shared, &[b"or", b"{g}dt", b"{g}src"]), b":1\r\n");
    assert_eq!(expire_of(&shared, b"{g}dt"), 0);
}

#[test]
fn bitop_arity_crossslot_and_bad_op() {
    let (_guard, shared) = shared_for("127.0.0.1:43111");
    assert_eq!(
        bop(&shared, &[b"and", b"{g}d"]),
        b"-ERR wrong number of arguments for 'bitop' command\r\n"
    );
    assert_eq!(
        bop(&shared, &[b"not", b"{g}d", b"{g}a", b"{g}s"]),
        b"-ERR BITOP NOT must be called with a single source key.\r\n"
    );
    assert_eq!(
        bop(&shared, &[b"nand", b"{g}d", b"{g}a"]),
        b"-ERR syntax error\r\n"
    );
    let crossslot = format!("-{}\r\n", crate::ds::setops::CROSSSLOT_ERROR).into_bytes();
    assert_eq!(bop(&shared, &[b"and", b"{a}d", b"{b}s"]), crossslot);
    assert!(!present(&shared, b"{a}d"), "no dest write on cross-slot");
}
