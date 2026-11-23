//! Multi-store scenario #103 -- the invoice renderer on the SQL plane:
//! the string family (UPPER/LOWER, SUBSTRING, LEFT/RIGHT, LPAD,
//! REPLACE, REPEAT) plus the control quartet (COALESCE/IFNULL/NULLIF)
//! and CAST shape each invoice line, with CONCAT/CONCAT_WS stitching
//! the final text. Expected values cribbed exactly from the
//! sql_funcs_{string,control} e2e suites (text protocol returns
//! Bytes). One REAL node, MySQL front.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, rows, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

fn grid(reply: &[Vec<MVal>]) -> Vec<Vec<String>> {
    reply
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    MVal::NULL => "<NULL>".to_string(),
                    MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                    other => format!("{other:?}"),
                })
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn invoice_lines_render_through_string_and_control_funcs() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("inv103", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Two invoice lines; the second carries a rush note, the first
    //    a NULL note (the NULL-propagation contrast below turns on it).
    ddl(&mut c, "CREATE TABLE lines (id INT PRIMARY KEY, sku VARCHAR(16) NOT NULL, \
        qty INT NOT NULL, unit_cents INT NOT NULL, note VARCHAR(16) NULL)").await;
    run(&mut c, "INSERT INTO lines (id, sku, qty, unit_cents, note) VALUES \
        (1, 'widget-blue', 7, 1250, NULL), (2, 'gizmo-red', 3, 450, 'rush-order')").await;

    // b) Case + slicing family over columns (cribbed exact shapes).
    let got = rows(
        &mut c,
        "SELECT UPPER(sku), LOWER(sku), SUBSTRING(sku, 7, 4), LEFT(sku, 6), RIGHT(sku, 4) \
         FROM lines WHERE id = 1",
    )
    .await;
    assert_eq!(
        grid(&got),
        vec![vec!["WIDGET-BLUE", "widget-blue", "-blu", "widget", "blue"]],
        "upper/lower/substring/left/right"
    );

    // c) Padding + substitution + repetition.
    let got = rows(
        &mut c,
        "SELECT LPAD(CAST(qty AS CHAR), 3, '0'), REPLACE(note, '-', '/'), \
         REPLACE(sku, '-', '_'), REPEAT('=', 3), REPEAT('-', qty) \
         FROM lines WHERE id = 2",
    )
    .await;
    assert_eq!(
        grid(&got),
        vec![vec!["003", "rush/order", "gizmo_red", "===", "---"]],
        "lpad/replace/repeat"
    );

    // d) The control quartet: COALESCE/IFNULL backfill the NULL note,
    //    NULLIF collapses an exact match to NULL.
    let got = rows(
        &mut c,
        "SELECT COALESCE(note, 'standard'), IFNULL(note, 'standard'), \
         NULLIF(qty, 7), NULLIF(qty, 3) FROM lines ORDER BY id",
    )
    .await;
    assert_eq!(
        grid(&got),
        vec![
            vec!["standard", "standard", "<NULL>", "7"],
            vec!["rush-order", "rush-order", "3", "<NULL>"],
        ],
        "coalesce/ifnull/nullif"
    );

    // e) CAST shapes the money column (DECIMAL keeps its scale) and
    //    the id to CHAR (text protocol bytes either way).
    let got = rows(
        &mut c,
        "SELECT CAST(unit_cents AS DECIMAL(6, 2)), CAST(id AS CHAR), CAST(' 42 ' AS SIGNED) \
         FROM lines WHERE id = 1",
    )
    .await;
    assert_eq!(grid(&got), vec![vec!["1250.00", "1", "42"]], "cast family");

    // f) The stitched invoice lines: CONCAT joins sku x qty (no
    //    separator), CONCAT_WS skips the NULL note entirely while the
    //    backfilled render keeps a placeholder in slot two.
    let got = rows(
        &mut c,
        "SELECT CONCAT(UPPER(sku), ' x', qty), \
         CONCAT_WS(' | ', UPPER(sku), CONCAT(qty, 'pcs'), note), \
         CONCAT_WS(' | ', UPPER(sku), CONCAT(qty, 'pcs'), COALESCE(note, 'standard')) \
         FROM lines ORDER BY id",
    )
    .await;
    assert_eq!(
        grid(&got),
        vec![
            vec!["WIDGET-BLUE x7", "WIDGET-BLUE | 7pcs", "WIDGET-BLUE | 7pcs | standard"],
            vec!["GIZMO-RED x3", "GIZMO-RED | 3pcs | rush-order", "GIZMO-RED | 3pcs | rush-order"],
        ],
        "invoice lines"
    );

    // g) CONCAT propagates a NULL operand (crib: CONCAT('a', NULL) is
    //    NULL), so a NULL note kills the naive join -- the renderer
    //    must COALESCE first.
    let got = rows(&mut c, "SELECT CONCAT(note, '!') FROM lines WHERE id = 1").await;
    assert_eq!(grid(&got), vec![vec!["<NULL>"]], "concat null propagation");
}
