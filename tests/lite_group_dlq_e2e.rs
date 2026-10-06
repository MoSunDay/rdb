//! XGROUP CREATE `DLQ` option validation e2e (in-process): the
//! negative matrix of the MAXDELIVERY+DLQ configuration -- a target
//! equal to the source stream (in-place overwrite + len inflation), an
//! explicit target in a DIFFERENT cluster slot (physical storage is
//! `<slot>/`-prefixed, so a cross-slot DLQ would land on the wrong
//! node's window in cluster form) and the explicit empty name (no
//! silent fallback to the default target) are each refused with a
//! dedicated error and leave no group behind; the implicit default and
//! a valid same-slot explicit target keep working, and a non-stream
//! name is still rejected by the shared topic-name validation.

mod common;

use common::lite::{call, shared_at, text};
use rdb::state::Shared;

/// XADD one entry (also materializes the stream).
fn add(shared: &Shared, stream: &[u8]) {
    let r = text(&call(shared, "xadd", &[stream, b"1-1", b"f", b"v"]));
    assert!(r.ends_with("$3\r\n1-1\r\n"), "xadd echoed the id: {r}");
}

/// XGROUP CREATE <s> <g> 0-0 <tail..>: the raw reply.
fn create(shared: &Shared, stream: &[u8], group: &[u8], tail: &[&[u8]]) -> String {
    let mut args: Vec<&[u8]> = vec![b"create", stream, group, b"0-0"];
    args.extend(tail);
    text(&call(shared, "xgroup", &args)).trim_end().to_string()
}

/// Decoded GroupPayload of (stream, group) straight from the store
/// (None when the group record was never written).
fn payload(shared: &Shared, stream: &[u8], group: &[u8]) -> Option<rdb::lite::model::GroupPayload> {
    let parent = stream.split(|&b| b == b'/').next().unwrap_or_default();
    let prefix = rdb::hash::slot_with_prefix(parent).1;
    let key = rdb::lite::model::group_key(&prefix, stream, group);
    rdb::store::ops::get_physical(&shared.store, &key)
        .ok()
        .flatten()
        .and_then(|raw| rdb::lite::model::decode_group(&raw))
}

/// A DLQ target parent whose CRC16 slot differs from `src_parent`'s
/// (deterministic: the slot function is pure, so the search is stable
/// across runs).
fn cross_slot_parent(src_parent: &[u8]) -> Vec<u8> {
    let want = rdb::hash::slot_number(src_parent);
    (0u32..)
        .map(|n| format!("xp{n}").into_bytes())
        .find(|p| rdb::hash::slot_number(p) != want)
        .expect("a cross-slot parent exists")
}

#[test]
fn dlq_equal_to_source_stream_refused() {
    let (shared, _dir) = shared_at("45430");
    let s = b"eq/q0".as_slice();
    add(&shared, s);
    // The literal self-target: the dead-letter transfer XADDs into the
    // target, so this would overwrite business payload in place.
    let r = create(&shared, s, b"g", &[b"MAXDELIVERY", b"3", b"DLQ", s]);
    assert!(r.contains("source stream"), "error names the clash: {r}");
    assert!(r.starts_with("-ERR"), "error frame: {r}");
    assert!(payload(&shared, s, b"g").is_none(), "no group created");
    // The stream itself is untouched by the refusal.
    assert_eq!(
        text(&call(&shared, "xlen", &[s])).trim_end(),
        ":1",
        "payload intact"
    );
    // A VALID target still creates afterwards (nothing half-committed).
    assert_eq!(create(&shared, s, b"g", &[b"MAXDELIVERY", b"3"]), "+OK");
    assert!(payload(&shared, s, b"g").is_some(), "retry created it");
}

#[test]
fn dlq_cross_slot_target_refused() {
    let (shared, _dir) = shared_at("45431");
    let s = b"cs/q0".as_slice();
    add(&shared, s);
    let other = cross_slot_parent(b"cs");
    let target = [other.clone(), b"/dlq".to_vec()].concat();
    // Sanity: the target really is a different cluster slot.
    assert_ne!(
        rdb::hash::slot_number(&other),
        rdb::hash::slot_number(b"cs"),
        "precondition: different slots"
    );
    let r = create(&shared, s, b"g", &[b"MAXDELIVERY", b"3", b"DLQ", &target]);
    assert!(r.contains("same slot"), "error names the slot rule: {r}");
    assert!(r.starts_with("-ERR"), "error frame: {r}");
    assert!(payload(&shared, s, b"g").is_none(), "no group created");
    // A same-slot explicit target (shared parent, different child) is
    // the accepted spelling of an explicit name.
    assert_eq!(
        create(
            &shared,
            s,
            b"g",
            &[b"MAXDELIVERY", b"3", b"DLQ", b"cs/dlq2"]
        ),
        "+OK"
    );
    assert!(payload(&shared, s, b"g").is_some(), "same-slot target ok");
}

#[test]
fn dlq_empty_explicit_name_refused() {
    let (shared, _dir) = shared_at("45432");
    let s = b"em/q0".as_slice();
    add(&shared, s);
    // `DLQ ""` with the cap: a dedicated refusal, NOT a silent fall
    // back to the default `<stream>/dlq` target.
    let r = create(&shared, s, b"g", &[b"MAXDELIVERY", b"3", b"DLQ", b""]);
    assert!(r.contains("empty DLQ"), "error names the emptiness: {r}");
    assert!(r.starts_with("-ERR"), "error frame: {r}");
    assert!(payload(&shared, s, b"g").is_none(), "no group created");
    // Without the cap an explicit option is a syntax error either way
    // (DLQ rides MAXDELIVERY), empty name included.
    let r = create(&shared, s, b"g", &[b"DLQ", b""]);
    assert_eq!(r, "-ERR syntax error", "empty DLQ without cap: {r}");
    assert!(payload(&shared, s, b"g").is_none(), "still no group");
    // A repeated DLQ option stays a syntax error (seen-once parsing).
    assert_eq!(
        create(
            &shared,
            s,
            b"g",
            &[b"MAXDELIVERY", b"3", b"DLQ", b"em/d", b"DLQ", b"em/e"]
        ),
        "-ERR syntax error"
    );
}

#[test]
fn dlq_default_and_name_validation_still_hold() {
    let (shared, _dir) = shared_at("45433");
    let s = b"dv/q0".as_slice();
    add(&shared, s);
    // No DLQ option + cap: the literal default `<stream>/dlq`.
    assert_eq!(create(&shared, s, b"g", &[b"MAXDELIVERY", b"2"]), "+OK");
    assert_eq!(
        payload(&shared, s, b"g").map(|p| p.dlq),
        Some(b"dv/q0/dlq".to_vec()),
        "default target resolved"
    );
    assert_eq!(
        payload(&shared, s, b"g").map(|p| p.maxdelivery),
        Some(2),
        "cap stored"
    );
    // Explicit target that is not a stream name (bare parent) is still
    // the shared topic-name validation's refusal.
    let r = create(&shared, s, b"h", &[b"MAXDELIVERY", b"2", b"DLQ", b"dv"]);
    assert_eq!(
        r, "-ERR invalid DLQ stream name",
        "reuse of validation: {r}"
    );
    // Same default for an ORDERED group (orthogonal flags).
    assert_eq!(
        create(&shared, s, b"o", &[b"ORDERED", b"MAXDELIVERY", b"2"]),
        "+OK"
    );
    assert_eq!(
        payload(&shared, s, b"o").map(|p| p.dlq),
        Some(b"dv/q0/dlq".to_vec()),
        "ordered default target"
    );
}
