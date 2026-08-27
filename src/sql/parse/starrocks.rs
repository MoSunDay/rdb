//! StarRocks table-model DDL front end (pre-parser, Phase 3).
//!
//! sqlparser 0.62's MySQL dialect has no grammar for StarRocks'
//! `DUPLICATE KEY(...)`, after-columns `PRIMARY KEY(...)` or
//! `DISTRIBUTED BY HASH(...) BUCKETS n` clauses, so those are lifted
//! out of the raw text BEFORE the MySQL parse:
//!
//! * recognized clauses are stripped; the PK model's key list is
//!   injected back as a MySQL `PRIMARY KEY(...)` table constraint, so
//!   the rest of the pipeline (the single-pk check included) is reused
//!   unchanged;
//! * statements without any StarRocks clause pass through
//!   byte-identical -- zero behavior change for MySQL DDL;
//! * other StarRocks clauses (`PARTITION BY`, `PROPERTIES`,
//!   `ORDER BY`, `UNIQUE KEY`, `AGGREGATE KEY`,
//!   `DISTRIBUTED BY RANDOM`) are rejected loudly (MySQL 1235) with a
//!   clause-naming message instead of a generic syntax error.
//!
//! Pure functions over token slices -- no regex.

use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{Distribution, KeyModel};

/// Table-model facts lifted out of one StarRocks CREATE TABLE.
#[derive(Debug, Clone, PartialEq)]
pub struct StarRocksModel {
    /// `PrimaryKey` or `Duplicate` (never `MySql` from the parser).
    pub kind: KeyModel,
    /// Columns named by the model clause (`keys[0]` is the schema pk
    /// of a DUPLICATE table -- metadata only, no dedup runs on it).
    pub keys: Vec<String>,
    /// `DISTRIBUTED BY HASH(...) BUCKETS n`, when present.
    pub distribution: Option<Distribution>,
}

/// One lexical token: `raw` keeps the exact source bytes (quotes
/// included), `upper` is the case-folded form of an unquoted word,
/// `start` is the byte offset of the first byte.
#[derive(Debug, Clone, PartialEq)]
struct Tok {
    raw: String,
    upper: Option<String>,
    start: usize,
}

fn is_kw(t: &Tok, kw: &str) -> bool {
    t.upper.as_deref() == Some(kw)
}

fn unsupported(what: &str) -> SqlError {
    SqlError::new(ErrorCode::NotSupported, format!("StarRocks {what}"))
}

/// Lex SQL text into tokens. Whitespace and (`--`, `#`, `/* */`)
/// comments are dropped; `'...'`/`"..."`/`` `...` `` spans (with their
/// doubling/backslash escapes) become single tokens keeping raw bytes.
fn lex(sql: &str) -> SqlResult<Vec<Tok>> {
    let b = sql.as_bytes();
    let mut i = 0usize;
    let mut toks = Vec::new();
    while i < b.len() {
        let c = b[i];
        let start = i;
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'-' | b'#' if c == b'#' || b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = match b[i + 2..].windows(2).position(|w| w == b"*/") {
                    Some(p) => i + 2 + p + 2,
                    None => return Err(SqlError::parse("unterminated /* comment")),
                };
            }
            b'\'' | b'"' | b'`' => {
                i = skip_quoted(b, i)?;
                toks.push(tok_of(sql, start, i, None));
            }
            _ if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$')
                {
                    i += 1;
                }
                let upper = sql[start..i].to_ascii_uppercase();
                toks.push(tok_of(sql, start, i, Some(upper)));
            }
            _ => {
                i += 1;
                toks.push(tok_of(sql, start, i, None));
            }
        }
    }
    Ok(toks)
}

fn tok_of(sql: &str, start: usize, end: usize, upper: Option<String>) -> Tok {
    Tok {
        raw: sql[start..end].to_string(),
        upper,
        start,
    }
}

/// Scan one quoted span starting at the quote byte; returns the index
/// just past the closing quote. Doubled quotes escape; backslash also
/// escapes inside '...' and "..." (MySQL default).
fn skip_quoted(b: &[u8], start: usize) -> SqlResult<usize> {
    let q = b[start];
    let backslash_escapes = q != b'`';
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' if backslash_escapes => i += 2,
            c if c == q => {
                if b.get(i + 1) == Some(&q) {
                    i += 2; // doubled quote: escaped
                } else {
                    return Ok(i + 1);
                }
            }
            _ => i += 1,
        }
    }
    Err(SqlError::parse("unterminated quoted token"))
}

