//! Transport e2e for delayed messages: the DUMP/RESTORE/MIGRATE hop must
//! carry the staged 0x1D delay rows with the stream family, or a stream
//! migrated while messages are outstanding silently loses them (the
//! source-side delete folds the rows; nothing would re-create them on the
//! target). Three paths: (a) DUMP -> DEL -> RESTORE same name, (b) the
//! same but RESTORE under a NEW name -- the row's embedded stream name
//! must be re-encoded at the target name, (c) the real MIGRATE data plane
//! between two spawned nodes. Staging/exchange semantics themselves live
//! in lite_delay_e2e.rs; the spawned-scanner wiring in lite_delay_proc.

mod common;

use common::lite::{boot_yaml, call, resp_full, resp_text, shared_at, text};
use common::{cmd_one_shot, TOKEN};
use rdb::state::Shared;

/// Sweep rhythm armed on every spawned node in the MIGRATE case.
const SWEEP_MS: u64 = 200;
/// Delayed-message due distance on the spawned nodes: comfortably past
/// the migrate round trip, short enough for a tight poll budget.
const DELAY_MS: u64 = 4000;
/// Poll budget for a due exchange on a spawned node (debug binary +
/// 200ms rhythm + slow CI runners).
const POLL_SECS: u64 = 20;

/// One manual due-sweep round at a wall clock of the caller's choosing
/// (in-process cases force `now` past the staged due time).
fn sweep(shared: &Shared, now: u64) -> (usize, usize) {
    rdb::lite::delay::sweep_due(shared, now)
}

/// Wall clock `ms` into the future.
fn later(ms: u64) -> u64 {
    rdb::ds::expire::now_ms() + ms
}

/// Outstanding staged 0x1D rows targeting exactly `stream` (ground truth
/// for "the row moved/retargeted": sibling streams share the slot window,
/// so the decoded target must match, not just the window).
fn staged_rows(shared: &Shared, stream: &[u8]) -> usize {
    let prefix = rdb::lite::model::stream_prefix(stream).expect("stream name");
    let (lower, upper) = rdb::ds::codec::delay_window(&prefix);
    let mut n = 0;
    rdb::store::ops::for_each_from(&shared.store, &lower, false, &mut |k, _| {
        if k >= upper.as_slice() {
            return false;
        }
        if let Some((_, target, _)) = rdb::ds::codec::decode_delay_row_key(k, prefix.len()) {
            if target == stream {
                n += 1;
            }
        }
        true
    })
    .expect("0x1D window scan");
    n
}

/// DUMP payload of `stream` (in-process; MIGRATE builds the same payload
/// per key on its data plane).
fn dump_stream(shared: &Shared, stream: &[u8]) -> Vec<u8> {
    let prefix = rdb::lite::model::stream_prefix(stream).expect("stream name");
    rdb::ds::dump::dump_key(&shared.store, &prefix, stream, rdb::ds::expire::now_ms())
        .expect("stream dumped")
}

/// `true` when the payload carries at least one staged 0x1D record.
fn carries_delay_row(payload: &[u8]) -> bool {
    let (_, records) = rdb::ds::dump::decode_dump(payload).expect("dump payload");
    records
        .iter()
        .any(|r| r.body.first() == Some(&rdb::ds::codec::KIND_STREAM_DELAY))
}

/// Wire-transportable lite streams need the full-name and parent slots
/// to collide (lite stores under the parent's prefix, DUMP/RESTORE/DEL/
/// MIGRATE derive theirs from the full name); stream names cannot carry
/// hash tags, so this brute-forced triple is re-verified at runtime.
const STREAM_A: &[u8] = b"dm9930/q0"; // slot 800 == parent's
const RENAME_SRC: &[u8] = b"dm125362/q0"; // slot 3327 == parent's
const RENAME_DST: &[u8] = b"dm134323/q0"; // slot 3327 == parent's

fn assert_slots_collide(stream: &[u8]) {
    let slot = |k: &[u8]| rdb::hash::slot_with_prefix(rdb::hash::hash_tag(k)).0;
    let parent = &stream[..stream.iter().position(|&b| b == b'/').expect("slash")];
    assert_eq!(slot(stream), slot(parent), "full name vs parent");
}

/// XLEN of one stream on a spawned node (0 on any parse surprise).
async fn xlen(a: &str, s: &[u8]) -> u64 {
    resp_text(a, &[b"xlen", s])
        .await
        .trim_start_matches(':')
        .trim_end()
        .parse()
        .unwrap_or(0)
}

/// Poll XLEN on a spawned node until it reaches `want` (context-tagged
/// panic when the budget ends).
async fn until_xlen(a: &str, s: &[u8], want: u64, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_SECS);
    loop {
        let got = xlen(a, s).await;
        if got == want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: xlen stuck at {got} (want {want})"
        );
        tokio::time::sleep(std::time::Duration::from_millis(SWEEP_MS / 2)).await;
    }
}

