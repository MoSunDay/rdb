//! Positional/move/blocking list tests (linsert, lpos, lmove,
//! rpoplpush, blpop/brpop, blmove/brpoplpush) -- split from
//! `list_tests.rs` to keep files small; the harness lives there.

use super::{bulk_of, bulks_of, call, elems, int_of, set_raw, shared_for};

#[test]
fn linsert_before_after_and_errors() {
    let (_g, s) = shared_for("127.0.0.1:40420");
    call(&s, "rpush", &[b"k", b"a", b"b"]);
    assert_eq!(
        int_of(&call(&s, "linsert", &[b"k", b"BEFORE", b"b", b"x"])),
        3
    );
    assert_eq!(
        elems(&s, b"k"),
        vec![b"a".to_vec(), b"x".to_vec(), b"b".to_vec()]
    );
    assert_eq!(
        int_of(&call(&s, "linsert", &[b"k", b"after", b"b", b"y"])),
        4
    );
    assert_eq!(
        elems(&s, b"k"),
        vec![b"a".to_vec(), b"x".to_vec(), b"b".to_vec(), b"y".to_vec()]
    );
    assert_eq!(
        int_of(&call(&s, "linsert", &[b"none", b"BEFORE", b"b", b"x"])),
        0
    );
    assert_eq!(
        int_of(&call(&s, "linsert", &[b"k", b"BEFORE", b"zz", b"x"])),
        -1
    );
    assert_eq!(
        call(&s, "linsert", &[b"k", b"sideways", b"b", b"x"]),
        b"-ERR syntax error\r\n".to_vec()
    );
}

