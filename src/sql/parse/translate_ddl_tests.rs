//! IR shapes and rejections of the M4 DDL surface: TRUNCATE, RENAME
//! (both spellings), ALTER TABLE index/rename forms, and the SHOW
//! metadata statements (including the LIKE/WHERE filter policy).

use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::ErrorCode;
use crate::sql::parse::parse_statement;

fn stmt(sql: &str) -> Statement {
    parse_statement(sql).expect("parse")
}

fn err_of(sql: &str) -> crate::sql::parse::error::SqlError {
    parse_statement(sql).expect_err("must reject")
}

#[test]
fn truncate_shapes() {
    for sql in ["TRUNCATE TABLE t", "TRUNCATE t"] {
        assert_eq!(
            stmt(sql),
            Statement::TruncateTable { name: "t".into() },
            "{sql}"
        );
    }
    // rejects: Snowflake/Postgres decorations and multi-table lists
    for sql in [
        "TRUNCATE TABLE IF EXISTS t",
        "TRUNCATE t, u",
        "TRUNCATE TABLE t PARTITION (p0)",
        "TRUNCATE TABLE t RESTART IDENTITY",
    ] {
        let e = err_of(sql);
        assert!(
            e.code == ErrorCode::Parse || e.code == ErrorCode::NotSupported,
            "{sql}: {e}"
        );
    }
    // qualified names stay single-part (single-db model)
    assert!(err_of("TRUNCATE TABLE other.t").code == ErrorCode::NotSupported);
}

#[test]
fn rename_shapes() {
    assert_eq!(
        stmt("RENAME TABLE a TO b"),
        Statement::RenameTable {
            from: "a".into(),
            to: "b".into()
        }
    );
    assert_eq!(
        stmt("ALTER TABLE a RENAME TO b"),
        stmt("ALTER TABLE a RENAME AS b")
    );
    // multi-pair list and cross-db targets reject
    assert!(err_of("RENAME TABLE a TO b, c TO d").code == ErrorCode::NotSupported);
    assert!(err_of("RENAME TABLE a TO other.b").code == ErrorCode::NotSupported);
}

#[test]
fn alter_index_forms_map_to_plain_statements() {
    assert_eq!(
        stmt("ALTER TABLE t ADD INDEX idx_v (v)"),
        Statement::CreateIndex {
            table: "t".into(),
            name: "idx_v".into(),
            column: "v".into(),
            unique: false,
            if_not_exists: false,
        }
    );
    // nameless ADD INDEX defaults like CREATE INDEX
    assert_eq!(
        stmt("ALTER TABLE t ADD INDEX (v)"),
        stmt("CREATE INDEX idx_v ON t (v)")
    );
    // UNIQUE spellings converge
    assert_eq!(
        stmt("ALTER TABLE t ADD UNIQUE INDEX uq_n (n)"),
        stmt("ALTER TABLE t ADD UNIQUE KEY uq_n (n)")
    );
    assert_eq!(
        stmt("ALTER TABLE t ADD UNIQUE (n)"),
        stmt("CREATE UNIQUE INDEX idx_n ON t (n)")
    );
    assert_eq!(
        stmt("ALTER TABLE t DROP INDEX idx_v"),
        Statement::DropIndex {
            table: "t".into(),
            name: "idx_v".into(),
            if_exists: false,
        }
    );
    // CREATE INDEX itself keeps its shape through the moved translator
    assert_eq!(
        stmt("CREATE INDEX i ON t (v)"),
        Statement::CreateIndex {
            table: "t".into(),
            name: "i".into(),
            column: "v".into(),
            unique: false,
            if_not_exists: false,
        }
    );
}

#[test]
fn index_key_rejections_stay_loud() {
    // composite and prefix keys are the P2 deferred surface
    for sql in [
        "ALTER TABLE t ADD INDEX c (a, b)",
        "ALTER TABLE t ADD UNIQUE KEY c (a, b)",
        "CREATE INDEX c ON t (a, b)",
        "ALTER TABLE t ADD INDEX p (v(10))",
        "CREATE INDEX p ON t (v(10))",
    ] {
        let e = err_of(sql);
        assert_eq!(e.code, ErrorCode::NotSupported, "{sql}: {e}");
        assert!(
            e.msg.contains("single-column") || e.msg.contains("index key"),
            "{sql}: {e}"
        );
    }
}

#[test]
fn alter_column_forms_stay_deferred() {
    for sql in [
        "ALTER TABLE t ADD COLUMN x BIGINT",
        "ALTER TABLE t DROP COLUMN x",
        "ALTER TABLE t MODIFY COLUMN v BIGINT NOT NULL",
        "ALTER TABLE t ADD PRIMARY KEY (id)",
        "ALTER TABLE t ADD FOREIGN KEY (a) REFERENCES u (b)",
    ] {
        let e = err_of(sql);
        assert_eq!(e.code, ErrorCode::NotSupported, "{sql}: {e}");
        assert!(e.msg.contains("P2 deferred"), "{sql}: {e}");
    }
}

#[test]
fn show_shapes() {
    assert_eq!(stmt("SHOW DATABASES"), Statement::ShowDatabases);
    assert_eq!(
        stmt("SHOW CREATE TABLE t"),
        Statement::ShowCreateTable("t".into())
    );
    assert_eq!(
        stmt("SHOW VARIABLES"),
        Statement::ShowVariables { like: None }
    );
    assert_eq!(
        stmt("SHOW GLOBAL VARIABLES LIKE 'wait%'"),
        Statement::ShowVariables {
            like: Some("wait%".into())
        }
    );
    assert_eq!(
        stmt("SHOW SESSION STATUS LIKE 'Uptime'"),
        Statement::ShowStatus {
            like: Some("Uptime".into())
        }
    );
    // WHERE filter rejects (LIKE only)
    let e = err_of("SHOW VARIABLES WHERE Variable_name = 'x'");
    assert_eq!(e.code, ErrorCode::NotSupported);
    assert!(e.msg.contains("LIKE only"), "{e}");
}

#[test]
fn session_function_arity_is_checked_at_translate() {
    for sql in [
        "SELECT DATABASE(1)",
        "SELECT USER('x')",
        "SELECT CONNECTION_ID(1)",
    ] {
        let e = err_of(sql);
        assert_eq!(e.code, ErrorCode::WrongParamCount, "{sql}: {e}");
    }
}
