//! Multi-store scenario #24 -- subscription renewals on the lite MQ:
//! each subscriber's renewal is a DELAY-staged job (XLEN-invisible
//! until the sweep exchanges it), the billing group delivers one at a
//! time -- active users get +30 service days (INCRBY) and one invoice
//! row guarded by a single-column UNIQUE(user, cycle) key so a replayed
//! billing run is refused at COMMIT, a churned subscriber is SMOVE'd to
//! the churn pool instead (same-slot hash-tagged Sets) -- and every
//! delivery is acked under its FRESH minted id.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, one, run};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, ok, r, rb, rf, stream_ids, until_contains, FrontSpec};

const SWEEP_MS: u64 = 200;

fn spec() -> FrontSpec {
    FrontSpec {
        mysql: true,
        extra_yaml: format!("lite:\n  delay_sweep_ms: {SWEEP_MS}\n"),
        ..Default::default()
    }
}

#[tokio::test]
async fn subscription_renewals_churn_and_idempotent_billing() {
    let (front, a) = front_world("subs24", &spec()).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE invoices (id BIGINT PRIMARY KEY, user_cycle VARCHAR(16) NOT NULL, \
         user VARCHAR(8) NOT NULL, cycle INT NOT NULL, amount INT NOT NULL, \
         UNIQUE KEY uk_user_cycle (user_cycle))",
    )
    .await;

    // a) The plan and the roster: three actives, each seeded with 30
    //    service days; u3 filed a churn request before the wave.
    assert_eq!(int_of(&r(&a, &[b"HSET", b"plan:pro", b"price", b"99", b"term", b"30"]).await), 2);
    assert_eq!(int_of(&r(&a, &[b"SADD", b"{subs}:active", b"u1", b"u2", b"u3"]).await), 3);
    assert_eq!(int_of(&r(&a, &[b"SADD", b"{subs}:churn", b"u3"]).await), 1, "u3 wants out");
    for u in ["u1", "u2", "u3"] {
        assert_eq!(int_of(&r(&a, &[b"INCRBY", &format!("svc:{u}").into_bytes(), b"30"]).await), 30);
    }

    // b) Stage the renewal wave: three DELAY jobs, XLEN stays 0.
    for u in ["u1", "u2", "u3"] {
        let staged = r(&a, &[b"XADD", b"sub/renew", b"*", b"DELAY", b"400", b"user", u.as_bytes()]).await;
        assert!(scenario_common::bulk_at(&staged, 0).contains('-'), "staged {u}");
    }
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"sub/renew"]).await), 0, "wave staged, nothing due");
    ok(&r(&a, &[b"XGROUP", b"CREATE", b"sub/renew", b"billers", b"0-0"]).await, "billing group");
    until_contains(&a, &[b"XLEN", b"sub/renew"], ":3", "renewal wave exchanged", 15).await;

    // c) Deliver one renewal at a time: active -> +30 days + one
    //    invoice; churned u3 -> SMOVE to the churn pool, nothing billed.
    let mut billed = 0i64;
    for _ in 0..3 {
        let one = rf(
            &a,
            &[b"XREADGROUP", b"GROUP", b"billers", b"w1", b"COUNT", b"1", b"STREAMS", b"sub/renew", b">"],
        )
        .await;
        let id = stream_ids(&one)[0].clone();
        let user = ["u1", "u2", "u3"]
            .iter()
            .find(|u| one.contains(*u))
            .expect("delivery names its user")
            .to_string();
        let churned = int_of(&r(&a, &[b"SISMEMBER", b"{subs}:churn", user.as_bytes()]).await);
        if churned == 0 {
            billed += 1;
            assert_eq!(
                int_of(&r(&a, &[b"INCRBY", &format!("svc:{user}").into_bytes(), b"30"]).await),
                60,
                "{user} extended to 60 days"
            );
            run(&mut c, &format!(
                "INSERT INTO invoices (id, user_cycle, user, cycle, amount) \
                 VALUES ({billed}, '{user}-202611', '{user}', 202611, 99)"
            ))
            .await;
        } else {
            assert_eq!(user, "u3", "only u3 churned");
            assert_eq!(
                int_of(&r(&a, &[b"SMOVE", b"{subs}:active", b"{subs}:churn", b"u3"]).await),
                1,
                "u3 parked in the churn pool"
            );
        }
        assert_eq!(int_of(&r(&a, &[b"XACK", b"sub/renew", b"billers", id.as_bytes()]).await), 1);
    }
    assert_eq!(billed, 2, "two renewals billed, one churned");

    // d) The billing replay: the same user+cycle invoice stages fine
    //    but COMMIT refuses it; nothing double-charges.
    run(&mut c, "BEGIN").await;
    run(&mut c, "INSERT INTO invoices (id, user_cycle, user, cycle, amount) VALUES (9, 'u1-202611', 'u1', 202611, 99)").await;
    let err = c
        .query_drop("COMMIT")
        .await
        .expect_err("replayed invoice must be refused at COMMIT");
    let msg = err.to_string();
    assert!(msg.contains("1062") || msg.contains("Duplicate") || msg.contains("conflict"), "{msg}");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM invoices").await, vec![2], "two invoices exactly");
    assert_eq!(rb(&a, &[b"GET", b"svc:u1"]).await, "60", "no double extension");

    // e) Reconciliation: roster, pools, days, and money agree.
    assert_eq!(int_of(&r(&a, &[b"SCARD", b"{subs}:active"]).await), 2, "u1+u2 remain");
    assert_eq!(int_of(&r(&a, &[b"SISMEMBER", b"{subs}:churn", b"u3"]).await), 1);
    assert_eq!(rb(&a, &[b"GET", b"svc:u3"]).await, "30", "churned user never extended");
    let n = |v: &MVal| match v {
        MVal::Int(i) => *i,
        MVal::Bytes(b) => String::from_utf8_lossy(b).parse().unwrap(),
        other => panic!("cell {other:?}"),
    };
    let revenue = n(&one(&mut c, "SELECT SUM(amount) FROM invoices").await);
    assert_eq!(revenue, 198, "2 x 99");
    assert_eq!(
        ints(&mut c, "SELECT cycle FROM invoices ORDER BY id").await,
        vec![202611, 202611],
        "one cycle per billed user"
    );
}
