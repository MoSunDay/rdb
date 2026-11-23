//! Multi-store scenario #26 -- a courier dispatch desk across THREE
//! stores: parcel orders stream through the RocksMQ HTTP front, the
//! dispatcher group drains them and claims each parcel in Redis with
//! `SET NX PX` (a racing second dispatcher loses the claim and the
//! parcel is never double-assigned), assignments land in MySQL with a
//! single-column UNIQUE order key, and acking empties the group
//! backlog. One REAL node, three fronts.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use mysql_async::prelude::Queryable;
use scenario_common::{codec::b64d, front_world, int_of, mq_post, ok, r, FrontSpec};

#[tokio::test]
async fn courier_dispatch_claims_assign_and_ack() {
    let _serial = scenario_common::serial_guard().await;
    // a) One node: RocksMQ HTTP + MySQL fronts together.
    let spec = FrontSpec {
        mysql: true,
        rocksmq: true,
        ..Default::default()
    };
    let (front, a) = front_world("courier26", &spec).await;
    let mq = front.rocksmq.clone();
    let mut c = connect_root(&front.node).await;

    // b) The fleet and the parcel manifest. The synthetic order key
    //    is UNIQUE, so one parcel can only ever be manifested once.
    ddl(
        &mut c,
        "CREATE TABLE couriers (id VARCHAR(8) PRIMARY KEY, zone VARCHAR(8))",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO couriers (id, zone) VALUES ('c1', 'north'), ('c2', 'south'), ('c3', 'north')",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE parcels (id BIGINT PRIMARY KEY, order_key VARCHAR(16) NOT NULL, \
         courier VARCHAR(8), zone VARCHAR(8), UNIQUE KEY uk_order (order_key))",
    )
    .await;

    // c) Five parcel orders hit the dispatch channel.
    let orders = [
        ("p-100", "north"),
        ("p-101", "south"),
        ("p-102", "north"),
        ("p-103", "south"),
        ("p-104", "north"),
    ];
    for (order, _) in orders {
        let (status, body) = mq_post(&mq, "/produce?channel=dispatch", order).await;
        assert_eq!(status, 200, "produce {order}: {body}");
    }

    // d) Dispatcher alpha drains the backlog, claims each parcel in
    //    Redis (`SET NX PX`: first claim wins, TTL leaves a stale
    //    reclaim window), and manifests the assignment.
    let (status, body) = mq_post(&mq, "/consume?channel=dispatch&group=desk&n=10", "").await;
    assert_eq!(status, 200, "consume: {body}");
    let items: Vec<(String, String)> = common::mq::msgs_of(&body)
        .into_iter()
        .map(|(id, b64)| (id, b64d(&b64)))
        .collect();
    assert_eq!(items.len(), 5, "all five parcels drained");
    for (i, (mid, order)) in items.iter().enumerate() {
        ok(
            &r(
                &a,
                &[
                    b"SET",
                    format!("claim:{order}").as_bytes(),
                    b"desk",
                    b"NX",
                    b"PX",
                    b"60000",
                ],
            )
            .await,
            "desk claims {order}",
        );
        let (_, zone) = orders.iter().find(|(o, _)| o == order).unwrap();
        let courier = if *zone == "north" { "c1" } else { "c2" };
        run(
            &mut c,
            &format!(
                "INSERT INTO parcels (id, order_key, courier, zone) VALUES ({i}, '{order}', '{courier}', '{zone}')"
            ),
        )
        .await;
        let (status, body) = mq_post(
            &mq,
            &format!("/ack?channel=dispatch&group=desk&id={mid}"),
            "",
        )
        .await;
        assert_eq!(status, 200, "ack {mid}: {body}");
    }
    assert_eq!(
        common::mq::pending_count(&mq, "dispatch", "desk").await,
        0,
        "desk acked everything"
    );

    // e) Dispatcher beta replays the same backlog (at-least-once red
    //    delivery): every claim is LOST (NX), so no second manifest
    //    row can appear and the fleet load is untouched.
    let (status, body) = mq_post(&mq, "/consume?channel=dispatch&group=beta&n=10", "").await;
    assert_eq!(status, 200, "replay consume: {body}");
    let replayed = common::mq::msgs_of(&body);
    let mut lost = 0;
    for (_, b64) in &replayed {
        let order = b64d(b64);
        let got = r(
            &a,
            &[
                b"SET",
                format!("claim:{order}").as_bytes(),
                b"beta",
                b"NX",
                b"PX",
                b"60000",
            ],
        )
        .await;
        if got.starts_with(b"$-1") || got.starts_with(b"*-1") || got.starts_with(b"-") {
            lost += 1;
        }
    }
    assert_eq!(lost, replayed.len(), "beta loses every claim");
    let err = c
        .query_drop(
            "INSERT INTO parcels (id, order_key, courier, zone) VALUES (9, 'p-100', 'c3', 'north')",
        )
        .await;
    assert!(
        err.is_err(),
        "manifest UNIQUE order_key refuses p-100 twice"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM parcels").await,
        vec![5],
        "exactly five manifest rows"
    );

    // f) The desk must survive a crash audit: every claim still holds
    //    its PX window and points at the desk owner.
    for (order, _) in &orders {
        let owner = scenario_common::rb(&a, &[b"GET", format!("claim:{order}").as_bytes()]).await;
        assert_eq!(owner, "desk", "{order} still claimed by desk");
        let ttl = int_of(&r(&a, &[b"TTL", format!("claim:{order}").as_bytes()]).await);
        assert!(ttl > 0 && ttl <= 60, "{order} claim TTL in window: {ttl}");
    }
    // g) Zone reconciliation: north parcels went to c1, south to c2.
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM parcels WHERE courier = 'c1'").await,
        vec![3],
        "c1 took the three north parcels"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM parcels WHERE courier = 'c2'").await,
        vec![2],
        "c2 took the two south parcels"
    );
}
