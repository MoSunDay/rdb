//! Delayed-message e2e (WP2): `XADD ... DELAY <ms>` staging through the
//! real command registry + manual due sweeps (`lite::delay::sweep_due`),
//! including the three-linked P0 leak regression -- XIDLE family expiry,
//! RENAME and FLUSHDB must take the staged kind-0x1D rows with the
//! stream family, or a deleted stream's leftovers would exchange later
//! and revive it. The spawned-scanner wiring (config knob -> background
//! loop) is covered process-level in lite_delay_proc_e2e.rs.

mod common;

use std::sync::mpsc;
use std::time::Duration;

use common::lite::{call, open_shared, shared_at, text};
use rdb::state::Shared;

/// One manual due-sweep round at a wall clock of the caller's choosing
/// (tests force `now` past/short of the staged due times).
fn sweep(shared: &Shared, now: u64) -> (usize, usize) {
    rdb::lite::delay::sweep_due(shared, now)
}

/// Wall clock `ms` into the future.
fn later(ms: u64) -> u64 {
    rdb::ds::expire::now_ms() + ms
}

/// Outstanding staged 0x1D rows targeting exactly `stream` (the leak
/// assertions' ground truth; sibling streams share the slot window, so
/// the decoded target must match, not just the window).
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

/// The first entry id of an XRANGE/XREADGROUP reply: the first token
/// shaped `<ms>-<seq>` (framing digits and bulk lengths carry no dash).
fn first_id(reply: &str) -> String {
    let b = reply.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'-') {
                i += 1;
            }
            let tok = &reply[start..i];
            if tok.contains('-') && !tok.starts_with('-') && !tok.ends_with('-') {
                return tok.to_string();
            }
        } else {
            i += 1;
        }
    }
    panic!("no entry id in {reply:?}");
}

#[test]
fn delayed_add_is_invisible_until_due_then_exchanges() {
    let (shared, _path) = shared_at("44101");
    let r = text(&call(
        &shared,
        "xadd",
        &[b"s/q0", b"*", b"DELAY", b"60000", b"evt", b"timeout"],
    ));
    assert!(r.starts_with('$'), "id reply: {r}");
    // Invisible everywhere before due: XLEN, XRANGE, group `>` reads.
    assert_eq!(call(&shared, "xlen", &[b"s/q0"]), b":0\r\n".to_vec());
    assert_eq!(
        text(&call(&shared, "xrange", &[b"s/q0", b"-", b"+"])),
        "*0\r\n"
    );
    assert_eq!(
        call(&shared, "xgroup", &[b"create", b"s/q0", b"g", b"$"]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(
        text(&call(
            &shared,
            "xreadgroup",
            &[b"group", b"g", b"c", b"streams", b"s/q0", b">"]
        )),
        "*-1\r\n"
    );
    assert_eq!(staged_rows(&shared, b"s/q0"), 1, "one staged row");
    // A sweep at the current clock exchanges nothing (not due).
    assert_eq!(sweep(&shared, rdb::ds::expire::now_ms()), (0, 0));
    assert_eq!(call(&shared, "xlen", &[b"s/q0"]), b":0\r\n".to_vec());
    // Past due: one exchange, row gone, plain entry semantics after.
    assert_eq!(sweep(&shared, later(61_000)), (1, 0));
    assert_eq!(staged_rows(&shared, b"s/q0"), 0);
    assert_eq!(call(&shared, "xlen", &[b"s/q0"]), b":1\r\n".to_vec());
    let range = text(&call(&shared, "xrange", &[b"s/q0", b"-", b"+"]));
    assert!(range.contains("timeout"), "{range}");
    let read = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c", b"streams", b"s/q0", b">"],
    ));
    assert!(read.contains("timeout"), "{read}");
    let id = first_id(&read);
    assert_eq!(
        call(&shared, "xack", &[b"s/q0", b"g", id.as_bytes()]),
        b":1\r\n".to_vec(),
        "acked {id}"
    );
}

#[test]
fn out_of_order_submission_exchanges_in_due_order() {
    let (shared, _path) = shared_at("44102");
    // The LATER due is submitted first: key order (due-major) must still
    // decide who exchanges first.
    call(
        &shared,
        "xadd",
        &[b"o/q0", b"*", b"DELAY", b"500000", b"tag", b"late"],
    );
    call(
        &shared,
        "xadd",
        &[b"o/q0", b"*", b"DELAY", b"100000", b"tag", b"early"],
    );
    assert_eq!(staged_rows(&shared, b"o/q0"), 2);
    // Between the two dues: only the early row exchanges.
    assert_eq!(sweep(&shared, later(150_000)), (1, 0));
    assert_eq!(staged_rows(&shared, b"o/q0"), 1);
    let mid = text(&call(&shared, "xrange", &[b"o/q0", b"-", b"+"]));
    assert!(mid.contains("early") && !mid.contains("late"), "{mid}");
    // Past both: the late row lands after the early one (due order is
    // the read order -- fresh ids are allocated at exchange time).
    assert_eq!(sweep(&shared, later(600_000)), (1, 0));
    let both = text(&call(&shared, "xrange", &[b"o/q0", b"-", b"+"]));
    let (e, l) = (both.find("early"), both.find("late"));
    assert!(e.is_some() && l.is_some() && e < l, "{both}");
    assert_eq!(call(&shared, "xlen", &[b"o/q0"]), b":2\r\n".to_vec());
}

