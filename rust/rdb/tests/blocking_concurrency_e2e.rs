//! Regression tests for the async per-key latch (`ds::latch`): concurrent
//! blocking commands must park TASKS, never OS threads. Under the old
//! Condvar latch a BLPOP holding its key across the commit `.await` could
//! pin the (single) runtime worker while another waiter blocked on the
//! same key -- a hard deadlock. Here three waiters share one key and two
//! more sit on distinct keys; single-element pushes wake exactly one
//! waiter each and everything completes well inside a bounded window.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rdb::{command, conf, hash, monitor, state, store, topology};

/// Mirror of `state::testutil::shared_with` (lib-internal); each test
/// gets its own bind, store dir and tag.
fn shared_for(tag: &str) -> state::Shared {
    let conf = conf::Config {
        bind: format!("127.0.0.1:{tag}"),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: format!("127.0.0.1:{}", tag.parse::<u16>().unwrap() + 100),
        raft_token: "test-token".to_string(),
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("rdb-blkconc-e2e-{}-{tag}", std::process::id()));
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

/// Dispatch like the RESP layer, INSIDE the caller's runtime (no nested
/// runtime): registry lookup, slot prefix from the first key arg.
async fn call(shared: &state::Shared, name: &str, args: &[&str]) -> Vec<u8> {
    let handler = command::lookup(name).unwrap_or_else(|| panic!("'{name}' not registered"));
    let prefix_key = if rdb::router::is_whitelisted(name) {
        Vec::new()
    } else {
        args.first()
            .map(|a| hash::slot_with_prefix(hash::hash_tag(a.as_bytes())).1)
            .unwrap_or_default()
    };
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
    let mut out = Vec::new();
    let mut ctx = command::Ctx {
        shared,
        prefix_key,
        args: argv,
        out: &mut out,
        close_conn: false,
    };
    handler(&mut ctx).await;
    out
}

/// A flat two-element RESP array of bulk payloads.
fn arr2(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut buf = b"*2\r\n".to_vec();
    for part in [a, b] {
        buf.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
        buf.extend_from_slice(part);
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// Three BLPOP waiters on ONE key plus one each on TWO other keys. Every
/// single-element RPUSH wakes exactly one waiter (the wait hub notifies
/// per push); all five must finish inside a bounded window on the
/// default current-thread runtime -- with a thread-blocking latch this
/// deadlocks instead of finishing.
#[tokio::test]
async fn concurrent_blpop_same_and_distinct_keys_wake_within_bounds() {
    let shared = Arc::new(shared_for("44110"));
    // Park three waiters on "hot" and one on each of "a1"/"a2" (5s cap so
    // a lost wake degrades into a visible null reply, not a hang).
    let mut waiters = Vec::new();
    for _ in 0..3 {
        let s = Arc::clone(&shared);
        waiters.push(tokio::spawn(async move {
            call(&s, "blpop", &["hot", "5"]).await
        }));
    }
    for key in ["a1", "a2"] {
        let s = Arc::clone(&shared);
        waiters.push(tokio::spawn(async move {
            call(&s, "blpop", &[key, "5"]).await
        }));
    }
    // Let every waiter reach its park before waking them.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Wake exactly the right number: one push per waiting element. The
    // replies may exceed :1 transiently -- a woken waiter pops
    // asynchronously from the push reply -- so only assert integer
    // length replies; final consistency is checked after the joins.
    for elem in ["e1", "e2", "e3"] {
        let reply = call(&shared, "rpush", &["hot", elem]).await;
        assert!(reply.starts_with(b":") && reply.ends_with(b"\r\n"), "{reply:?}");
    }
    for (key, elem) in [("a1", "v1"), ("a2", "v2")] {
        let reply = call(&shared, "rpush", &[key, elem]).await;
        assert!(reply.starts_with(b":") && reply.ends_with(b"\r\n"), "{reply:?}");
    }

    // Bounded completion: 10s wall clock for the whole fan.
    let joined = tokio::time::timeout(Duration::from_secs(10), async {
        let mut replies = Vec::new();
        for w in waiters {
            replies.push(w.await.expect("waiter task"));
        }
        replies
    })
    .await;
    let replies = joined.expect("all blocking pops must complete within 10s");

    // First three replies: ("hot", distinct e1/e2/e3) in wake order.
    let mut hot_elems: Vec<String> = replies[..3]
        .iter()
        .map(|r| {
            let text = String::from_utf8_lossy(r).into_owned();
            assert!(text.starts_with("*2\r\n$3\r\nhot\r\n"), "same-key pop: {text:?}");
            let elem = text.trim_start_matches("*2\r\n$3\r\nhot\r\n$2\r\n");
            assert_eq!(elem.len(), 4, "element plus CRLF: {text:?}");
            elem[..2].to_string()
        })
        .collect();
    hot_elems.sort();
    assert_eq!(hot_elems, vec!["e1", "e2", "e3"], "each waiter got one element");

    // Distinct-key waiters were not serialized behind "hot".
    assert_eq!(replies[3], arr2(b"a1", b"v1"));
    assert_eq!(replies[4], arr2(b"a2", b"v2"));
    // Everything pushed was consumed exactly once.
    assert_eq!(call(&shared, "llen", &["hot"]).await, b":0\r\n".to_vec());
    assert_eq!(call(&shared, "llen", &["a1"]).await, b":0\r\n".to_vec());
    assert_eq!(call(&shared, "llen", &["a2"]).await, b":0\r\n".to_vec());
}

/// The drains after the pops: nothing left over, and a fresh BLPOP on the
/// same key still works (waiter bookkeeping stayed consistent).
#[tokio::test]
async fn blocking_concurrency_leaves_no_stray_data_or_waiters() {
    let shared = Arc::new(shared_for("44111"));
    let s = Arc::clone(&shared);
    let waiter = tokio::spawn(async move { call(&s, "blpop", &["k", "1"]).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(call(&shared, "rpush", &["k", "x"]).await, b":1\r\n".to_vec());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .expect("pop completes")
            .expect("task"),
        arr2(b"k", b"x")
    );
    assert_eq!(call(&shared, "llen", &["k"]).await, b":0\r\n".to_vec());
    // A second blocked pop on the drained key times out cleanly (1s).
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), call(&shared, "blpop", &["k", "0.5"]))
            .await
            .expect("timeout path completes"),
        b"*-1\r\n".to_vec()
    );
}
