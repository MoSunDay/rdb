//! Multi-store scenario #25 -- an election night tally across THREE
//! stores: ballots land in MySQL keyed by a synthetic `voter-race`
//! PRIMARY KEY (one vote per voter per race), a replayed ballot is
//! rejected 1062 and nothing double counts, the live tally runs in
//! Redis (HINCRBY per race) and must reconcile with the SQL GROUP BY,
//! and the ES front mirrors every ballot by the same doc id so the
//! PUT replay is idempotent and a region term-search count matches
//! SQL. One REAL node, three fronts.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use mysql_async::prelude::Queryable;
use scenario_common::{front_world, http, json_of, rb, FrontSpec};

#[tokio::test]
async fn election_ballots_dedupe_tally_and_region_search() {
    // a) One node: MySQL + ES fronts together.
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, a) = front_world("vote25", &spec).await;
    let es = front.node.es.clone();
    let mut c = connect_root(&front.node).await;

    // b) Ballot box: the synthetic `ballot_key` (`voter-race`) IS the
    // PRIMARY KEY, so one vote per voter per race is enforced by the
    // box itself, not by the client.
    ddl(
        &mut c,
        "CREATE TABLE ballots (id BIGINT PRIMARY KEY, ballot_key VARCHAR(64) NOT NULL, \
         voter VARCHAR(24), race VARCHAR(16), cand VARCHAR(16), region VARCHAR(8), \
         UNIQUE KEY uk_ballot (ballot_key))",
    )
    .await;
    // b0) ES needs the index before any doc lands (no auto-create).
    let (status, body) = http(
        &es,
        "PUT",
        "/ballots",
        Some(r#"{"mappings":{"properties":{"voter":{"type":"keyword"},"race":{"type":"keyword"},"cand":{"type":"keyword"},"region":{"type":"keyword"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create ballots index: {body}");

    // c) Seven voters cast ballots in the mayor race.
    let casts = [
        ("v1", "alice", "east"),
        ("v2", "alice", "east"),
        ("v3", "bob", "east"),
        ("v4", "bob", "west"),
        ("v5", "carol", "west"),
        ("v6", "alice", "west"),
        ("v7", "bob", "east"),
    ];
    let mut running: std::collections::HashMap<&str, i64> =
        [("alice", 0), ("bob", 0), ("carol", 0)].into_iter().collect();
    for (i, (voter, cand, region)) in casts.iter().enumerate() {
        let key = format!("{voter}-mayor");
        run(
            &mut c,
            &format!(
                "INSERT INTO ballots (id, ballot_key, voter, race, cand, region) \
                 VALUES ({i}, '{key}', '{voter}', 'mayor', '{cand}', '{region}')"
            ),
        )
        .await;
        // c0) The mirror PUT is idempotent on doc id = ballot_key.
        let doc = format!(
            "{{\"voter\":\"{voter}\",\"race\":\"mayor\",\"cand\":\"{cand}\",\"region\":\"{region}\"}}"
        );
        let (status, body) = http(&es, "PUT", &format!("/ballots/_doc/{key}"), Some(&doc)).await;
        assert_eq!(status, 201, "index {key}: {body}");
        // c1) Live tally: HINCRBY returns the NEW total, which must
        //     equal the running count of casts for that candidate.
        *running.get_mut(cand).unwrap() += 1;
        let n = scenario_common::int_of(
            &scenario_common::r(&a, &[b"HINCRBY", b"tally:mayor", cand.as_bytes(), b"1"]).await,
        );
        assert_eq!(n, running[cand], "tally for {cand} after this cast");
    }

    // d) Replay attack: v1 tries to vote again -- the box rejects the
    //    second INSERT (1062 on the ballot_key PK) and the tally must
    //    not move.
    run(&mut c, "BEGIN").await;
    c.query_drop(
        "INSERT INTO ballots (id, ballot_key, voter, race, cand, region) \
         VALUES (9, 'v1-mayor', 'v1', 'mayor', 'carol', 'east')",
    )
    .await
    .expect("the dup INSERT itself stages fine");
    let err = c
        .query_drop("COMMIT")
        .await
        .expect_err("duplicate ballot_key must be rejected at COMMIT");
    let msg = err.to_string();
    assert!(msg.contains("1062") || msg.contains("Duplicate") || msg.contains("conflict"), "{msg}");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM ballots").await, vec![7], "still seven ballots after replay");
    let alice: i64 = rb(&a, &[b"HGET", b"tally:mayor", b"alice"]).await.parse().unwrap();
    assert_eq!(alice, 3, "alice tally unmoved by replay");

    // e) Tally reconciliation: SQL GROUP BY vs the Redis hash fields.
    let sql_counts = ints(
        &mut c,
        "SELECT COUNT(*) FROM ballots WHERE race = 'mayor' GROUP BY cand ORDER BY cand",
    )
    .await;
    assert_eq!(sql_counts, vec![3, 3, 1], "alice 3 / bob 3 / carol 1");
    for (cand, want) in [("alice", 3), ("bob", 3), ("carol", 1)] {
        let got: i64 = rb(&a, &[b"HGET", b"tally:mayor", cand.as_bytes()]).await.parse().unwrap();
        assert_eq!(got, want, "redis tally for {cand}");
    }

    // f) Region search: ES term count == SQL count for the east box.
    let (status, _) = http(&es, "POST", "/ballots/_refresh", None).await;
    assert_eq!(status, 200, "refresh");
    let (status, body) = http(
        &es,
        "POST",
        "/ballots/_search",
        Some(r#"{"query":{"term":{"region":"east"}},"size":0}"#),
    )
    .await;
    assert_eq!(status, 200, "search: {body}");
    let hits = json_of(&body)
        .pointer("/hits/total/value")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM ballots WHERE region = 'east'").await,
        vec![4],
        "four east ballots in SQL"
    );
    assert_eq!(hits, 4, "ES mirrors exactly the east ballots");
}