/// Pre-parse one statement: extract StarRocks table-model clauses and
/// rewrite the text into MySQL-parseable form. Statements that are not
/// CREATE TABLE, or carry no recognized StarRocks clause, are returned
/// unchanged with `None` (the MySQL path behaves exactly as before).
pub fn preparse(sql: &str) -> SqlResult<(String, Option<StarRocksModel>)> {
    let toks = lex(sql)?;
    let Some((open, close)) = create_table_span(&toks) else {
        return Ok((sql.to_string(), None));
    };
    let mut model: Option<KeyModel> = None;
    let mut keys: Vec<String> = Vec::new();
    let mut distribution: Option<Distribution> = None;
    let mut kept: Vec<&Tok> = Vec::new();
    let mut any = false;
    let mut i = close + 1;
    // Clauses belong to THIS statement; stop extracting at `;`.
    while i < toks.len() && toks[i].raw != ";" {
        if kw2(&toks, i, "DUPLICATE", "KEY") || kw2(&toks, i, "PRIMARY", "KEY") {
            let kind = if is_kw(&toks[i], "DUPLICATE") {
                KeyModel::Duplicate
            } else {
                KeyModel::PrimaryKey
            };
            let (cols, next) = ident_list(&toks, i + 2)?;
            if model.replace(kind).is_some() {
                return Err(unsupported(
                    "both DUPLICATE KEY and PRIMARY KEY table models",
                ));
            }
            keys = cols;
            i = next;
            any = true;
        } else if is_kw(&toks[i], "UNIQUE") {
            return Err(unsupported(
                "UNIQUE KEY model (use DUPLICATE KEY or PRIMARY KEY)",
            ));
        } else if is_kw(&toks[i], "AGGREGATE") {
            return Err(unsupported(
                "AGGREGATE KEY model (use DUPLICATE KEY or PRIMARY KEY)",
            ));
        } else if is_kw(&toks[i], "DISTRIBUTED") {
            let (dist, next) = distributed_clause(&toks, i)?;
            distribution = Some(dist);
            i = next;
            any = true;
        } else if is_kw(&toks[i], "PARTITION") {
            return Err(unsupported("PARTITION BY (range/list partitioning)"));
        } else if is_kw(&toks[i], "PROPERTIES") {
            return Err(unsupported("PROPERTIES (...) table properties"));
        } else if kw2(&toks, i, "ORDER", "BY") {
            return Err(unsupported("ORDER BY (...) sort key"));
        } else {
            if is_kw(&toks[i], "ENGINE") {
                reject_dup_row_engine(&toks, i, model)?;
            }
            kept.push(&toks[i]);
            i += 1;
        }
    }
    if !any {
        return Ok((sql.to_string(), None));
    }
    let Some(kind) = model else {
        return Err(unsupported(
            "DISTRIBUTED BY without DUPLICATE KEY/PRIMARY KEY table model",
        ));
    };
    if kind == KeyModel::Duplicate && has_inline_pk(&toks, open, close) {
        return Err(unsupported(
            "DUPLICATE KEY table cannot also declare PRIMARY KEY",
        ));
    }
    Ok((
        rewrite(sql, &toks, open, close, kind, &keys, &kept),
        Some(StarRocksModel {
            kind,
            keys,
            distribution,
        }),
    ))
}

/// `toks[i] == a && toks[i+1] == b` (case-insensitive keywords)?
fn kw2(toks: &[Tok], i: usize, a: &str, b: &str) -> bool {
    toks.get(i + 1).is_some_and(|t| is_kw(t, b)) && is_kw(&toks[i], a)
}

/// A token usable as a column name: an unquoted word or a quoted
/// identifier (backticks / double quotes).
fn is_ident(t: &Tok) -> bool {
    if t.upper.is_some() {
        return true;
    }
    matches!(t.raw.as_bytes().first(), Some(b'`') | Some(b'"'))
}

