//! Lite Mode consumer-group e2e: regression cover for the offset-cache key
//! type. Group names are NOT charset-validated at the command layer, so
//! invalid-UTF8 names are legal; the cache key must stay raw bytes or
//! `from_utf8_lossy` collapses distinct names to U+FFFD (phantom NOGROUP
//! hits, cross-group acks). Kept separate so no test file crosses the
//! 400-line gate.

mod common;

use common::lite::{call, open_shared, shared_at, text};
use rdb::conf;

const G80: &[u8] = b"g\x80"; // invalid UTF-8 (lone continuation byte)
const G81: &[u8] = b"g\x81"; // distinct bytes, same lossy shape

/// Two distinct invalid-UTF8 group names must be DISTINCT cache entries:
/// delivering + acking on one may not move the other's watermarks.
#[test]
fn non_utf8_group_names_stay_distinct() {
    let (shared, _dir) = shared_at("43012");
    for i in 1..=2u8 {
        call(
            &shared,
            "xadd",
            &[b"s/qb", format!("1-{i}").as_bytes(), b"f", &[b'v', i]],
        );
    }
    assert_eq!(
        call(&shared, "xgroup", &[b"create", b"s/qb", G80, b"0-0"]),
        b"+OK\r\n".to_vec()
    );
    // Distinct bytes -> a second group, not a BUSYGROUP collision (the
    // on-disk record was always byte-keyed; only the cache was lossy).
    assert_eq!(
        call(&shared, "xgroup", &[b"create", b"s/qb", G81, b"0-0"]),
        b"+OK\r\n".to_vec()
    );
    // Deliver everything to G80 and commit it.
    let r = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", G80, b"c1", b"streams", b"s/qb", b">"],
    ));
    assert!(r.contains("1-1") && r.contains("1-2"), "{r}");
    assert_eq!(
        call(&shared, "xack", &[b"s/qb", G80, b"1-1", b"1-2"]),
        b":2\r\n".to_vec()
    );
    // G80's acks must NOT have touched G81: `>` still delivers both.
    let r = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", G81, b"c1", b"streams", b"s/qb", b">"],
    ));
    assert!(r.contains("1-1") && r.contains("1-2"), "{r}");
    // And G80 itself is drained: re-ack counts 0.
    assert_eq!(
        call(&shared, "xack", &[b"s/qb", G80, b"1-1"]),
        b":0\r\n".to_vec()
    );
    // G81 has its own fresh watermark.
    assert_eq!(
        call(&shared, "xack", &[b"s/qb", G81, b"1-2"]),
        b":1\r\n".to_vec()
    );
}

/// An invalid-UTF8 name that was NEVER created must still get NOGROUP even
/// when another invalid-UTF8 group exists: with the old lossy String key
/// both collapsed to the same U+FFFD entry, so reads delivered from a
/// stranger's watermark and acks silently moved it (phantom group).
#[test]
fn uncreated_non_utf8_group_gets_nogroup_no_phantom() {
    let (shared, _dir) = shared_at("43013");
    call(&shared, "xadd", &[b"t/qc", b"1-1", b"f", b"v"]);
    assert_eq!(
        call(&shared, "xgroup", &[b"create", b"t/qc", b"h\x80", b"0-0"]),
        b"+OK\r\n".to_vec()
    );
    // Never created, lossy-indistinguishable from h\x80: NOGROUP, and the
    // XACK must not move h\x80's committed watermark.
    let r = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"h\x81", b"c1", b"streams", b"t/qc", b">"],
    ));
    assert!(r.contains("NOGROUP"), "{r}");
    assert_eq!(
        call(&shared, "xack", &[b"t/qc", b"h\x81", b"1-1"]),
        b":0\r\n".to_vec()
    );
    // h\x80 is untouched: `>` still delivers its first entry.
    let r = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"h\x80", b"c1", b"streams", b"t/qc", b">"],
    ));
    assert!(r.contains("1-1"), "{r}");
}

/// The flusher path (flush_dirty -> drop_superseded -> build_flush_batch,
/// exactly what `lite::spawn_background` runs) must persist byte-identical
/// group records, and a restart must reload each invalid-UTF8 group with
/// its OWN committed watermark.
#[test]
fn flush_and_restart_keep_non_utf8_groups_separate() {
    let (shared, path) = shared_at("43014");
    for i in 1..=2u8 {
        call(
            &shared,
            "xadd",
            &[b"u/qd", format!("1-{i}").as_bytes(), b"f", &[b'v', i]],
        );
    }
    call(
        &shared,
        "xgroup",
        &[b"create", b"u/qd", b"w\x80", b"0-0"],
    );
    call(
        &shared,
        "xgroup",
        &[b"create", b"u/qd", b"w\x81", b"0-0"],
    );
    call(
        &shared,
        "xreadgroup",
        &[b"group", b"w\x80", b"c1", b"streams", b"u/qd", b">"],
    );
    assert_eq!(
        call(&shared, "xack", &[b"u/qd", b"w\x80", b"1-1", b"1-2"]),
        b":2\r\n".to_vec()
    );
    // Manual flush round, mirroring the background loop.
    let dirty = rdb::lite::offset::flush_dirty(&shared.lite.offsets);
    let dirty = rdb::lite::offset::drop_superseded(&shared.lite.offsets, dirty);
    if let Some(batch) = rdb::lite::offset::build_flush_batch(&dirty) {
        rdb::store::ops::batch_write(&shared.store, batch).unwrap();
    }
    // Restart: fresh cache, records reload from raw-byte keys.
    drop(shared);
    let restarted = open_shared(
        &conf::Config {
            bind: "127.0.0.1:43014".to_string(),
            ..Default::default()
        },
        &path,
    );
    // w\x80 resumes from its committed watermark: nothing new.
    assert_eq!(
        call(
            &restarted,
            "xreadgroup",
            &[b"group", b"w\x80", b"c1", b"streams", b"u/qd", b">"]
        ),
        b"*-1\r\n".to_vec()
    );
    // w\x81 kept its own (zero) watermark: both entries redeliver.
    let r = text(&call(
        &restarted,
        "xreadgroup",
        &[b"group", b"w\x81", b"c1", b"streams", b"u/qd", b">"],
    ));
    assert!(r.contains("1-1") && r.contains("1-2"), "{r}");
    assert_eq!(
        call(&restarted, "xack", &[b"u/qd", b"w\x81", b"1-1", b"1-2"]),
        b":2\r\n".to_vec()
    );
}
