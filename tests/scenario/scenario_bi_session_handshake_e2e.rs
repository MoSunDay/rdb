//! Multi-store scenario #138 -- BI session handshake: a dashboard client
//! opens its session with the start-of-session script (SET NAMES in any
//! charset, autocommit probing, the isolation level echo on
//! @@transaction_isolation / @@tx_isolation, READ ONLY refused), reads the
//! server identity (@@version, @@version_comment, @@max_allowed_packet,
//! USER(), CONNECTION_ID(), DATABASE() before/after USE) and lands today's
//! KPI cards through a prepared INSERT with a CJK label, cross-checked
//! against a RESP hash dashboard. One REAL node, MySQL + RESP.
//! Deviation: `DATABASE()` answers the implicit default `rdb` before any
//! USE (MySQL: NULL; src/sql/exec/mod.rs:170-174); `SET NAMES`/charset
//! @@vars are a static utf8mb4 no-op, `SET autocommit = 0` / `SET
//! TRANSACTION READ ONLY` are refused 1235, a `SET GLOBAL TRANSACTION
//! ISOLATION LEVEL` echoes in the session (src/sql/parse/translate.rs:533-588,
//! src/sql/parse/session.rs:33-49, src/sql/exec/mod.rs:308-316,
//! src/sql/front/vars.rs:69-83).

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{
    connect_root, ddl, one, rows, run, server_error, ER_BAD_DB_ERROR, ER_NOT_SUPPORTED_YET,
};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, r, rb, FrontSpec};

/// One wire cell as text (`<NULL>` for the NULL variant).
fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

fn cells(row: &[MVal]) -> Vec<String> {
    row.iter().map(cell).collect()
}

/// `@@transaction_isolation, @@tx_isolation` of the session, as text.
async fn isolation(c: &mut mysql_async::Conn) -> Vec<String> {
    cells(&rows(c, "SELECT @@transaction_isolation, @@tx_isolation").await.remove(0))
}

/// The KPI cards of the day, Rust side: (card, total, label).
const CARDS: [(&str, i64, &str); 2] = [
    ("cards", 3, "\u{5361}\u{7247}\u{603B}\u{6570}"), // card total
    ("up", 1, "\u{1F680} \u{4E0A}\u{5347}"),          // rocket + rising
];
/// The prepared-statement card: bound parameters (total, label).
const BOUND: (i64, &str) = (10, "\u{53C2}\u{6570}\u{1F680}"); // params + rocket

/// `card, total, LENGTH(label), CHAR_LENGTH(label), day = CURDATE()` for
/// every card (ASCII card order, what `ORDER BY card` yields).
fn expected_cards() -> Vec<Vec<String>> {
    let mut all: Vec<(&str, i64, &str)> = CARDS.to_vec();
    all.push(("bound", BOUND.0, BOUND.1));
    all.sort_by_key(|c| c.0);
    all.iter()
        .map(|(card, total, label)| {
            vec![
                (*card).to_string(),
                total.to_string(),
                label.len().to_string(),
                label.chars().count().to_string(),
                "1".to_string(),
            ]
        })
        .collect()
}

