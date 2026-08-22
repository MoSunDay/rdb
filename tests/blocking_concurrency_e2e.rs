//! Regression tests for the async per-key latch (`ds::latch`): concurrent
//! blocking commands must park TASKS, never OS threads. Under the old
//! Condvar latch a BLPOP holding its key across the commit `.await` could
//! pin the (single) runtime worker while another waiter blocked on the
//! same key -- a hard deadlock. Here three waiters share one key and two
//! more sit on distinct keys; single-element pushes wake exactly one
//! waiter each and everything completes well inside a bounded window.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

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
        sql_ts: std::sync::Arc::new(rdb::sql::tx::Oracle::new()),
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
    let mut conn_state = rdb::tx::session::ConnState::default();
    let mut ctx = command::Ctx {
        shared,
        prefix_key,
        args: argv,
        out: &mut out,
        close_conn: false,
        conn: &mut conn_state,
        wrote: false,
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
        waiters.push(tokio::spawn(
            async move { call(&s, "blpop", &[key, "5"]).await },
        ));
    }
    // Let every waiter reach its park before waking them.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Wake exactly the right number: one push per waiting element. The
    // replies may exceed :1 transiently -- a woken waiter pops
    // asynchronously from the push reply -- so only assert integer
    // length replies; final consistency is checked after the joins.
    for elem in ["e1", "e2", "e3"] {
        let reply = call(&shared, "rpush", &["hot", elem]).await;
        assert!(
            reply.starts_with(b":") && reply.ends_with(b"\r\n"),
            "{reply:?}"
        );
    }
    for (key, elem) in [("a1", "v1"), ("a2", "v2")] {
        let reply = call(&shared, "rpush", &[key, elem]).await;
        assert!(
            reply.starts_with(b":") && reply.ends_with(b"\r\n"),
            "{reply:?}"
        );
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
            assert!(
                text.starts_with("*2\r\n$3\r\nhot\r\n"),
                "same-key pop: {text:?}"
            );
            let elem = text.trim_start_matches("*2\r\n$3\r\nhot\r\n$2\r\n");
            assert_eq!(elem.len(), 4, "element plus CRLF: {text:?}");
            elem[..2].to_string()
        })
        .collect();
    hot_elems.sort();
    assert_eq!(
        hot_elems,
        vec!["e1", "e2", "e3"],
        "each waiter got one element"
    );

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
    assert_eq!(
        call(&shared, "rpush", &["k", "x"]).await,
        b":1\r\n".to_vec()
    );
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
        tokio::time::timeout(
            Duration::from_secs(5),
            call(&shared, "blpop", &["k", "0.5"])
        )
        .await
        .expect("timeout path completes"),
        b"*-1\r\n".to_vec()
    );
}

