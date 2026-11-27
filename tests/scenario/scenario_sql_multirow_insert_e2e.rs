//! Multi-store scenario #172 -- a shipment manifest loader feeds multi-row
//! INSERTs: affected = rows, LAST_INSERT_ID = the FIRST auto id, and the
//! per-table counter reserves a 64-id batch above its floor on every
//! statement that auto-allocates (sequence.rs persist_target: the id after
//! 65 is 129, not 66). A pk duplicate SILENTLY overwrites (deviation,
//! exec/write.rs plain_insert); a UNIQUE hit is 1062, whole-statement
//! atomic, but the reserved ids ARE burned; arity / Str->Int rejections are
//! 1136, NULL into NOT NULL 1048, a bad DATE 1292 -- none reaches the
//! allocator. A RESP ZSET holds the rejected manifest refs (score =
//! candidate index); only the Rust-validated rows load. One REAL node,
//! MySQL front + RESP.

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{aff, col, connect_root, ddl, ints, one, server_error, s};
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, r, FrontSpec};

const ER_DUP_ENTRY: u16 = 1062;
const ER_BAD_NULL_ERROR: u16 = 1048;
const ER_WRONG_VALUE_COUNT_ON_ROW: u16 = 1136;
const ER_TRUNCATED_WRONG_VALUE: u16 = 1292;

/// Ids reserved per raft round-trip when a statement auto-allocates
/// (sequence.rs RESERVE_BATCH).
const RESERVE_BATCH: i64 = 64;

/// The counter model of sequence.rs (`assign` + `persist_target`): `autos`
/// rows from `floor` get `floor..floor+autos`; a statement that
/// auto-allocated persists `max(next, floor + 64)`.
fn seq_step(floor: i64, autos: usize) -> (Vec<i64>, i64) {
    let base = floor.max(1);
    let ids: Vec<i64> = (0..autos as i64).map(|k| base + k).collect();
    (ids, (base + autos as i64).max(base + RESERVE_BATCH))
}

/// One manifest candidate as typed by the loader: (ref_code, units text).
type Candidate<'a> = (&'a str, &'a str);

/// The validation split: accepted `(ref, units)` and rejected
/// `(candidate index, ref)`.
type Split<'a> = (Vec<(&'a str, i64)>, Vec<(i64, &'a str)>);

/// Manifest validation: `units` must parse as i64 within 1..=100. Returns
/// the accepted `(ref, units)` and the rejected `(candidate index, ref)`.
fn validate<'a>(candidates: &[Candidate<'a>]) -> Split<'a> {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for (i, (code, units)) in candidates.iter().enumerate() {
        match units.parse::<i64>().ok().filter(|n| (1..=100).contains(n)) {
            Some(n) => ok.push((*code, n)),
            None => bad.push((i as i64, *code)),
        }
    }
    (ok, bad)
}

fn cell(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        other => panic!("unexpected cell {other:?}"),
    }
}

async fn count(c: &mut mysql_async::Conn, table: &str) -> i64 {
    ints(c, &format!("SELECT COUNT(*) FROM {table}")).await[0]
}

/// One INSERT: affected is read right away, LAST_INSERT_ID() is the very
/// next statement (both are connection state).
async fn insert_ids(c: &mut mysql_async::Conn, sql: &str) -> (u64, i64) {
    let n = aff(c, sql).await;
    let last = ints(c, "SELECT LAST_INSERT_ID()").await[0];
    (n, last)
}

/// Members of a ZSET in ZRANGE (score) order: the bulk payloads of the
/// array, read through the quiet-gap drain (arrays need the full reply).
async fn zrange(addr: &str, key: &str) -> Vec<String> {
    let args: Vec<&[u8]> = vec![b"ZRANGE", key.as_bytes(), b"0", b"-1"];
    let full = common::lite::text(
        &common::lite::cmd_full_reply_wait(addr, common::TOKEN, &args, 25, 10_000).await,
    );
    full.split("\r\n")
        .collect::<Vec<&str>>()
        .windows(2)
        .filter(|w| w[0].starts_with('$') && w[0] != "$-1")
        .map(|w| w[1].to_string())
        .collect()
}

