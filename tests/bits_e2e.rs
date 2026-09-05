//! Wire-level e2e for the bit family (SETBIT/GETBIT/BITCOUNT/BITPOS/
//! BITOP): raw RESP2 sockets against in-proc `resp::serve` (harness of
//! `resp_e2e.rs`), asserting EXACT reply bytes cross-checked against
//! `src/command/bitops/tests.rs` and a live Redis 7.2.5. Bits are
//! MSB-FIRST per byte: offset 0 is the 0x80 bit, offset 7 the 0x01 bit.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rdb::{conf, hash, monitor, resp, state, store, topology};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(5);
const TOKEN: &[u8] = b"test-token";
const QUEUED: &str = "+QUEUED";
const CROSSSLOT: &str = "-ERR CROSSSLOT Keys in request don't hash to the same slot";
const WRONGTYPE: &str = "-WRONGTYPE Operation against a key holding the wrong kind of value";
const BIT_ARG: &str = "-ERR The bit argument must be 1 or 0.";
/// This file's port window (other e2e files use other ranges).
const PORTS: std::ops::RangeInclusive<u16> = 32740..=32760;
/// Bits 7,8,9,15,16 (\x01\xc1\x80) + nine zeros + bit 100 (\x08).
const SPARSE: &[u8] = b"\x01\xc1\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x08";
/// Thirteen zero bytes (a cleared bit at offset 100).
const ZEROS: &[u8] = b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";

