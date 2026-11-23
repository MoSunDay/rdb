//! Multi-store scenario #106 -- the schema audit console on the SQL
//! plane: SHOW TABLES / SHOW DATABASES list what lives, DESCRIBE
//! answers the 6-column Field/Type/Null/Key/Default/Extra shape with
//! PRI/UNI/MUL flags, SHOW INDEX lays one row per indexed column,
//! SHOW CREATE TABLE renders MySQL-exact DDL that ROUND-TRIPS (drop +
//! re-create from the render == the same render), EXPLAIN reports
//! SeqScan vs IndexScan verdicts, and ALTER TABLE .. RENAME TO swaps
//! the name the console sees. One REAL node, MySQL front.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, rows, run, server_error, ER_NO_SUCH_TABLE};
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
async fn schema_audit_walks_show_describe_index_explain_and_rename() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("audit106", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The audited schema: pk + one plain index + one unique index.
    ddl(&mut c, "CREATE TABLE audit_events (id BIGINT PRIMARY KEY, \
        src VARCHAR(16) NULL, tag VARCHAR(16) NULL)").await;
    ddl(&mut c, "CREATE INDEX ix_src ON audit_events (src)").await;
    ddl(&mut c, "ALTER TABLE audit_events ADD UNIQUE INDEX uk_tag (tag)").await;
    run(&mut c, "INSERT INTO audit_events (id, src, tag) VALUES \
        (1, 'app', 'boot'), (2, 'app', 'auth'), (3, 'cron', 'scan')").await;

    // b) SHOW TABLES lists both worlds; SHOW DATABASES names the one
    //    hosted database.
    let tables: Vec<String> = rows(&mut c, "SHOW TABLES").await
        .iter().map(|r| cell(&r[0])).collect();
    assert!(tables.contains(&"audit_events".to_string()), "{tables:?}");
    let dbs: Vec<String> = rows(&mut c, "SHOW DATABASES").await
        .iter().map(|r| cell(&r[0])).collect();
    assert_eq!(dbs, vec!["rdb".to_string()], "{dbs:?}");

    // c) DESCRIBE: the 6-column shape (every row carries six cells)
    //    with the Key flags PRI / MUL / UNI.
    let desc = rows(&mut c, "DESCRIBE audit_events").await;
    assert_eq!(desc.len(), 3, "three columns");
    for r in &desc {
        assert_eq!(r.len(), 6, "six DESCRIBE cells: {r:?}");
    }
    let flags: Vec<(String, String, String)> = desc
        .iter()
        .map(|r| (cell(&r[0]), cell(&r[1]), cell(&r[3])))
        .collect();
    assert_eq!(
        flags,
        vec![
            ("id".into(), "bigint".into(), "PRI".into()),
            ("src".into(), "varchar".into(), "MUL".into()),
            ("tag".into(), "varchar".into(), "UNI".into()),
        ],
        "field/type/key"
    );

    // d) SHOW INDEX: one row per indexed column -- PRIMARY + ix_src +
    //    uk_tag; unique rows are Non_unique 0, the plain index is 1.
    let idx = rows(&mut c, "SHOW INDEX FROM audit_events").await;
    let pairs: Vec<(String, String)> = idx
        .iter()
        .map(|r| (cell(&r[2]), cell(&r[4])))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("PRIMARY".into(), "id".into()),
            ("ix_src".into(), "src".into()),
            ("uk_tag".into(), "tag".into()),
        ],
        "one row per indexed column"
    );
    let non_unique: Vec<String> = idx.iter().map(|r| cell(&r[1])).collect();
    assert_eq!(non_unique, vec!["0", "1", "0"], "non_unique flags");

    // f) EXPLAIN verdicts: unfiltered is a SeqScan headline; the
    //    indexed equality routes through IndexScan with the matched
    //    pk count (src='app' hits 2 rows).
    let plan = rows(&mut c, "EXPLAIN SELECT id FROM audit_events").await;
    assert_eq!(cell(&plan[0][0]), "SeqScan audit_events", "{plan:?}");
    let plan = rows(&mut c, "EXPLAIN SELECT id FROM audit_events WHERE src = 'app'").await;
    assert_eq!(cell(&plan[0][0]), "IndexScan ix_src -> 2 pks", "{plan:?}");

    // g) SHOW CREATE TABLE renders MySQL-exact DDL (backticks,
    //    newlines, ENGINE=InnoDB) -- asserted via fragments, then the
    //    round-trip: dropping and re-creating FROM the render yields
    //    the identical render.
    let rendered = cell(&rows(&mut c, "SHOW CREATE TABLE audit_events").await[0][1]);
    for frag in [
        "CREATE TABLE `audit_events` (",
        "`id` bigint NOT NULL",
        "PRIMARY KEY (`id`)",
        "KEY `ix_src` (`src`)",
        "UNIQUE KEY `uk_tag` (`tag`)",
        ") ENGINE=InnoDB",
    ] {
        assert!(rendered.contains(frag), "fragment {frag:?} in:\n{rendered}");
    }
    assert!(rendered.contains('\n'), "multi-line render");
    ddl(&mut c, "DROP TABLE audit_events").await;
    ddl(&mut c, &rendered).await;
    let again = cell(&rows(&mut c, "SHOW CREATE TABLE audit_events").await[0][1]);
    assert_eq!(again, rendered, "round-trip re-creates the same shape");
    // The round-trip table starts empty: re-seed for the rename walk.
    run(&mut c, "INSERT INTO audit_events (id, src, tag) VALUES \
        (1, 'app', 'boot'), (2, 'app', 'auth'), (3, 'cron', 'scan')").await;

    // h) ALTER TABLE .. RENAME TO swaps the name the console sees;
    //    the old name is honestly gone (1146).
    ddl(&mut c, "ALTER TABLE audit_events RENAME TO events_v2").await;
    let tables: Vec<String> = rows(&mut c, "SHOW TABLES").await
        .iter().map(|r| cell(&r[0])).collect();
    assert!(tables.contains(&"events_v2".to_string()) && !tables.contains(&"audit_events".to_string()), "{tables:?}");
    let count = rows(&mut c, "SELECT COUNT(*) FROM events_v2").await;
    assert_eq!(cell(&count[0][0]), "3", "data rode the rename");
    let e = server_error(&mut c, "SELECT id FROM audit_events").await;
    assert_eq!(e.code, ER_NO_SUCH_TABLE, "{}", e.message);
}