#[test]
fn due_exchange_wakes_a_parked_xread() {
    // The parked reader outlives the harness binding: run the whole
    // scenario against an Arc'd Shared (the sweep uses the same
    // wait hub the reader parked on).
    let shared = std::sync::Arc::new(shared_at("44103").0);
    call(&shared, "xadd", &[b"b/q0", b"1-1", b"seed", b"0"]);
    // Staged now; `$` resolves to the reserved id, so the parked reader
    // waits exactly for the exchange.
    call(
        &shared,
        "xadd",
        &[b"b/q0", b"*", b"DELAY", b"60000", b"wake", b"now"],
    );
    let reader = {
        let shared = std::sync::Arc::clone(&shared);
        std::thread::spawn(move || {
            call(
                &shared,
                "xread",
                &[b"BLOCK", b"8000", b"STREAMS", b"b/q0", b"$"],
            )
        })
    };
    // Let the reader park, then exchange: the notify must wake it well
    // inside its 8s BLOCK budget.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(sweep(&shared, later(61_000)), (1, 0));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(reader.join().expect("reader thread"));
    });
    let reply = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("reader woke on the exchange");
    assert!(text(&reply).contains("now"), "woken reply: {:?}", reply);
}

#[test]
fn xidle_lazy_purge_removes_delay_rows_no_ghost() {
    let (shared, _path) = shared_at("44104");
    call(&shared, "xadd", &[b"x/q0", b"1-1", b"seed", b"0"]);
    assert_eq!(
        call(&shared, "xidle", &[b"x/q0", b"1"]),
        b"+OK\r\n".to_vec()
    );
    // The staged write retouches the idle deadline (an exchange is a
    // write too), so expiry runs from the XADD moment.
    call(
        &shared,
        "xadd",
        &[b"x/q0", b"*", b"DELAY", b"3600000", b"evt", b"zz"],
    );
    assert_eq!(staged_rows(&shared, b"x/q0"), 1);
    std::thread::sleep(Duration::from_millis(1200));
    // Any read lazily purges the expired family -- staged rows included.
    let info = text(&call(&shared, "xinfo", &[b"stream", b"x/q0"]));
    assert!(info.contains("no such key"), "{info}");
    assert_eq!(staged_rows(&shared, b"x/q0"), 0, "family expiry folds rows");
    // No ghost: even a far-future sweep has nothing to exchange.
    assert_eq!(sweep(&shared, later(7_200_000)), (0, 0));
    assert!(text(&call(&shared, "xinfo", &[b"stream", b"x/q0"])).contains("no such key"));
}

#[test]
fn xidle_sampler_reaps_delay_rows_too() {
    let (shared, _path) = shared_at("44105");
    call(&shared, "xadd", &[b"y/q0", b"1-1", b"seed", b"0"]);
    assert_eq!(
        call(&shared, "xidle", &[b"y/q0", b"1"]),
        b"+OK\r\n".to_vec()
    );
    call(
        &shared,
        "xadd",
        &[b"y/q0", b"*", b"DELAY", b"3600000", b"evt", b"zz"],
    );
    std::thread::sleep(Duration::from_millis(1200));
    let purged = rdb::ds::expire::sample_once(
        &shared.store,
        rdb::ds::expire::now_ms(),
        10,
        b"",
        Some(shared.lite.as_ref()),
    )
    .0;
    assert_eq!(purged, 1, "sampler reaped the idle family");
    assert_eq!(staged_rows(&shared, b"y/q0"), 0, "sampler fold");
    assert_eq!(sweep(&shared, later(7_200_000)), (0, 0));
}

/// Wire-renameable lite streams need the full-name and parent slots to
/// collide (dispatch derives the prefix from the full name; lite stores
/// under the parent's) -- this brute-forced pair is re-verified below.
const RENAME_SRC: &[u8] = b"t3107/q5";
const RENAME_DST: &[u8] = b"t43847/q5";

fn parent_of(stream: &[u8]) -> &[u8] {
    &stream[..stream.iter().position(|&b| b == b'/').expect("slash")]
}