#[tokio::test]
async fn session_handshake_echoes_names_autocommit_isolation() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let turso_dir = std::env::temp_dir().join(format!(
        "rdb-scenario-turso138-db-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&turso_dir);
    std::fs::create_dir_all(&turso_dir).expect("create turso db dir");
    let extra_yaml = format!(
        "turso_engine: true\nturso_db_dir: \"{}\"\n",
        turso_dir.display()
    );
    let spec = FrontSpec {
        mysql: true,
        extra_yaml,
        ..Default::default()
    };
    let (front, a) = front_world("sess138", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Hot block: the start-of-session script. Every client hello
    //    statement is an OK packet; the charset/autocommit/sql_mode/
    //    wait_timeout variables answer with the documented static values
    //    regardless of what was assigned.
    let t = scenario_common::perf::now();
    for sql in [
        "SET NAMES utf8mb4",
        "SET NAMES utf8mb4 COLLATE utf8mb4_general_ci",
        "SET NAMES latin1",
        "SET NAMES DEFAULT",
        "SET autocommit = 1",
        "SET @@autocommit = 1",
        "SET sql_mode = 'STRICT_TRANS_TABLES'",
        "SET wait_timeout = 60",
    ] {
        run(&mut c, sql).await;
    }
    assert_eq!(
        cells(&rows(&mut c, "SELECT @@character_set_client, @@character_set_connection, \
                             @@character_set_results, @@autocommit, @@sql_mode, @@wait_timeout")
            .await
            .remove(0)),
        ["utf8mb4", "utf8mb4", "utf8mb4", "1", "", "28800"],
        "session variables after the hello script"
    );
    //    Isolation: default REPEATABLE-READ; every accepted level echoes
    //    hyphenated on both names; the GLOBAL spelling lands in the
    //    session too (recorded deviation).
    assert_eq!(
        isolation(&mut c).await,
        ["REPEATABLE-READ", "REPEATABLE-READ"],
        "default isolation"
    );
    for (sql, echoed) in [
        (
            "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
            "READ-COMMITTED",
        ),
        ("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE", "SERIALIZABLE"),
        (
            "SET GLOBAL TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
            "READ-UNCOMMITTED",
        ),
        (
            "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
            "READ-COMMITTED",
        ),
    ] {
        run(&mut c, sql).await;
        assert_eq!(isolation(&mut c).await, [echoed, echoed], "{sql}");
    }
    run(&mut c, "SET SESSION TRANSACTION READ WRITE").await; // no-op
    assert_eq!(
        isolation(&mut c).await,
        ["READ-COMMITTED", "READ-COMMITTED"],
        "READ WRITE keeps the level"
    );
    //    Refused loudly (1235): autocommit off, READ ONLY, time_zone.
    for (sql, needle) in [
        ("SET autocommit = 0", "autocommit"),
        ("SET SESSION TRANSACTION READ ONLY", "READ ONLY"),
        ("SET time_zone = '+00:00'", "session/transaction settings"),
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {e:?}");
        assert!(e.message.contains(needle), "{sql}: {e:?}");
    }
    //    The refused statements left the session intact.
    assert_eq!(
        cells(&rows(&mut c, "SELECT @@version_comment, @@autocommit, @@max_allowed_packet")
            .await
            .remove(0)),
        ["rdb", "1", "67108864"],
        "identity variables"
    );
    assert_eq!(
        cell(&one(&mut c, "SELECT @@version").await),
        "8.0.32-rdb",
        "server version"
    );
    scenario_common::perf::within("sess138/handshake", t, Duration::from_secs(2));

    // b) Identity: USER() is the login at the peer, CURRENT_USER() and
    //    SESSION_USER() agree; CONNECTION_ID() is positive, stable on
    //    the connection and distinct on a second login, which starts
    //    over at the default isolation (the echo is session state).
    let ident = cells(&rows(
        &mut c,
        "SELECT USER(), CURRENT_USER(), SESSION_USER(), CONNECTION_ID(), DATABASE(), SCHEMA()",
    )
    .await
    .remove(0));
    assert!(ident[0].starts_with("root@"), "{ident:?}");
    assert_eq!(ident[1], ident[0], "CURRENT_USER()");
    assert_eq!(ident[2], ident[0], "SESSION_USER()");
    let id: i64 = ident[3].parse().expect("connection id digits");
    assert!(id > 0, "{ident:?}");
    assert_eq!(ident[4], "rdb", "DATABASE() before USE (deviation: not NULL)");
    assert_eq!(ident[5], "rdb", "SCHEMA()");
    assert_eq!(
        cell(&one(&mut c, "SELECT CONNECTION_ID()").await),
        id.to_string(),
        "connection id is stable"
    );
    let mut c2 = connect_root(&front.node).await;
    let id2: i64 = cell(&one(&mut c2, "SELECT CONNECTION_ID()").await)
        .parse()
        .expect("second connection id");
    assert!(id2 > 0 && id2 != id, "distinct connection ids: {id} vs {id2}");
    assert_eq!(
        isolation(&mut c2).await,
        ["REPEATABLE-READ", "REPEATABLE-READ"],
        "a fresh session starts at the default"
    );
    assert!(
        cell(&one(&mut c2, "SELECT USER()").await).starts_with("root@"),
        "second login user"
    );
    //    DATABASE() follows USE: the turso library, back to rdb, and an
    //    unknown name is 1049 without moving the session.
    ddl(&mut c, "CREATE DATABASE turso_sess138").await;
    run(&mut c, "USE turso_sess138").await;
    assert_eq!(
        cell(&one(&mut c, "SELECT DATABASE()").await),
        "turso_sess138",
        "DATABASE() after USE"
    );
    run(&mut c, "USE rdb").await;
    assert_eq!(cell(&one(&mut c, "SELECT DATABASE()").await), "rdb");
    let e = server_error(&mut c, "USE sess138_missing").await;
    assert_eq!(e.code, ER_BAD_DB_ERROR, "{e:?}");
    assert_eq!(
        cell(&one(&mut c, "SELECT DATABASE()").await),
        "rdb",
        "a failed USE keeps the current database"
    );

    // c) Today's KPI cards: two literal rows (CURDATE(), CJK labels) and
    //    one prepared INSERT with bound parameters. LENGTH counts bytes,
    //    CHAR_LENGTH counts characters -- expected values Rust side.
    ddl(&mut c, "CREATE TABLE bi_cards (day DATE, card VARCHAR(16) PRIMARY KEY, total INT, \
                 label VARCHAR(32))")
        .await;
    let values: Vec<String> = CARDS
        .iter()
        .map(|(card, total, label)| format!("(CURDATE(), '{card}', {total}, '{label}')"))
        .collect();
    run(
        &mut c,
        &format!(
            "INSERT INTO bi_cards (day, card, total, label) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
    let stmt = c
        .prep("INSERT INTO bi_cards (day, card, total, label) VALUES (CURDATE(), 'bound', ?, ?)")
        .await
        .expect("prepare bound card");
    let _: Vec<mysql_async::Row> = c
        .exec(&stmt, (BOUND.0, BOUND.1.to_string()))
        .await
        .expect("exec bound card");
    let got: Vec<Vec<String>> = rows(
        &mut c,
        "SELECT card, total, LENGTH(label), CHAR_LENGTH(label), day = CURDATE() \
         FROM bi_cards ORDER BY card",
    )
    .await
    .iter()
    .map(|r| cells(r))
    .collect();
    assert_eq!(got, expected_cards(), "KPI cards (Rust-side expectation)");

    // d) The RESP dashboard: one hash mirrors the SQL cards field by
    //    field; the SQL count is the hash length.
    let cards = rows(
        &mut c,
        "SELECT card, total FROM bi_cards WHERE day = CURDATE() ORDER BY card",
    )
    .await;
    assert_eq!(cards.len(), CARDS.len() + 1, "one card per row");
    for row in &cards {
        let (card, total) = (cell(&row[0]), cell(&row[1]));
        assert_eq!(
            int_of(&r(&a, &[b"HSET", b"dash:sess138", card.as_bytes(), total.as_bytes()]).await),
            1,
            "hset dash card"
        );
    }
    assert_eq!(
        int_of(&r(&a, &[b"HLEN", b"dash:sess138"]).await),
        (CARDS.len() + 1) as i64,
        "HLEN == SQL COUNT(*)"
    );
    assert_eq!(
        cell(&one(&mut c, "SELECT COUNT(*) FROM bi_cards WHERE day = CURDATE()").await),
        (CARDS.len() + 1).to_string()
    );
    assert_eq!(
        rb(&a, &[b"HGET", b"dash:sess138", b"bound"]).await,
        BOUND.0.to_string(),
        "bound card mirrored"
    );
    assert_eq!(
        rb(&a, &[b"HGET", b"dash:sess138", CARDS[0].0.as_bytes()]).await,
        CARDS[0].1.to_string(),
        "literal card mirrored"
    );
    scenario_common::perf::scenario("sess138", t0);
}