/// Token indexes of the column list's `( .. )` of a CREATE TABLE
/// statement, or None for any other shape (let the MySQL parser
/// produce its own error for those).
fn create_table_span(toks: &[Tok]) -> Option<(usize, usize)> {
    if !is_kw(toks.first()?, "CREATE") {
        return None;
    }
    let mut i = 1 + usize::from(is_kw(toks.get(1)?, "TEMPORARY"));
    if !is_kw(toks.get(i)?, "TABLE") {
        return None;
    }
    i += 1;
    // table name: words / quoted idents joined by `.`
    while toks.get(i).is_some_and(|t| t.raw != "(" && t.raw != ";") {
        i += 1;
    }
    let open = i;
    let mut depth = 0usize;
    while i < toks.len() {
        match toks[i].raw.as_str() {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Comma-separated identifier list starting at `toks[at] == "("`.
/// Returns the names (quotes stripped) and the index AFTER the `)`.
fn ident_list(toks: &[Tok], at: usize) -> SqlResult<(Vec<String>, usize)> {
    if toks.get(at).map(|t| t.raw.as_str()) != Some("(") {
        return Err(SqlError::parse("expected '('"));
    }
    let mut cols = Vec::new();
    let mut i = at + 1;
    let mut want_ident = true;
    let mut closed = false;
    while let Some(t) = toks.get(i) {
        match t.raw.as_str() {
            ")" => {
                closed = true;
                break;
            }
            "," if !want_ident => want_ident = true,
            _ if want_ident && is_ident(t) => {
                cols.push(unquote(&t.raw));
                want_ident = false;
            }
            _ => return Err(SqlError::parse("bad column list")),
        }
        i += 1;
    }
    if !closed || cols.is_empty() || want_ident {
        return Err(SqlError::parse("empty or malformed column list"));
    }
    Ok((cols, i + 1))
}

/// `DISTRIBUTED BY HASH(cols) [BUCKETS n]`; anything else after
/// DISTRIBUTED BY (RANDOM, ...) is rejected loudly.
fn distributed_clause(toks: &[Tok], at: usize) -> SqlResult<(Distribution, usize)> {
    let bad = || unsupported("DISTRIBUTED BY shape (only HASH(cols) BUCKETS n)");
    if toks.get(at + 1).and_then(|t| t.upper.as_deref()) != Some("BY")
        || toks.get(at + 2).and_then(|t| t.upper.as_deref()) != Some("HASH")
    {
        if toks.get(at + 2).and_then(|t| t.upper.as_deref()) == Some("RANDOM") {
            return Err(unsupported(
                "DISTRIBUTED BY RANDOM (only HASH is supported)",
            ));
        }
        return Err(bad());
    }
    let (columns, mut i) = ident_list(toks, at + 3).map_err(|_| bad())?;
    let mut buckets = 10u32; // StarRocks' default bucket count
    if toks.get(i).and_then(|t| t.upper.as_deref()) == Some("BUCKETS") {
        buckets = toks
            .get(i + 1)
            .and_then(|t| t.upper.as_ref())
            .and_then(|u| u.parse::<u32>().ok())
            .ok_or_else(bad)?;
        i += 2;
    }
    Ok((Distribution { columns, buckets }, i))
}

/// A `DUPLICATE KEY` table is columnar; an explicit row-store ENGINE
/// (`row`/`innodb`) contradicts the model, so reject loudly here --
/// after the rewrite the distinction would be unrecoverable.
fn reject_dup_row_engine(toks: &[Tok], at: usize, model: Option<KeyModel>) -> SqlResult<()> {
    if model != Some(KeyModel::Duplicate) {
        return Ok(());
    }
    let value = toks[at + 1..]
        .iter()
        .find(|t| t.upper.is_some())
        .and_then(|t| t.upper.clone())
        .unwrap_or_default();
    if value == "ROW" || value == "INNODB" {
        return Err(unsupported(
            "DUPLICATE KEY tables are columnar (ENGINE=row is not supported)",
        ));
    }
    Ok(())
}

/// Any `PRIMARY KEY` word pair strictly inside the column list?
fn has_inline_pk(toks: &[Tok], open: usize, close: usize) -> bool {
    toks[open + 1..close]
        .windows(2)
        .any(|w| is_kw(&w[0], "PRIMARY") && is_kw(&w[1], "KEY"))
}

/// Rebuild the statement: the head up to and including the column-list
/// `(`, the column region verbatim, an injected MySQL `PRIMARY KEY`
/// constraint for the PK model, the closing `)`, then the kept tail
/// tokens joined by single spaces (their raw bytes are untouched).
fn rewrite(
    sql: &str,
    toks: &[Tok],
    open: usize,
    close: usize,
    kind: KeyModel,
    keys: &[String],
    kept: &[&Tok],
) -> String {
    let head_end = toks[open].start + 1; // include '('
    let close_at = toks[close].start;
    let mut out = String::with_capacity(sql.len() + 32);
    out.push_str(&sql[..head_end]);
    out.push_str(&sql[head_end..close_at]);
    if kind == KeyModel::PrimaryKey {
        // Multi-column keys are injected verbatim: the MySQL layer's
        // exactly-one-pk check rejects them with its usual message.
        out.push_str(", PRIMARY KEY (");
        out.push_str(&keys.join(", "));
        out.push(')');
    }
    out.push(')');
    for t in kept {
        out.push(' ');
        out.push_str(&t.raw);
    }
    out
}

/// Strip `` ` `` / `"` quoting of an identifier token.
fn unquote(raw: &str) -> String {
    let b = raw.as_bytes();
    if b.len() >= 2 {
        let q = b[0];
        if (q == b'`' || q == b'"') && b[b.len() - 1] == q {
            return raw[1..raw.len() - 1].to_string();
        }
    }
    raw.to_string()
}

#[cfg(test)]
#[path = "starrocks_tests.rs"]
mod tests;
