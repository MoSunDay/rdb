use super::*;
use crate::sql::exec::{ddl, write, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::ErrorCode;
use crate::sql::parse::{bind_placeholders, parse_statement, placeholder_count};
use crate::state::testutil;

/// Engine with `t(id BIGINT PK, v VARCHAR NULL)` holding
/// (1,'b'), (2,NULL), (3,'a'), (4,NULL).
async fn setup() -> crate::state::Shared {
    let shared = testutil::shared_with(testutil::test_config());
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)").unwrap(),
    )
    .await
    .unwrap();
    write::insert(
        &shared,
        &mut SqlSession::default(),
        parse_statement("INSERT INTO t (id, v) VALUES (1, 'b'), (2, NULL), (3, 'a'), (4, NULL)")
            .unwrap(),
    )
    .await
    .unwrap();
    shared
}

async fn select_all(shared: &crate::state::Shared, sql: &str) -> (Vec<ColMeta>, Vec<Vec<Value>>) {
    let Statement::Select(q) = parse_statement(sql).unwrap() else {
        panic!("select");
    };
    run(shared, &SqlSession::default(), q).await.unwrap()
}

async fn col(shared: &crate::state::Shared, sql: &str) -> Vec<Value> {
    select_all(shared, sql)
        .await
        .1
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

#[tokio::test]
async fn where_drops_null_comparands() {
    // v = 'a' never matches NULL v rows
    let got = col(&setup().await, "SELECT id FROM t WHERE v = 'a'").await;
    assert_eq!(got, vec![Value::Int(3)]);
    // NULL-safe IS NULL keeps them
    let got = col(
        &setup().await,
        "SELECT id FROM t WHERE v IS NULL ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![Value::Int(2), Value::Int(4)]);
}

#[tokio::test]
async fn order_by_treats_null_as_smallest() {
    let asc = col(&setup().await, "SELECT v FROM t ORDER BY v ASC, id ASC").await;
    assert_eq!(
        asc,
        vec![
            Value::Null,
            Value::Null,
            Value::Str("a".into()),
            Value::Str("b".into())
        ]
    );
    // DESC flips both the values and the NULL placement
    let desc = col(&setup().await, "SELECT v FROM t ORDER BY v DESC, id DESC").await;
    assert_eq!(
        desc,
        vec![
            Value::Str("b".into()),
            Value::Str("a".into()),
            Value::Null,
            Value::Null
        ]
    );
}

#[tokio::test]
async fn limit_offset_and_distinct() {
    let got = col(
        &setup().await,
        "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1",
    )
    .await;
    assert_eq!(got, vec![Value::Int(2), Value::Int(3)]);
    // distinct over the projected v: NULL appears once
    let got = col(&setup().await, "SELECT DISTINCT v FROM t ORDER BY v ASC").await;
    assert_eq!(
        got,
        vec![Value::Null, Value::Str("a".into()), Value::Str("b".into())]
    );
}

#[tokio::test]
async fn group_by_aggregates_and_empty_global() {
    let shared = setup().await;
    let (meta, rows) = select_all(
        &shared,
        "SELECT v, COUNT(*) AS n, SUM(id) AS s FROM t GROUP BY v ORDER BY v",
    )
    .await;
    // NULL group first (smallest), SUM skips nothing (ids never NULL)
    assert_eq!(
        rows,
        vec![
            vec![Value::Null, Value::Int(2), Value::Int(6)],
            vec![Value::Str("a".into()), Value::Int(1), Value::Int(3)],
            vec![Value::Str("b".into()), Value::Int(1), Value::Int(1)],
        ]
    );
    assert_eq!(meta[1].name, "n", "alias is the output column name");
    // global aggregate over zero rows: COUNT(*)=0, SUM=NULL
    let (_, rows) = select_all(&shared, "SELECT COUNT(*), SUM(id) FROM t WHERE id > 100").await;
    assert_eq!(rows, vec![vec![Value::Int(0), Value::Null]]);
    // HAVING filters groups
    let got = col(&shared, "SELECT v FROM t GROUP BY v HAVING COUNT(*) > 1").await;
    assert_eq!(got, vec![Value::Null]);
}

#[tokio::test]
async fn join_qualified_columns_and_ambiguity() {
    let shared = setup().await;
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE u (id BIGINT PRIMARY KEY, tag VARCHAR(8) NULL)").unwrap(),
    )
    .await
    .unwrap();
    write::insert(
        &shared,
        &mut SqlSession::default(),
        parse_statement("INSERT INTO u (id, tag) VALUES (1, 'x'), (3, 'y')").unwrap(),
    )
    .await
    .unwrap();
    let (_, rows) = select_all(
        &shared,
        "SELECT t.id, u.tag FROM t JOIN u ON t.id = u.id ORDER BY t.id",
    )
    .await;
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Str("x".into())],
            vec![Value::Int(3), Value::Str("y".into())],
        ]
    );
    // bare `id` exists on both sides -> ambiguity is an error
    let Statement::Select(q) = parse_statement("SELECT id FROM t JOIN u ON t.id = u.id").unwrap()
    else {
        panic!("select");
    };
    let read_ts = shared.sql_ts.now();
    let ctes = crate::sql::exec::relation::CteScope::default();
    let _ = scan::materialize(&shared, &q.from, read_ts, None, None, &ctes)
        .await
        .unwrap();
    let err = run(&shared, &SqlSession::default(), q).await.unwrap_err();
    assert!(err.msg.contains("ambiguous column 'id'"), "{}", err.msg);
    // unknown column
    let Statement::Select(q) = parse_statement("SELECT nope FROM t").unwrap() else {
        panic!("select");
    };
    let err = run(&shared, &SqlSession::default(), q)
        .await
        .expect_err("unknown column must error");
    assert!(err.msg.contains("unknown column"), "{}", err.msg);
}

