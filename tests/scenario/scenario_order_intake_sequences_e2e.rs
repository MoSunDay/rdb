//! Multi-store scenario #154 -- an order intake desk: orders take AUTO_INCREMENT ids, TRUNCATE
//! resets the counter, a batch import rides ONE prepared typed statement plus a SAVEPOINT that
//! rolls back only the bad row, and a RESP mirror key holds the last id the desk handed out.
//! One REAL node, MySQL + RESP.
//! Deviation: an auto-allocating statement reserves a 64-wide id batch; a unique-key rejection
//! burns its whole reservation (after ids 1,2,3 the next id is 65, after a rejected duplicate
//! 129) while a NOT NULL rejection burns nothing (src/sql/exec/sequence.rs:37,84-88,
//! src/sql/exec/write.rs:125); LAST_INSERT_ID() answers the FIRST id of the statement, 1 for
//! the 3-row intake (sequence.rs:46-47, write.rs:127-128); `CREATE TABLE t (id INT AUTO_INCREMENT)`
//! is 1235 "need a PRIMARY KEY" (src/sql/parse/translate.rs:661-668); SQL-level PREPARE/EXECUTE are
//! absent (1064/1235), a wrong parameter type is 1136 (src/sql/exec/expr.rs:770-776) and an unknown
//! table answers 1146 (src/sql/parse/error.rs:84-88), not the brief's 1305.

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, one, run, server_error};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{front_world, ok, r, rb, FrontSpec};

/// MySQL 1048 (ER_BAD_NULL_ERROR): a NOT NULL column left empty.
const ER_BAD_NULL: u16 = 1048;
/// MySQL 1062 (ER_DUP_ENTRY).
const ER_DUP_ENTRY: u16 = 1062;
/// MySQL 1064 (ER_PARSE_ERROR).
const ER_PARSE_ERROR: u16 = 1064;
/// MySQL 1075 (ER_WRONG_AUTO_KEY).
const ER_WRONG_AUTO_KEY: u16 = 1075;
/// MySQL 1136 (ER_WRONG_VALUE_COUNT_ON_ROW).
const ER_WRONG_VALUE_COUNT: u16 = 1136;
/// MySQL 1146 (ER_NO_SUCH_TABLE).
const ER_NO_SUCH_TABLE: u16 = 1146;
/// MySQL 1235 (ER_NOT_SUPPORTED_YET).
const ER_NOT_SUPPORTED: u16 = 1235;
/// MySQL 1305 (ER_SP_DOES_NOT_EXIST): an unknown savepoint.
const ER_UNKNOWN_SAVEPOINT: u16 = 1305;
/// Ids one auto-allocating statement reserves ahead (sequence.rs:37).
const RESERVE: i64 = 64;

/// The intake batch, Rust side: (customer, amount).
const BATCH: [(&str, i32); 3] = [("acme", 120), ("globex", 45), ("initech", 310)];

/// The import batch: one good row, one bad row the savepoint discards.
const GOOD_IMPORT: (&str, i32) = ("umbrella", 80);
const BAD_IMPORT: (&str, i32) = ("cyberdyne", 999);

/// One wire cell as text.
fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

/// Import one order through the prepared typed statement.
async fn import(
    c: &mut mysql_async::Conn,
    st: &mysql_async::Statement,
    customer: &str,
    amount: i32,
) {
    c.exec::<mysql_async::Row, _, _>(st, (customer, amount))
        .await
        .unwrap_or_else(|e| panic!("import {customer}: {e}"));
}

/// Mirror the desk's last id into RESP and read it back.
async fn mirror(a: &str, last: i64) -> String {
    ok(
        &r(a, &[b"SET", b"order:seq", last.to_string().as_bytes()]).await,
        "mirror the last id",
    );
    rb(a, &[b"GET", b"order:seq"]).await
}

/// After an intake: the RESP mirror must equal SQL's last id.
async fn sync_mirror(c: &mut mysql_async::Conn, a: &str, what: &str) {
    let last = ints(c, "SELECT MAX(id) FROM orders").await[0];
    assert_eq!(mirror(a, last).await, last.to_string(), "{what}");
}

