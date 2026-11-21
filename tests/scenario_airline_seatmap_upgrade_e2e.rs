//! Multi-store scenario #41 -- airline seat lock + upgrade auction:
//! HSETNX claims a seat exactly once, the miles waitlist is a ZSET,
//! the top-mileage upgrade pops off with ZPOPMAX, standby flyers
//! rotate onto the confirmed list with LMOVE, LPOS verifies the
//! boarding position, and MySQL keeps the bookings ledger every RESP
//! mutation reconciles against. One REAL node, RESP + MySQL.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{bulk_at, front_world, int_of, r, rb, rf, rt, FrontSpec};

#[tokio::test]
async fn seatmap_locks_waitlist_and_standby_rotation() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("airline41", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The seat map: HSETNX claims each seat once; a second claim on
    //    the same seat loses (:0) and writes nothing.
    assert_eq!(
        int_of(&r(&a, &[b"HSETNX", b"seat:map", b"12A", b"kim"]).await),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"HSETNX", b"seat:map", b"12A", b"lee"]).await),
        0,
        "second claim loses"
    );
    assert_eq!(rb(&a, &[b"HGET", b"seat:map", b"12A"]).await, "kim");
    assert_eq!(int_of(&r(&a, &[b"HSETNX", b"seat:map", b"12B", b"lee"]).await), 1);

    // b) Bookings ledger in MySQL mirrors the seat map.
    ddl(
        &mut c,
        "CREATE TABLE bookings (id BIGINT PRIMARY KEY, seat VARCHAR(8) NOT NULL, \
         flyer VARCHAR(32) NOT NULL, status VARCHAR(16) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO bookings (id, seat, flyer, status) \
                 VALUES (1, '12A', 'kim', 'confirmed'), (2, '12B', 'lee', 'confirmed')").await;

    // c) The upgrade waitlist: miles buy priority; the top flyer pops.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"wait:upgrade", b"54000", b"kim", b"81000", b"lee", b"12000", b"pat"]).await),
        3
    );
    let grant = rf(&a, &[b"ZPOPMAX", b"wait:upgrade"]).await;
    assert!(grant.contains("lee") && grant.contains("81000"), "top miles pop: {grant}");
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"wait:upgrade"]).await), 2);
    run(&mut c, "INSERT INTO bookings (id, seat, flyer, status) VALUES (3, 'upgrade', 'lee', 'upgraded')").await;

    // d) Standby rotation: LMOVE walks the head of the standby list
    //    onto the tail of the confirmed list, one seat per hop.
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{sb}standby", b"pat", b"mel", b"joe"]).await), 3);
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{sb}confirmed", b"kim", b"lee"]).await), 2);
    assert_eq!(rb(&a, &[b"LMOVE", b"{sb}standby", b"{sb}confirmed", b"LEFT", b"RIGHT"]).await, "pat");
    assert_eq!(rb(&a, &[b"LMOVE", b"{sb}standby", b"{sb}confirmed", b"LEFT", b"RIGHT"]).await, "mel");
    let board = rf(&a, &[b"LRANGE", b"{sb}confirmed", b"0", b"-1"]).await;
    let order: Vec<String> = (0..4).map(|i| bulk_at(board.as_bytes(), i)).collect();
    assert_eq!(order, vec!["kim", "lee", "pat", "mel"], "rotation preserved order");
    assert_eq!(rb(&a, &[b"LINDEX", b"{sb}standby", b"0"]).await, "joe");

    // e) LPOS gives joe's boarding position (0-based in the standby).
    assert_eq!(int_of(&r(&a, &[b"LPOS", b"{sb}standby", b"joe"]).await), 0);
    assert_eq!(rb(&a, &[b"LPOS", b"{sb}confirmed", b"zzz"]).await, "", "missing member is nil");

    // f) Ledger reconciles: two confirmed seats + one upgrade + two
    //    rotated standby flyers on the RESP side.
    run(&mut c, "INSERT INTO bookings (id, seat, flyer, status) \
                 VALUES (4, 'standby', 'pat', 'confirmed'), (5, 'standby', 'mel', 'confirmed')").await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM bookings WHERE status = 'confirmed'").await,
        vec![4],
        "12A + 12B + pat + mel"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM bookings WHERE flyer = 'joe'").await,
        vec![0],
        "joe never made the ledger"
    );
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"{sb}confirmed"]).await), 4);
    assert_eq!(rt(&a, &[b"TYPE", b"seat:map"]).await.trim_start_matches('+'), "hash");
}