#[tokio::test]
async fn explain_and_alias_metadata() {
    let shared = setup().await;
    let (meta, _) = select_all(&shared, "SELECT v AS label FROM t LIMIT 1").await;
    assert_eq!(meta.len(), 1);
    assert_eq!(meta[0].name, "label");
    // wildcard expands to every column, metadata included
    let (meta, rows) = select_all(&shared, "SELECT * FROM t WHERE id = 1").await;
    assert_eq!(meta.len(), 2);
    assert_eq!(rows, vec![vec![Value::Int(1), Value::Str("b".into())]]);
}

/// EXPLAIN headline: single-node topologies keep the plain SeqScan
/// (or planner index) verdict; a ready multi-node cluster plans the
/// same single-table read as scatter-gather, banner on top.
#[test]
fn explain_headline_prefers_gather_in_cluster_mode() {
    let shared = testutil::shared_with(testutil::test_config());
    let Statement::Select(q) = parse_statement("SELECT id FROM t").unwrap() else {
        panic!("select");
    };
    // No cluster: no storage-aware verdict (no catalog entry either).
    assert!(headline_lines(&shared, &q).is_empty());
    *shared.topology.write().unwrap() = crate::topology::refresh("a, b, c");
    assert_eq!(
        headline_lines(&shared, &q),
        vec!["Gather(bands=3)".to_string(), "SeqScan t".to_string()]
    );
}

#[test]
fn result_type_pins_literals_and_typed_functions() {
    let scope = FromScope::default();
    let lit = |v| result_type(&Expr::Lit(v), &scope);
    assert_eq!(lit(Value::Int(1)), SqlType::Int);
    assert_eq!(lit(Value::Double(1.5)), SqlType::Double);
    assert_eq!(lit(Value::Str("x".into())), SqlType::VarChar);
    assert_eq!(lit(Value::Bool(true)), SqlType::Bool);
    // NULL has no domain of its own; the wire type stays text.
    assert_eq!(lit(Value::Null), SqlType::VarChar);
    let func = |name: &str| {
        result_type(
            &Expr::Func {
                name: name.to_string(),
                args: vec![],
            },
            &scope,
        )
    };
    assert_eq!(func("last_insert_id"), SqlType::Int);
    assert_eq!(func("length"), SqlType::Int);
    assert_eq!(func("now"), SqlType::DateTime);
    assert_eq!(func("CURDATE"), SqlType::Date);
    // everything else stays text: heterogeneous string funcs dominate.
    assert_eq!(func("concat"), SqlType::VarChar);
    assert_eq!(func("upper"), SqlType::VarChar);
    // placeholders are unknown until bind.
    assert_eq!(result_type(&Expr::Placeholder, &scope), SqlType::VarChar);
}

