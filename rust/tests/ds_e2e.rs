//! Integration tests for the `ds` foundation against a real RocksDB store:
//! envelope roundtrips, the active-expire sampler, and family delete_range
//! wiping meta + element + index records together.

use std::sync::{Arc, RwLock};

use rocksdb::WriteBatch;

use rdb::ds::{codec, expire};
use rdb::store::{self, ops};
use rdb::{conf, monitor, state, topology};

/// Mirror of `state::testutil::shared_with` (lib-internal, invisible here);
/// `tag` isolates the parallel #[test]s in this file.
fn shared_for(tag: &str) -> state::Shared {
    let mut conf = conf::Config {
        bind: "127.0.0.1:32681".to_string(),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: "127.0.0.1:22681".to_string(),
        raft_token: "test-token".to_string(),
        ..Default::default()
    };
    conf.bind = format!("127.0.0.1:{tag}");
    let dir = std::env::temp_dir().join(format!("rdb-ds-e2e-{}-{tag}", std::process::id()));
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
        lite: std::sync::Arc::new(rdb::lite::new_runtime()),
        conf,
    }
}

const P: &[u8] = b"70/";

#[test]
fn envelope_roundtrip_through_real_store() {
    let shared = shared_for("41001");
    let store = &shared.store;

    // Raw strings stay bare (legacy-compatible shape).
    store::set(store, P, b"legacy", b"plain").unwrap();
    let raw_phys = codec::string_key(P, b"legacy");
    assert_eq!(
        ops::get_physical(store, &raw_phys).unwrap().unwrap(),
        b"plain".to_vec()
    );

    // Enveloped STRING_TTL: value carries the varuint expire prefix.
    // Deadline far in the future so lazy expiry never fires mid-test.
    let deadline = expire::now_ms() + 42_000;
    let root = codec::data_key(P, codec::KIND_STRING_TTL, b"ttl-key");
    let mut batch = WriteBatch::default();
    batch.put(&root, codec::encode_envelope(deadline, b"payload"));
    expire::set_ttl_entries(&mut batch, P, root.clone(), 0, deadline);
    ops::batch_write(store, batch).unwrap();

    let value = ops::get_physical(store, &root).unwrap().unwrap();
    let (expire_ms, payload) = codec::decode_envelope(&value);
    assert_eq!((expire_ms, payload), (deadline, &b"payload"[..]));

    let (read_expire, read_payload) = expire::read_enveloped(store, P, b"ttl-key")
        .unwrap()
        .expect("present");
    assert_eq!((read_expire, read_payload), (deadline, b"payload".to_vec()));

    // The index entry exists and decodes back to the record body.
    let idx = codec::expire_index_key(P, deadline, &root);
    assert!(ops::get_physical(store, &idx).unwrap().is_some());
    let (idx_expire, body) = codec::decode_expire_index_key(&idx, P.len()).unwrap();
    assert_eq!(idx_expire, deadline);
    assert_eq!(body, root[P.len()..].to_vec());
}

#[test]
fn sampler_purges_expired_families_and_stale_index() {
    let shared = shared_for("41002");
    let store = &shared.store;
    // Sample "now" slightly in the future of the wall clock so the "alive"
    // record (deadline now + 60s) is also alive for real lazy reads.
    let now = expire::now_ms() + 10_000;
    let past = now - 1;

    // A due hash: meta + two members + index entry.
    let meta = codec::data_key(P, codec::KIND_HASH_META, b"due");
    let mut batch = WriteBatch::default();
    batch.put(&meta, codec::encode_envelope(past, b"2"));
    batch.put(
        codec::elem_key(P, codec::KIND_HASH_FLD, b"due", b"f1"),
        b"v1",
    );
    batch.put(
        codec::elem_key(P, codec::KIND_HASH_FLD, b"due", b"f2"),
        b"v2",
    );
    expire::set_ttl_entries(&mut batch, P, meta.clone(), 0, past);
    // A live record the sampler must NOT touch.
    batch.put(
        codec::data_key(P, codec::KIND_STRING_TTL, b"alive"),
        codec::encode_envelope(now + 60_000, b"v"),
    );
    // A stale index entry whose record no longer exists.
    let ghost_root = codec::data_key(P, codec::KIND_SET_META, b"ghost");
    batch.put(codec::expire_index_key(P, past, &ghost_root), b"");
    ops::batch_write(store, batch).unwrap();

    let purged = expire::sample_once(store, now, 10, b"").0;
    // "due" + the stale ghost index entry.
    assert_eq!(purged, 2, "one real purge plus one stale index sweep");

    for (lower, _upper) in codec::family_delete_ranges(P, codec::HASH_FAMILY, b"due") {
        assert_eq!(
            ops::prefix_iter_collect(store, &lower, 0).unwrap(),
            Vec::new(),
            "family fully purged"
        );
    }
    assert!(ops::get_physical(store, &ghost_root).unwrap().is_none());
    // The live record survives.
    assert!(expire::read_enveloped(store, P, b"alive")
        .unwrap()
        .is_some());
}

#[test]
fn delete_range_wipes_family_records_sampler_sweeps_index() {
    let shared = shared_for("41003");
    let store = &shared.store;
    // Deadline in the future: the sampler is driven by a synthetic "now"
    // one millisecond past it below.
    let expire_at = expire::now_ms() + 3_600_000;

    let meta = codec::data_key(P, codec::KIND_ZSET_META, b"z");
    let mut batch = WriteBatch::default();
    batch.put(&meta, codec::encode_envelope(expire_at, b"meta"));
    for m in [b"a", b"b", b"c"] {
        batch.put(
            codec::elem_key(P, codec::KIND_ZSET_MEMBER, b"z", m),
            b"member",
        );
    }
    expire::set_ttl_entries(&mut batch, P, meta.clone(), 0, expire_at);
    // Neighbouring key that must survive the range delete.
    batch.put(
        codec::data_key(P, codec::KIND_ZSET_META, b"zz"),
        codec::encode_envelope(0, b"x"),
    );
    ops::batch_write(store, batch).unwrap();

    // Plain range delete wipes meta + members but NOT the 0xFD index
    // entry (it sorts outside the family span by design).
    for (lower, upper) in codec::family_delete_ranges(P, codec::ZSET_FAMILY, b"z") {
        ops::delete_range(store, &lower, &upper).unwrap();
    }
    for probe in [
        meta.clone(),
        codec::elem_key(P, codec::KIND_ZSET_MEMBER, b"z", b"b"),
    ] {
        assert!(
            ops::get_physical(store, &probe).unwrap().is_none(),
            "{probe:?}"
        );
    }
    let idx = codec::expire_index_key(P, expire_at, &meta);
    assert!(
        ops::get_physical(store, &idx).unwrap().is_some(),
        "index entry outside the family range"
    );

    // The active-expire sampler sweeps the now-stale index entry.
    let purged = expire::sample_once(store, expire_at + 1, 10, b"").0;
    assert_eq!(purged, 1, "stale index swept");
    assert!(ops::get_physical(store, &idx).unwrap().is_none());
    assert!(
        ops::get_physical(store, &codec::data_key(P, codec::KIND_ZSET_META, b"zz"))
            .unwrap()
            .is_some(),
        "neighbouring key untouched"
    );
}