/// A flat three-element RESP array of bulk payloads.
fn arr3(a: &[u8], b: &[u8], c: &[u8]) -> Vec<u8> {
    let mut buf = b"*3\r\n".to_vec();
    for part in [a, b, c] {
        buf.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
        buf.extend_from_slice(part);
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// BLOCK 0 means FOREVER: with the key empty the BLPOP must still be
/// parked after 300ms (a regression turns it into an instant null),
/// then wake from a push with the popped pair -- all under a 10s cap.
#[tokio::test]
async fn blpop_zero_blocks_until_push() {
    let shared = Arc::new(shared_for("44112"));
    let s = Arc::clone(&shared);
    let waiter = tokio::spawn(async move { call(&s, "blpop", &["k", "0"]).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished(), "BLOCK 0 must park, not time out");
    assert_eq!(
        call(&shared, "rpush", &["k", "v"]).await,
        b":1\r\n".to_vec()
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .expect("pop completes")
            .expect("task"),
        arr2(b"k", b"v")
    );
    assert_eq!(call(&shared, "llen", &["k"]).await, b":0\r\n".to_vec());
}

/// BZPOPMIN with BLOCK 0: parked after 300ms, then a ZADD wakes it with
/// the (key, member, score) triple.
#[tokio::test]
async fn bzpopmin_zero_blocks_until_zadd() {
    let shared = Arc::new(shared_for("44113"));
    let s = Arc::clone(&shared);
    let waiter = tokio::spawn(async move { call(&s, "bzpopmin", &["z", "0"]).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished(), "BLOCK 0 must park, not time out");
    let added = call(&shared, "zadd", &["z", "1.5", "m"]).await;
    assert!(added.starts_with(b":"), "zadd reply: {added:?}");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .expect("pop completes")
            .expect("task"),
        arr3(b"z", b"m", b"1.5")
    );
    assert_eq!(call(&shared, "zcard", &["z"]).await, b":0\r\n".to_vec());
}

/// Wake-time race guard: dst is a valid (missing) list when BLMOVE 0
/// starts blocking, becomes a raw string while it is parked, and the
/// push that wakes it pops the element -- the restore path must put it
/// back onto src before the WRONGTYPE reply instead of dropping it.
#[tokio::test]
async fn blmove_restore_on_wake_time_wrongtype_dst() {
    let shared = Arc::new(shared_for("44114"));
    let s = Arc::clone(&shared);
    let waiter = tokio::spawn(async move {
        call(&s, "blmove", &["{g}src", "{g}dst", "LEFT", "LEFT", "0"]).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished(), "BLMOVE 0 must park on empty src");
    // dst turns wrong-type while the move is parked.
    assert_eq!(
        call(&shared, "set", &["{g}dst", "x"]).await,
        b"+OK\r\n".to_vec()
    );
    // The wake pops the element off src; dst cannot receive it.
    assert_eq!(
        call(&shared, "rpush", &["{g}src", "a"]).await,
        b":1\r\n".to_vec()
    );
    let reply = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("move completes")
        .expect("task");
    assert!(reply.starts_with(b"-WRONGTYPE"), "reply: {reply:?}");
    // The popped element was restored onto src (same LEFT end), intact.
    assert_eq!(
        call(&shared, "lrange", &["{g}src", "0", "-1"]).await,
        b"*1\r\n$1\r\na\r\n".to_vec()
    );
    assert_eq!(call(&shared, "llen", &["{g}src"]).await, b":1\r\n".to_vec());
}

/// A MULTI-element push must wake as many parked waiters as it has
/// elements: three BLPOPs on one key, then ONE `LPUSH k e1 e2 e3`. The
/// push notifies min(3 elements, 3 waiters) = 3; each woken waiter pops
/// exactly one element and the key drains to zero -- all inside 10s.
#[tokio::test]
async fn multi_element_lpush_wakes_all_parked_waiters() {
    let shared = Arc::new(shared_for("44115"));
    let mut waiters = Vec::new();
    for _ in 0..3 {
        let s = Arc::clone(&shared);
        waiters.push(tokio::spawn(async move {
            call(&s, "blpop", &["key", "5"]).await
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    for w in &waiters {
        assert!(!w.is_finished(), "all three BLPOPs must park first");
    }

    let reply = call(&shared, "lpush", &["key", "e1", "e2", "e3"]).await;
    assert_eq!(reply, b":3\r\n".to_vec(), "lpush reply");

    let replies = tokio::time::timeout(Duration::from_secs(10), async {
        let mut replies = Vec::new();
        for w in waiters {
            replies.push(w.await.expect("waiter task"));
        }
        replies
    })
    .await
    .expect("all three waiters wake within 10s of one multi-push");

    let mut elems: Vec<String> = replies
        .iter()
        .map(|r| {
            let text = String::from_utf8_lossy(r).into_owned();
            assert!(
                text.starts_with("*2\r\n$3\r\nkey\r\n"),
                "pair reply: {text:?}"
            );
            let elem = text.trim_start_matches("*2\r\n$3\r\nkey\r\n$2\r\n");
            assert_eq!(elem.len(), 4, "element plus CRLF: {text:?}");
            elem[..2].to_string()
        })
        .collect();
    elems.sort();
    assert_eq!(elems, vec!["e1", "e2", "e3"], "each waiter got one element");
    assert_eq!(call(&shared, "llen", &["key"]).await, b":0\r\n".to_vec());
}

/// The zset twin: two BZPOPMIN waiters parked on one key, then ONE
/// `ZADD` landing two members. The notify wakes min(2, 2) = 2; both
/// waiters pop one member each and the zset is fully drained.
#[tokio::test]
async fn zadd_multiple_members_wake_all_parked_bzpopmin() {
    let shared = Arc::new(shared_for("44116"));
    let mut waiters = Vec::new();
    for _ in 0..2 {
        let s = Arc::clone(&shared);
        waiters.push(tokio::spawn(async move {
            call(&s, "bzpopmin", &["z", "5"]).await
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    for w in &waiters {
        assert!(!w.is_finished(), "both BZPOPMINs must park first");
    }

    let added = call(&shared, "zadd", &["z", "1.5", "m1", "2.5", "m2"]).await;
    assert_eq!(added, b":2\r\n".to_vec(), "zadd reply");

    let replies = tokio::time::timeout(Duration::from_secs(10), async {
        let mut replies = Vec::new();
        for w in waiters {
            replies.push(w.await.expect("waiter task"));
        }
        replies
    })
    .await
    .expect("both waiters wake within 10s of one multi-zadd");

    let mut triples = replies.clone();
    triples.sort();
    assert_eq!(
        triples,
        vec![arr3(b"z", b"m1", b"1.5"), arr3(b"z", b"m2", b"2.5")],
        "each waiter popped one member"
    );
    assert_eq!(call(&shared, "zcard", &["z"]).await, b":0\r\n".to_vec());
}

/// Reversed SMOVEs (`a->b` vs `b->a`) hammer the same latch pair from
/// opposite sides across many rounds. SMOVE must lock the two latch
/// keys in SORTED byte order (ABBA rule): the pre-fix handler locked
/// them in argument order, so an interleaving where each task held one
/// latch and parked on the other deadlocked forever. Every reply is :0
/// or :1 and the whole exchange must finish inside a bounded window.
#[tokio::test]
async fn concurrent_reversed_smove_never_deadlocks() {
    let shared = Arc::new(shared_for("44117"));
    call(&shared, "sadd", &["{g}a", "m"]).await;
    call(&shared, "sadd", &["{g}b", "n"]).await;

    const ROUNDS: usize = 500;
    let forward = {
        let s = Arc::clone(&shared);
        tokio::spawn(async move {
            let mut replies = Vec::with_capacity(ROUNDS);
            for _ in 0..ROUNDS {
                replies.push(call(&s, "smove", &["{g}a", "{g}b", "m"]).await);
            }
            replies
        })
    };
    let backward = {
        let s = Arc::clone(&shared);
        tokio::spawn(async move {
            let mut replies = Vec::with_capacity(ROUNDS);
            for _ in 0..ROUNDS {
                replies.push(call(&s, "smove", &["{g}b", "{g}a", "m"]).await);
            }
            replies
        })
    };

    let (fwd, bwd) = tokio::time::timeout(Duration::from_secs(60), async {
        (
            forward.await.expect("forward smove task"),
            backward.await.expect("backward smove task"),
        )
    })
    .await
    .expect("reversed SMOVE pair must not deadlock on the latch pair");

    for r in fwd.iter().chain(bwd.iter()) {
        assert!(r == b":0\r\n" || r == b":1\r\n", "smove reply {r:?}");
    }
    // The member ping-ponged atomically: it lives on exactly one side
    // and the two cardinalities still sum to the initial total.
    let mut on_sides = 0i64;
    let mut cards = 0i64;
    for (key, args) in [("{g}a", "m"), ("{g}b", "m"), ("{g}a", "n"), ("{g}b", "n")] {
        let r = call(&shared, "sismember", &[key, args]).await;
        on_sides += if r == b":1\r\n" { 1 } else { 0 };
    }
    assert_eq!(on_sides, 2, "m and n each live on exactly one side");
    for key in ["{g}a", "{g}b"] {
        let r = call(&shared, "scard", &[key]).await;
        cards += std::str::from_utf8(&r[1..r.len() - 2])
            .unwrap()
            .parse::<i64>()
            .unwrap();
    }
    assert_eq!(cards, 2, "members m and n both survive");
}

/// Park-pool isolation: 600 forever-blocked BLPOPs must not stall
/// writes. Before the dedicated park pool, every park consumed a
/// tokio `spawn_blocking` slot; 512 forever-parks filled the whole
/// shared pool, the RocksDB fsync task never got a thread, and every
/// write hung. Now parks live on their own pool, so while they hold
/// >WORKERS slots the SET's fsync still completes well under 1s. The
/// pops are then woken (one LPUSH per key) so the test ends promptly.
#[tokio::test]
async fn park_pool_saturation_does_not_stall_writes() {
    let shared = Arc::new(shared_for("44118"));
    const PARKS: usize = 600;
    // One BLPOP per distinct key (same-slot would serialize nothing
    // here, but distinct keys also prove the pool fans out).
    let mut waiters = Vec::new();
    for i in 0..PARKS {
        let s = Arc::clone(&shared);
        waiters.push(tokio::spawn(async move {
            call(&s, "blpop", &[&format!("ppk{i}"), "0"]).await
        }));
    }
    // Let every pop reach its park: 600 > 512 pool workers saturates it
    // and leaves 88 jobs queued -- exactly the old starvation setup.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The write must land quickly even with the park pool full.
    let t0 = Instant::now();
    assert_eq!(
        call(&shared, "set", &["{pp}w", "v"]).await,
        b"+OK\r\n".to_vec()
    );
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "SET stalled behind parks: {:?}",
        t0.elapsed()
    );

    // Wake every parked pop so the test finishes promptly.
    for i in 0..PARKS {
        assert_eq!(
            call(&shared, "lpush", &[&format!("ppk{i}"), "e"]).await,
            b":1\r\n".to_vec()
        );
    }
    let drained = tokio::time::timeout(Duration::from_secs(10), async {
        for w in waiters {
            w.await.expect("parked pop task");
        }
    })
    .await;
    assert!(drained.is_ok(), "all parked pops must wake after LPUSH");
}