/// Full-pipeline smoke of the M1 groundwork nodes: parse -> execute ->
/// rows, plus the result-column metadata of CASE/CAST/REGEXP/`<=>`.
#[tokio::test]
async fn case_cast_regexp_operators_execute() {
    let shared = setup().await;
    // CASE searched + simple forms over real rows (v is NULL on 2, 4).
    let rows = col(
        &shared,
        "SELECT CASE WHEN v IS NULL THEN 'none' ELSE v END FROM t WHERE id <= 2 ORDER BY id",
    )
    .await;
    assert_eq!(
        rows,
        vec![Value::Str("b".into()), Value::Str("none".into())]
    );
    let rows = col(
        &shared,
        "SELECT CASE id WHEN 1 THEN 'one' ELSE 'other' END FROM t ORDER BY id",
    )
    .await;
    assert_eq!(rows[0], Value::Str("one".into()));

    // CAST matrix through the executor + typed result columns.
    let (meta, rows) = select_all(&shared, "SELECT CAST('12.5' AS SIGNED)").await;
    assert_eq!(meta[0].sql_type, SqlType::Int);
    assert_eq!(rows[0][0], Value::Int(13));
    let (meta, rows) = select_all(&shared, "SELECT CAST(42 AS CHAR(2))").await;
    assert_eq!(meta[0].sql_type, SqlType::VarChar);
    assert_eq!(rows[0][0], Value::Str("42".into()));
    let (meta, rows) = select_all(&shared, "SELECT CAST(1 AS DECIMAL(5,2))").await;
    assert_eq!(
        meta[0].sql_type,
        SqlType::Decimal {
            precision: 5,
            scale: 2
        }
    );
    assert_eq!(rows[0][0], Value::Decimal(100, 2));

    // REGEXP (1/0, NOT form) and the new operators in WHERE.
    let rows = col(&shared, "SELECT 'hello' REGEXP '^h'").await;
    assert_eq!(rows, vec![Value::Int(1)]);
    let rows = col(&shared, "SELECT 'hello' NOT REGEXP '^z'").await;
    assert_eq!(rows, vec![Value::Int(1)]);
    let ids = col(&shared, "SELECT id FROM t WHERE v <=> NULL ORDER BY id").await;
    assert_eq!(ids, vec![Value::Int(2), Value::Int(4)]);
    let (_, rows) = select_all(&shared, "SELECT 5 & 3, 1 << 4, 1 XOR 0").await;
    assert_eq!(
        rows,
        vec![vec![Value::Int(1), Value::Int(16), Value::Bool(true)]]
    );
    // EXPLAIN renders the new nodes without panicking.
    let (meta, rows) = select_all(&shared, "SELECT CASE WHEN 1=1 THEN 1 ELSE 2 END").await;
    assert_eq!(meta[0].sql_type, SqlType::Int);
    assert_eq!(rows[0][0], Value::Int(1));
}

#[tokio::test]
async fn select_now_metadata_is_datetime() {
    let shared = setup().await;
    let (meta, rows) = select_all(&shared, "SELECT NOW()").await;
    assert_eq!(meta[0].sql_type, SqlType::DateTime);
    assert!(matches!(rows[0][0], Value::DateTime(_)));
    // literals type by value: 1 -> INT, 'x' -> text (kills the old
    // "everything is VARCHAR" deviation for typed result columns)
    let (meta, _) = select_all(&shared, "SELECT 1").await;
    assert_eq!(meta[0].sql_type, SqlType::Int);
    let (meta, _) = select_all(&shared, "SELECT 'x'").await;
    assert_eq!(meta[0].sql_type, SqlType::VarChar);
    let (meta, _) = select_all(&shared, "SELECT CURDATE()").await;
    assert_eq!(meta[0].sql_type, SqlType::Date);
}

