//! End-to-end tests for the vector-set family: in-process lifecycle
//! through the real command registry with the REAL slot-prefix
//! derivation (no spawned node needed -- every command is route-local).
//! `expect` asserts exact RESP reply bytes.

use std::sync::{Arc, RwLock};

use rdb::{command, conf, hash, monitor, state, store, topology};

/// Mirror of `state::testutil::shared_with` (lib-internal, invisible
/// here); each test gets its own bind, store dir and tag.
fn shared_for(tag: &str) -> state::Shared {
    let conf = conf::Config {
        bind: format!("127.0.0.1:{tag}"),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: format!("127.0.0.1:{}", tag.parse::<u16>().unwrap() + 100),
        raft_token: "test-token".to_string(),
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("rdb-vs-e2e-{}-{tag}", std::process::id()));
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
        conf,
    }
}

/// Dispatch like the RESP layer: registry lookup, slot prefix from the
/// first key arg, one current-thread runtime per call.
fn call(shared: &state::Shared, name: &str, args: &[&[u8]]) -> Vec<u8> {
    let handler = command::lookup(name).unwrap_or_else(|| panic!("'{name}' not registered"));
    let prefix_key = args
        .first()
        .map(|a| hash::slot_with_prefix(hash::hash_tag(a)).1)
        .unwrap_or_default();
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    let mut ctx = command::Ctx {
        shared,
        prefix_key,
        args: argv,
        out: &mut out,
        close_conn: false,
    };
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(handler(&mut ctx));
    out
}

/// Assert a command replies exactly `want`.
fn expect(shared: &state::Shared, name: &str, args: &[&[u8]], want: &[u8]) {
    assert_eq!(call(shared, name, args), want.to_vec(), "in '{name}'");
}

/// Seed a 2-D set: e1=[1,0] e2=[0,1] e3=[1,1].
fn seed(shared: &state::Shared) {
    expect(
        shared,
        "vadd",
        &[b"k", b"VALUES", b"2", b"e1", b"1", b"0"],
        b":1\r\n",
    );
    expect(
        shared,
        "vadd",
        &[b"k", b"VALUES", b"2", b"e2", b"0", b"1"],
        b":1\r\n",
    );
    expect(
        shared,
        "vadd",
        &[b"k", b"VALUES", b"2", b"e3", b"1", b"1"],
        b":1\r\n",
    );
}

#[test]
fn vadd_vsim_recall_exact_query() {
    let shared = shared_for("45351");
    seed(&shared);
    expect(&shared, "vcard", &[b"k"], b":3\r\n");
    expect(&shared, "vdim", &[b"k"], b":2\r\n");
    // Query = one stored vector exactly: first hit is that element,
    // score "1" (shortest-roundtrip f64), then the diagonal, then the
    // orthogonal one at 0.5.
    let mid = format!("{}", (1.0f64 / 2.0f64.sqrt() + 1.0) / 2.0);
    expect(
        &shared,
        "vsim",
        &[b"k", b"WITHSCORES", b"VALUES", b"1", b"0"],
        format!(
            "*6\r\n$2\r\ne1\r\n$1\r\n1\r\n$2\r\ne3\r\n${}\r\n{}\r\n$2\r\ne2\r\n$3\r\n0.5\r\n",
            mid.len(),
            mid
        )
        .as_bytes(),
    );
    expect(
        &shared,
        "vsim",
        &[b"k", b"VALUES", b"0", b"1"],
        b"*3\r\n$2\r\ne2\r\n$2\r\ne3\r\n$2\r\ne1\r\n",
    );
    // Re-adding an existing element replaces the vector, keeps the attr.
    expect(&shared, "vsetattr", &[b"k", b"e1", b"t=1"], b":1\r\n");
    expect(
        &shared,
        "vadd",
        &[b"k", b"VALUES", b"2", b"e1", b"0", b"1"],
        b":0\r\n",
    );
    expect(&shared, "vcard", &[b"k"], b":3\r\n");
    expect(&shared, "vgetattr", &[b"k", b"e1"], b"$3\r\nt=1\r\n");
    // e1 now equals e2: both score 1, byte order breaks the tie.
    expect(
        &shared,
        "vsim",
        &[b"k", b"COUNT", b"2", b"WITHSCORES", b"VALUES", b"0", b"1"],
        b"*4\r\n$2\r\ne1\r\n$1\r\n1\r\n$2\r\ne2\r\n$1\r\n1\r\n",
    );
}