#[tokio::test]
async fn sql_multirow_insert() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, resp) = front_world("multirow172", &spec).await;
    let mut c = connect_root(&front.node).await;

    // The manifest tables: the unique ref index is an inline UNIQUE KEY.
    ddl(
        &mut c,
        "CREATE TABLE shipments (id INT AUTO_INCREMENT PRIMARY KEY, ref_code VARCHAR(32), \
         units INT NOT NULL, shipped_at DATE, UNIQUE KEY uq_ref (ref_code))",
    )
    .await;
    ddl(&mut c, "CREATE TABLE carriers (id INT PRIMARY KEY, name VARCHAR(32))").await;

    // 1) The first manifest: three rows in ONE statement -> affected 3,
    //    LAST_INSERT_ID = the FIRST auto id, consecutive ids from the
    //    fresh table's floor (1).
    let (ids1, counter1) = seq_step(1, 3);
    let (n, last) = insert_ids(
        &mut c,
        "INSERT INTO shipments (ref_code, units, shipped_at) VALUES \
         ('a',1,'2026-01-02'),('b',2,'2026-01-03'),('c',3,'2026-01-04')",
    )
    .await;
    assert_eq!(n, 3, "affected = rows");
    assert_eq!(last, ids1[0], "LAST_INSERT_ID = first auto id");
    assert_eq!(ints(&mut c, "SELECT id FROM shipments ORDER BY id").await, ids1, "consecutive ids");

    // 2) Counter persistence: every auto-allocating statement reserves a
    //    batch above its floor, so the next single-row inserts land on
    //    the persisted counter (65, then 129), not on 4 and 5.
    let (ids2, counter2) = seq_step(counter1, 1);
    let (n, last) = insert_ids(
        &mut c,
        "INSERT INTO shipments (ref_code, units, shipped_at) VALUES ('p',7,'2026-01-05')",
    )
    .await;
    assert_eq!((n, last), (1, ids2[0]), "first single-row insert after the batch");
    let (ids3, counter3) = seq_step(counter2, 1);
    let (n, last) = insert_ids(
        &mut c,
        "INSERT INTO shipments (ref_code, units, shipped_at) VALUES ('q',8,'2026-01-06')",
    )
    .await;
    assert_eq!((n, last), (1, ids3[0]), "second single-row insert");
    assert_eq!(ints(&mut c, "SELECT MAX(id) FROM shipments").await, vec![ids3[0]], "MAX(id)");

    // 3) PK duplicate SILENTLY overwrites (deviation, exec/write.rs
    //    plain_insert): affected 1 and the last writer's row is the row.
    assert_eq!(aff(&mut c, "INSERT INTO carriers VALUES (1,'x')").await, 1, "first carrier");
    assert_eq!(aff(&mut c, "INSERT INTO carriers VALUES (1,'y')").await, 1, "pk duplicate affected");
    assert_eq!(
        col(&mut c, "SELECT name FROM carriers WHERE id = 1").await,
        vec![s("y")],
        "last writer wins, silently"
    );
    assert_eq!(count(&mut c, "carriers").await, 1, "still one carrier");

    // 4) UNIQUE hit: whole-statement atomic, 1062 names the value and the
    //    index; the ids the statement reserved ARE burned (next id jumps).
    let before = count(&mut c, "shipments").await;
    let (_burned, counter4) = seq_step(counter3, 3);
    let e = server_error(
        &mut c,
        "INSERT INTO shipments (ref_code, units, shipped_at) VALUES \
         ('d',4,'2026-01-05'),('a',5,'2026-01-06'),('e',6,'2026-01-07')",
    )
    .await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (ER_DUP_ENTRY, "Duplicate entry 'a' for key 'uq_ref'"),
        "{e:?}"
    );
    assert_eq!(count(&mut c, "shipments").await, before, "no 'd' or 'e' landed");
    assert!(col(&mut c, "SELECT id FROM shipments WHERE ref_code IN ('d','e')").await.is_empty());
    let (ids5, counter5) = seq_step(counter4, 1);
    let (n, last) = insert_ids(
        &mut c,
        "INSERT INTO shipments (ref_code, units, shipped_at) VALUES ('r',9,'2026-01-08')",
    )
    .await;
    assert_eq!((n, last), (1, ids5[0]), "the failed statement's reservation is burned");

    // 5) Hot block: the error ladder -- each rejection atomic, and none of
    //    them reaches the allocator (the next id is the one predicted
    //    before the ladder).
    let t = scenario_common::perf::now();
    let before = count(&mut c, "shipments").await;
    let e = server_error(
        &mut c,
        "INSERT INTO shipments (ref_code, units, shipped_at) VALUES ('h',1,'2026-01-08'),('i',2)",
    )
    .await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (ER_WRONG_VALUE_COUNT_ON_ROW, "INSERT row arity does not match column list"),
        "{e:?}"
    );
    let e = server_error(
        &mut c,
        "INSERT INTO shipments (ref_code, units, shipped_at) VALUES ('g','12abc','2026-01-08')",
    )
    .await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (ER_WRONG_VALUE_COUNT_ON_ROW, "cannot store Str(\"12abc\") into Int column"),
        "{e:?}"
    );
    let e = server_error(
        &mut c,
        "INSERT INTO shipments (ref_code, units, shipped_at) VALUES ('h',NULL,'2026-01-08')",
    )
    .await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (ER_BAD_NULL_ERROR, "column 'units' cannot be null"),
        "{e:?}"
    );
    let e = server_error(
        &mut c,
        "INSERT INTO shipments (ref_code, units, shipped_at) VALUES ('i',1,'2026-13-40')",
    )
    .await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (ER_TRUNCATED_WRONG_VALUE, "Incorrect DATE value: '2026-13-40'"),
        "{e:?}"
    );
    assert_eq!(count(&mut c, "shipments").await, before, "every rejection is atomic");
    let (ids6, counter6) = seq_step(counter5, 1);
    let (n, last) = insert_ids(
        &mut c,
        "INSERT INTO shipments (ref_code, units, shipped_at) VALUES ('h',1,'2026-01-08')",
    )
    .await;
    assert_eq!((n, last), (1, ids6[0]), "no rejection burned an id");
    scenario_common::perf::within("multirow/error-ladder", t, Duration::from_secs(2));

    // 6) RESP-driven acceptance: the Rust side validates the candidate
    //    manifest, the rejected refs are parked in a ZSET (score =
    //    candidate index) and only the accepted rows load, in one
    //    multi-row statement built from the Vec.
    let candidates: [(&str, &str); 6] = [
        ("k1", "12"),
        ("k2", "0"),
        ("k3", "101"),
        ("k4", "7"),
        ("k5", "abc"),
        ("k6", "55"),
    ];
    let (accepted, rejected) = validate(&candidates);
    assert_eq!((accepted.len(), rejected.len()), (3, 3), "validation split");
    let prior = count(&mut c, "shipments").await;
    let mut args: Vec<Vec<u8>> = vec![b"ZADD".to_vec(), b"manifest".to_vec()];
    for (i, code) in &rejected {
        args.push(i.to_string().into_bytes());
        args.push(code.as_bytes().to_vec());
    }
    let borrowed: Vec<&[u8]> = args.iter().map(|v| v.as_slice()).collect();
    let added = r(&resp, &borrowed).await;
    assert_eq!(int_of(&added), rejected.len() as i64, "rejected refs parked: {added:?}");
    let (ids7, _counter7) = seq_step(counter6, accepted.len());
    let values: Vec<String> = accepted
        .iter()
        .map(|(code, units)| format!("('{code}',{units},'2026-01-09')"))
        .collect();
    let (n, last) = insert_ids(
        &mut c,
        &format!(
            "INSERT INTO shipments (ref_code, units, shipped_at) VALUES {}",
            values.join(",")
        ),
    )
    .await;
    assert_eq!((n, last), (accepted.len() as u64, ids7[0]), "accepted manifest loaded");
    assert_eq!(count(&mut c, "shipments").await, prior + accepted.len() as i64, "count = prior + accepted");
    let parked: Vec<String> = rejected.iter().map(|(_, code)| code.to_string()).collect();
    assert_eq!(zrange(&resp, "manifest").await, parked, "ZRANGE = rejected refs by index");
    let stored: Vec<String> = col(&mut c, "SELECT ref_code FROM shipments ORDER BY ref_code")
        .await
        .iter()
        .map(cell)
        .collect();
    for (_, code) in &rejected {
        assert!(!stored.contains(&code.to_string()), "rejected {code} must not load");
    }
    for (code, _) in &accepted {
        assert!(stored.contains(&code.to_string()), "accepted {code} loaded");
    }
    let units_sum: i64 = accepted.iter().map(|(_, units)| units).sum();
    assert_eq!(
        ints(&mut c, "SELECT SUM(units) FROM shipments WHERE ref_code LIKE 'k%'").await,
        vec![units_sum],
        "loaded units agree with the validated numbers"
    );
    assert_eq!(one(&mut c, "SELECT ref_code FROM shipments WHERE id = 1").await, s("a"), "first manifest row intact");

    scenario_common::perf::scenario("multirow", t0);
}