// ---- M0 query semantics: ordinals, aliases, LIMIT ?, FROM DUAL ----

/// Same harness as `select_all` but for statements carrying `?`
/// placeholders: count, bind, then run.
async fn select_bound(
    shared: &crate::state::Shared,
    sql: &str,
    values: &[Value],
) -> crate::sql::parse::SqlResult<(Vec<ColMeta>, Vec<Vec<Value>>)> {
    let mut stmt = parse_statement(sql).unwrap();
    assert_eq!(
        placeholder_count(&stmt),
        values.len(),
        "placeholder count must include LIMIT / OFFSET parameters"
    );
    bind_placeholders(&mut stmt, values).expect("bind");
    let Statement::Select(q) = stmt else {
        panic!("select");
    };
    run(shared, &SqlSession::default(), q).await
}

#[tokio::test]
async fn order_by_ordinal_sorts_by_output_column() {
    let shared = setup().await;
    // ORDER BY 2 == ORDER BY v (NULLs smallest, stable); col() takes
    // the first projected column (id).
    let got = col(&shared, "SELECT id, v FROM t ORDER BY 2").await;
    assert_eq!(
        got,
        vec![Value::Int(2), Value::Int(4), Value::Int(3), Value::Int(1)]
    );
    // ordinal over an expression projection sorts by its value
    let got = col(&shared, "SELECT id * id FROM t ORDER BY 1 DESC").await;
    assert_eq!(
        got,
        vec![Value::Int(16), Value::Int(9), Value::Int(4), Value::Int(1)]
    );
    // quoted '1' is a constant, not a position: scan order survives
    let got = col(&shared, "SELECT id * id FROM t ORDER BY '1'").await;
    assert_eq!(
        got,
        vec![Value::Int(1), Value::Int(4), Value::Int(9), Value::Int(16)]
    );
}

#[tokio::test]
async fn order_by_ordinal_out_of_range_errors() {
    // out-of-range ordinals fail at translate time, ER 1054 style
    let err = parse_statement("SELECT id FROM t ORDER BY 9").expect_err("out of range");
    assert_eq!(err.code, ErrorCode::BadField);
    assert!(
        err.msg.contains("Unknown column '9' in 'order clause'"),
        "{}",
        err.msg
    );
}

#[tokio::test]
async fn group_by_ordinal_groups_by_output_column() {
    let shared = setup().await;
    let (_, rows) = select_all(&shared, "SELECT v, COUNT(*) FROM t GROUP BY 1 ORDER BY 1").await;
    assert_eq!(
        rows,
        vec![
            vec![Value::Null, Value::Int(2)],
            vec![Value::Str("a".into()), Value::Int(1)],
            vec![Value::Str("b".into()), Value::Int(1)],
        ]
    );
    // out-of-range GROUP BY ordinal fails at translate time
    let err = parse_statement("SELECT v FROM t GROUP BY 3").expect_err("out of range");
    assert!(
        err.msg.contains("Unknown column '3' in 'group statement'"),
        "{}",
        err.msg
    );
}

#[tokio::test]
async fn order_by_alias_sorts_by_projection() {
    let shared = setup().await;
    let got = col(&shared, "SELECT id * id AS sq FROM t ORDER BY sq DESC").await;
    assert_eq!(
        got,
        vec![Value::Int(16), Value::Int(9), Value::Int(4), Value::Int(1)]
    );
    // a bare alias reference that matches no projection or FROM column
    // still fails unknown-column
    let Statement::Select(q) = parse_statement("SELECT id FROM t ORDER BY nope").unwrap() else {
        panic!("select");
    };
    let err = run(&shared, &SqlSession::default(), q)
        .await
        .expect_err("unknown column");
    assert!(err.msg.contains("unknown column"), "{}", err.msg);
}