#[test]
fn dump_restore_same_name_carries_the_staged_delay_row() {
    let stream = STREAM_A;
    assert_slots_collide(stream);
    let (shared, _path) = shared_at("44301");
    let id = text(&call(
        &shared,
        "xadd",
        &[stream, b"*", b"DELAY", b"600000", b"evt", b"kept"],
    ));
    assert!(id.starts_with('$'), "id reply: {id}");
    assert_eq!(call(&shared, "xlen", &[stream]), b":0\r\n".to_vec());
    assert_eq!(staged_rows(&shared, stream), 1, "one staged row");
    // The transport payload carries the 0x1D row with the stream family.
    let payload = dump_stream(&shared, stream);
    assert!(carries_delay_row(&payload), "0x1D row in the dump");
    // Delete the source (DEL folds the staged rows -- without the dump
    // carrying them, the message would be gone right here), then restore
    // under the SAME name: the row round-trips unchanged.
    assert_eq!(call(&shared, "del", &[stream]), b":1\r\n".to_vec());
    assert_eq!(staged_rows(&shared, stream), 0, "DEL folded the rows");
    assert_eq!(
        call(&shared, "restore", &[stream, b"0", payload.as_slice()]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(staged_rows(&shared, stream), 1, "RESTORE re-staged the row");
    assert_eq!(
        call(&shared, "xlen", &[stream]),
        b":0\r\n".to_vec(),
        "still invisible before due"
    );
    // Past due the restored row exchanges into the RESTORED stream.
    assert_eq!(sweep(&shared, later(700_000)), (1, 0));
    assert_eq!(staged_rows(&shared, stream), 0);
    assert_eq!(call(&shared, "xlen", &[stream]), b":1\r\n".to_vec());
    let range = text(&call(&shared, "xrange", &[stream, b"-", b"+"]));
    assert!(range.contains("kept"), "{range}");
}

#[test]
fn dump_restore_rename_retargets_the_staged_delay_row() {
    let (src, dst) = (RENAME_SRC, RENAME_DST);
    assert_slots_collide(src);
    assert_slots_collide(dst);
    let (shared, _path) = shared_at("44302");
    let id = text(&call(
        &shared,
        "xadd",
        &[src, b"*", b"DELAY", b"600000", b"evt", b"moved"],
    ));
    assert!(id.starts_with('$'), "id reply: {id}");
    assert_eq!(staged_rows(&shared, src), 1);
    let payload = dump_stream(&shared, src);
    assert!(carries_delay_row(&payload), "0x1D row in the dump");
    assert_eq!(call(&shared, "del", &[src]), b":1\r\n".to_vec());
    assert_eq!(staged_rows(&shared, src), 0, "DEL folded the rows");
    // Restore under a DIFFERENT name: the row's embedded stream name is
    // re-encoded at `dst` (a leaked source-name row would target a dead
    // stream and the sweep would drop it, not exchange it).
    assert_eq!(
        call(&shared, "restore", &[dst, b"0", payload.as_slice()]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(staged_rows(&shared, src), 0, "old name stays vacated");
    assert_eq!(
        staged_rows(&shared, dst),
        1,
        "row re-encoded at the new name"
    );
    assert_eq!(
        sweep(&shared, later(700_000)),
        (1, 0),
        "exchanged, not dropped"
    );
    assert_eq!(call(&shared, "xlen", &[dst]), b":1\r\n".to_vec());
    let range = text(&call(&shared, "xrange", &[dst, b"-", b"+"]));
    assert!(range.contains("moved"), "{range}");
    assert_eq!(
        call(&shared, "xlen", &[src]),
        b":0\r\n".to_vec(),
        "no revival at the dead name"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_moves_outstanding_delay_rows_across_nodes() {
    let yaml = format!("lite:\n  delay_sweep_ms: {SWEEP_MS}\n");
    let (_src_node, src) = boot_yaml("44311", &yaml).await;
    let (_dst_node, dst) = boot_yaml("44312", &yaml).await;
    let stream = STREAM_A;
    let (host, port) = dst.rsplit_once(':').expect("dst addr");
    // Stage an outstanding delayed message on the source node (the sweep
    // is armed there too: a leaked source row would exchange into the
    // deleted stream and revive it -- asserted absent below).
    let id = resp_text(
        &src,
        &[
            b"xadd",
            stream,
            b"*",
            b"DELAY",
            DELAY_MS.to_string().as_bytes(),
            b"evt",
            b"moved",
        ],
    )
    .await;
    assert!(id.starts_with('$'), "id reply: {id}");
    assert_eq!(xlen(&src, stream).await, 0, "staged, not an entry");
    // The MIGRATE data plane (per-key DUMP/RESTORE hop + source delete).
    let r = cmd_one_shot(
        &src,
        TOKEN,
        &[
            b"migrate",
            host.as_bytes(),
            port.as_bytes(),
            stream,
            b"0",
            b"10000",
            b"REPLACE",
        ],
    )
    .await;
    assert_eq!(r, b"+OK", "migrate reply: {r:?}");
    assert_eq!(xlen(&src, stream).await, 0, "source vacated");
    // Not lost: the destination's spawned sweep exchanges the moved row
    // at due -- nothing else on that node could create the entry.
    until_xlen(&dst, stream, 1, "migrated delayed exchange").await;
    let range = resp_full(&dst, &[b"xrange", stream, b"-", b"+"]).await;
    assert!(range.contains("moved"), "{range}");
    // The source stays dead past the due time (its own armed sweep finds
    // no staged row left behind by the source-side delete).
    tokio::time::sleep(std::time::Duration::from_millis(4 * SWEEP_MS)).await;
    assert_eq!(xlen(&src, stream).await, 0, "no revival on the source");
}
