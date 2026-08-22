//! Handler-level tests for the vector-set commands: crate-wide store
//! lock, fresh `Shared` per test, dispatch through the command registry
//! (`command::lookup`) so the registered names are exercised too.

use crate::command::test_ctx;
use crate::command::Handler;
use crate::state::{testutil, Shared};

pub(super) const PREFIX: &[u8] = b"70/";

pub(super) fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
    let guard = crate::command::string::TEST_STORE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut conf = testutil::test_config();
    conf.bind = bind.to_string();
    (guard, testutil::shared_with(conf))
}

pub(super) fn call(shared: &Shared, name: &str, args: &[&[u8]]) -> Vec<u8> {
    let handler: Handler =
        crate::command::lookup(name).unwrap_or_else(|| panic!("{name} registered"));
    let mut out = Vec::new();
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let mut ctx = test_ctx(shared, PREFIX.to_vec(), argv, &mut out);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(handler(&mut ctx));
    out
}

pub(super) fn int_of(reply: &[u8]) -> i64 {
    let text = String::from_utf8(reply.to_vec()).unwrap();
    text.trim_start_matches(':').trim_end().parse().unwrap()
}

#[test]
fn vadd_values_creates_readd_replaces_vector() {
    let (_g, s) = shared_for("127.0.0.1:40761");
    assert_eq!(
        call(&s, "vadd", &[b"k", b"VALUES", b"2", b"e", b"1", b"0"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(int_of(&call(&s, "vcard", &[b"k"])), 1);
    assert_eq!(int_of(&call(&s, "vdim", &[b"k"])), 2);
    // Exact-match query scores 1.
    assert_eq!(
        call(&s, "vsim", &[b"k", b"VALUES", b"1", b"0"]),
        b"*1\r\n$1\r\ne\r\n".to_vec()
    );
    // Re-add same element: :0, count unchanged, vector REPLACED.
    assert_eq!(
        call(&s, "vadd", &[b"k", b"VALUES", b"2", b"e", b"0", b"1"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(int_of(&call(&s, "vcard", &[b"k"])), 1);
    // Now orthogonal to the old query: score 0.5.
    assert_eq!(
        call(&s, "vsim", &[b"k", b"WITHSCORES", b"VALUES", b"1", b"0"]),
        b"*2\r\n$1\r\ne\r\n$3\r\n0.5\r\n".to_vec()
    );
    // A second element bumps the count.
    assert_eq!(
        call(&s, "vadd", &[b"k", b"VALUES", b"2", b"f", b"1", b"1"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(int_of(&call(&s, "vcard", &[b"k"])), 2);
}

#[test]
fn vadd_fp16_blob_and_attribute_preservation() {
    let (_g, s) = shared_for("127.0.0.1:40762");
    // [1.0, 0.0] as LE u16 halves: 3C00 0000 -> bytes 00 3C 00 00.
    assert_eq!(
        call(
            &s,
            "vadd",
            &[b"k", b"FP16", b"2", b"e", &[0x00, 0x3C, 0x00, 0x00]]
        ),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(
            &s,
            "vsim",
            &[b"k", b"WITHSCORES", b"FP16", &[0x00, 0x3C, 0x00, 0x00]]
        ),
        b"*2\r\n$1\r\ne\r\n$1\r\n1\r\n".to_vec()
    );
    // Mode keyword is case-insensitive.
    assert_eq!(
        call(
            &s,
            "vadd",
            &[b"k", b"fp16", b"2", b"f", &[0x00, 0x3C, 0x00, 0x00]]
        ),
        b":1\r\n".to_vec()
    );
    // Re-VADD keeps an existing attribute...
    call(&s, "vsetattr", &[b"k", b"e", b"year=2026"]);
    assert_eq!(
        call(&s, "vadd", &[b"k", b"VALUES", b"2", b"e", b"0", b"1"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vgetattr", &[b"k", b"e"]),
        b"$9\r\nyear=2026\r\n".to_vec()
    );
    // ...and its vector still moved (orthogonal to the FP16 insert);
    // COUNT parses before VALUES because VALUES swallows the tail.
    assert_eq!(
        call(
            &s,
            "vsim",
            &[b"k", b"WITHSCORES", b"COUNT", b"1", b"VALUES", b"1", b"0"]
        ),
        b"*2\r\n$1\r\nf\r\n$1\r\n1\r\n".to_vec()
    );
}

#[test]
fn vadd_argument_errors() {
    let (_g, s) = shared_for("127.0.0.1:40763");
    call(&s, "vadd", &[b"k", b"VALUES", b"2", b"e", b"1", b"0"]);
    // Dimension must parse and sit in 1..=4096.
    for bad in [b"0".as_slice(), b"4097", b"xyz"] {
        assert_eq!(
            call(&s, "vadd", &[b"k2", b"VALUES", bad, b"e", b"1"]),
            b"-ERR invalid dim\r\n".to_vec()
        );
    }
    // Existing set disagrees with the given dim.
    assert_eq!(
        call(&s, "vadd", &[b"k", b"VALUES", b"3", b"e", b"1", b"0", b"0"]),
        b"-ERR dimension mismatch\r\n".to_vec()
    );
    assert_eq!(
        call(
            &s,
            "vadd",
            &[
                b"k",
                b"FP16",
                b"3",
                b"e",
                &[0x00, 0x3C, 0x00, 0x3C, 0x00, 0x3C]
            ]
        ),
        b"-ERR dimension mismatch\r\n".to_vec()
    );
    // VALUES arity / parse failures.
    assert_eq!(
        call(&s, "vadd", &[b"k", b"VALUES", b"2", b"e", b"1"]),
        b"-ERR invalid vector value\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vadd", &[b"k", b"VALUES", b"2", b"e", b"1", b"x"]),
        b"-ERR invalid vector value\r\n".to_vec()
    );
    // FP16 blob must be exactly dim*2 bytes.
    assert_eq!(
        call(&s, "vadd", &[b"k", b"FP16", b"2", b"e", &[0x00, 0x3C]]),
        b"-ERR invalid FP16 vector\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vadd", &[b"k", b"QUANT", b"2", b"e", b"1", b"0"]),
        b"-ERR syntax error\r\n".to_vec()
    );
    // Foreign kind (raw string) and arity.
    call(&s, "set", &[b"raw", b"x"]);
    assert_eq!(
        call(&s, "vadd", &[b"raw", b"VALUES", b"2", b"e", b"1", b"0"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vadd", &[b"k", b"VALUES", b"2", b"e"]),
        b"-ERR wrong number of arguments for 'vadd' command\r\n".to_vec()
    );
}

#[test]
fn vrem_and_last_element_family_delete() {
    let (_g, s) = shared_for("127.0.0.1:40764");
    call(&s, "vadd", &[b"k", b"VALUES", b"1", b"a", b"1"]);
    call(&s, "vadd", &[b"k", b"VALUES", b"1", b"b", b"1"]);
    assert_eq!(call(&s, "vrem", &[b"k", b"a"]), b":1\r\n".to_vec());
    assert_eq!(int_of(&call(&s, "vcard", &[b"k"])), 1);
    assert_eq!(call(&s, "vrem", &[b"k", b"a"]), b":0\r\n".to_vec());
    assert_eq!(call(&s, "vrem", &[b"missing", b"a"]), b":0\r\n".to_vec());
    // Removing the last element drops the key entirely.
    assert_eq!(call(&s, "vrem", &[b"k", b"b"]), b":1\r\n".to_vec());
    assert_eq!(int_of(&call(&s, "vcard", &[b"k"])), 0);
    assert_eq!(call(&s, "exists", &[b"k"]), b":0\r\n".to_vec());
    call(&s, "set", &[b"raw", b"x"]);
    assert_eq!(
        call(&s, "vrem", &[b"raw", b"a"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
}

#[test]
fn vcard_vdim_missing_key() {
    let (_g, s) = shared_for("127.0.0.1:40765");
    assert_eq!(int_of(&call(&s, "vcard", &[b"nope"])), 0);
    assert_eq!(
        call(&s, "vdim", &[b"nope"]),
        b"-ERR vector set does not exist\r\n".to_vec()
    );
    call(&s, "vadd", &[b"k", b"VALUES", b"3", b"e", b"1", b"2", b"3"]);
    assert_eq!(int_of(&call(&s, "vdim", &[b"k"])), 3);
    call(&s, "set", &[b"raw", b"x"]);
    assert_eq!(
        call(&s, "vdim", &[b"raw"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
}

#[test]
fn vsetattr_vgetattr_roundtrip_and_clear() {
    let (_g, s) = shared_for("127.0.0.1:40766");
    call(&s, "vadd", &[b"k", b"VALUES", b"1", b"e", b"1"]);
    // No attribute yet: null bulk (same as a missing element).
    assert_eq!(call(&s, "vgetattr", &[b"k", b"e"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "vgetattr", &[b"k", b"zz"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "vgetattr", &[b"nope", b"e"]), b"$-1\r\n".to_vec());
    assert_eq!(
        call(&s, "vsetattr", &[b"k", b"e", b"clr=blue"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vgetattr", &[b"k", b"e"]),
        b"$8\r\nclr=blue\r\n".to_vec()
    );
    // Vector untouched by attribute rewrites.
    assert_eq!(
        call(&s, "vsim", &[b"k", b"WITHSCORES", b"VALUES", b"1"]),
        b"*2\r\n$1\r\ne\r\n$1\r\n1\r\n".to_vec()
    );
    // Missing element / missing key -> :0.
    assert_eq!(
        call(&s, "vsetattr", &[b"k", b"zz", b"x"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vsetattr", &[b"nope", b"e", b"x"]),
        b":0\r\n".to_vec()
    );
    // Empty string clears back to null bulk.
    assert_eq!(call(&s, "vsetattr", &[b"k", b"e", b""]), b":1\r\n".to_vec());
    assert_eq!(call(&s, "vgetattr", &[b"k", b"e"]), b"$-1\r\n".to_vec());
    call(&s, "set", &[b"raw", b"x"]);
    assert_eq!(
        call(&s, "vsetattr", &[b"raw", b"e", b"x"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
}

#[test]
fn vsim_ranking_count_and_flags() {
    let (_g, s) = shared_for("127.0.0.1:40767");
    call(&s, "vadd", &[b"k", b"VALUES", b"2", b"a", b"1", b"0"]);
    call(&s, "vadd", &[b"k", b"VALUES", b"2", b"b", b"0", b"1"]);
    call(&s, "vadd", &[b"k", b"VALUES", b"2", b"c", b"1", b"1"]);
    // a (cos 1) > c (cos 1/sqrt2) > b (cos 0, orthogonal -> 0.5).
    let mid = format!("{}", (1.0f64 / 2.0f64.sqrt() + 1.0) / 2.0);
    let want = "*3\r\n$1\r\na\r\n$1\r\nc\r\n$1\r\nb\r\n";
    assert_eq!(
        call(&s, "vsim", &[b"k", b"VALUES", b"1", b"0"]),
        want.as_bytes()
    );
    let scored = format!(
        "*6\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nc\r\n${}\r\n{}\r\n$1\r\nb\r\n$3\r\n0.5\r\n",
        mid.len(),
        mid
    );
    assert_eq!(
        call(&s, "vsim", &[b"k", b"WITHSCORES", b"VALUES", b"1", b"0"]),
        scored.as_bytes().to_vec()
    );
    // COUNT truncates; options parse in any order before the vector.
    assert_eq!(
        call(&s, "vsim", &[b"k", b"COUNT", b"1", b"VALUES", b"1", b"0"]),
        b"*1\r\n$1\r\na\r\n".to_vec()
    );
    assert_eq!(
        call(
            &s,
            "vsim",
            &[b"k", b"COUNT", b"1", b"WITHSCORES", b"VALUES", b"1", b"0"]
        ),
        b"*2\r\n$1\r\na\r\n$1\r\n1\r\n".to_vec()
    );
    // Zero-norm query: every cosine is 0, every score 0.5; ties break
    // by element byte order.
    assert_eq!(
        call(&s, "vsim", &[b"k", b"WITHSCORES", b"VALUES", b"0", b"0"]),
        b"*6\r\n$1\r\na\r\n$3\r\n0.5\r\n$1\r\nb\r\n$3\r\n0.5\r\n$1\r\nc\r\n$3\r\n0.5\r\n".to_vec()
    );
}

#[test]
fn vsim_attribs_and_errors() {
    let (_g, s) = shared_for("127.0.0.1:40768");
    call(&s, "vadd", &[b"k", b"VALUES", b"1", b"a", b"1"]);
    call(&s, "vadd", &[b"k", b"VALUES", b"1", b"b", b"1"]);
    call(&s, "vsetattr", &[b"k", b"b", b"tag"]);
    assert_eq!(
        call(&s, "vsim", &[b"k", b"WITHATTRIBS", b"VALUES", b"1"]),
        b"*4\r\n$1\r\na\r\n$-1\r\n$1\r\nb\r\n$3\r\ntag\r\n".to_vec()
    );
    assert_eq!(
        call(
            &s,
            "vsim",
            &[b"k", b"WITHATTRIBS", b"WITHSCORES", b"VALUES", b"1"]
        ),
        b"*6\r\n$1\r\na\r\n$-1\r\n$1\r\n1\r\n$1\r\nb\r\n$3\r\ntag\r\n$1\r\n1\r\n".to_vec()
    );
    // Query arity must match dim; unparsable values error; a missing
    // vector spec, unknown option and missing key are errors too.
    assert_eq!(
        call(&s, "vsim", &[b"k", b"VALUES", b"1", b"2"]),
        b"-ERR invalid vector value\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vsim", &[b"k", b"VALUES", b"x"]),
        b"-ERR invalid vector value\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vsim", &[b"k", b"FP16", b"123"]),
        b"-ERR invalid FP16 vector\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vsim", &[b"k", b"WITHSCORES"]),
        b"-ERR wrong number of arguments for 'vsim' command\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vsim", &[b"k", b"EF", b"10", b"VALUES", b"1"]),
        b"-ERR wrong number of arguments for 'vsim' command\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vsim", &[b"k", b"COUNT", b"zz", b"VALUES", b"1"]),
        b"-ERR invalid COUNT\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "vsim", &[b"nope", b"VALUES", b"1"]),
        b"-ERR vector set does not exist\r\n".to_vec()
    );
    call(&s, "set", &[b"raw", b"x"]);
    assert_eq!(
        call(&s, "vsim", &[b"raw", b"VALUES", b"1"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
}

#[test]
fn ttl_survives_vadd_and_expires() {
    let (_g, s) = shared_for("127.0.0.1:40769");
    call(&s, "vadd", &[b"k", b"VALUES", b"1", b"e", b"1"]);
    // PEXPIREAT migrates the family into the enveloped TTL shape; later
    // VADDs must KEEP the deadline (not reset it to 0).
    assert_eq!(call(&s, "pexpireat", &[b"k", b"9999999999999"]), b":1\r\n");
    assert_eq!(
        call(&s, "vadd", &[b"k", b"VALUES", b"1", b"f", b"1"]),
        b":1\r\n".to_vec()
    );
    let ttl = call(&s, "ttl", &[b"k"]);
    let secs: i64 = std::str::from_utf8(&ttl[1..ttl.len() - 2])
        .unwrap()
        .parse()
        .unwrap();
    assert!(secs > 1_000_000_000, "ttl lost after vadd: {secs}");
    // A past deadline lazily purges the whole family on the next read.
    assert_eq!(call(&s, "pexpireat", &[b"k", b"1"]), b":1\r\n");
    assert_eq!(int_of(&call(&s, "vcard", &[b"k"])), 0);
    assert_eq!(call(&s, "exists", &[b"k"]), b":0\r\n".to_vec());
    assert_eq!(
        call(&s, "vsim", &[b"k", b"VALUES", b"1"]),
        b"-ERR vector set does not exist\r\n".to_vec()
    );
}