#[tokio::test]
async fn having_alias_filters_groups() {
    let shared = setup().await;
    let rows = select_all(
        &shared,
        "SELECT v, COUNT(*) AS cnt FROM t GROUP BY v HAVING cnt > 1",
    )
    .await
    .1;
    assert_eq!(rows, vec![vec![Value::Null, Value::Int(2)]]);
}

#[tokio::test]
async fn alias_beats_source_column_in_order_by() {
    let shared = setup().await;
    // `v` names both a FROM column and the alias of `id`: the alias
    // wins (MySQL select-list preference), so rows come out by id.
    let got = col(&shared, "SELECT id AS v, v AS id FROM t ORDER BY v").await;
    assert_eq!(
        got,
        vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]
    );
}

#[tokio::test]
async fn limit_and_offset_placeholders_bind_and_run() {
    let shared = setup().await;
    let (meta, rows) = select_bound(
        &shared,
        "SELECT id FROM t ORDER BY id LIMIT ? OFFSET ?",
        &[Value::Int(2), Value::Int(2)],
    )
    .await
    .expect("bound limit");
    assert_eq!(meta.len(), 1);
    assert_eq!(rows, vec![vec![Value::Int(3)], vec![Value::Int(4)]]);
    // bad bindings reject at execution with the LIMIT literal error
    for values in [
        vec![Value::Int(-1), Value::Int(0)],
        vec![Value::Double(2.0), Value::Int(0)],
        vec![Value::Str("2".into()), Value::Int(0)],
    ] {
        let err = select_bound(&shared, "SELECT id FROM t LIMIT ? OFFSET ?", &values)
            .await
            .expect_err("bad limit binding");
        assert!(
            err.msg.contains("LIMIT must be a non-negative integer"),
            "{values:?}: {err}"
        );
    }
}

#[tokio::test]
async fn from_dual_behaves_like_no_from() {
    let shared = setup().await;
    let rows = select_all(&shared, "SELECT 1 AS one FROM DUAL").await.1;
    assert_eq!(rows, vec![vec![Value::Int(1)]]);
    // WHERE / LIMIT apply to the single synthetic row
    let rows = select_all(&shared, "SELECT 2 FROM dual WHERE 1 = 0")
        .await
        .1;
    assert!(rows.is_empty());
    let rows = select_all(&shared, "SELECT 3 FROM DUAL LIMIT 1").await.1;
    assert_eq!(rows, vec![vec![Value::Int(3)]]);
    // a lookalike name is still an ordinary (missing) table
    let Statement::Select(q) = parse_statement("SELECT 1 FROM dual2").unwrap() else {
        panic!("select");
    };
    let err = run(&shared, &SqlSession::default(), q)
        .await
        .expect_err("dual2 must stay a table name");
    assert_eq!(err.code, ErrorCode::NoSuchTable, "{}", err.msg);
}

// ---- part B: full-pipeline smoke of the function families ----