/// Mirror of the lib-internal `state::testutil::shared_with`; `bind`
/// doubles as the store-dir tag so parallel tests never share a path.
fn shared_for(bind: &str) -> state::Shared {
    let conf = conf::Config {
        bind: bind.to_string(),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: "127.0.0.1:23740".to_string(),
        raft_token: "test-token".to_string(),
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("rdb-bits-e2e-{}-{bind}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = store::data_path(dir.to_str().unwrap(), &conf.bind);
    let st = store::open(path.to_str().unwrap()).unwrap();
    state::Shared {
        mode: state::Mode::Normal,
        store: Arc::new(st),
        topology: Arc::new(RwLock::new(topology::empty())),
        raft: Arc::new(RwLock::new(state::stub_raft(&conf))),
        monitor: Arc::new(monitor::new_collector()),
        latch: rdb::ds::latch::Latch::new(),
        wait_hub: rdb::ds::wait::WaitHub::new(),
        lite: Arc::new(rdb::lite::new_runtime()),
        sql_ts: Arc::new(rdb::sql::tx::Oracle::new()),
        migrating: Arc::new(RwLock::new(std::collections::HashMap::new())),
        importing: Arc::new(RwLock::new(std::collections::HashMap::new())),
        migrate_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        conf,
    }
}

/// First free listener in this file's port window; one per test.
fn bind_listener() -> (TcpListener, String) {
    for port in PORTS {
        if let Ok(l) = resp::bind(&format!("127.0.0.1:{port}")) {
            return (l, format!("127.0.0.1:{port}"));
        }
    }
    let l = resp::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("local_addr").to_string();
    (l, addr)
}

/// Fresh server + one AUTHed client socket.
async fn authed_server() -> TcpStream {
    let (listener, addr) = bind_listener();
    let shared = Arc::new(shared_for(&addr));
    tokio::spawn(resp::serve(listener, shared));
    let mut s = tokio::time::timeout(TIMEOUT, TcpStream::connect(&addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    rpc(&mut s, &resp_req(&[b"AUTH", TOKEN]), b"+OK\r\n").await;
    s
}

async fn read_n(sock: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(TIMEOUT, sock.read_exact(&mut buf))
        .await
        .expect("read timed out")
        .expect("read failed (EOF?)");
    buf
}

async fn rpc(sock: &mut TcpStream, req: &[u8], expect: &[u8]) {
    sock.write_all(req).await.expect("write");
    assert_eq!(read_n(sock, expect.len()).await, expect, "req: {req:?}");
}

/// Issue `parts` (command + args) and assert the exact single-LINE
/// reply (":0", "+OK", "-ERR ..."): `\r\n` is appended here.
async fn c(sock: &mut TcpStream, parts: &[&[u8]], line: &str) {
    let expect = format!("{line}\r\n").into_bytes();
    rpc(sock, &resp_req(parts), &expect).await;
}

/// Read one `:<int>\r\n` reply (variable width, e.g. PTTL) as an i64.
async fn read_int(sock: &mut TcpStream) -> i64 {
    let mut buf = Vec::new();
    while *buf.last().unwrap_or(&0) != b'\n' {
        assert!(buf.len() < 32, "not an int reply: {buf:?}");
        buf.extend_from_slice(&read_n(sock, 1).await);
    }
    let s = std::str::from_utf8(&buf).expect("int reply utf8");
    assert!(s.starts_with(':') && s.ends_with("\r\n"), "reply {s:?}");
    s[1..s.len() - 2].parse().expect("int")
}

fn resp_req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

/// `SET key val` -> `+OK`.
async fn set(sock: &mut TcpStream, key: &[u8], val: &[u8]) {
    c(sock, &[b"SET", key, val], "+OK").await;
}

/// `GET key` -> the exact bulk reply carrying `payload`.
async fn bulk(sock: &mut TcpStream, key: &[u8], payload: &[u8]) {
    let mut e = format!("${}\r\n", payload.len()).into_bytes();
    e.extend_from_slice(payload);
    e.extend_from_slice(b"\r\n");
    rpc(sock, &resp_req(&[b"GET", key]), &e).await;
}

/// `GETBIT key offset` -> ":0"/":1".
async fn getbit(sock: &mut TcpStream, key: &[u8], off: &str, want: u8) {
    c(sock, &[b"GETBIT", key, off.as_bytes()], &format!(":{want}")).await;
}

/// First plain `<prefix><i>` key (no hash tag) outside slot `avoid`.
fn key_in_other_slot(prefix: &str, avoid: u16) -> String {
    (0..10_000)
        .map(|i| format!("{prefix}{i}"))
        .find(|k| hash::slot_number(k.as_bytes()) != avoid)
        .expect("candidate key in another slot")
}

#[tokio::test]
async fn missing_keys_framing_and_msb_first_numbering() {
    let mut s = authed_server().await;
    // Perspective 1: missing keys reply zero-bit answers, byte-exact.
    c(&mut s, &[b"GETBIT", b"miss", b"5"], ":0").await;
    c(&mut s, &[b"GETBIT", b"miss", b"100000"], ":0").await;
    c(&mut s, &[b"BITCOUNT", b"miss"], ":0").await;
    c(&mut s, &[b"BITPOS", b"miss", b"0"], ":0").await;
    c(&mut s, &[b"BITPOS", b"miss", b"1"], ":-1").await;
    c(&mut s, &[b"SETBIT", b"fresh", b"3", b"1"], ":0").await;
    c(&mut s, &[b"STRLEN", b"fresh"], ":1").await;
    // Perspective 2: 'a' = 0x61 = 0110 0001 (MSB first).
    set(&mut s, b"a", b"a").await;
    for (off, want) in b"\x00\x01\x01\x00\x00\x00\x00\x01".iter().enumerate() {
        getbit(&mut s, b"a", &off.to_string(), *want).await;
    }
    c(&mut s, &[b"BITCOUNT", b"a"], ":3").await;
    c(&mut s, &[b"BITPOS", b"a", b"1"], ":1").await;
    c(&mut s, &[b"BITPOS", b"a", b"0"], ":0").await;
    // Canonical MSB-first proof on empty keys: bit 7 -> \x01, bit 0 -> \x80.
    c(&mut s, &[b"SETBIT", b"low", b"7", b"1"], ":0").await;
    bulk(&mut s, b"low", b"\x01").await;
    c(&mut s, &[b"SETBIT", b"high", b"0", b"1"], ":0").await;
    bulk(&mut s, b"high", b"\x80").await;
}

#[tokio::test]
async fn setbit_crosses_byte_boundaries_with_zero_fill() {
    let mut s = authed_server().await;
    c(&mut s, &[b"SETBIT", b"s", b"7", b"1"], ":0").await;
    bulk(&mut s, b"s", b"\x01").await;
    c(&mut s, &[b"SETBIT", b"s", b"8", b"1"], ":0").await;
    c(&mut s, &[b"SETBIT", b"s", b"9", b"1"], ":0").await;
    bulk(&mut s, b"s", b"\x01\xc0").await;
    c(&mut s, &[b"SETBIT", b"s", b"15", b"1"], ":0").await;
    c(&mut s, &[b"SETBIT", b"s", b"16", b"1"], ":0").await;
    c(&mut s, &[b"STRLEN", b"s"], ":3").await;
    // Sparse offset 100: byte 12 bit 4 (mask 0x08), zero fill in between.
    c(&mut s, &[b"SETBIT", b"s", b"100", b"1"], ":0").await;
    c(&mut s, &[b"STRLEN", b"s"], ":13").await;
    c(&mut s, &[b"BITCOUNT", b"s"], ":6").await;
    bulk(&mut s, b"s", SPARSE).await;
    getbit(&mut s, b"s", "100", 1).await;
    // Clearing returns the old bit and keeps the extended bytes.
    c(&mut s, &[b"SETBIT", b"s", b"7", b"0"], ":1").await;
    c(&mut s, &[b"BITCOUNT", b"s"], ":5").await;
    // Redis >= 7: clearing a bit on a MISSING key still materializes
    // offset/8+1 zero bytes.
    c(&mut s, &[b"SETBIT", b"z", b"100", b"0"], ":0").await;
    c(&mut s, &[b"STRLEN", b"z"], ":13").await;
    c(&mut s, &[b"BITCOUNT", b"z"], ":0").await;
    bulk(&mut s, b"z", ZEROS).await;
}

#[tokio::test]
async fn setbit_getbit_argument_errors() {
    let mut s = authed_server().await;
    set(&mut s, b"k", b"x").await;
    let off_err = "-ERR bit offset is not an integer or out of range";
    let bit_err = "-ERR bit is not an integer or out of range";
    c(&mut s, &[b"SETBIT", b"k", b"abc", b"1"], off_err).await;
    c(&mut s, &[b"SETBIT", b"k", b"-1", b"1"], off_err).await;
    // 2^32 is one past the 512MB bitmap cap (offsets span 0..=2^32-1);
    // probing the max offset would materialize 512MB, so only the cap.
    c(&mut s, &[b"SETBIT", b"k", b"4294967296", b"1"], off_err).await;
    c(&mut s, &[b"SETBIT", b"k", b"0", b"2"], bit_err).await;
    c(&mut s, &[b"SETBIT", b"k", b"0", b"abc"], bit_err).await;
    c(&mut s, &[b"GETBIT", b"k", b"abc"], off_err).await;
    c(&mut s, &[b"BITPOS", b"k", b"2"], BIT_ARG).await;
    let arity = "-ERR wrong number of arguments for 'setbit' command";
    c(&mut s, &[b"SETBIT", b"k", b"0"], arity).await;
}

#[tokio::test]
async fn bitcount_ranges_units_and_errors() {
    let mut s = authed_server().await;
    set(&mut s, b"b", b"\xff\xf0\x00").await;
    c(&mut s, &[b"BITCOUNT", b"b"], ":12").await;
    c(&mut s, &[b"BITCOUNT", b"b", b"0", b"-1"], ":12").await;
    c(&mut s, &[b"BITCOUNT", b"b", b"-100", b"100"], ":12").await;
    // [2,-2] = bytes 2..1: inverted -> empty window -> 0; [3,1] too.
    c(&mut s, &[b"BITCOUNT", b"b", b"2", b"-2"], ":0").await;
    c(&mut s, &[b"BITCOUNT", b"b", b"3", b"1"], ":0").await;
    // Same indexes, different units: [0,7] BYTE clamps to the whole
    // 3-byte string (12), [0,7] BIT is just the 0xff byte (8).
    c(&mut s, &[b"BITCOUNT", b"b", b"0", b"7", b"BYTE"], ":12").await;
    c(&mut s, &[b"BITCOUNT", b"b", b"0", b"7", b"BIT"], ":8").await;
    c(&mut s, &[b"BITCOUNT", b"b", b"0", b"0", b"BIT"], ":1").await;
    // Missing key short-circuits to 0 before range parsing.
    c(&mut s, &[b"BITCOUNT", b"miss", b"0", b"junk"], ":0").await;
    let (syn, ni) = (
        "-ERR syntax error",
        "-ERR value is not an integer or out of range",
    );
    c(&mut s, &[b"BITCOUNT", b"b", b"0"], syn).await;
    c(&mut s, &[b"BITCOUNT", b"b", b"0", b"1", b"XY"], syn).await;
    c(&mut s, &[b"BITCOUNT", b"b", b"0", b"x"], ni).await;
}

#[tokio::test]
async fn bitpos_four_documented_rules_and_ranges() {
    let mut s = authed_server().await;
    set(&mut s, b"b", b"\xff\xf0\x00").await;
    set(&mut s, b"ones", b"\xff\xff").await;
    // (a) missing key, bit 0, no range: the void right of a (missing)
    // string reads as zero-padded.
    c(&mut s, &[b"BITPOS", b"miss", b"0"], ":0").await;
    // (b) all-ones key, bit 0, no range: first zero past the content.
    c(&mut s, &[b"BITPOS", b"ones", b"0"], ":16").await;
    // (c) missing key, bit 1.
    c(&mut s, &[b"BITPOS", b"miss", b"1"], ":-1").await;
    // (d) an explicit end confines the window: all-ones bytes -> -1...
    c(&mut s, &[b"BITPOS", b"b", b"0", b"0", b"0"], ":-1").await;
    c(&mut s, &[b"BITPOS", b"b", b"1", b"2", b"2"], ":-1").await;
    // ...while a window reaching the string end finds the zero content.
    c(&mut s, &[b"BITPOS", b"b", b"0", b"0", b"-1"], ":12").await;
    c(&mut s, &[b"BITPOS", b"b", b"0", b"2", b"2"], ":16").await;
    // Positive `(`-free ranges, start-only form included.
    c(&mut s, &[b"BITPOS", b"b", b"1"], ":0").await;
    c(&mut s, &[b"BITPOS", b"b", b"1", b"1"], ":8").await;
    c(&mut s, &[b"BITPOS", b"b", b"0", b"2"], ":16").await;
    c(&mut s, &[b"BITPOS", b"ones", b"0", b"1"], ":16").await;
    // BIT unit: edge masks inside the boundary bytes.
    let bp = b"BITPOS";
    c(&mut s, &[bp, b"b", b"0", b"0", b"7", b"BIT"], ":-1").await;
    c(&mut s, &[bp, b"b", b"0", b"8", b"15", b"BIT"], ":12").await;
    c(&mut s, &[bp, b"b", b"1", b"0", b"7", b"BIT"], ":0").await;
    c(&mut s, &[bp, b"b", b"0", b"12", b"12", b"BIT"], ":12").await;
    // Existing empty strings have no bits at all.
    set(&mut s, b"empty", b"").await;
    c(&mut s, &[b"BITPOS", b"empty", b"0"], ":-1").await;
    c(&mut s, &[b"BITPOS", b"empty", b"1"], ":-1").await;
}

#[tokio::test]
async fn bitop_algebra_mixed_lengths_and_arity() {
    let mut s = authed_server().await;
    // One hash tag -> one slot for the whole BITOP request.
    let (d, a, e, t) = (b"{b}d", b"{b}k1", b"{b}k2", b"{b}kt");
    set(&mut s, a, b"\xff\x0f").await;
    set(&mut s, e, b"\xf0\xf0").await;
    set(&mut s, t, b"\xf0").await;
    // Same-length AND; the shorter OR/XOR source reads as zero bytes.
    c(&mut s, &[b"BITOP", b"AND", d, a, e], ":2").await;
    bulk(&mut s, d, b"\xf0\x00").await;
    c(&mut s, &[b"BITOP", b"OR", d, a, t], ":2").await;
    bulk(&mut s, d, b"\xff\x0f").await;
    c(&mut s, &[b"BITOP", b"XOR", d, a, t], ":2").await;
    bulk(&mut s, d, b"\x0f\x0f").await;
    // NOT with exactly one source; multi-source NOT and unknown ops.
    c(&mut s, &[b"BITOP", b"NOT", d, a], ":2").await;
    bulk(&mut s, d, b"\x00\xf0").await;
    let (not2, syn) = (
        "-ERR BITOP NOT must be called with a single source key.",
        "-ERR syntax error",
    );
    c(&mut s, &[b"BITOP", b"NOT", d, a, e], not2).await;
    c(&mut s, &[b"BITOP", b"NAND", d, a], syn).await;
    let arity = "-ERR wrong number of arguments for 'bitop' command";
    c(&mut s, &[b"BITOP", b"AND", d], arity).await;
}

#[tokio::test]
async fn bitop_dest_ttl_kinds_and_crossslot() {
    let mut s = authed_server().await;
    // src keeps its TTL; dst is longer AND expiring: the result shrinks
    // dst to the source length and drops its deadline.
    let (st, d, h) = (b"{b}src", b"{b}dst", b"{b}dh");
    set(&mut s, st, b"\x0f").await;
    c(&mut s, &[b"EXPIRE", st, b"100"], ":1").await;
    set(&mut s, d, b"zzzzzzzz").await;
    c(&mut s, &[b"EXPIRE", d, b"100"], ":1").await;
    c(&mut s, &[b"BITOP", b"OR", d, st], ":1").await;
    bulk(&mut s, d, b"\x0f").await;
    c(&mut s, &[b"STRLEN", d], ":1").await;
    c(&mut s, &[b"PTTL", d], ":-1").await;
    s.write_all(&resp_req(&[b"PTTL", st])).await.unwrap();
    let ttl = read_int(&mut s).await;
    assert!(ttl > 0 && ttl <= 100_000, "src ttl {ttl}");
    // Destination of any kind is overwritten (hash -> string); bit
    // reads against a hash are WRONGTYPE (key lookup precedes ranges).
    c(&mut s, &[b"HSET", h, b"f", b"v"], ":1").await;
    c(&mut s, &[b"GET", h], WRONGTYPE).await;
    c(&mut s, &[b"BITCOUNT", h, b"0", b"junk"], WRONGTYPE).await;
    c(&mut s, &[b"BITOP", b"NOT", h, st], ":1").await;
    bulk(&mut s, h, b"\xf0").await;
    // WRONGTYPE applies to sources only; nothing is written then.
    c(&mut s, &[b"HSET", b"{b}hs", b"f", b"v"], ":1").await;
    c(&mut s, &[b"BITOP", b"AND", b"{b}dd", b"{b}hs"], WRONGTYPE).await;
    c(&mut s, &[b"EXISTS", b"{b}dd"], ":0").await;
    // An all-empty result deletes the destination.
    set(&mut s, b"{b}de", b"stale").await;
    c(&mut s, &[b"BITOP", b"AND", b"{b}de", b"{b}n1"], ":0").await;
    c(&mut s, &[b"GET", b"{b}de"], "$-1").await;
    c(&mut s, &[b"EXISTS", b"{b}de"], ":0").await;
    // Untagged keys in different slots: CROSSSLOT before any write.
    let k1: &[u8] = b"csrc1";
    let k2 = key_in_other_slot("csrc2", hash::slot_number(k1));
    let k2: &[u8] = k2.as_bytes();
    c(&mut s, &[b"BITOP", b"AND", b"cdst", k1, k2], CROSSSLOT).await;
    // EXISTS is itself multi-key/CROSSSLOT-checked: probe one key each.
    c(&mut s, &[b"EXISTS", b"cdst"], ":0").await;
    c(&mut s, &[b"EXISTS", k1], ":0").await;
    c(&mut s, &[b"EXISTS", k2], ":0").await;
}

#[tokio::test]
async fn multi_exec_replay_and_queue_time_crossslot_abort() {
    let mut s = authed_server().await;
    c(&mut s, &[b"MULTI"], "+OK").await;
    c(&mut s, &[b"SETBIT", b"m", b"0", b"1"], QUEUED).await;
    c(&mut s, &[b"BITCOUNT", b"m"], QUEUED).await;
    c(&mut s, &[b"GETBIT", b"m", b"0"], QUEUED).await;
    // Replies reflect execution order: SETBIT returned the old 0, then
    // the key holds 0x80 for both reads.
    rpc(&mut s, &resp_req(&[b"EXEC"]), b"*3\r\n:0\r\n:1\r\n:1\r\n").await;
    bulk(&mut s, b"m", b"\x80").await;
    // A queued key from another slot is rejected at QUEUE time (tx goes
    // dirty); later commands queue, EXEC aborts wholesale, nothing runs.
    let near: &[u8] = b"abase";
    let far = key_in_other_slot("afar", hash::slot_number(near));
    let far: &[u8] = far.as_bytes();
    c(&mut s, &[b"MULTI"], "+OK").await;
    c(&mut s, &[b"SETBIT", near, b"0", b"1"], QUEUED).await;
    c(&mut s, &[b"SETBIT", far, b"0", b"1"], CROSSSLOT).await;
    c(&mut s, &[b"BITCOUNT", near], QUEUED).await;
    let abort = "-EXECABORT Transaction discarded because of previous errors.";
    c(&mut s, &[b"EXEC"], abort).await;
    // Nothing executed (EXISTS is multi-key, so probe each key alone);
    // the connection stays usable afterwards.
    c(&mut s, &[b"EXISTS", near], ":0").await;
    c(&mut s, &[b"EXISTS", far], ":0").await;
    c(&mut s, &[b"SETBIT", near, b"5", b"1"], ":0").await;
}

#[tokio::test]
async fn pipelined_setbits_roundtrip() {
    let mut s = authed_server().await;
    // 50 SETBITs in ONE write; the server must drain the whole batch.
    let mut batch = Vec::new();
    for off in 0..50u32 {
        let off = off.to_string();
        batch.extend_from_slice(&resp_req(&[b"SETBIT", b"pipe", off.as_bytes(), b"1"]));
    }
    rpc(&mut s, &batch, b":0\r\n".repeat(50).as_slice()).await;
    c(&mut s, &[b"BITCOUNT", b"pipe"], ":50").await;
    c(&mut s, &[b"STRLEN", b"pipe"], ":7").await;
}