#[tokio::test]
async fn order_intake_sequences_and_savepoints() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("seq154", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Hot block: the desk opens. One intake batch of three orders
    //    takes ids 1,2,3; LAST_INSERT_ID() answers the FIRST id of the
    //    statement and MAX(id) the last one handed out.
    let t = scenario_common::perf::now();
    ddl(
        &mut c,
        "CREATE TABLE orders (id INT AUTO_INCREMENT PRIMARY KEY, \
         customer VARCHAR(16) NOT NULL, amount INT NOT NULL, \
         UNIQUE KEY uq_customer (customer))",
    )
    .await;
    let intake: Vec<String> = BATCH
        .iter()
        .map(|(customer, amount)| format!("('{customer}', {amount})"))
        .collect();
    let batch_sql = format!("INSERT INTO orders (customer, amount) VALUES {}", intake.join(", "));
    run(&mut c, &batch_sql).await;
    let ids = ints(&mut c, "SELECT id FROM orders ORDER BY id").await;
    assert_eq!(ids, vec![1, 2, 3], "consecutive ids inside one batch");
    assert_eq!(
        cell(&one(&mut c, "SELECT LAST_INSERT_ID()").await),
        "1",
        "LAST_INSERT_ID() is the first id of the statement (deviation)"
    );
    assert_eq!(
        ints(&mut c, "SELECT MAX(id) FROM orders").await[0],
        BATCH.len() as i64,
        "the last id handed out"
    );
    sync_mirror(&mut c, &a, "the RESP mirror holds the last id").await;

    // b) A batch import rides ONE prepared typed statement and a
    //    savepoint: the good row stays, the bad row is rolled back.
    let st = c
        .prep("INSERT INTO orders (customer, amount) VALUES (?, ?)")
        .await
        .expect("prepare the import statement");
    run(&mut c, "START TRANSACTION").await;
    run(&mut c, "SAVEPOINT batch").await;
    import(&mut c, &st, GOOD_IMPORT.0, GOOD_IMPORT.1).await;
    run(&mut c, "SAVEPOINT bad_row").await;
    import(&mut c, &st, BAD_IMPORT.0, BAD_IMPORT.1).await;
    assert_eq!(
        cell(&one(&mut c, "SELECT COUNT(*) FROM orders").await),
        "5",
        "the staged bad row is visible to its own transaction"
    );
    run(&mut c, "ROLLBACK TO bad_row").await;
    assert_eq!(
        cell(&one(
            &mut c,
            "SELECT COUNT(*) FROM orders WHERE customer = 'cyberdyne'"
        )
        .await),
        "0",
        "ROLLBACK TO dropped only the bad row"
    );
    run(&mut c, "RELEASE SAVEPOINT bad_row").await;
    run(&mut c, "COMMIT").await;
    let imported = ints(
        &mut c,
        "SELECT id FROM orders WHERE customer = 'umbrella'",
    )
    .await;
    assert_eq!(imported.len(), 1, "the good import row committed");
    assert_eq!(
        cell(&one(
            &mut c,
            "SELECT COUNT(*) FROM orders WHERE customer = 'cyberdyne'"
        )
        .await),
        "0",
        "the rolled-back row never landed"
    );
    sync_mirror(&mut c, &a, "mirror after the import").await;
    scenario_common::perf::within("seq154/intake-import", t, Duration::from_secs(2));
    let e = server_error(&mut c, "ROLLBACK TO bad_row").await;
    assert_eq!(e.code, ER_UNKNOWN_SAVEPOINT, "released savepoint: {e:?}");

    // c) A rejected order burns id space. A NOT NULL violation is caught
    //    BEFORE allocation (the next id only pays the reservation step),
    //    while a unique-key rejection lands AFTER allocation and burns the
    //    whole 64-wide reservation.
    ddl(&mut c, "TRUNCATE TABLE orders").await;
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM orders").await, vec![0]);
    run(&mut c, &batch_sql).await;
    let e = server_error(
        &mut c,
        "INSERT INTO orders (customer) VALUES ('no-amount')",
    )
    .await;
    assert_eq!(e.code, ER_BAD_NULL, "not null: {e:?}");
    run(&mut c, "INSERT INTO orders (customer, amount) VALUES ('after-null', 7)").await;
    assert_eq!(
        ints(&mut c, "SELECT id FROM orders WHERE customer = 'after-null'").await,
        vec![1 + RESERVE],
        "a pre-allocation rejection burns no id"
    );
    ddl(&mut c, "TRUNCATE TABLE orders").await;
    run(&mut c, &batch_sql).await;
    let e = server_error(
        &mut c,
        &format!("INSERT INTO orders (customer, amount) VALUES ('{}', 99)", BATCH[0].0),
    )
    .await;
    assert_eq!(e.code, ER_DUP_ENTRY, "duplicate order: {e:?}");
    assert!(e.message.contains("uq_customer"), "{e:?}");
    run(&mut c, "INSERT INTO orders (customer, amount) VALUES ('after-dup', 7)").await;
    assert_eq!(
        ints(&mut c, "SELECT id FROM orders WHERE customer = 'after-dup'").await,
        vec![1 + 2 * RESERVE],
        "the rejected order burned its whole reservation (deviation)"
    );
    sync_mirror(&mut c, &a, "mirror after the rejected order").await;

    // d) TRUNCATE resets the counter -- and is refused inside a
    //    transaction like every other DDL.
    ddl(&mut c, "TRUNCATE TABLE orders").await;
    run(&mut c, "INSERT INTO orders (customer, amount) VALUES ('fresh', 1)").await;
    assert_eq!(ints(&mut c, "SELECT id FROM orders").await, vec![1], "truncate reset");
    sync_mirror(&mut c, &a, "mirror after the truncate reset").await;
    run(&mut c, "START TRANSACTION").await;
    let e = server_error(&mut c, "TRUNCATE TABLE orders").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED, "truncate in a txn: {e:?}");
    assert!(e.message.contains("inside a transaction"), "{e:?}");
    run(&mut c, "ROLLBACK").await;

    // e) The DDL guard: the auto column must be THE primary key.
    let e = server_error(&mut c, "CREATE TABLE bad_seq (id INT AUTO_INCREMENT)").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED, "no primary key: {e:?}");
    assert!(e.message.contains("need a PRIMARY KEY"), "{e:?}");
    let e = server_error(
        &mut c,
        "CREATE TABLE bad_seq (id INT PRIMARY KEY, v INT AUTO_INCREMENT)",
    )
    .await;
    assert_eq!(e.code, ER_WRONG_AUTO_KEY, "auto column outside the pk: {e:?}");

    // f) SQL-level PREPARE/EXECUTE are not implemented: the text form is a
    //    parse error and EXECUTE .. USING is unsupported, so the prepared
    //    path is the binary protocol (used by the import in (b)).
    let e = server_error(
        &mut c,
        "PREPARE ins FROM 'INSERT INTO orders (customer, amount) VALUES (?, ?)'",
    )
    .await;
    assert_eq!(e.code, ER_PARSE_ERROR, "PREPARE .. FROM: {e:?}");
    let e = server_error(&mut c, "EXECUTE ins USING @a, @b").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED, "EXECUTE .. USING: {e:?}");
    run(&mut c, "SET @a = 'x', @b = 10").await;
    match c
        .prep("INSERT INTO orders (customer, amount, id) VALUES (?, ?)")
        .await
    {
        Ok(_) => panic!("a wrong column count must not prepare"),
        Err(mysql_async::Error::Server(se)) => {
            assert_eq!(se.code, ER_WRONG_VALUE_COUNT, "column count: {se:?}");
            assert!(se.message.contains("arity"), "{se:?}");
        }
        Err(other) => panic!("expected a server error, got {other:?}"),
    }
    match c
        .exec::<mysql_async::Row, _, _>(&st, ("bad-type", "not-a-number"))
        .await
    {
        Ok(_) => panic!("a wrong parameter type must not store"),
        Err(mysql_async::Error::Server(se)) => {
            assert_eq!(se.code, ER_WRONG_VALUE_COUNT, "param type: {se:?}");
            assert!(se.message.contains("cannot store"), "{se:?}");
        }
        Err(other) => panic!("expected a server error, got {other:?}"),
    }
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM orders WHERE customer = 'bad-type'").await,
        vec![0],
        "the rejected import stored nothing"
    );

    // g) Session replies: BEGIN inside a transaction commits the open one
    //    (MySQL semantics), ROLLBACK AND CHAIN is 1235 and an unknown
    //    table answers 1146.
    run(&mut c, "START TRANSACTION").await;
    run(&mut c, "INSERT INTO orders (customer, amount) VALUES ('committed', 5)").await;
    run(&mut c, "BEGIN").await;
    run(&mut c, "ROLLBACK").await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM orders WHERE customer = 'committed'").await,
        vec![1],
        "BEGIN committed the open transaction"
    );
    sync_mirror(&mut c, &a, "mirror after the implicit commit").await;
    let e = server_error(&mut c, "ROLLBACK AND CHAIN").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED, "rollback and chain: {e:?}");
    let e = server_error(&mut c, "SELECT * FROM nope").await;
    assert_eq!(e.code, ER_NO_SUCH_TABLE, "unknown table: {e:?}");

    // h) The mirror tracked every intake: it still equals the last id SQL
    //    handed out (read bracketed, right after the final mirror).
    let last = ints(&mut c, "SELECT MAX(id) FROM orders").await[0];
    assert_eq!(
        rb(&a, &[b"GET", b"order:seq"]).await,
        last.to_string(),
        "the mirror tracks the sequence"
    );

    scenario_common::perf::scenario("seq154", t0);
}
