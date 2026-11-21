//! Multi-store scenario #59 -- audit stream compaction: explicit-id
//! XADDs seed five audit events (the reply is the id as a single
//! bulk), XTRIM MINID compacts the head away (:N = removed entries),
//! XDEL retires single entries (:1 then :0), XREVRANGE replays
//! newest-first, a non-group XREAD drains the tail, and a MySQL
//! archive table takes the compacted ids. One REAL node, RESP + SQL.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, int_of, r, rf, stream_ids, FrontSpec};

#[tokio::test]
async fn audit_stream_trims_deletes_and_archives() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("sc59", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Seed: five audit events with explicit ids (single-bulk
    //    replies echo the id).
    for (id, what) in [("1-1", "login"), ("1-2", "read"), ("1-3", "write"), ("1-4", "read"), ("1-5", "logout")] {
        let reply = r(&a, &[b"XADD", b"sc/audit", id.as_bytes(), b"ev", what.as_bytes()]).await;
        assert!(reply.starts_with(b"$"), "{id}: {reply:?}");
    }
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"sc/audit"]).await), 5);

    // b) Compaction: everything before 1-3 leaves (XTRIM answers the
    //    removed count), 1-3 becomes the oldest survivor.
    assert_eq!(int_of(&r(&a, &[b"XTRIM", b"sc/audit", b"MINID", b"1-3"]).await), 2);
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"sc/audit"]).await), 3);
    let live = rf(&a, &[b"XRANGE", b"sc/audit", b"-", b"+"]).await;
    assert!(!live.contains("1-1") && !live.contains("1-2"), "{live}");
    assert!(live.contains("1-3") && live.contains("1-5"), "{live}");

    // c) Point deletes: 1-3 retires (:1), the retry misses (:0).
    assert_eq!(int_of(&r(&a, &[b"XDEL", b"sc/audit", b"1-3"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"XDEL", b"sc/audit", b"1-3"]).await), 0);
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"sc/audit"]).await), 2);

    // d) Replay order: XREVRANGE leads with the newest id, and a
    //    non-group XREAD drains the tail after 0.
    let rev = stream_ids(&rf(&a, &[b"XREVRANGE", b"sc/audit", b"+", b"-"]).await);
    assert_eq!(rev.first().map(String::as_str), Some("1-5"), "newest first");
    assert_eq!(rev.len(), 2);
    let tail = rf(&a, &[b"XREAD", b"STREAMS", b"sc/audit", b"0-0"]).await;
    assert!(tail.contains("sc/audit") && tail.contains("1-4") && tail.contains("1-5"), "{tail}");

    // e) The MySQL archive takes everything the stream shed.
    ddl(
        &mut c,
        "CREATE TABLE audit_archive (id VARCHAR(8) PRIMARY KEY, ev VARCHAR(32) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO audit_archive (id, ev) VALUES ('1-1','login'), ('1-2','read'), ('1-3','write')").await;
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM audit_archive").await, vec![3]);
}