#[test]
fn lpos_rank_count_maxlen() {
    let (_g, s) = shared_for("127.0.0.1:40421");
    call(&s, "rpush", &[b"k", b"a", b"x", b"b", b"x", b"c", b"x"]);
    assert_eq!(int_of(&call(&s, "lpos", &[b"k", b"x"])), 1);
    assert_eq!(call(&s, "lpos", &[b"k", b"zz"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "lpos", &[b"none", b"x"]), b"$-1\r\n".to_vec());
    // Negative rank scans from the tail; positions stay logical.
    assert_eq!(int_of(&call(&s, "lpos", &[b"k", b"x", b"RANK", b"-1"])), 5);
    assert_eq!(int_of(&call(&s, "lpos", &[b"k", b"x", b"RANK", b"-2"])), 3);
    // COUNT turns the reply into an array of matches; COUNT 0 = all.
    let all = call(&s, "lpos", &[b"k", b"x", b"COUNT", b"0"]);
    assert_eq!(all, b"*3\r\n:1\r\n:3\r\n:5\r\n".to_vec());
    let two = call(&s, "lpos", &[b"k", b"x", b"COUNT", b"2"]);
    assert_eq!(two, b"*2\r\n:1\r\n:3\r\n".to_vec());
    // MAXLEN bounds the scan window from the starting end: 1 element
    // from the head holds only "a"; 2 reaches the first x; from the
    // tail the only element IS an x.
    assert_eq!(
        call(&s, "lpos", &[b"k", b"x", b"MAXLEN", b"1"]),
        b"$-1\r\n".to_vec()
    );
    assert_eq!(int_of(&call(&s, "lpos", &[b"k", b"x", b"MAXLEN", b"2"])), 1);
    assert_eq!(
        int_of(&call(
            &s,
            "lpos",
            &[b"k", b"x", b"MAXLEN", b"1", b"RANK", b"-1"]
        )),
        5
    );
    assert_eq!(
        call(&s, "lpos", &[b"k", b"x", b"RANK", b"0"]),
        b"-ERR RANK can't be zero\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "lpos", &[b"k", b"x", b"COUNT", b"-1"]),
        b"-ERR COUNT can't be negative\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "lpos", &[b"k", b"x", b"MAXLEN", b"-1"]),
        b"-ERR MAXLEN can't be negative\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "lpos", &[b"k", b"x", b"RANK"]),
        b"-ERR syntax error\r\n".to_vec()
    );
}

#[test]
fn lmove_directions_rotation_and_null() {
    let (_g, s) = shared_for("127.0.0.1:40422");
    call(&s, "rpush", &[b"{g}s", b"a", b"b"]);
    call(&s, "rpush", &[b"{g}d", b"z"]);
    // LEFT LEFT: head of src to head of dst.
    assert_eq!(
        bulk_of(&call(&s, "lmove", &[b"{g}s", b"{g}d", b"LEFT", b"LEFT"])),
        b"a".to_vec()
    );
    assert_eq!(elems(&s, b"{g}s"), vec![b"b".to_vec()]);
    assert_eq!(elems(&s, b"{g}d"), vec![b"a".to_vec(), b"z".to_vec()]);
    // RIGHT RIGHT: tail of src to tail of dst.
    assert_eq!(
        bulk_of(&call(&s, "lmove", &[b"{g}s", b"{g}d", b"RIGHT", b"RIGHT"])),
        b"b".to_vec()
    );
    assert_eq!(int_of(&call(&s, "exists", &[b"{g}s"])), 0);
    assert_eq!(
        elems(&s, b"{g}d"),
        vec![b"a".to_vec(), b"z".to_vec(), b"b".to_vec()]
    );
    // Empty source replies null without touching dst.
    assert_eq!(
        call(&s, "lmove", &[b"{g}s", b"{g}d", b"LEFT", b"LEFT"]),
        b"$-1\r\n".to_vec()
    );
    assert_eq!(elems(&s, b"{g}d").len(), 3);
    // Same-key rotation: pop RIGHT, push LEFT reverses the list.
    call(&s, "rpush", &[b"{g}r", b"1", b"2", b"3"]);
    assert_eq!(
        bulk_of(&call(&s, "lmove", &[b"{g}r", b"{g}r", b"RIGHT", b"LEFT"])),
        b"3".to_vec()
    );
    assert_eq!(
        elems(&s, b"{g}r"),
        vec![b"3".to_vec(), b"1".to_vec(), b"2".to_vec()]
    );
    assert_eq!(
        call(&s, "lmove", &[b"{g}r", b"{g}r", b"UP", b"LEFT"]),
        b"-ERR syntax error\r\n".to_vec()
    );
    // Destination holding a string fails with WRONGTYPE, source intact.
    set_raw(&s, b"{g}str", b"v");
    assert!(call(&s, "lmove", &[b"{g}r", b"{g}str", b"LEFT", b"LEFT"]).starts_with(b"-WRONGTYPE"));
    assert_eq!(elems(&s, b"{g}r").len(), 3);
}

#[test]
fn lmove_crossslot_and_rpoplpush() {
    let (_g, s) = shared_for("127.0.0.1:40423");
    assert_eq!(
        call(&s, "lmove", &[b"{x}s", b"{y}d", b"LEFT", b"RIGHT"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
    // RPOPLPUSH = LMOVE src dst RIGHT LEFT.
    call(&s, "rpush", &[b"{g}s", b"a", b"b"]);
    assert_eq!(
        bulk_of(&call(&s, "rpoplpush", &[b"{g}s", b"{g}d"])),
        b"b".to_vec()
    );
    assert_eq!(elems(&s, b"{g}s"), vec![b"a".to_vec()]);
    assert_eq!(elems(&s, b"{g}d"), vec![b"b".to_vec()]);
    assert_eq!(
        call(&s, "rpoplpush", &[b"{g}none", b"{g}d"]),
        b"$-1\r\n".to_vec()
    );
}

#[test]
fn blpop_immediate_and_multi_key_order() {
    let (_g, s) = shared_for("127.0.0.1:40424");
    call(&s, "rpush", &[b"{g}a", b"1"]);
    call(&s, "rpush", &[b"{g}b", b"2"]);
    // First non-empty key in argument order wins.
    let reply = call(&s, "blpop", &[b"{g}b", b"{g}a", b"0.1"]);
    assert_eq!(bulks_of(&reply), vec![b"{g}b".to_vec(), b"2".to_vec()]);
    assert_eq!(int_of(&call(&s, "llen", &[b"{g}b"])), 0);
    assert_eq!(int_of(&call(&s, "llen", &[b"{g}a"])), 1);
    let reply = call(&s, "blpop", &[b"{g}a", b"0.1"]);
    assert_eq!(bulks_of(&reply), vec![b"{g}a".to_vec(), b"1".to_vec()]);
}

#[test]
fn blpop_timeout_and_bad_timeout() {
    let (_g, s) = shared_for("127.0.0.1:40425");
    let started = std::time::Instant::now();
    assert_eq!(call(&s, "blpop", &[b"none", b"0.1"]), b"*-1\r\n".to_vec());
    assert!(started.elapsed() >= std::time::Duration::from_millis(90));
    assert_eq!(
        call(&s, "blpop", &[b"none", b"abc"]),
        b"-ERR timeout is not a float or out of range\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "blpop", &[b"none", b"-1"]),
        b"-ERR timeout is not a float or out of range\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "blpop", &[b"k"]),
        b"-ERR wrong number of arguments for 'blpop' command\r\n".to_vec()
    );
    set_raw(&s, b"str", b"v");
    assert!(call(&s, "blpop", &[b"str", b"0.01"]).starts_with(b"-WRONGTYPE"));
}

#[test]
fn blpop_crossslot() {
    let (_g, s) = shared_for("127.0.0.1:40426");
    assert_eq!(
        call(&s, "blpop", &[b"{x}a", b"{y}b", b"1"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
}

#[test]
fn brpop_immediate_and_timeout() {
    let (_g, s) = shared_for("127.0.0.1:40427");
    call(&s, "rpush", &[b"k", b"a", b"b"]);
    let reply = call(&s, "brpop", &[b"k", b"0.1"]);
    assert_eq!(bulks_of(&reply), vec![b"k".to_vec(), b"b".to_vec()]);
    assert_eq!(
        bulks_of(&call(&s, "brpop", &[b"k", b"0.1"])),
        vec![b"k".to_vec(), b"a".to_vec()]
    );
    // The drained key now times out.
    assert_eq!(call(&s, "brpop", &[b"k", b"0.05"]), b"*-1\r\n".to_vec());
}

#[test]
fn blmove_immediate_moves_element() {
    let (_g, s) = shared_for("127.0.0.1:40428");
    call(&s, "rpush", &[b"{g}s", b"a", b"b"]);
    assert_eq!(
        bulk_of(&call(
            &s,
            "blmove",
            &[b"{g}s", b"{g}d", b"LEFT", b"RIGHT", b"0.1"]
        )),
        b"a".to_vec()
    );
    assert_eq!(elems(&s, b"{g}s"), vec![b"b".to_vec()]);
    assert_eq!(elems(&s, b"{g}d"), vec![b"a".to_vec()]);
    // Rotation through the same key works too.
    assert_eq!(
        bulk_of(&call(
            &s,
            "blmove",
            &[b"{g}s", b"{g}s", b"LEFT", b"LEFT", b"0.1"]
        )),
        b"b".to_vec()
    );
    assert_eq!(elems(&s, b"{g}s"), vec![b"b".to_vec()]);
}

#[test]
fn blmove_timeout_brpoplpush_immediate() {
    let (_g, s) = shared_for("127.0.0.1:40429");
    assert_eq!(
        call(
            &s,
            "blmove",
            &[b"{g}s", b"{g}d", b"LEFT", b"RIGHT", b"0.05"]
        ),
        b"$-1\r\n".to_vec()
    );
    assert_eq!(
        call(
            &s,
            "blmove",
            &[b"{g}s", b"{g}d", b"SIDEWAYS", b"LEFT", b"0.1"]
        ),
        b"-ERR syntax error\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "blmove", &[b"{g}s", b"{g}d", b"LEFT", b"RIGHT", b"zz"]),
        b"-ERR timeout is not a float or out of range\r\n".to_vec()
    );
    // BRPOPLPUSH immediate: tail of src to head of dst.
    call(&s, "rpush", &[b"{g}src", b"1", b"2"]);
    assert_eq!(
        bulk_of(&call(&s, "brpoplpush", &[b"{g}src", b"{g}dst", b"0.1"])),
        b"2".to_vec()
    );
    assert_eq!(elems(&s, b"{g}src"), vec![b"1".to_vec()]);
    assert_eq!(elems(&s, b"{g}dst"), vec![b"2".to_vec()]);
    assert_eq!(
        call(&s, "brpoplpush", &[b"{g}none", b"{g}dst", b"0.05"]),
        b"$-1\r\n".to_vec()
    );
}

#[test]
fn blmove_wrongtype_dst_precheck_keeps_src() {
    let (_g, s) = shared_for("127.0.0.1:40430");
    set_raw(&s, b"{g}dst", b"x");
    call(&s, "rpush", &[b"{g}src", b"a"]);
    // The dst precheck fires BEFORE the pop: WRONGTYPE, src untouched.
    assert!(call(
        &s,
        "blmove",
        &[b"{g}src", b"{g}dst", b"LEFT", b"LEFT", b"0.1"]
    )
    .starts_with(b"-WRONGTYPE"));
    assert_eq!(elems(&s, b"{g}src"), vec![b"a".to_vec()]);
    // BRPOPLPUSH shares the precheck: the tail element survives too.
    call(&s, "rpush", &[b"{g}src", b"b"]);
    assert!(call(&s, "brpoplpush", &[b"{g}src", b"{g}dst", b"0.1"]).starts_with(b"-WRONGTYPE"));
    assert_eq!(elems(&s, b"{g}src"), vec![b"a".to_vec(), b"b".to_vec()]);
    // A wrong-type SRC still surfaces WRONGTYPE through the pop itself.
    set_raw(&s, b"{g}str", b"y");
    assert!(call(
        &s,
        "blmove",
        &[b"{g}str", b"{g}dst", b"LEFT", b"LEFT", b"0.1"]
    )
    .starts_with(b"-WRONGTYPE"));
}