#[tokio::test]
async fn part_b_function_families_execute() {
    let shared = setup().await;
    // String family through SQL text, including the FROM/FOR and TRIM
    // special forms.
    let (_, rows) = select_all(
        &shared,
        "SELECT CONCAT('a', 1, 2.5), CONCAT_WS('-', 'a', NULL, 'b')",
    )
    .await;
    assert_eq!(
        rows,
        vec![vec![Value::Str("a12.5".into()), Value::Str("a-b".into())]]
    );
    let rows = col(&shared, "SELECT SUBSTRING('Quadratically' FROM 5 FOR 6)").await;
    assert_eq!(rows, vec![Value::Str("ratica".into())]);
    let rows = col(&shared, "SELECT TRIM(BOTH 'x' FROM 'xxbarxx')").await;
    assert_eq!(rows, vec![Value::Str("bar".into())]);
    let rows = col(&shared, "SELECT LPAD('hi', 7, '?.')").await;
    assert_eq!(rows, vec![Value::Str("?.?.?hi".into())]);
    let (_, rows) = select_all(&shared, "SELECT HEX('ab'), UNHEX('6162')").await;
    // HEX answers text; UNHEX answers bytes.
    assert_eq!(
        rows,
        vec![vec![
            Value::Str("6162".into()),
            Value::Bytes(b"ab".to_vec())
        ]]
    );

    // Numeric family: exact decimal round with NEWDECIMAL metadata.
    let (meta, rows) = select_all(
        &shared,
        "SELECT ROUND(2.005, 2), TRUNCATE(-1.999, 1), GREATEST(1, 2, 3)",
    )
    .await;
    assert_eq!(
        meta[0].sql_type,
        SqlType::Decimal {
            precision: 38,
            scale: 2
        }
    );
    assert_eq!(
        rows[0],
        vec![
            Value::Decimal(201, 2),
            Value::Decimal(-19, 1), // exact: -1.999 truncated to -1.9
            Value::Int(3)
        ]
    );

    // Datetime family: INTERVAL forms, DATEDIFF, DATE_FORMAT, the
    // UNIX_TIMESTAMP pair.
    let rows = col(&shared, "SELECT DATE_ADD('2024-01-31', INTERVAL 1 MONTH)").await;
    assert_eq!(rows, vec![Value::Str("2024-02-29".into())]);
    let rows = col(
        &shared,
        "SELECT DATE_ADD('2024-01-31 00:00:00', INTERVAL 1 DAY)",
    )
    .await;
    assert_eq!(rows, vec![Value::Str("2024-02-01 00:00:00".into())]);
    let rows = col(&shared, "SELECT '2024-03-01' - INTERVAL 2 DAY").await;
    assert_eq!(rows, vec![Value::Str("2024-02-28".into())]);
    let rows = col(&shared, "SELECT DATEDIFF('2024-01-02', '2024-01-05')").await;
    assert_eq!(rows, vec![Value::Int(-3)]);
    let rows = col(
        &shared,
        "SELECT DATE_FORMAT('2024-02-29 13:05:09', '%Y-%m-%d %T %W')",
    )
    .await;
    assert_eq!(
        rows,
        vec![Value::Str("2024-02-29 13:05:09 Thursday".into())]
    );
    let rows = col(
        &shared,
        "SELECT FROM_UNIXTIME(UNIX_TIMESTAMP('2024-01-02 03:04:05'))",
    )
    .await;
    assert_eq!(rows, vec![Value::Str("2024-01-02 03:04:05".into())]);

    // Lazy control family over the NULL-bearing column.
    let rows = col(&shared, "SELECT IFNULL(v, 'none') FROM t ORDER BY id").await;
    assert_eq!(
        rows,
        vec![
            Value::Str("b".into()),
            Value::Str("none".into()),
            Value::Str("a".into()),
            Value::Str("none".into())
        ]
    );
    let rows = col(
        &shared,
        "SELECT IF(v <=> NULL, 'null', 'set') FROM t WHERE id <= 2",
    )
    .await;
    assert_eq!(
        rows,
        vec![Value::Str("set".into()), Value::Str("null".into())]
    );
    let rows = col(&shared, "SELECT COALESCE(NULL, NULLIF('a', 'a'), 3)").await;
    assert_eq!(rows, vec![Value::Int(3)]);

    // GROUP_CONCAT: separator, DISTINCT, NULL skipping.
    let rows = col(&shared, "SELECT GROUP_CONCAT(v SEPARATOR '|') FROM t").await;
    assert_eq!(rows, vec![Value::Str("b|a".into())]);
    let rows = col(
        &shared,
        "SELECT GROUP_CONCAT(DISTINCT IFNULL(v, 'a')) FROM t",
    )
    .await;
    // Rows scan in pk order: 'b','a','a','a' -> dedup -> b,a.
    assert_eq!(rows, vec![Value::Str("b,a".into())]);
    // Functions ride WHERE too.
    let ids = col(
        &shared,
        "SELECT id FROM t WHERE CHAR_LENGTH(IFNULL(v, '')) = 0 ORDER BY id",
    )
    .await;
    assert_eq!(ids, vec![Value::Int(2), Value::Int(4)]);
}
