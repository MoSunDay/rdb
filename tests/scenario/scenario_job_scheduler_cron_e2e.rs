//! Multi-store scenario #54 -- a cron scheduler: due jobs ride a ZSET
//! whose members are cron specs in byte order (ZRANGEBYLEX windows the
//! pre-dawn batch and LIMIT pages the whole queue, ZLEXCOUNT sizes a
//! window and counts an empty queue 0, ZREMRANGEBYLEX retires the
//! executed window), every fire appends to a stream (XADD auto-ids),
//! XREVRANGE replays newest-first while XRANGE walks oldest-first, and
//! a MySQL run ledger keeps the audit trail. One REAL node, RESP+SQL.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{bulk_at, front_world, int_of, r, rf, stream_ids, FrontSpec};

#[tokio::test]
async fn cron_queue_pages_counts_retires_and_logs_fires() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("job54", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The queue: five cron specs as members, one shared score, so
    //    byte order is the schedule order (0x.. < 1x.. < 3x.. < 5x..).
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"ZADD",
                    b"{jb}queue",
                    b"0",
                    b"0 4 * * *:backup",
                    b"0",
                    b"0 4 * * *:vacuum",
                    b"0",
                    b"15 8 * * 1:report",
                    b"0",
                    b"30 8 * * 1:digest",
                    b"0",
                    b"59 23 * * *:dump"
                ]
            )
            .await
        ),
        5
    );

    // b) The pre-dawn batch: [0 (1 windows exactly the 04:00 pair;
    //    ZLEXCOUNT agrees; a missing queue counts 0.
    let dawn = rf(&a, &[b"ZRANGEBYLEX", b"{jb}queue", b"[0", b"(1"]).await;
    assert_eq!(bulk_at(dawn.as_bytes(), 0), "0 4 * * *:backup", "{dawn}");
    assert_eq!(bulk_at(dawn.as_bytes(), 1), "0 4 * * *:vacuum", "{dawn}");
    assert_eq!(
        int_of(&r(&a, &[b"ZLEXCOUNT", b"{jb}queue", b"[0", b"(1"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZLEXCOUNT", b"{jb}missing", b"-", b"+"]).await),
        0
    );

    // c) LIMIT pages the full range in byte order.
    let page = rf(
        &a,
        &[
            b"ZRANGEBYLEX",
            b"{jb}queue",
            b"-",
            b"+",
            b"LIMIT",
            b"0",
            b"2",
        ],
    )
    .await;
    assert_eq!(bulk_at(page.as_bytes(), 1), "0 4 * * *:vacuum", "{page}");

    // d) The Monday-morning window executes and retires: the two 8am
    //    jobs leave the queue ([1 (4), three remain.
    assert_eq!(
        int_of(&r(&a, &[b"ZREMRANGEBYLEX", b"{jb}queue", b"[1", b"(4"]).await),
        2,
        "report + digest retire"
    );
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"{jb}queue"]).await), 3);

    // e) The fire log: three fires append with auto ids (a slash
    //    stream name answers the id as a single bulk); XREVRANGE
    //    answers newest-first, XRANGE oldest-first (same id set).
    for job in ["backup", "vacuum", "dump"] {
        let reply = r(
            &a,
            &[
                b"XADD",
                b"jb/fire",
                b"*",
                b"job",
                job.as_bytes(),
                b"rc",
                b"0",
            ],
        )
        .await;
        assert!(
            reply.starts_with(b"$") && reply.contains(&b'-'),
            "{job}: {reply:?}"
        );
    }
    let rev = stream_ids(&rf(&a, &[b"XREVRANGE", b"jb/fire", b"+", b"-"]).await);
    let fwd = stream_ids(&rf(&a, &[b"XRANGE", b"jb/fire", b"-", b"+"]).await);
    assert_eq!(rev.len(), 3, "three fires");
    assert_eq!(rev[0], fwd[2], "XREVRANGE leads with the newest");
    assert_eq!(rev[2], fwd[0], "and ends with the oldest");
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"jb/fire"]).await), 3);

    // f) The MySQL run ledger mirrors the fire log.
    ddl(
        &mut c,
        "CREATE TABLE cron_runs (id BIGINT PRIMARY KEY, job VARCHAR(32) NOT NULL, rc INT NOT NULL)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO cron_runs (id, job, rc) VALUES (1,'backup',0), (2,'vacuum',0), (3,'dump',1)",
    )
    .await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM cron_runs WHERE rc = 0").await,
        vec![2]
    );
}