#[test]
fn rename_carries_delay_rows_and_they_exchange_there() {
    let slot = |k: &[u8]| rdb::hash::slot_with_prefix(k).0;
    assert_eq!(slot(RENAME_SRC), slot(parent_of(RENAME_SRC)));
    assert_eq!(slot(RENAME_SRC), slot(RENAME_DST));
    assert_eq!(slot(RENAME_SRC), slot(parent_of(RENAME_DST)));
    let (shared, _path) = shared_at("44106");
    call(
        &shared,
        "xadd",
        &[RENAME_SRC, b"*", b"DELAY", b"600000", b"evt", b"moved"],
    );
    assert_eq!(staged_rows(&shared, RENAME_SRC), 1);
    assert_eq!(
        call(&shared, "rename", &[RENAME_SRC, RENAME_DST]),
        b"+OK\r\n".to_vec()
    );
    // The 0x1D segment moved with the family: nothing at the old name,
    // one staged row at the new one.
    assert_eq!(staged_rows(&shared, RENAME_SRC), 0, "old name vacated");
    assert_eq!(staged_rows(&shared, RENAME_DST), 1, "rows follow RENAME");
    assert_eq!(sweep(&shared, later(700_000)), (1, 0));
    assert_eq!(
        call(&shared, "xlen", &[RENAME_DST]),
        b":1\r\n".to_vec(),
        "exchanged at the new name"
    );
    let range = text(&call(&shared, "xrange", &[RENAME_DST, b"-", b"+"]));
    assert!(range.contains("moved"), "{range}");
    assert!(text(&call(&shared, "xinfo", &[b"stream", RENAME_SRC])).contains("no such key"));
}

#[test]
fn flushdb_clears_delay_rows() {
    let (shared, _path) = shared_at("44107");
    call(
        &shared,
        "xadd",
        &[b"f/q0", b"*", b"DELAY", b"600000", b"evt", b"a"],
    );
    call(
        &shared,
        "xadd",
        &[b"f/q1", b"*", b"DELAY", b"600000", b"evt", b"b"],
    );
    assert_eq!(
        staged_rows(&shared, b"f/q0") + staged_rows(&shared, b"f/q1"),
        2
    );
    assert_eq!(call(&shared, "flushdb", &[]), b"+OK\r\n".to_vec());
    assert_eq!(staged_rows(&shared, b"f/q0"), 0);
    assert_eq!(staged_rows(&shared, b"f/q1"), 0);
    assert_eq!(sweep(&shared, later(700_000)), (0, 0), "nothing to revive");
}

#[test]
fn delay_option_validation_and_zero_delay_semantics() {
    let (shared, _path) = shared_at("44108");
    let bad_int = "ERR value is not an integer or out of range";
    for arg in [b"abc".as_slice(), b"-1", b"1.5", b"+5", b""] {
        let r = text(&call(
            &shared,
            "xadd",
            &[b"v/q0", b"*", b"DELAY", arg, b"f", b"v"],
        ));
        assert_eq!(r, format!("-{bad_int}\r\n"), "DELAY {arg:?}");
    }
    // A dangling option (no field-value pairs left) is an arity error.
    assert_eq!(
        text(&call(&shared, "xadd", &[b"v/q0", b"*", b"DELAY", b"100"])),
        "-ERR wrong number of arguments for 'xadd' command\r\n"
    );
    // DELAY 0 = no delay: synchronously visible, nothing staged.
    let now_reply = text(&call(
        &shared,
        "xadd",
        &[b"v/q0", b"*", b"DELAY", b"0", b"f", b"now"],
    ));
    assert!(now_reply.starts_with('$'), "id reply: {now_reply}");
    assert_eq!(staged_rows(&shared, b"v/q0"), 0);
    assert_eq!(call(&shared, "xlen", &[b"v/q0"]), b":1\r\n".to_vec());
    // Explicit id + DELAY threads through the normal id path.
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[
                b"v/q0",
                b"99999999999999-1",
                b"DELAY",
                b"1000",
                b"f",
                b"later"
            ]
        )),
        "$16\r\n99999999999999-1\r\n"
    );
    assert_eq!(staged_rows(&shared, b"v/q0"), 1, "explicit-id row staged");
    // An overflowing deadline is refused, never wrapped into the past.
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[b"v/q0", b"*", b"DELAY", b"18446744073709551615", b"f", b"v"]
        )),
        "-ERR delay deadline overflow\r\n"
    );
}

#[test]
fn staged_rows_survive_a_same_path_reopen() {
    let (shared, path) = shared_at("44109");
    call(
        &shared,
        "xadd",
        &[b"r/q0", b"*", b"DELAY", b"600000", b"evt", b"durable"],
    );
    assert_eq!(staged_rows(&shared, b"r/q0"), 1);
    // Reopen the same store (fresh runtime/caches): the staging row and
    // the due exchange both survive -- the two stable states of the
    // single-batch exchange (kill -9 process variant: lite_delay_proc).
    drop(shared);
    let reopened = open_shared(
        &rdb::conf::Config {
            bind: "127.0.0.1:44109".to_string(),
            ..Default::default()
        },
        &path,
    );
    assert_eq!(staged_rows(&reopened, b"r/q0"), 1, "rows are durable");
    assert_eq!(sweep(&reopened, later(700_000)), (1, 0));
    assert_eq!(call(&reopened, "xlen", &[b"r/q0"]), b":1\r\n".to_vec());
}
