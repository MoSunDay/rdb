//! trim_default unit tests: the injected default, the untouched
//! shapes, and the literal/comment skipping that keeps prose SQL safe.

use super::expand;

#[test]
fn injects_space_default_into_keyword_only_forms() {
    for (sql, mode) in [
        ("TRIM(BOTH FROM x)", "BOTH"),
        ("TRIM(LEADING FROM x)", "LEADING"),
        ("TRIM(TRAILING FROM x)", "TRAILING"),
    ] {
        let out = expand(&format!("SELECT {sql}"));
        assert_eq!(out, format!("SELECT TRIM({mode} ' ' FROM x)"), "{sql}");
    }
}

#[test]
fn lower_case_and_gaps_still_match() {
    assert_eq!(
        expand("select trim( leading\tfrom a )"),
        "select trim( leading\t' ' from a )"
    );
    // Several TRIMs in one statement each get their default.
    assert_eq!(
        expand("SELECT TRIM(BOTH FROM a), TRIM(TRAILING FROM b)"),
        "SELECT TRIM(BOTH ' ' FROM a), TRIM(TRAILING ' ' FROM b)"
    );
}

#[test]
fn other_trim_shapes_pass_through_byte_identical() {
    for sql in [
        "SELECT TRIM(a)",
        "SELECT TRIM('x' FROM a)",
        "SELECT TRIM(LEADING 'x' FROM a)",
        "SELECT TRIM(BOTH '?' FROM a)",
        // A bare `trim` column name never opens a call.
        "SELECT trim FROM t",
        // The remstr expression may itself spell FROM-adjacent text.
        "SELECT TRIM(LEADING FROMX FROM a)",
    ] {
        assert_eq!(expand(sql), sql.to_string(), "{sql}");
    }
}

#[test]
fn literals_and_comments_are_not_scanned() {
    for sql in [
        // The shape inside a string literal stays put.
        "SELECT 'TRIM(LEADING FROM x)'",
        "SELECT CONCAT('a', 'TRIM(BOTH FROM b)') FROM t",
        "-- TRIM(LEADING FROM x)\nSELECT 1",
        "/* TRIM(BOTH FROM x) */ SELECT 1",
        "SELECT 1 # TRIM(TRAILING FROM x)",
        // Unterminated string: the sqlparser pass owns that error.
        "SELECT 'unterminated TRIM(LEADING FROM x)",
    ] {
        assert_eq!(expand(sql), sql.to_string(), "{sql}");
    }
}
