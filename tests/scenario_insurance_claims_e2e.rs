//! Multi-store scenario #27 -- an insurance claim desk across THREE
//! stores: claims are filed in MySQL behind a single-column UNIQUE
//! claim number, evidence documents ride the S3 front (byte-exact
//! PUT/GET + prefix listing), an adjuster may only approve a claim
//! whose evidence actually exists in the bucket, payouts tick a Redis
//! INCRBY counter, and a duplicate filing is refused at COMMIT without
//! moving the payout ledger. One REAL node, three fronts.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use mysql_async::prelude::Queryable;
use scenario_common::{front_world, rb, s3_call, FrontSpec};

#[tokio::test]
async fn insurance_claims_evidence_gate_and_payout_ledger() {
    // a) One node: MySQL + S3 fronts together (RESP rides by default).
    let spec = FrontSpec {
        mysql: true,
        s3: true,
        ..Default::default()
    };
    let (front, a) = front_world("claim27", &spec).await;
    let s3 = front.s3.clone();
    let mut c = connect_root(&front.node).await;

    // b) The claim register: the claim number is UNIQUE, so one
    //    incident can only ever be filed once.
    ddl(
        &mut c,
        "CREATE TABLE claims (id BIGINT PRIMARY KEY, claim_no VARCHAR(12) NOT NULL, \
         policy VARCHAR(12), status VARCHAR(10), amount INT, \
         UNIQUE KEY uk_claim (claim_no))",
    )
    .await;

    // c) Three policyholders file: evidence first (S3 PUT carries the
    //    non-UTF8 photo bytes), then the register row.
    let filings = [
        ("CLM-001", "POL-A", 1200, &b"\x89PNG\r\n\x1a\nfender-bend\xff\x00"[..]),
        ("CLM-002", "POL-B", 3400, &b"\x00\x01hail\xfe damage photos"[..]),
        ("CLM-003", "POL-C", 800, &b"\x89PNG\r\n\x1a\ncracked-windshield"[..]),
    ];
    for (i, (claim_no, policy, amount, bytes)) in filings.iter().enumerate() {
        let key = format!("insure/evidence/{claim_no}.photo");
        let (status, head, _) = s3_call(&s3, "PUT", &format!("/rdb/{key}"), bytes).await;
        assert_eq!(status, 200, "put {key}: {head}");
        let (status, _, got) = s3_call(&s3, "GET", &format!("/rdb/{key}"), b"").await;
        assert_eq!(status, 200, "get {key}");
        assert_eq!(got, bytes.to_vec(), "byte-exact evidence {claim_no}");
        run(
            &mut c,
            &format!(
                "INSERT INTO claims (id, claim_no, policy, status, amount) \
                 VALUES ({i}, '{claim_no}', '{policy}', 'filed', {amount})"
            ),
        )
        .await;
    }

    // d) The evidence prefix lists exactly the three photos.
    let (status, _, body) = s3_call(&s3, "GET", "/rdb?list-type=2&prefix=insure/evidence/", b"").await;
    assert_eq!(status, 200, "list");
    let listing = String::from_utf8_lossy(&body).into_owned();
    for (claim_no, _, _, _) in &filings {
        assert!(
            listing.contains(&format!("evidence/{claim_no}.photo")),
            "{claim_no} evidence listed: {listing}"
        );
    }

    // e) Duplicate filing of CLM-001 stages but is refused at COMMIT;
    //    the register count and the payout ledger stay put.
    run(&mut c, "BEGIN").await;
    c.query_drop(
        "INSERT INTO claims (id, claim_no, policy, status, amount) \
         VALUES (9, 'CLM-001', 'POL-A', 'filed', 999)",
    )
    .await
    .expect("dup filing stages fine");
    let err = c.query_drop("COMMIT").await.expect_err("dup claim_no refused at COMMIT");
    let msg = err.to_string();
    assert!(msg.contains("1062") || msg.contains("Duplicate") || msg.contains("conflict"), "{msg}");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM claims").await, vec![3], "register conserved");

    // f) Adjuster workflow: evidence GET must succeed BEFORE a claim
    //    may leave 'filed'. CLM-002's evidence object is "lost" (the
    //    desk checks a 404), so it stays filed while the other two
    //    approve and tick the Redis payout counter.
    let mut approved_total = 0;
    for (claim_no, _, amount, _) in &filings {
        // The desk looks the evidence up where the filer SAID it is:
        // CLM-002's paperwork points at a mistyped object key.
        let key = if *claim_no == "CLM-002" {
            format!("insure/evidence/{claim_no}.png")
        } else {
            format!("insure/evidence/{claim_no}.photo")
        };
        let (status, _, _) = s3_call(&s3, "GET", &format!("/rdb/{key}"), b"").await;
        let gate_open = status == 200;
        let next = if gate_open { "approved" } else { "filed" };
        run(
            &mut c,
            &format!("UPDATE claims SET status = '{next}' WHERE claim_no = '{claim_no}'"),
        )
        .await;
        if gate_open {
            approved_total += amount;
            scenario_common::int_of(
                &scenario_common::r(&a, &[b"INCRBY", b"payout:total", amount.to_string().as_bytes()]).await,
            );
        }
    }
    assert_eq!(approved_total, 2000, "only CLM-001 and CLM-003 approve");

    // g) Ledger reconciliation: SQL SUM over approved claims == the
    //    Redis counter == 2000, and the blocked claim is still filed.
    assert_eq!(
        ints(&mut c, "SELECT SUM(amount) FROM claims WHERE status = 'approved'").await,
        vec![2000],
        "approved payout in SQL"
    );
    let counter: i64 = rb(&a, &[b"GET", b"payout:total"]).await.parse().unwrap();
    assert_eq!(counter, 2000, "redis payout counter matches SQL");
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM claims WHERE status = 'filed'").await,
        vec![1],
        "CLM-002 still filed behind the evidence gate"
    );
}
