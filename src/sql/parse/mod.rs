//! SQL text -> IR: sqlparser (MySQL dialect) front end.
//!
//! `parse_statement` is the single entry point; `bind_placeholders` fills
//! `?` parameters of prepared statements before execution.

pub mod ast;
pub mod error;
pub(crate) mod expr;
pub(crate) mod query;
pub(crate) mod table;
pub(crate) mod translate;

pub use ast::*;
pub use error::{ErrorCode, SqlError, SqlResult};
pub use translate::{bind_placeholders, parse_statement, placeholder_count};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::storage::schema::{SqlType, Value};

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
    fn unsupported_types_rejected() {
        assert!(parse_statement("CREATE TABLE t (d DECIMAL(10,2), id INT PRIMARY KEY)").is_err());
        assert!(parse_statement("CREATE TABLE t (d DATE, id INT PRIMARY KEY)").is_err());
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
        assert!(matches!(stmt("SET autocommit = 1"), Statement::SetIgnored));
        assert!(matches!(
            stmt("EXPLAIN SELECT * FROM t"),
            Statement::Explain(_)
        ));
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
    fn for_update_flag() {
        let Statement::Select(q) = stmt("SELECT * FROM t WHERE id = 1 FOR UPDATE") else {
            panic!("shape");
        };
        assert!(q.for_update);
    }

    #[test]
    fn unsupported_is_explicit() {
        let e = parse_statement("SELECT * FROM t UNION SELECT * FROM t2").expect_err("u");
        assert!(e.msg.contains("not supported"), "{e}");
        assert!(parse_statement("SELECT 1").is_err()); // no FROM in v1
    }
}
