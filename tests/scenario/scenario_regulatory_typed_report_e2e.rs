//! Batch 11 scenario #123 -- a regulatory filing desk stores a typed
//! report table (BIGINT / BOOLEAN / BLOB / DATE / DATETIME /
//! DECIMAL(10,2)) and reads it back through the MySQL face: DESCRIBE
//! spells the engine's exact type names, BOOLEAN answers 1/0, BLOB
//! round-trips byte-exact, DATE/DATETIME normalize to the canonical
//! `YYYY-MM-DD` / `YYYY-MM-DD HH:MM:SS` spellings (lenient input
//! forms included), an impossible date is 1292, and the session
//! surface (`SHOW VARIABLES LIKE 'char%'`, `SHOW STATUS LIKE 'Up%'`,
//! `@@version`) answers its fixed shapes. The ES face mirrors the
//! same rows and the summary counts agree. One REAL node, MySQL + ES.

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{
    col, connect_root, ddl, one, rows, run, server_error, s, ER_NOT_SUPPORTED_YET,
    ER_TRUNCATED_WRONG_VALUE,
};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{front_world, http, json_of, FrontSpec};

/// `(Field, Type)` pairs of a DESCRIBE result, in column order.
fn field_types(reply: &[Vec<MVal>]) -> Vec<(String, String)> {
    reply
        .iter()
        .map(|r| (cell(&r[0]), cell(&r[1])))
        .collect()
}

/// One wire cell as text.
fn cell(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        MVal::NULL => "NULL".to_string(),
        other => panic!("unexpected cell {other:?}"),
    }
}

/// ES `_count` of a query body.
async fn es_count(es: &str, body: &str) -> i64 {
    let (status, out) = http(es, "POST", "/report/_count", Some(body)).await;
    assert_eq!(status, 200, "count {body}: {out}");
    json_of(&out)["count"]
        .as_i64()
        .unwrap_or_else(|| panic!("no count in {out}"))
}

