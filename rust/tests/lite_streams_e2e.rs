//! Lite Mode e2e: the producing-side verbs XRANGE / XTRIM / XDEL against a
//! real RocksDB store through the real command registry. Kept separate from
//! lite_e2e.rs so no test file crosses the 400-line gate.

mod common;

use common::lite::{call, shared_at, text};

#[test]
fn xrange_bounds_count_xtrim_and_xdel() {
    let (shared, _dir) = shared_at("43007");
    // Missing stream: an empty array, not an error.
    assert_eq!(
        call(&shared, "xrange", &[b"orders/q7", b"-", b"+"]),
        b"*0\r\n".to_vec()
    );
    for i in 1..=4u8 {
        let id = format!("1-{i}");
        assert_eq!(
            call(
                &shared,
                "xadd",
                &[b"orders/q7", id.as_bytes(), b"sku", &[b'v', b'0' + i]]
            ),
            format!("${}\r\n{id}\r\n", id.len()).into_bytes()
        );
    }
    // `-`..`+` full range: four entries in ascending id order.
    let all = text(&call(&shared, "xrange", &[b"orders/q7", b"-", b"+"]));
    assert!(all.starts_with("*4\r\n"), "{all}");
    assert!(
        all.find("1-1").unwrap() < all.find("1-4").unwrap(),
        "ascending order {all}"
    );
    assert!(all.contains("sku"), "{all}");
    // COUNT caps the reply at the first N entries.
    let two = text(&call(
        &shared,
        "xrange",
        &[b"orders/q7", b"-", b"+", b"COUNT", b"2"],
    ));
    assert!(two.starts_with("*2\r\n"), "{two}");
    assert!(
        two.contains("1-1") && two.contains("1-2") && !two.contains("1-3"),
        "{two}"
    );
    // Exclusive bounds `(`: start skips 1-1, end excludes 1-4.
    let mid = text(&call(&shared, "xrange", &[b"orders/q7", b"(1-1", b"(1-4"]));
    assert!(mid.starts_with("*2\r\n"), "only 1-2 and 1-3: {mid}");
    assert!(
        mid.contains("1-2") && mid.contains("1-3") && !mid.contains("1-1"),
        "{mid}"
    );
    // XTRIM MAXLEN 2: drops the two oldest, reports how many went.
    assert_eq!(
        call(&shared, "xtrim", &[b"orders/q7", b"MAXLEN", b"2"]),
        b":2\r\n".to_vec()
    );
    assert_eq!(call(&shared, "xlen", &[b"orders/q7"]), b":2\r\n".to_vec());
    let kept = text(&call(&shared, "xrange", &[b"orders/q7", b"-", b"+"]));
    assert!(
        kept.starts_with("*2\r\n") && kept.contains("1-3") && kept.contains("1-4"),
        "{kept}"
    );
    assert!(!kept.contains("1-1") && !kept.contains("1-2"), "{kept}");
    // The `~` form with a cap above the current length trims nothing.
    assert_eq!(
        call(&shared, "xtrim", &[b"orders/q7", b"MAXLEN", b"~", b"10"]),
        b":0\r\n".to_vec()
    );
    // XDEL: only ids that physically exist count (hit + miss in one call).
    assert_eq!(
        call(&shared, "xdel", &[b"orders/q7", b"1-3", b"9-9"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(call(&shared, "xlen", &[b"orders/q7"]), b":1\r\n".to_vec());
    // Re-deleting the same id reports 0; a malformed id is rejected.
    assert_eq!(
        call(&shared, "xdel", &[b"orders/q7", b"1-3"]),
        b":0\r\n".to_vec()
    );
    assert!(
        text(&call(&shared, "xdel", &[b"orders/q7", b"not-an-id"])).starts_with("-ERR"),
        "bad id rejected"
    );
    // The survivor is still fully readable end-to-end.
    let last = text(&call(&shared, "xrange", &[b"orders/q7", b"-", b"+"]));
    assert!(last.starts_with("*1\r\n") && last.contains("1-4"), "{last}");
}
