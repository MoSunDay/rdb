//! Multi-store scenario #11 -- a payment ledger pipeline, one story over
//! FOUR storage shapes: wallets are RESP strings (INCRBY top-ups), every
//! payment posts a double-entry D/C pair into a SQL ledger guarded by a
//! UNIQUE entry_no (idempotency: a replayed payment is refused at COMMIT
//! -- order_pipeline/sql_index 1062-at-commit semantics), events ride
//! the `pay/journal` stream consumed by XREADGROUP/XACK (lite_group),
//! and the end-of-day reconcile job is a DELAY-staged entry exchanged
//! by the sweep (lite_delay_proc: staged rows are XLEN-invisible and
//! delivered under FRESH minted ids). Final reconciliation: SUM(D) ==
//! SUM(C) == the ZSET receipt board, transfers conserve wallets.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, one, run};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{
    bulk_at, front_world, int_of, ok, r, rb, rf, stream_ids, until_contains, FrontSpec,
};

const SWEEP_MS: u64 = 200;

fn spec() -> FrontSpec {
    FrontSpec {
        mysql: true,
        extra_yaml: format!("lite:\n  delay_sweep_ms: {SWEEP_MS}\n"),
        ..Default::default()
    }
}

#[tokio::test]
async fn payment_ledger_double_entry_to_reconciliation() {
    let _serial = scenario_common::serial_guard().await;
    let (front, a) = front_world("pay11", &spec()).await;
    let mut c = connect_root(&front.node).await;

    // a) The ledger: double-entry rows; entry_no is globally UNIQUE.
    ddl(
        &mut c,
        "CREATE TABLE pay_ledger (id BIGINT PRIMARY KEY, entry_no VARCHAR(40) NOT NULL, \
         acct VARCHAR(24) NOT NULL, side CHAR(1) NOT NULL, amount INT NOT NULL, \
         UNIQUE KEY uk_entry (entry_no))",
    )
    .await;

    // b) Top up three wallets: alice 500, bob 300, carol 150.
    assert_eq!(
        int_of(&r(&a, &[b"INCRBY", b"wallet:alice", b"500"]).await),
        500
    );
    assert_eq!(
        int_of(&r(&a, &[b"INCRBY", b"wallet:bob", b"300"]).await),
        300
    );
    assert_eq!(
        int_of(&r(&a, &[b"INCRBY", b"wallet:carol", b"150"]).await),
        150
    );

    // c) Two transfers, each = one SQL D/C pair + two INCRBY moves +
    //    one journal event + one ZINCRBY receipt: carol pays bob 120,
    //    bob pays alice 50. (from_bal/to_bal are the post-move wallets.)
    let payments = [
        ("PAY-101", "carol", "bob", 120i64, "30", "420"),
        ("PAY-102", "bob", "alice", 50, "370", "550"),
    ];
    for (i, (no, from, to, amount, from_bal, to_bal)) in payments.iter().enumerate() {
        run(&mut c, "BEGIN").await;
        run(
            &mut c,
            &format!(
                "INSERT INTO pay_ledger (id, entry_no, acct, side, amount) VALUES \
                 ({}, '{}-D', '{}', 'D', {}), ({}, '{}-C', '{}', 'C', {})",
                2 * i + 1,
                no,
                from,
                amount,
                2 * i + 2,
                no,
                to,
                amount
            ),
        )
        .await;
        run(&mut c, "COMMIT").await;
        assert_eq!(
            int_of(
                &r(
                    &a,
                    &[
                        b"INCRBY",
                        &format!("wallet:{from}").into_bytes(),
                        format!("-{amount}").as_bytes()
                    ]
                )
                .await
            ),
            from_bal.parse::<i64>().unwrap(),
            "{from} debited"
        );
        assert_eq!(
            int_of(
                &r(
                    &a,
                    &[
                        b"INCRBY",
                        &format!("wallet:{to}").into_bytes(),
                        amount.to_string().as_bytes()
                    ]
                )
                .await
            ),
            to_bal.parse::<i64>().unwrap(),
            "{to} credited"
        );
        let event = r(
            &a,
            &[
                b"XADD",
                b"pay/journal",
                b"*",
                b"pay",
                no.as_bytes(),
                b"amount",
                amount.to_string().as_bytes(),
            ],
        )
        .await;
        assert!(bulk_at(&event, 0).contains('-'), "event {no} minted");
        let recv = r(
            &a,
            &[
                b"ZINCRBY",
                b"board:recv",
                amount.to_string().as_bytes(),
                to.as_bytes(),
            ],
        )
        .await;
        assert_eq!(bulk_at(&recv, 0), amount.to_string(), "{to} receipt");
    }

    // d) A replay of PAY-101 under the same entry_no stages fine but is
    //    refused at COMMIT (UNIQUE validated there); the ledger count
    //    stays 4 and NO wallet moved.
    run(&mut c, "BEGIN").await;
    run(
        &mut c,
        "INSERT INTO pay_ledger (id, entry_no, acct, side, amount) \
         VALUES (9, 'PAY-101-D', 'carol', 'D', 120)",
    )
    .await;
    let refused = c
        .query_drop("COMMIT")
        .await
        .expect_err("replayed entry_no must be refused at COMMIT");
    let msg = refused.to_string();
    assert!(
        msg.contains("1062") || msg.contains("Duplicate") || msg.contains("conflict"),
        "expected a dup refusal, got: {msg}"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM pay_ledger").await,
        vec![4]
    );
    assert_eq!(
        rb(&a, &[b"GET", b"wallet:carol"]).await,
        "30",
        "no double debit"
    );

    // e) The journal: the auditors group consumes both payment events
    //    exactly once and acks them; a second read is empty.
    ok(
        &r(
            &a,
            &[b"XGROUP", b"CREATE", b"pay/journal", b"auditors", b"0-0"],
        )
        .await,
        "xgroup",
    );
    let batch = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"auditors",
            b"c1",
            b"COUNT",
            b"10",
            b"STREAMS",
            b"pay/journal",
            b">",
        ],
    )
    .await;
    assert!(
        batch.contains("PAY-101") && batch.contains("PAY-102"),
        "{batch}"
    );
    let ids = stream_ids(&batch);
    assert_eq!(ids.len(), 2, "two events delivered: {batch}");
    let mut ack_args: Vec<&[u8]> = vec![b"XACK", b"pay/journal", b"auditors"];
    ids.iter().for_each(|id| ack_args.push(id.as_bytes()));
    assert_eq!(int_of(&r(&a, &ack_args).await), 2, "acked by minted ids");
    let again = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"auditors",
            b"c1",
            b"STREAMS",
            b"pay/journal",
            b">",
        ],
    )
    .await;
    assert!(!again.contains("PAY-"), "no redelivery: {again}");

    // f) Receipt board: bob(120) outranks alice(50); carol never appears.
    let board = rf(
        &a,
        &[b"ZREVRANGE", b"board:recv", b"0", b"-1", b"WITHSCORES"],
    )
    .await;
    let (b_ix, a_ix) = (
        board.find("bob").expect("bob"),
        board.find("alice").expect("alice"),
    );
    assert!(b_ix < a_ix, "bob outranks alice: {board}");
    assert!(
        !board.contains("carol"),
        "carol only paid, never received: {board}"
    );

    // g) End-of-day reconcile job: DELAY-staged (XLEN still 2), the
    //    sweep exchanges it (XLEN 3), the group delivers it under a
    //    FRESH id which is then acked.
    let staged = r(
        &a,
        &[
            b"XADD",
            b"pay/journal",
            b"*",
            b"DELAY",
            b"400",
            b"job",
            b"reconcile",
        ],
    )
    .await;
    assert!(bulk_at(&staged, 0).contains('-'), "staged receipt minted");
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"pay/journal"]).await),
        2,
        "staged row invisible"
    );
    until_contains(
        &a,
        &[b"XLEN", b"pay/journal"],
        ":3",
        "reconcile job exchanged",
        15,
    )
    .await;
    let job = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"auditors",
            b"c1",
            b"STREAMS",
            b"pay/journal",
            b">",
        ],
    )
    .await;
    assert!(job.contains("reconcile"), "{job}");
    let job_ids = stream_ids(&job);
    assert_eq!(job_ids.len(), 1, "one job delivered: {job}");
    assert_ne!(
        job_ids[0],
        bulk_at(&staged, 0),
        "delay exchange mints fresh ids"
    );
    assert_eq!(
        int_of(
            &r(
                &a,
                &[b"XACK", b"pay/journal", b"auditors", job_ids[0].as_bytes()]
            )
            .await
        ),
        1,
        "reconcile acked by its fresh id"
    );

    // h) Reconciliation: debits == credits == the receipt board == 170,
    //    and transfers conserved the wallet total at 950.
    let n = |v: &MVal| match v {
        MVal::Int(i) => *i,
        MVal::Bytes(b) => String::from_utf8_lossy(b).parse().unwrap(),
        other => panic!("cell {other:?}"),
    };
    let debits = n(&one(
        &mut c,
        "SELECT SUM(amount) FROM pay_ledger WHERE side = 'D'",
    )
    .await);
    let credits = n(&one(
        &mut c,
        "SELECT SUM(amount) FROM pay_ledger WHERE side = 'C'",
    )
    .await);
    assert_eq!(debits, 170, "SUM(D)");
    assert_eq!(credits, 170, "SUM(C) == SUM(D)");
    let mut board_sum = 0i64;
    for m in ["bob", "alice"] {
        board_sum += rb(&a, &[b"ZSCORE", b"board:recv", m.as_bytes()])
            .await
            .parse::<i64>()
            .unwrap();
    }
    assert_eq!(board_sum, 170, "ZSET board == SQL ledger");
    let mut total = 0i64;
    for (w, want) in [
        ("wallet:alice", 550i64),
        ("wallet:bob", 370),
        ("wallet:carol", 30),
    ] {
        let bal: i64 = rb(&a, &[b"GET", w.as_bytes()]).await.parse().unwrap();
        assert_eq!(bal, want, "{w} final balance");
        total += bal;
    }
    assert_eq!(total, 950, "transfers conserve the wallet total");
}