#[test]
fn vsim_fp16_query_and_attrib_variants() {
    let shared = shared_for("45352");
    seed(&shared);
    expect(&shared, "vsetattr", &[b"k", b"e2", b"clr=red"], b":1\r\n");
    // [1,0] as two LE u16 halves: 3C00 0000 (raw blob, not base64).
    expect(
        &shared,
        "vsim",
        &[b"k", b"FP16", &[0x00, 0x3C, 0x00, 0x00]],
        b"*3\r\n$2\r\ne1\r\n$2\r\ne3\r\n$2\r\ne2\r\n",
    );
    expect(
        &shared,
        "vsim",
        &[b"k", b"WITHATTRIBS", b"FP16", &[0x00, 0x3C, 0x00, 0x00]],
        b"*6\r\n$2\r\ne1\r\n$-1\r\n$2\r\ne3\r\n$-1\r\n$2\r\ne2\r\n$7\r\nclr=red\r\n",
    );
    expect(
        &shared,
        "vsim",
        &[
            b"k",
            b"WITHATTRIBS",
            b"WITHSCORES",
            b"COUNT",
            b"1",
            b"FP16",
            &[0x00, 0x3C, 0x00, 0x00],
        ],
        b"*3\r\n$2\r\ne1\r\n$-1\r\n$1\r\n1\r\n",
    );
    // Zero-norm query: all cosines 0, all scores 0.5, byte order.
    expect(
        &shared,
        "vsim",
        &[b"k", b"WITHSCORES", b"VALUES", b"0", b"0"],
        b"*6\r\n$2\r\ne1\r\n$3\r\n0.5\r\n$2\r\ne2\r\n$3\r\n0.5\r\n$2\r\ne3\r\n$3\r\n0.5\r\n",
    );
}

#[test]
fn vadd_fp16_storage_path() {
    let shared = shared_for("45353");
    // FP16 blob [0.5, -2.0]: halves 3800 C000 (raw bytes).
    expect(
        &shared,
        "vadd",
        &[b"k", b"FP16", b"2", b"e", &[0x00, 0x38, 0x00, 0xC0]],
        b":1\r\n",
    );
    expect(
        &shared,
        "vsim",
        &[b"k", b"WITHSCORES", b"VALUES", b"0.5", b"-2"],
        b"*2\r\n$1\r\ne\r\n$1\r\n1\r\n",
    );
    expect(
        &shared,
        "vadd",
        &[b"k", b"FP16", b"2", b"short", &[0x00, 0x3C, 0x00]],
        b"-ERR invalid FP16 vector\r\n",
    );
}

#[test]
fn attribute_roundtrip_and_clear() {
    let shared = shared_for("45354");
    expect(
        &shared,
        "vadd",
        &[b"k", b"VALUES", b"1", b"e", b"1"],
        b":1\r\n",
    );
    expect(&shared, "vgetattr", &[b"k", b"e"], b"$-1\r\n");
    expect(&shared, "vsetattr", &[b"k", b"e", b"brand=rdb"], b":1\r\n");
    expect(&shared, "vgetattr", &[b"k", b"e"], b"$9\r\nbrand=rdb\r\n");
    // Vector survives attribute rewrites.
    expect(
        &shared,
        "vsim",
        &[b"k", b"WITHSCORES", b"VALUES", b"1"],
        b"*2\r\n$1\r\ne\r\n$1\r\n1\r\n",
    );
    // "" clears back to the null bulk; missing elements answer :0.
    expect(&shared, "vsetattr", &[b"k", b"e", b""], b":1\r\n");
    expect(&shared, "vgetattr", &[b"k", b"e"], b"$-1\r\n");
    expect(&shared, "vsetattr", &[b"k", b"zz", b"x"], b":0\r\n");
    expect(&shared, "vgetattr", &[b"k", b"zz"], b"$-1\r\n");
    expect(&shared, "vgetattr", &[b"nope", b"e"], b"$-1\r\n");
}

#[test]
fn vrem_lifecycle_and_counts() {
    let shared = shared_for("45355");
    seed(&shared);
    expect(&shared, "vrem", &[b"k", b"e3"], b":1\r\n");
    expect(&shared, "vcard", &[b"k"], b":2\r\n");
    expect(&shared, "vrem", &[b"k", b"e3"], b":0\r\n");
    expect(&shared, "vrem", &[b"nope", b"e3"], b":0\r\n");
    // Last element out: the whole key disappears.
    expect(&shared, "vrem", &[b"k", b"e1"], b":1\r\n");
    expect(&shared, "vrem", &[b"k", b"e2"], b":1\r\n");
    expect(&shared, "vcard", &[b"k"], b":0\r\n");
    expect(&shared, "exists", &[b"k"], b":0\r\n");
    expect(
        &shared,
        "vdim",
        &[b"k"],
        b"-ERR vector set does not exist\r\n",
    );
    expect(&shared, "vcard", &[b"nope"], b":0\r\n");
}

