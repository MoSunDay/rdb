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

/// Regression: duplicate ids in one XDEL used to count twice (the batched
/// delete is invisible to the physical reads), corrupting XLEN.
#[test]
fn xdel_duplicate_ids_count_once_and_xlen_stays_correct() {
    let (shared, _dir) = shared_at("43008");
    for i in 1..=3i64 {
        let id = format!("5-{i}");
        assert_eq!(
            call(&shared, "xadd", &[b"s/qa", id.as_bytes(), b"f", b"v"]),
            format!("${}\r\n{id}\r\n", id.len()).into_bytes()
        );
    }
    // Same id twice in one call, then again in a second call (already gone).
    assert_eq!(
        call(&shared, "xdel", &[b"s/qa", b"5-1", b"5-1"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xdel", &[b"s/qa", b"5-1"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(call(&shared, "xlen", &[b"s/qa"]), b":2\r\n".to_vec());
    // All three flavors in one call still dedupe correctly.
    assert_eq!(
        call(&shared, "xdel", &[b"s/qa", b"5-2", b"5-3", b"5-2"]),
        b":2\r\n".to_vec()
    );
    assert_eq!(call(&shared, "xlen", &[b"s/qa"]), b":0\r\n".to_vec());
}

/// Regression: XTRIM's victim scan must stop at the stream's key range
/// edge — an over-counted/corrupted meta.len used to let it walk into
/// NEIGHBOUR records in the same slot (the sibling stream `t/q2` sorts
/// right after `t/q1`'s entry base) — and the victim prealloc is capped.
#[test]
fn xtrim_maxlen_zero_never_deletes_neighbours() {
    let (shared, _dir) = shared_at("43009");
    for i in 1..=3i64 {
        let id = format!("7-{i}");
        call(&shared, "xadd", &[b"t/q1", id.as_bytes(), b"f", b"v"]);
        call(&shared, "xadd", &[b"t/q2", id.as_bytes(), b"f", b"v"]);
    }
    // Corrupt meta.len (10 > 3 real entries) via a direct store write, so
    // XTRIM MAXLEN 0 wants 10 victims: the range guard must stop after 3
    // instead of eating t/q2's entries (same slot, sorts after t/q1/).
    let prefix = rdb::hash::slot_with_prefix(b"t").1;
    let corrupted = rdb::lite::model::MetaPayload {
        created_ms: 0,
        last_ms: 7,
        last_seq: 3,
        len: 10,
        idle_ms: 0,
    };
    let mkey = rdb::lite::model::meta_key(&prefix, b"t/q1");
    let mut wb = rocksdb::WriteBatch::default();
    wb.put(&mkey, rdb::lite::model::encode_meta_at(&corrupted, 0));
    rdb::store::ops::batch_write(&shared.store, wb).unwrap();
    assert_eq!(
        call(&shared, "xtrim", &[b"t/q1", b"MAXLEN", b"0"]),
        b":3\r\n".to_vec()
    );
    // meta.len stays corrupted (10-3=7; XTRIM subtracts victims only), but
    // no neighbour record was touched.
    assert_eq!(call(&shared, "xlen", &[b"t/q1"]), b":7\r\n".to_vec());
    // The sibling stream is untouched.
    assert_eq!(call(&shared, "xlen", &[b"t/q2"]), b":3\r\n".to_vec());
    let kept = text(&call(&shared, "xrange", &[b"t/q2", b"-", b"+"]));
    assert!(kept.starts_with("*3\r\n") && kept.contains("7-2"), "{kept}");
}

/// Regression (C6): `XIDLE <stream> <huge-seconds>` — `secs * 1000` used
/// to overflow u64 silently (e.g. secs = u64::MAX/1000 + 1 wraps idle_ms
/// to ~384ms) and arm an instant TTL that reaped the whole stream family.
/// The overflow must be REJECTED without touching the stored idle value.
#[test]
fn xidle_overflow_seconds_is_rejected_not_wrapped() {
    let (shared, _dir) = shared_at("43010");
    call(&shared, "xadd", &[b"s/qi", b"1-1", b"f", b"v"]);
    assert_eq!(
        call(&shared, "xidle", &[b"s/qi", b"30"]),
        b"+OK\r\n".to_vec()
    );
    // u64::MAX seconds: secs.checked_mul(1000) overflows -> error reply.
    assert_eq!(
        call(&shared, "xidle", &[b"s/qi", b"18446744073709551615"]),
        b"-ERR invalid idle seconds\r\n".to_vec()
    );
    // u64::MAX/1000 + 1: the classic wrap-to-small-value case.
    assert_eq!(
        call(&shared, "xidle", &[b"s/qi", b"18446744073709552"]),
        b"-ERR invalid idle seconds\r\n".to_vec()
    );
    // u64::MAX/1000: idle_ms itself fits u64, but now_ms + idle_ms
    // (epoch ~1.7e12) still overflows -> the deadline guard must reject.
    assert_eq!(
        call(&shared, "xidle", &[b"s/qi", b"18446744073709551"]),
        b"-ERR invalid idle seconds\r\n".to_vec()
    );
    // The configured idle value is unchanged (read form), and the stream
    // was not expired by an instant wrapped deadline.
    assert_eq!(call(&shared, "xidle", &[b"s/qi"]), b":30\r\n".to_vec());
    assert_eq!(call(&shared, "xlen", &[b"s/qi"]), b":1\r\n".to_vec());
    // A sane value still works after the rejections.
    assert_eq!(
        call(&shared, "xidle", &[b"s/qi", b"60"]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(call(&shared, "xidle", &[b"s/qi"]), b":60\r\n".to_vec());
}

/// Regression (C6): an explicit XADD id with a huge `id.ms` on a stream
/// with XIDLE armed — `now.max(id.ms) + idle_ms` used to overflow u64 and
/// wrap into a corrupted (small/past) deadline. Must reply an error,
/// write NOTHING, and leave the stream usable.
#[test]
fn xadd_huge_id_with_idle_is_rejected_not_wrapped() {
    let (shared, _dir) = shared_at("43011");
    call(&shared, "xadd", &[b"s/qh", b"1-1", b"f", b"v"]);
    assert_eq!(
        call(&shared, "xidle", &[b"s/qh", b"3600"]),
        b"+OK\r\n".to_vec()
    );
    // ms = 18446744073709551000 parses as u64 but now.max(ms)+idle_ms
    // overflows: error, no write.
    assert_eq!(
        call(
            &shared,
            "xadd",
            &[b"s/qh", b"18446744073709551000-0", b"f", b"v"]
        ),
        b"-ERR ID or idle deadline overflow\r\n".to_vec()
    );
    // Nothing was written: length unchanged, meta not clobbered.
    assert_eq!(call(&shared, "xlen", &[b"s/qh"]), b":1\r\n".to_vec());
    // A normal XADD still works afterwards.
    assert_eq!(
        call(&shared, "xadd", &[b"s/qh", b"1-2", b"f", b"v2"]),
        b"$3\r\n1-2\r\n".to_vec()
    );
    assert_eq!(call(&shared, "xlen", &[b"s/qh"]), b":2\r\n".to_vec());
    // Sanity: an id that itself overflows u64 fails at parse instead.
    assert!(text(&call(
        &shared,
        "xadd",
        &[b"s/qh", b"18446744073709551616-0", b"f", b"v"]
    ))
    .contains("ERR Invalid stream ID"));
}

/// Regression (auto-id saturation): with the last id at `<ms, u64::MAX>`
/// (seeded here via the absolute ceiling `<u64::MAX, u64::MAX>`), XADD `*`
/// can no longer generate a strictly-greater id. It must reply Redis's
/// exhaustion error, write NOTHING (the old entry stays, no silent
/// overwrite), and keep `last_id`/XLEN intact.
#[test]
fn xadd_auto_id_at_ceiling_errors_instead_of_reusing_last_id() {
    let (shared, _dir) = shared_at("43016");
    const MAX: &str = "18446744073709551615";
    let ceiling = format!("{MAX}-{MAX}");
    // Seed the last id at the ceiling via an explicit id (legal on a
    // fresh stream: greater than the implicit 0-0).
    assert_eq!(
        call(&shared, "xadd", &[b"c/q0", ceiling.as_bytes(), b"f", b"v"]),
        format!("${}\r\n{ceiling}\r\n", ceiling.len()).into_bytes()
    );
    // Auto-id would saturate and EQUAL the ceiling id: rejected verbatim.
    assert_eq!(
        call(&shared, "xadd", &[b"c/q0", b"*", b"f", b"v"]),
        b"-ERR The stream has exhausted the last possible ID, unable to add more items\r\n"
            .to_vec()
    );
    // The failed add wrote nothing: still one entry, same last id.
    assert_eq!(call(&shared, "xlen", &[b"c/q0"]), b":1\r\n".to_vec());
    assert!(text(&call(&shared, "xrange", &[b"c/q0", b"+", b"+"])).contains(&ceiling));
    // An explicit id still strictly greater does not exist; one BELOW the
    // ceiling keeps the old equal-or-smaller rejection.
    assert!(text(&call(&shared, "xadd", &[b"c/q0", b"1-1", b"f", b"v"]))
        .contains("equal or smaller than the stream last item"));
}

/// Regression (XADD broadcast wake): XADD must notify every WaitHub key
/// a blocked reader could be parked on for the append -- the appended
/// child stream's meta key AND the parent topic's key (both derive
/// their slot prefix from the parent name). A reader blocked on the
/// child must be woken by a bare-parent `XADD parent *` too, promptly,
/// well before its BLOCK timeout.
#[test]
fn xadd_wakes_blocked_reader_and_parent_key_waiters() {
    use rdb::ds::wait::{self, WaitOutcome};

    let shared = std::sync::Arc::new(shared_at("43019").0);
    call(&shared, "xadd", &[b"w/q0", b"1-1", b"f", b"seed"]);
    // (a) A real blocked reader on the child stream, woken by a
    // BARE-PARENT XADD whose auto-pick lands on that same queue.
    let reader = {
        let s = std::sync::Arc::clone(&shared);
        std::thread::spawn(move || {
            call(&s, "xread", &[b"BLOCK", b"8000", b"streams", b"w/q0", b"$"])
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(300)); // let it park
    let t0 = std::time::Instant::now();
    let r = text(&call(&shared, "xadd", &[b"w", b"*", b"f", b"wake"]));
    assert!(r.contains("w/q0"), "auto-pick must land on q0: {r}");
    let reply = reader.join().expect("reader thread");
    let elapsed = t0.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "reader slept {elapsed:?} past the xadd"
    );
    let got = text(&reply);
    assert!(
        got.contains("wake") && !got.contains("*-1"),
        "blocked reader must get the entry, got {got}"
    );
    // (b) Direct WaitHub view of the broadcast: waiters parked on the
    // child meta key AND on the bare-parent topic key are both signaled
    // by one explicit `parent/child` XADD (no-op when nobody listens).
    // (Separate stream: part (a)'s auto id sits at wall-clock ms, far
    // above any small explicit id.)
    call(&shared, "xadd", &[b"w2/q0", b"1-1", b"f", b"seed"]);
    let prefix = rdb::hash::slot_with_prefix(b"w2").1;
    let child_key = rdb::lite::model::meta_key(&prefix, b"w2/q0");
    let parent_key = rdb::lite::model::meta_key(&prefix, b"w2");
    let on_child = wait::register(&shared.wait_hub, &child_key);
    let on_parent = wait::register(&shared.wait_hub, &parent_key);
    assert!(text(&call(&shared, "xadd", &[b"w2/q0", b"9-9", b"f", b"v"])).contains("9-9"));
    let soon = std::time::Duration::from_millis(100);
    assert_eq!(wait::wait(&on_child, soon), WaitOutcome::Signaled);
    assert_eq!(
        wait::wait(&on_parent, soon),
        WaitOutcome::Signaled,
        "the parent topic key must be notified too"
    );
}
