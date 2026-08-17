//! Handler-level tests for the json array/object commands and the
//! expire-envelope interplay. Harness (`shared_for`/`call`/reply
//! helpers) is shared with `json_tests`, the crate-wide store lock
//! serializes RocksDB access across both files.

use super::json_tests::{bulk_of, call, int_of, shared_for, PREFIX};

#[test]
fn arrappend_and_arrpop() {
    let (_g, s) = shared_for("127.0.0.1:40708");
    call(&s, "json.set", &[b"k", b".", b"[1]"]);
    assert_eq!(
        int_of(&call(&s, "json.arrappend", &[b"k", b".", b"2", b"3"])),
        3
    );
    assert_eq!(bulk_of(&call(&s, "json.arrpop", &[b"k"])), b"3".to_vec());
    assert_eq!(
        bulk_of(&call(&s, "json.arrpop", &[b"k", b".", b"0"])),
        b"1".to_vec()
    );
    call(&s, "json.set", &[b"k", b".", b"[1,2,3]"]);
    assert_eq!(
        bulk_of(&call(&s, "json.arrpop", &[b"k", b".", b"-2"])),
        b"2".to_vec()
    );
    assert_eq!(
        call(&s, "json.arrpop", &[b"k", b".", b"9"]),
        b"-ERR index out of range\r\n".to_vec()
    );
    call(&s, "json.set", &[b"e", b".", b"[]"]);
    assert_eq!(call(&s, "json.arrpop", &[b"e"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "json.arrpop", &[b"gone"]), b"$-1\r\n".to_vec());
    call(&s, "json.set", &[b"o", b".", b"{}"]);
    assert_eq!(
        call(&s, "json.arrpop", &[b"o"]),
        b"-ERR wrong type of path value\r\n".to_vec()
    );
}

#[test]
fn arrindex_window_semantics() {
    let (_g, s) = shared_for("127.0.0.1:40709");
    call(&s, "json.set", &[b"k", b".", b"{}"]);
    call(&s, "json.set", &[b"k", b".a", b"[1,2,3,2,1]"]);
    assert_eq!(int_of(&call(&s, "json.arrindex", &[b"k", b".a", b"2"])), 1);
    assert_eq!(int_of(&call(&s, "json.arrindex", &[b"k", b".a", b"9"])), -1);
    // stop is exclusive: [1,2) holds only index 1.
    assert_eq!(
        int_of(&call(&s, "json.arrindex", &[b"k", b".a", b"2", b"0", b"2"])),
        1
    );
    assert_eq!(
        int_of(&call(&s, "json.arrindex", &[b"k", b".a", b"2", b"2", b"2"])),
        -1
    );
    // negative start counts from the end; stop -1 = through the end.
    assert_eq!(
        int_of(&call(&s, "json.arrindex", &[b"k", b".a", b"1", b"-2"])),
        4
    );
    assert_eq!(
        int_of(&call(
            &s,
            "json.arrindex",
            &[b"k", b".a", b"1", b"0", b"-1"]
        )),
        0
    );
    assert_eq!(
        int_of(&call(
            &s,
            "json.arrindex",
            &[b"k", b".a", b"1", b"1", b"-2"]
        )),
        -1
    );
    assert_eq!(
        int_of(&call(&s, "json.arrindex", &[b"gone", b".a", b"1"])),
        -1
    );
    assert_eq!(
        call(&s, "json.arrindex", &[b"k", b".zzz", b"1"]),
        b":-1\r\n".to_vec()
    );
}

#[test]
fn arrinsert_positions() {
    let (_g, s) = shared_for("127.0.0.1:40710");
    call(&s, "json.set", &[b"k", b".", b"[1,4]"]);
    assert_eq!(
        int_of(&call(&s, "json.arrinsert", &[b"k", b".", b"1", b"2", b"3"])),
        4
    );
    assert_eq!(
        bulk_of(&call(&s, "json.get", &[b"k"])),
        b"[1,2,3,4]".to_vec()
    );
    assert_eq!(
        int_of(&call(&s, "json.arrinsert", &[b"k", b".", b"4", b"5"])),
        5
    );
    assert_eq!(
        int_of(&call(&s, "json.arrinsert", &[b"k", b".", b"-1", b"0"])),
        6
    );
    assert_eq!(
        bulk_of(&call(&s, "json.get", &[b"k"])),
        b"[1,2,3,4,0,5]".to_vec()
    );
    assert_eq!(
        call(&s, "json.arrinsert", &[b"k", b".", b"99", b"0"]),
        b"-ERR index out of range\r\n".to_vec()
    );
    assert_eq!(int_of(&call(&s, "json.arrlen", &[b"k"])), 6);
    assert_eq!(call(&s, "json.arrlen", &[b"gone"]), b"$-1\r\n".to_vec());
}

#[test]
fn arrtrim_inclusive_windows() {
    let (_g, s) = shared_for("127.0.0.1:40711");
    call(&s, "json.set", &[b"k", b".", b"[0,1,2,3,4]"]);
    assert_eq!(
        int_of(&call(&s, "json.arrtrim", &[b"k", b".", b"1", b"3"])),
        3
    );
    assert_eq!(bulk_of(&call(&s, "json.get", &[b"k"])), b"[1,2,3]".to_vec());
    assert_eq!(
        int_of(&call(&s, "json.arrtrim", &[b"k", b".", b"-2", b"-1"])),
        2
    );
    assert_eq!(bulk_of(&call(&s, "json.get", &[b"k"])), b"[2,3]".to_vec());
    assert_eq!(
        int_of(&call(&s, "json.arrtrim", &[b"k", b".", b"1", b"0"])),
        0
    );
    assert_eq!(bulk_of(&call(&s, "json.get", &[b"k"])), b"[]".to_vec());
    assert_eq!(
        call(&s, "json.arrtrim", &[b"gone", b".", b"0", b"1"]),
        b"$-1\r\n".to_vec()
    );
}

#[test]
fn objkeys_objlen_order_and_errors() {
    let (_g, s) = shared_for("127.0.0.1:40712");
    call(&s, "json.set", &[b"k", b".", b"{\"b\":1,\"a\":2}"]);
    assert_eq!(
        call(&s, "json.objkeys", &[b"k"]),
        b"*2\r\n$1\r\nb\r\n$1\r\na\r\n".to_vec()
    );
    assert_eq!(int_of(&call(&s, "json.objlen", &[b"k"])), 2);
    assert_eq!(
        call(&s, "json.objkeys", &[b"k", b".zzz"]),
        b"$-1\r\n".to_vec()
    );
    assert_eq!(call(&s, "json.objlen", &[b"gone"]), b"$-1\r\n".to_vec());
    call(&s, "json.set", &[b"a", b".", b"[1]"]);
    assert_eq!(
        call(&s, "json.objlen", &[b"a"]),
        b"-ERR wrong type of path value\r\n".to_vec()
    );
}

#[test]
fn ttl_interplay_lazy_purge() {
    let (_g, s) = shared_for("127.0.0.1:40713");
    call(&s, "json.set", &[b"k", b".", b"{\"a\":1}"]);
    // Plant a document whose deadline is already due; reads purge it.
    let mut batch = rocksdb::WriteBatch::default();
    crate::ds::json_ds::write_doc(&mut batch, PREFIX, b"k", 0, 1, b"[9]");
    crate::store::ops::batch_write(&s.store, batch).expect("batch");
    assert_eq!(call(&s, "json.get", &[b"k"]), b"$-1\r\n".to_vec());
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
    // The expire index entry went with it.
    assert_eq!(crate::ds::expire::sample_once(&s.store, 1000, 10), 0);
}