#[test]
fn argument_and_type_errors() {
    let shared = shared_for("45356");
    seed(&shared);
    expect(
        &shared,
        "vadd",
        &[b"k", b"VALUES", b"3", b"e4", b"1", b"0", b"0"],
        b"-ERR dimension mismatch\r\n",
    );
    expect(
        &shared,
        "vadd",
        &[b"k2", b"VALUES", b"0", b"e", b"1"],
        b"-ERR invalid dim\r\n",
    );
    expect(
        &shared,
        "vadd",
        &[b"k2", b"VALUES", b"4097", b"e", b"1"],
        b"-ERR invalid dim\r\n",
    );
    expect(
        &shared,
        "vadd",
        &[b"k", b"VALUES", b"2", b"e4", b"1", b"zz"],
        b"-ERR invalid vector value\r\n",
    );
    expect(
        &shared,
        "vadd",
        &[b"k", b"VALUES", b"2", b"e4", b"1"],
        b"-ERR invalid vector value\r\n",
    );
    expect(
        &shared,
        "vsim",
        &[b"k", b"COUNT", b"x", b"VALUES", b"1", b"0"],
        b"-ERR invalid COUNT\r\n",
    );
    expect(
        &shared,
        "vsim",
        &[b"k", b"EF", b"80", b"VALUES", b"1", b"0"],
        b"-ERR wrong number of arguments for 'vsim' command\r\n",
    );
    expect(
        &shared,
        "vsim",
        &[b"nope", b"VALUES", b"1", b"0"],
        b"-ERR vector set does not exist\r\n",
    );
    // Foreign kinds are WRONGTYPE everywhere in the family.
    expect(&shared, "set", &[b"raw", b"x"], b"+OK\r\n");
    let wrongtype = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
    for (name, args) in [
        ("vadd", vec![b"raw".as_slice(), b"VALUES", b"1", b"e", b"1"]),
        ("vrem", vec![b"raw".as_slice(), b"e"]),
        ("vcard", vec![b"raw".as_slice()]),
        ("vdim", vec![b"raw".as_slice()]),
        ("vsetattr", vec![b"raw".as_slice(), b"e", b"a"]),
        ("vgetattr", vec![b"raw".as_slice(), b"e"]),
        ("vsim", vec![b"raw".as_slice(), b"VALUES", b"1"]),
    ] {
        expect(&shared, name, &args, wrongtype);
    }
    expect(&shared, "type", &[b"k"], b"+vectorset\r\n");
}

#[test]
fn ttl_interplay_with_vadd() {
    let shared = shared_for("45357");
    expect(
        &shared,
        "vadd",
        &[b"k", b"VALUES", b"1", b"e", b"1"],
        b":1\r\n",
    );
    // PEXPIREAT migrates the family into the enveloped TTL shape; a
    // later VADD must KEEP the deadline, not reset it.
    expect(&shared, "pexpireat", &[b"k", b"9999999999999"], b":1\r\n");
    expect(
        &shared,
        "vadd",
        &[b"k", b"VALUES", b"1", b"f", b"1"],
        b":1\r\n",
    );
    let ttl = call(&shared, "ttl", &[b"k"]);
    let secs: i64 = std::str::from_utf8(&ttl[1..ttl.len() - 2])
        .unwrap()
        .parse()
        .unwrap();
    assert!(secs > 1_000_000_000, "ttl lost after vadd: {secs}");
    // A past deadline lazily purges the whole family on the next read.
    expect(&shared, "pexpireat", &[b"k", b"1"], b":1\r\n");
    expect(&shared, "vcard", &[b"k"], b":0\r\n");
    expect(&shared, "exists", &[b"k"], b":0\r\n");
}

#[test]
fn vrem_under_ttl_keeps_expiry_shape() {
    let shared = shared_for("45358");
    expect(
        &shared,
        "vadd",
        &[b"k", b"VALUES", b"1", b"a", b"1"],
        b":1\r\n",
    );
    expect(
        &shared,
        "vadd",
        &[b"k", b"VALUES", b"1", b"b", b"1"],
        b":1\r\n",
    );
    expect(&shared, "pexpireat", &[b"k", b"9999999999999"], b":1\r\n");
    // VREM under a live TTL: count drops, deadline still kept.
    expect(&shared, "vrem", &[b"k", b"a"], b":1\r\n");
    expect(&shared, "vcard", &[b"k"], b":1\r\n");
    let ttl = call(&shared, "ttl", &[b"k"]);
    let secs: i64 = std::str::from_utf8(&ttl[1..ttl.len() - 2])
        .unwrap()
        .parse()
        .unwrap();
    assert!(secs > 1_000_000_000, "ttl lost after vrem: {secs}");
    // Removing the last element wipes the family AND the index entry.
    expect(&shared, "vrem", &[b"k", b"b"], b":1\r\n");
    expect(&shared, "vcard", &[b"k"], b":0\r\n");
    expect(&shared, "ttl", &[b"k"], b":-2\r\n");
}
