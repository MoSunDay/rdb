//! SQL text -> IR: sqlparser (MySQL dialect) front end.
//!
//! `parse_statement` is the single entry point; `bind_placeholders` fills
//! `?` parameters of prepared statements before execution.

pub mod ast;
pub mod error;
pub(crate) mod expr;
pub(crate) mod query;
pub(crate) mod session;
pub mod starrocks;
pub(crate) mod table;
pub(crate) mod translate;

pub use ast::*;
pub use error::{ErrorCode, SqlError, SqlResult};
pub use translate::{bind_placeholders, parse_statement, placeholder_count};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::storage::schema::{Engine, SqlType, Value};

    fn stmt(sql: &str) -> Statement {
        parse_statement(sql).expect("parse")
    }

    #[test]
    fn create_table_single_pk() {
        let s = stmt("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR(64) NULL, score DOUBLE NOT NULL)");
        let Statement::CreateTable {
            name, columns, pk, ..
        } = s
        else {
            panic!("shape");
        };
        assert_eq!(name, "users");
        assert_eq!(pk, "id");
        assert_eq!(columns[0].sql_type, SqlType::Int);
        assert!(!columns[0].nullable);
        assert!(columns[1].nullable);
        assert_eq!(columns[2].sql_type, SqlType::Double);
    }

    #[test]
    fn create_table_table_constraint_pk() {
        let s = stmt("CREATE TABLE t (k VARCHAR(20), v INT, PRIMARY KEY (k))");
        let Statement::CreateTable { pk, .. } = s else {
            panic!("shape")
        };
        assert_eq!(pk, "k");
    }

    #[test]
    fn create_table_engine_option() {
        let Statement::CreateTable { engine, .. } =
            stmt("CREATE TABLE t (id BIGINT PRIMARY KEY) ENGINE=columnar")
        else {
            panic!("shape");
        };
        assert_eq!(engine, Engine::Columnar);
        let Statement::CreateTable { engine, .. } =
            stmt("CREATE TABLE t (id BIGINT PRIMARY KEY) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4")
        else {
            panic!("shape");
        };
        assert_eq!(engine, Engine::Row);
        let Statement::CreateTable { engine, .. } = stmt("CREATE TABLE t (id BIGINT PRIMARY KEY)")
        else {
            panic!("shape");
        };
        assert_eq!(engine, Engine::Row);
    }

    #[test]
    fn unsupported_types_rejected() {
        assert!(parse_statement("CREATE TABLE t (d UUID, id INT PRIMARY KEY)").is_err());
        // DATE used to live here; it parses now (see
        // temporal_column_types_translate).
    }

    /// DECIMAL/DEC/NUMERIC share the fixed-point type; widths follow
    /// MySQL (bare = (10,0), (p) = scale 0) and stay inside the i128
    /// mantissa budget (precision <= 38, scale <= precision).
    #[test]
    fn decimal_column_types_translate() {
        let Statement::CreateTable { columns, .. } = stmt(
            "CREATE TABLE t (id BIGINT PRIMARY KEY,\
             a DECIMAL, b DEC(12,5), c NUMERIC(38), d DECIMAL(38,38))",
        ) else {
            panic!("shape");
        };
        let ty = |i: usize| columns[i].sql_type;
        assert_eq!(
            ty(1),
            SqlType::Decimal {
                precision: 10,
                scale: 0
            }
        );
        assert_eq!(
            ty(2),
            SqlType::Decimal {
                precision: 12,
                scale: 5
            }
        );
        assert_eq!(
            ty(3),
            SqlType::Decimal {
                precision: 38,
                scale: 0
            }
        );
        assert_eq!(
            ty(4),
            SqlType::Decimal {
                precision: 38,
                scale: 38
            }
        );
        for (bad, needle) in [
            ("DECIMAL(0)", "precision"),
            ("DECIMAL(39)", "precision"),
            ("DECIMAL(10,11)", "scale"),
            ("DECIMAL(5,-1)", "scale"),
        ] {
            let e = parse_statement(&format!("CREATE TABLE t (id BIGINT PRIMARY KEY, c {bad})"))
                .expect_err(bad);
            assert_eq!(e.code, ErrorCode::NotSupported, "{bad}: {e}");
            assert!(e.msg.contains(needle), "{bad}: {e}");
        }
    }

    #[test]
    fn no_pk_rejected() {
        assert!(parse_statement("CREATE TABLE t (a INT)").is_err());
    }

    #[test]
    fn select_shapes() {
        let Statement::Select(q) = stmt(
            "SELECT id, name AS n FROM users WHERE score > 1.5 ORDER BY id DESC LIMIT 5 OFFSET 2",
        ) else {
            panic!("shape");
        };
        assert_eq!(q.items.len(), 2);
        assert!(matches!(
            q.items[1],
            SelectItem::Expr { ref alias, .. } if alias.as_deref() == Some("n")
        ));
        assert_eq!(q.limit, Some(5));
        assert_eq!(q.offset, 2);
        assert!(!q.order_by[0].asc);
    }

    #[test]
    fn placeholders_count_and_bind() {
        let mut s = stmt("UPDATE users SET name = ? WHERE id = ?");
        assert_eq!(placeholder_count(&s), 2);
        bind_placeholders(&mut s, &[Value::Str("x".into()), Value::Int(3)]).expect("bind");
        let Statement::Update {
            assignments,
            filter,
            ..
        } = &s
        else {
            panic!()
        };
        assert!(matches!(assignments[0].1, Expr::Lit(Value::Str(_))));
        let f = filter.as_ref().unwrap();
        assert!(matches!(
            f,
            Expr::BinaryOp { right, .. } if matches!(right.as_ref(), Expr::Lit(Value::Int(3)))
        ));
    }

    #[test]
    fn placeholders_in_insert_and_select() {
        let mut s = stmt("INSERT INTO t (a, b) VALUES (?, ?), (10, 'x')");
        assert_eq!(placeholder_count(&s), 2);
        bind_placeholders(&mut s, &[Value::Int(1), Value::Null]).expect("bind");
        let mut q = stmt("SELECT * FROM t WHERE a = ? AND b IN (?, ?)");
        assert_eq!(placeholder_count(&q), 3);
        bind_placeholders(&mut q, &[Value::Int(1), Value::Int(2), Value::Int(3)]).expect("bind");
    }

    #[test]
    fn temporal_column_types_translate() {
        let Statement::CreateTable { columns, .. } = stmt(
            "CREATE TABLE t (id BIGINT PRIMARY KEY, d DATE, dt DATETIME, dt3 DATETIME(3), ts TIMESTAMP, ts6 TIMESTAMP(6))",
        )
        else {
            panic!("shape");
        };
        assert_eq!(columns[1].sql_type, SqlType::Date);
        // fsp is accepted (and ignored): storage is always microseconds.
        for c in &columns[2..] {
            assert_eq!(c.sql_type, SqlType::DateTime, "{c:?}");
        }
        // TIME (and the wider temporal family) stays loud unsupported
        // (1235), never mis-stored; DECIMAL now translates (see
        // `decimal_column_types_translate`). Note: MySQL dialect maps
        // `TIMESTAMP WITHOUT TIME ZONE` onto plain TIMESTAMP.
        for bad in ["TIME", "TIME(3)", "DATE32", "DATETIME64(3, 'UTC')"] {
            let e = parse_statement(&format!("CREATE TABLE t (id BIGINT PRIMARY KEY, c {bad})"))
                .expect_err(bad);
            assert_eq!(e.code, ErrorCode::NotSupported, "{bad}: {e}");
            assert!(e.msg.contains("DATE/DATETIME/TIMESTAMP"), "{bad}: {e}");
        }
    }

    #[test]
    fn temporal_typed_string_literals() {
        // DATE '...' / TIMESTAMP '...' ride as plain strings; the
        // engine coerces them on use (coerce/cmp parse the string).
        let Statement::Insert { rows, .. } =
            stmt("INSERT INTO t (d) VALUES (DATE '2024-02-29'), (TIMESTAMP '2024-02-29 12:00:00')")
        else {
            panic!("shape");
        };
        assert_eq!(rows[0][0], Expr::Lit(Value::Str("2024-02-29".into())));
        assert_eq!(
            rows[1][0],
            Expr::Lit(Value::Str("2024-02-29 12:00:00".into()))
        );
        let e = parse_statement("SELECT TIME '12:00:00'").expect_err("typed");
        assert_eq!(e.code, ErrorCode::NotSupported, "{e}");
    }

    #[test]
    fn dml_and_ddl_shapes() {
        assert!(matches!(
            stmt("DELETE FROM t WHERE a = 1 LIMIT 3"),
            Statement::Delete { .. }
        ));
        assert!(matches!(
            stmt("DROP TABLE IF EXISTS t"),
            Statement::DropTable { .. }
        ));
        assert!(matches!(
            stmt("CREATE UNIQUE INDEX ui ON t (name)"),
            Statement::CreateIndex { unique: true, column, .. } if column == "name"
        ));
        assert!(matches!(
            stmt("DROP INDEX ui ON t"),
            Statement::DropIndex { .. }
        ));
        assert!(matches!(stmt("BEGIN"), Statement::Begin));
        assert!(matches!(stmt("BEGIN WORK"), Statement::Begin));
        assert!(matches!(stmt("START TRANSACTION"), Statement::Begin));
        assert!(matches!(stmt("COMMIT"), Statement::Commit));
        assert!(matches!(stmt("ROLLBACK"), Statement::Rollback));
        assert!(matches!(stmt("USE mydb"), Statement::Use(_)));
        assert!(matches!(stmt("SHOW TABLES"), Statement::ShowTables));
        assert!(matches!(
            stmt("SHOW COLUMNS FROM t"),
            Statement::ShowColumns(_)
        ));
        // Real-client handshake: restating the engine's only mode is OK...
        assert!(parse_statement("SET autocommit = 1").is_ok());
        // ...while requesting a mode we do not implement still fails loud.
        assert!(parse_statement("SET autocommit = 0").is_err());
        // Real-client handshake: charset declaration is accepted (no-op).
        assert!(parse_statement("SET NAMES utf8mb4").is_ok());
        assert!(parse_statement("SET NAMES utf8mb4 COLLATE utf8mb4_bin").is_ok());
        // Isolation declarations are accepted and mapped onto the
        // engine's snapshot isolation (REPEATABLE READ semantics).
        assert!(matches!(
            stmt("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED"),
            Statement::SetIsolation { level } if level == "READ COMMITTED"
        ));
        assert!(matches!(
            stmt("SET GLOBAL TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
            Statement::SetIsolation { level } if level == "SERIALIZABLE"
        ));
        assert!(parse_statement("SET TRANSACTION READ ONLY").is_err());
        assert!(matches!(stmt("SET sql_mode = ''"), Statement::SetIgnored));
        assert!(matches!(
            stmt("EXPLAIN SELECT * FROM t"),
            Statement::Explain(_)
        ));
    }

    #[test]
    fn savepoint_and_outer_joins_translate() {
        assert!(matches!(stmt("SAVEPOINT sp1"), Statement::Savepoint(n) if n == "sp1"));
        assert!(matches!(
            stmt("ROLLBACK TO SAVEPOINT sp1"),
            Statement::RollbackTo { name } if name == "sp1"
        ));
        assert!(matches!(
            stmt("RELEASE SAVEPOINT sp1"),
            Statement::ReleaseSavepoint { name } if name == "sp1"
        ));
        let Statement::Select(q) = stmt("SELECT * FROM t1 LEFT JOIN t2 ON t1.id = t2.id") else {
            panic!("left join select");
        };
        let TableRef::Join { kind, .. } = q.from else {
            panic!("join");
        };
        assert_eq!(kind, JoinKind::Left);
        let Statement::Select(q) = stmt("SELECT * FROM t1 CROSS JOIN t2") else {
            panic!("cross join select");
        };
        let TableRef::Join { kind, using, .. } = q.from else {
            panic!("join");
        };
        assert_eq!(kind, JoinKind::Cross);
        assert!(using.is_empty());
        let Statement::Select(q) = stmt("SELECT * FROM t1 JOIN t2 USING (id)") else {
            panic!("using select");
        };
        let TableRef::Join { using, .. } = q.from else {
            panic!("join");
        };
        assert_eq!(using, vec!["id".to_string()]);
    }

    #[test]
    fn join_translates() {
        let Statement::Select(q) =
            stmt("SELECT a.id FROM t1 AS a INNER JOIN t2 AS b ON a.id = b.id WHERE a.v = 1")
        else {
            panic!("shape");
        };
        let TableRef::Join { on: Some(_), .. } = q.from else {
            panic!("join")
        };
    }

    #[test]
    fn aggregates_translate() {
        let Statement::Select(q) = stmt(
            "SELECT COUNT(*), SUM(score), AVG(v), MIN(k), MAX(k), COUNT(DISTINCT name) FROM t",
        ) else {
            panic!("shape");
        };
        assert_eq!(q.items.len(), 6);
        assert!(matches!(
            &q.items[0],
            SelectItem::Expr {
                expr: Expr::Agg {
                    func: AggFunc::Count,
                    arg: None,
                    distinct: false
                },
                ..
            }
        ));
        assert!(matches!(
            &q.items[5],
            SelectItem::Expr {
                expr: Expr::Agg { distinct: true, .. },
                ..
            }
        ));
    }

    #[test]
    fn for_update_rejected() {
        // NOWAIT / SKIP LOCKED stay unsupported: the engine fails
        // fast on latch conflicts instead of skipping/waiting.
        let e = parse_statement("SELECT * FROM t WHERE id = 1 FOR UPDATE NOWAIT").expect_err("u");
        assert!(e.msg.contains("NOWAIT"), "{e}");
        let e = parse_statement("SELECT * FROM t FOR SHARE SKIP LOCKED").expect_err("u");
        assert!(e.msg.contains("SKIP LOCKED"), "{e}");
    }

    #[test]
    fn locking_reads_translate_into_query_lock() {
        let Statement::Select(q) = stmt("SELECT * FROM t WHERE id = 1 FOR UPDATE") else {
            panic!("select");
        };
        assert_eq!(q.lock, Some(LockRead::ForUpdate));
        // same result shape: the lock is metadata on the same Query.
        assert_eq!(q.items.len(), 1);

        let Statement::Select(q) = stmt("SELECT id FROM t FOR SHARE") else {
            panic!("select");
        };
        assert_eq!(q.lock, Some(LockRead::ForShare));

        // no clause -> None; `FOR UPDATE OF t` naming the FROM table is
        // the same lock; OF naming anything else is rejected loudly.
        let Statement::Select(q) = stmt("SELECT id FROM t") else {
            panic!("select");
        };
        assert_eq!(q.lock, None);
        let Statement::Select(q) = stmt("SELECT id FROM t FOR UPDATE OF t") else {
            panic!("select");
        };
        assert_eq!(q.lock, Some(LockRead::ForUpdate));
        assert!(parse_statement("SELECT id FROM t FOR UPDATE OF other").is_err());
    }

    #[test]
    fn unsupported_is_explicit() {
        let e = parse_statement("SELECT * FROM t EXCEPT SELECT * FROM t2").expect_err("e");
        assert!(e.msg.contains("not supported"), "{e}");
        assert!(parse_statement("SELECT * FROM t INTERSECT SELECT * FROM t2").is_err());
        assert!(parse_statement("WITH RECURSIVE r (n) AS (SELECT 1) SELECT n FROM r").is_err());
        // FROM-less SELECT and UNION are supported since phase 1.
        assert!(parse_statement("SELECT 1").is_ok());
        assert!(matches!(
            parse_statement("SELECT * FROM t UNION SELECT * FROM t2"),
            Ok(Statement::SelectCompound(_))
        ));
    }

    // Plain int.frac literals translate to exact Decimals (i128 based,
    // no f64 on the path); exponents and >38-digit spellings stay Doubles.
    #[test]
    fn decimal_literals_translate_exactly() {
        let Statement::Select(q) = stmt("SELECT 0.1, -1.5, 1.50, 2.5e0, 12") else {
            panic!("shape");
        };
        let lits: Vec<Expr> = q
            .items
            .into_iter()
            .map(|i| match i {
                SelectItem::Expr { expr, .. } => expr,
                SelectItem::Wildcard => panic!("wildcard"),
            })
            .collect();
        assert_eq!(lits[0], Expr::Lit(Value::Decimal(1, 1)));
        assert_eq!(
            lits[1],
            Expr::Neg(Box::new(Expr::Lit(Value::Decimal(15, 1))))
        );
        assert_eq!(lits[2], Expr::Lit(Value::Decimal(150, 2)));
        assert_eq!(lits[3], Expr::Lit(Value::Double(2.5)));
        assert_eq!(lits[4], Expr::Lit(Value::Int(12)));
        // 39 significant digits fall back to Double instead of losing digits.
        let wide = format!("SELECT 1{}.5", "2".repeat(38));
        let Statement::Select(q) = stmt(&wide) else {
            panic!("shape");
        };
        let expected: f64 = format!("1{}.5", "2".repeat(38)).parse().unwrap();
        assert_eq!(
            q.items[0],
            SelectItem::Expr {
                expr: Expr::Lit(Value::Double(expected)),
                alias: None
            }
        );
    }
}
