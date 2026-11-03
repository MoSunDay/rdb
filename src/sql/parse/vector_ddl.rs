//! `CREATE VECTOR INDEX` front end (pre-parser, P4).
//!
//! sqlparser's MySQL grammar has no vector-index production, so the
//! raw text is rewritten BEFORE the parse into the plain-index shape
//! `CREATE [IF NOT EXISTS] INDEX name ON t (vec_ann(col, 'metric', n))`
//! and `translate_ddl` lifts the marker function back out into
//! [`crate::sql::parse::ast::Statement::CreateVectorIndex`]. Any other
//! statement passes through byte-identical. Pure token slicing, no
//! regex -- same conventions as `starrocks`.

use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};

/// Whitespace/punctuation lexer: `(` `)` `,` stand alone, quoted runs
/// (`'..'`, `` `..` ``, `".."`) are one token, everything else splits
/// on whitespace. Just enough for the fixed CREATE VECTOR INDEX shape.
fn tokenize(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' | b';' => i += 1,
            b'(' | b')' | b',' => {
                out.push((c as char).to_string());
                i += 1;
            }
            b'\'' | b'`' | b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != c {
                    j += 1;
                }
                out.push(String::from_utf8_lossy(&b[i..(j + 1).min(b.len())]).into_owned());
                i = j + 1;
            }
            _ => {
                let mut j = i;
                while j < b.len()
                    && !matches!(
                        b[j],
                        b' ' | b'\t' | b'\r' | b'\n' | b';' | b'(' | b')' | b','
                    )
                {
                    j += 1;
                }
                out.push(String::from_utf8_lossy(&b[i..j]).into_owned());
                i = j;
            }
        }
    }
    out
}

/// Marker function name injected by the rewrite (never a real scalar
/// function: it exists only between this module and translate_ddl).
pub const MARKER: &str = "vec_ann";

/// Rewrite one statement if it is `CREATE VECTOR INDEX name ON t (col)
/// [WITH (METRIC 'cosine'|'euclidean'|'dot', NPROBE n)]`. Both `WITH`
/// keys are optional and case-insensitive; the metric quote style is
/// MySQL text literal.
pub fn rewrite(sql: &str) -> SqlResult<String> {
    let tokens: Vec<String> = tokenize(sql);
    if tokens.len() < 2 || !eq_kw(&tokens[0], "CREATE") || !eq_kw(&tokens[1], "VECTOR") {
        return Ok(sql.to_string());
    }
    if tokens.len() < 3 || !eq_kw(&tokens[2], "INDEX") {
        return Err(SqlError::unsupported("expected INDEX after CREATE VECTOR"));
    }
    let mut at = 3;
    let if_not_exists = tokens.get(at).map(|t| eq_kw(t, "IF")).unwrap_or(false) && {
        let ok = tokens.get(at + 1).map(|t| eq_kw(t, "NOT")).unwrap_or(false)
            && tokens
                .get(at + 2)
                .map(|t| eq_kw(t, "EXISTS"))
                .unwrap_or(false);
        at += 3;
        ok
    };
    let Some(name) = tokens.get(at) else {
        return Err(SqlError::parse("CREATE VECTOR INDEX: missing index name"));
    };
    at += 1;
    if !tokens.get(at).map(|t| eq_kw(t, "ON")).unwrap_or(false) {
        return Err(SqlError::parse("CREATE VECTOR INDEX: expected ON <table>"));
    }
    let Some(table) = tokens.get(at + 1) else {
        return Err(SqlError::parse("CREATE VECTOR INDEX: missing table"));
    };
    at += 2;
    // ( col ) -- one bare column between the parens
    if tokens.get(at).map(|t| t.as_str()) != Some("(") {
        return Err(SqlError::parse("CREATE VECTOR INDEX: expected (column)"));
    }
    let Some(col) = tokens.get(at + 1) else {
        return Err(SqlError::parse("CREATE VECTOR INDEX: missing column"));
    };
    if tokens.get(at + 2).map(|t| t.as_str()) != Some(")") {
        return Err(SqlError::parse(
            "CREATE VECTOR INDEX: exactly one column, no options",
        ));
    }
    at += 3;
    let mut metric = "cosine".to_string();
    let mut nprobe = crate::sql::index::vector::DEFAULT_NPROBE.to_string();
    if tokens.get(at).map(|t| eq_kw(t, "WITH")).unwrap_or(false) {
        let (m, n, next) = parse_with(&tokens, at + 1)?;
        metric = m;
        nprobe = n;
        at = next;
    }
    if tokens.get(at).is_some() {
        return Err(SqlError::unsupported("CREATE VECTOR INDEX: trailing input"));
    }
    Ok(format!(
        "CREATE INDEX{ine} {name} ON {table} ({MARKER}({col}, '{metric}', {nprobe}))",
        ine = if if_not_exists { " IF NOT EXISTS" } else { "" },
    ))
}

/// `WITH ( key value, ... )`: keys METRIC (quoted string) and NPROBE
/// (integer). Returns (metric, nprobe, index after `)`).
fn parse_with(tokens: &[String], mut at: usize) -> SqlResult<(String, String, usize)> {
    if tokens.get(at).map(|t| t.as_str()) != Some("(") {
        return Err(SqlError::parse(
            "CREATE VECTOR INDEX: expected ( after WITH",
        ));
    }
    at += 1;
    let mut metric = None;
    let mut nprobe = None;
    loop {
        let Some(key) = tokens.get(at) else {
            return Err(SqlError::parse("CREATE VECTOR INDEX: unterminated WITH"));
        };
        let Some(val) = tokens.get(at + 1) else {
            return Err(SqlError::parse("CREATE VECTOR INDEX: missing value"));
        };
        match key.to_ascii_uppercase().as_str() {
            "METRIC" => {
                let v = val.trim_matches('\'');
                if crate::sql::index::vector::Metric::parse(v).is_none() {
                    return Err(SqlError::new(
                        ErrorCode::NotSupported,
                        format!("unknown vector metric '{v}' (cosine|euclidean|dot)"),
                    ));
                }
                metric = Some(v.to_string());
            }
            "NPROBE" => {
                if val.parse::<u16>().is_err() {
                    return Err(SqlError::parse(
                        "CREATE VECTOR INDEX: NPROBE must be an integer",
                    ));
                }
                nprobe = Some(val.clone());
            }
            other => {
                return Err(SqlError::unsupported(format!(
                    "CREATE VECTOR INDEX WITH: unknown option '{other}'"
                )))
            }
        }
        at += 2;
        match tokens.get(at).map(|t| t.as_str()) {
            Some(",") => at += 1,
            Some(")") => {
                return Ok((
                    metric.unwrap_or_else(|| "cosine".into()),
                    nprobe.unwrap_or_else(|| "4".into()),
                    at + 1,
                ))
            }
            _ => {
                return Err(SqlError::parse(
                    "CREATE VECTOR INDEX: expected , or ) in WITH",
                ))
            }
        }
    }
}

fn eq_kw(tok: &str, kw: &str) -> bool {
    tok.eq_ignore_ascii_case(kw)
}