#[tokio::test]
async fn regulatory_typed_report_round_trip_and_es_mirror() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, _a) = front_world("typed123", &spec).await;
    let es = front.node.es.clone();
    let mut c = connect_root(&front.node).await;

    // a) The filing table: six typed columns, BIGINT pk.
    ddl(
        &mut c,
        "CREATE TABLE report (id BIGINT PRIMARY KEY, ok BOOLEAN, payload BLOB, \
         d DATE, ts DATETIME, amount DECIMAL(10,2))",
    )
    .await;

    // b) DESCRIBE spells the engine's exact type names (no MySQL
    //    width suffix on varchar/blob: the type IS the pair for
    //    decimal, which keeps its precision/scale).
    let desc = field_types(&rows(&mut c, "DESCRIBE report").await);
    assert_eq!(
        desc,
        vec![
            ("id".to_string(), "bigint".to_string()),
            ("ok".to_string(), "bool".to_string()),
            ("payload".to_string(), "blob".to_string()),
            ("d".to_string(), "date".to_string()),
            ("ts".to_string(), "datetime".to_string()),
            ("amount".to_string(), "decimal(10,2)".to_string()),
        ],
        "{desc:?}"
    );

    // c) Hot block: 50 inserts (even ids file an ok report, odd ids a
    //    rejected one; the first row carries the lenient date
    //    spellings) then 20 typed reads.
    let t = scenario_common::perf::now();
    for i in 1..=50i64 {
        let ok = if i % 2 == 0 { 1 } else { 0 };
        let (d, ts) = if i == 1 {
            ("2024-3-5", "2024-3-5 9:5:1")
        } else {
            ("2024-03-05", "2024-03-05 09:05:01")
        };
        run(
            &mut c,
            &format!(
                "INSERT INTO report (id, ok, payload, d, ts, amount) VALUES \
                 ({i}, {ok}, NULL, '{d}', '{ts}', {})",
                i * 10
            ),
        )
        .await;
    }
    for _ in 0..5 {
        let n = one(&mut c, "SELECT COUNT(*) FROM report").await;
        assert_eq!(n, s("50"), "count");
        let head = rows(
            &mut c,
            "SELECT id, ok, d, ts, amount FROM report WHERE id <= 2 ORDER BY id",
        )
        .await;
        assert_eq!(head.len(), 2, "{head:?}");
        let rejected =
            col(&mut c, "SELECT id FROM report WHERE ok = FALSE ORDER BY id LIMIT 5").await;
        assert_eq!(rejected.len(), 5, "{rejected:?}");
        let filed = col(&mut c, "SELECT COUNT(*) FROM report WHERE ok = TRUE").await;
        assert_eq!(filed, vec![s("25")], "{filed:?}");
    }
    scenario_common::perf::within("typed123/50-insert+20-select", t, Duration::from_secs(3));

    // d) BOOLEAN reads back as 1/0 (the tinyint wire form), and the
    //    two flags partition the table. The comparison domain is Bool
    //    ONLY: `ok = TRUE` / `ok = FALSE` are the spellings (an integer
    //    literal is 1235 -- rdb does not widen BOOLEAN to TINYINT(1)
    //    for comparisons the way MySQL does; asserted below).
    assert_eq!(
        col(&mut c, "SELECT DISTINCT ok FROM report ORDER BY ok").await,
        vec![s("0"), s("1")],
        "boolean cells"
    );
    assert_eq!(
        one(&mut c, "SELECT COUNT(*) FROM report WHERE ok = FALSE").await,
        s("25")
    );
    assert_eq!(
        one(&mut c, "SELECT COUNT(*) FROM report WHERE ok = TRUE").await,
        s("25")
    );
    // The documented deviation: an INT literal against a BOOLEAN
    // column is refused loudly (1235) instead of comparing as 0/1.
    for sql in [
        "SELECT id FROM report WHERE ok = 1",
        "SELECT id FROM report WHERE ok = 0",
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {e:?}");
        assert!(e.message.contains("cannot compare"), "{sql}: {e:?}");
    }

    // e) DATE / DATETIME normalize: the lenient `2024-3-5 9:5:1`
    //    spelling lands as the canonical fixed-width form, and the
    //    compact integer spellings coerce the same way.
    let got = rows(&mut c, "SELECT d, ts FROM report WHERE id = 1").await;
    assert_eq!(got, vec![vec![s("2024-03-05"), s("2024-03-05 09:05:01")]], "{got:?}");
    let got = rows(&mut c, "SELECT d, ts FROM report WHERE id = 2").await;
    assert_eq!(got, vec![vec![s("2024-03-05"), s("2024-03-05 09:05:01")]], "{got:?}");
    // The compact integer spellings coerce the same way (CAST to a
    // temporal target is NOT supported -- 1235 -- so the coercion is
    // exercised through the column types instead).
    let e = server_error(&mut c, "SELECT CAST(20240305 AS DATE)").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{e:?}");
    run(
        &mut c,
        "INSERT INTO report (id, ok, d, ts, amount) VALUES (800, 1, 20240305, 20240305090501, 0)",
    )
    .await;
    assert_eq!(
        rows(&mut c, "SELECT d, ts FROM report WHERE id = 800").await,
        vec![vec![s("2024-03-05"), s("2024-03-05 09:05:01")]],
        "compact temporal forms"
    );
    assert_eq!(
        rows(&mut c, "SELECT d FROM report WHERE id = 1").await,
        vec![vec![s("2024-03-05")]]
    );

    // f) DECIMAL(10,2) keeps its scale through the round trip.
    let got = col(&mut c, "SELECT amount FROM report WHERE id = 3").await;
    assert_eq!(got, vec![s("30.00")], "{got:?}");

    // g) BLOB bytes round-trip byte-exact, including NUL and 0xFF.
    c.exec_drop(
        "INSERT INTO report (id, ok, payload, d, ts, amount) VALUES (900, 1, ?, NULL, NULL, 0)",
        (vec![0u8, 255, 10, 13, 0, 32],),
    )
    .await
    .expect("blob insert");
    let got = col(&mut c, "SELECT payload FROM report WHERE id = 900").await;
    assert_eq!(
        got,
        vec![MVal::Bytes(vec![0, 255, 10, 13, 0, 32])],
        "blob bytes"
    );

    // h) An impossible date is a loud 1292 (never a silent NULL), and
    //    the rejected statement leaves no row behind.
    let e = server_error(
        &mut c,
        "INSERT INTO report (id, ok, d) VALUES (901, 1, '2026-02-30')",
    )
    .await;
    assert_eq!(e.code, ER_TRUNCATED_WRONG_VALUE, "{e:?}");
    assert!(
        e.message.contains("Incorrect DATE value"),
        "{}",
        e.message
    );
    assert_eq!(
        col(&mut c, "SELECT id FROM report WHERE id = 901").await,
        Vec::<MVal>::new(),
        "rejected insert left a row"
    );

    // i) Session surface: the four character_set variables, the
    //    Uptime status counter and a non-empty @@version.
    let vars = col(&mut c, "SHOW VARIABLES LIKE 'char%'").await;
    let names: Vec<String> = vars.iter().map(cell).collect();
    assert_eq!(
        names,
        vec![
            "character_set_client",
            "character_set_connection",
            "character_set_database",
            "character_set_results",
        ],
        "{names:?}"
    );
    assert_eq!(vars.len(), 4, "exactly four char% rows: {names:?}");
    let status = col(&mut c, "SHOW STATUS LIKE 'Up%'").await;
    assert!(
        status.iter().map(cell).any(|n| n == "Uptime"),
        "{status:?}"
    );
    let version = one(&mut c, "SELECT @@version").await;
    assert_ne!(cell(&version), "", "@@version");
    assert!(cell(&version).contains('.'), "@@version {version:?}");

    // j) ES mirror: the same 50 filings as documents; the summary
    //    counts (by ok flag, and the amount threshold) agree with the
    //    SQL answers.
    let (status, body) = http(
        &es,
        "PUT",
        "/report",
        Some(r#"{"mappings":{"properties":{"id":{"type":"long"},"ok":{"type":"long"},"amount":{"type":"double"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create report index: {body}");
    for i in 1..=50 {
        // The ES face has no `boolean` field type (numeric types map
        // to the kernel's one f64 Numeric field), so the mirror spells
        // the flag as 1/0 -- the same wire form the BOOLEAN column
        // reads back as.
        let ok = if i % 2 == 0 { 1 } else { 0 };
        let (status, body) = http(
            &es,
            "PUT",
            &format!("/report/_doc/r{i}"),
            Some(&format!(r#"{{"id":{i},"ok":{ok},"amount":{}}}"#, i * 10)),
        )
        .await;
        assert_eq!(status, 201, "index r{i}: {body}");
    }
    assert_eq!(
        es_count(&es, r#"{"query":{"term":{"ok":1}}}"#).await,
        25,
        "ES ok=true must match SQL"
    );
    assert_eq!(
        es_count(&es, r#"{"query":{"term":{"ok":0}}}"#).await,
        25,
        "ES ok=false must match SQL"
    );
    assert_eq!(
        es_count(&es, r#"{"query":{"range":{"amount":{"gt":250}}}}"#).await,
        25,
        "ES amount>250 must match SQL"
    );
    assert_eq!(
        one(&mut c, "SELECT COUNT(*) FROM report WHERE amount > 250 AND id <= 50").await,
        s("25"),
        "SQL amount>250 agrees with ES"
    );

    scenario_common::perf::scenario("typed123", t0);
}
