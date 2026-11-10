//! Vector-family rendering and write-path validation for the turso
//! engine (T4): rdb's MySQL vector face mapped onto turso's built-in
//! vector functions (research doc section 5 / D5-D6). Exact KNN only -- CREATE
//! VECTOR INDEX is a loud 1235 at the router, never a silent ANN.
//!
//! Function map: STRING_TO_VECTOR/TO_VECTOR -> `vector32(x)`;
//! VECTOR_TO_STRING/VEC_TO_TEXT -> `vector_extract(v)`; VECTOR_DIM ->
//! `length(vector32(v)) / 4` (Float32Dense blobs are raw LE f32 with
//! no trailer byte, so 4 bytes/element; the vector32 wrap re-bases
//! TEXT inputs on their parsed blob because SQLite's length() counts
//! CHARACTERS of text -- 7 for '[1,2,3]' -- and the NULL guard keeps
//! VECTOR_DIM(NULL) = NULL: vector32(NULL) is an engine error);
//! DISTANCE(v1,v2,'metric') -> vector_distance_{cos,l2} and
//! `(-vector_distance_dot(..))` (turso's dot is the NEGATED inner
//! product -- simsimd's "smaller = closer" convention -- so the sign
//! flip restores MySQL VECTOR_DISTANCE('dot')'s true dot product).

use crate::sql::parse::ast::Expr;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::Value;
use crate::sql::storage::vector as vecstore;

use super::super::engine;
use super::expr::render_expr;

/// Render one vector-family call, or `Ok(None)` when `name` is not
/// ours (the scalar allowlist in `funcs` does not carry these names,
/// so refusal order between the two is irrelevant). Arity was already
/// validated by the parse-time signature table; the defensive arity
/// arms below stay loud rather than inventing SQLite spellings.
pub(super) fn render_fn(name: &str, args: &[Expr]) -> SqlResult<Option<String>> {
    let sql = match name {
        // TEXT '[e,e,...]' or BLOB in, engine-checked blob out; a
        // malformed text argument errors engine-side ("Invalid vector
        // value" -> MySQL 1292 via engine::map_error).
        "string_to_vector" | "to_vector" => fn1("vector32", args)?,
        "vector_to_string" | "vec_to_text" => fn1("vector_extract", args)?,
        "vector_dim" => {
            let [v] = args else {
                return Err(engine::turso_unsupported("function 'vector_dim' arity"));
            };
            let v = render_expr(v)?;
            format!("(CASE WHEN {v} IS NULL THEN NULL ELSE length(vector32({v})) / 4 END)")
        }
        // The rocks parser only ever yields the bare spelling (the
        // signature table has no VECTOR_DISTANCE); the alias arm is
        // kept so a future spelling maps the same way.
        "distance" | "vector_distance" => render_distance(args)?,
        _ => return Ok(None),
    };
    Ok(Some(sql))
}

/// One fixed-arity-1 call rendered as `fname(arg)`.
fn fn1(fname: &str, args: &[Expr]) -> SqlResult<String> {
    let [v] = args else {
        return Err(engine::turso_unsupported(format!(
            "function '{fname}' arity"
        )));
    };
    Ok(format!("{fname}({})", render_expr(v)?))
}

/// DISTANCE(v1, v2 [, 'metric']): metric is a literal third argument
/// (ASCII case-insensitive, exactly the rocks trio -- no 'l2' alias).
/// The parse-time signature pins 3 args; the absent-metric arm below
/// is defensive and defaults to cosine, MySQL's own default.
fn render_distance(args: &[Expr]) -> SqlResult<String> {
    let (a, b, metric) = match args {
        [a, b] => (a, b, "cosine"),
        [a, b, m] => {
            let Expr::Lit(Value::Str(m)) = m else {
                return Err(engine::turso_unsupported(
                    "DISTANCE with a non-literal metric",
                ));
            };
            (a, b, m.as_str())
        }
        _ => return Err(engine::turso_unsupported("function 'distance' arity")),
    };
    let (a, b) = (render_expr(a)?, render_expr(b)?);
    Ok(match metric.to_ascii_lowercase().as_str() {
        // 1 - cosine similarity; IDENTICAL vectors read ~1.79e-7, not
        // 0 (simsimd float noise) -- comparisons need an epsilon.
        "cosine" => format!("(vector_distance_cos({a}, {b}))"),
        "euclidean" => format!("(vector_distance_l2({a}, {b}))"),
        // Negated engine dot (module doc): the outer sign flip
        // restores the true, "bigger = more aligned" dot product.
        "dot" => format!("(-vector_distance_dot({a}, {b}))"),
        // Unknown metric names are unrenderable, not wrong values:
        // 1235 (rocks answers 1292 from its own metric parser -- a
        // documented ledger deviation).
        _ => {
            return Err(engine::turso_unsupported(format!(
                "DISTANCE metric '{metric}'"
            )))
        }
    })
}

/// A vector value as a SQLite blob literal: raw LE f32 bytes in the
/// X'<hex>' spelling (turso's Float32Dense wire form -- no trailer
/// byte). Chosen over `vector32('[..]')` because the value is already
/// parsed: no re-parse, no affinity surprise (a `vector(n)` column is
/// NUMERIC affinity, which leaves blobs alone but keeps text
/// spellings TEXT), and the bytes are exactly what the distance
/// functions and the read-back decoder expect.
pub fn blob_literal(v: &[f32]) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(v.len() * 8);
    for f in v {
        for b in f.to_le_bytes() {
            let _ = write!(hex, "{b:02x}");
        }
    }
    format!("X'{hex}'")
}

/// Render one write-path value destined for a vector(n) column
/// (`dim` = the declared n, None when unrecoverable -- see
/// [`declared_vector_dims`]). Mirrors the rocks write-path coercion
/// (`storage::vector::coerce_value`) message-for-message so both
/// engines answer the same MySQL 1292 texts.
pub(super) fn write_value(e: &Expr, dim: Option<u32>) -> SqlResult<String> {
    match e {
        // The '[e,e,...]' literal spelling: parse + dimension-check
        // here (turso has no column constraint to do it), then store
        // as the canonical blob.
        Expr::Lit(Value::Str(text)) => {
            let vals = vecstore::parse_vector_literal(text)
                .map_err(|m| SqlError::new(ErrorCode::WrongValue, m))?;
            check_dim(&vals, dim)?;
            Ok(blob_literal(&vals))
        }
        // A vector value riding the AST (defensive: the parser spells
        // literals as text, and prepared-statement binds are typed on
        // the wire, which has no vector class yet).
        Expr::Lit(Value::Vector(vals)) => {
            check_dim(vals, dim)?;
            Ok(blob_literal(vals))
        }
        // NULL is the engine's NOT NULL business, as on rocks.
        Expr::Lit(Value::Null) => Ok("NULL".to_string()),
        // Any other literal domain is a loud wrong-value with the
        // rocks catch-all's exact shape: errno 1136
        // (WrongValueCount) and the `{value:?} into {SqlType:?}`
        // message -- exec::expr::coerce's final arm, which is what
        // the rocks write path answers for `VALUES (9, 5)`.
        Expr::Lit(other) => Err(SqlError::new(
            ErrorCode::WrongValueCount,
            match dim {
                Some(n) => format!("cannot store {other:?} into Vector {{ dim: {n} }} column"),
                None => format!("cannot store {other:?} into Vector column"),
            },
        )),
        // STRING_TO_VECTOR/TO_VECTOR of a literal: the one
        // engine-side spelling whose argument is still app-visible,
        // so it validates here too (rocks coerces the evaluated
        // vector through the same column check).
        Expr::Func { name, args }
            if matches!(name.as_str(), "string_to_vector" | "to_vector")
                && matches!(args.as_slice(), [Expr::Lit(Value::Str(_))]) =>
        {
            if let [Expr::Lit(Value::Str(text))] = args.as_slice() {
                let vals = vecstore::parse_vector_literal(text)
                    .map_err(|m| SqlError::new(ErrorCode::WrongValue, m))?;
                check_dim(&vals, dim)?;
            }
            render_expr(e)
        }
        // Everything else (columns, arithmetic, non-literal function
        // calls) is engine-side: render as-is.
        other => render_expr(other),
    }
}

/// Dimension check with the rocks message shape ("vector has 2
/// dimensions, column expects 3" -- identical text, errno 1292).
fn check_dim(vals: &[f32], dim: Option<u32>) -> SqlResult<()> {
    if let Some(n) = dim {
        vecstore::validate_vector_dim(vals, n)
            .map_err(|m| SqlError::new(ErrorCode::WrongValue, m))?;
    }
    Ok(())
}

/// Does a table_info declared type spell the vector domain? Handles
/// turso's normalized bare `vector` and external files' `vector(N)`.
pub fn is_vector_decl(decl: &str) -> bool {
    decl.trim().to_ascii_lowercase().starts_with("vector")
}

/// Vector-typed columns of a CREATE TABLE statement text, in column
/// order: (name, dim). turso's own DDL normalizes `vector(3)` to bare
/// `vector` in table_info, but sqlite_master keeps the `sql` text
/// (spelled `vector (3)`, with a space), so the dimension is
/// recovered by re-scanning the column list. A `vector` column whose
/// dim cannot be recovered yields None: validation degrades to
/// format-only, never a wrong-dim rejection.
pub fn declared_vector_dims(sql: &str) -> Vec<(String, Option<u32>)> {
    let tokens = tokenize(sql);
    // The column-list body: the first '(' and its matching ')'.
    // Table-level constraints (PRIMARY KEY (..) etc.) ride inside too
    // and are skipped by the keyword check below.
    let Some(start) = tokens.iter().position(|t| t == "(") else {
        return Vec::new();
    };
    let mut depth = 0usize;
    let mut end = tokens.len();
    for (i, t) in tokens.iter().enumerate().skip(start) {
        if t == "(" {
            depth += 1;
        } else if t == ")" {
            depth -= 1;
            if depth == 0 {
                end = i;
                break;
            }
        }
    }
    let mut out = Vec::new();
    let mut part: Vec<&str> = Vec::new();
    let mut depth = 0usize;
    for t in &tokens[start + 1..end] {
        if t == "(" {
            depth += 1;
        } else if t == ")" {
            depth -= 1;
        }
        if t == "," && depth == 0 {
            push_vector_column(&mut out, &part);
            part.clear();
        } else {
            part.push(t);
        }
    }
    push_vector_column(&mut out, &part);
    out
}

/// One comma-part of the column list -> a vector entry when the
/// declared type says vector.
fn push_vector_column(out: &mut Vec<(String, Option<u32>)>, part: &[&str]) {
    let Some(name_tok) = part.first() else {
        return;
    };
    let name = dequote(name_tok);
    // Table-level constraints lead with a keyword, never a name.
    if matches!(
        name.to_ascii_uppercase().as_str(),
        "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "CONSTRAINT"
    ) {
        return;
    }
    let Some(type_tok) = part.get(1) else {
        return;
    };
    if !is_vector_decl(&dequote(type_tok)) {
        return;
    }
    // The dimension is the first number inside the type's own parens
    // (`vector ( 3 )`); modifiers (NOT NULL, DEFAULT ...) never carry
    // a parenthesized bare number here.
    let mut dim = None;
    let mut rest = part[2..].iter();
    while let Some(t) = rest.next() {
        if *t == "(" {
            if let Some(n) = rest.next().and_then(|t| t.parse::<u32>().ok()) {
                dim = Some(n);
            }
        }
    }
    out.push((name, dim));
}

/// Minimal CREATE TABLE scanner: whitespace-split words with `(` `)`
/// `,` standalone and quoted runs kept whole (the token shape
/// `parse::vector_ddl` uses for its rewrite).
fn tokenize(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'(' | b')' | b',' => {
                out.push((b[i] as char).to_string());
                i += 1;
            }
            b'\'' | b'`' | b'"' => {
                let q = b[i];
                let mut j = i + 1;
                while j < b.len() && b[j] != q {
                    j += 1;
                }
                out.push(String::from_utf8_lossy(&b[i..(j + 1).min(b.len())]).into_owned());
                i = j + 1;
            }
            _ => {
                let mut j = i;
                while j < b.len()
                    && !matches!(b[j], b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b',')
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

/// Strip one level of SQL quoting (`"x"` / `` `x` `` / `'x'`).
fn dequote(tok: &str) -> String {
    let b = tok.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'"' | b'`' | b'\'') && b[b.len() - 1] == b[0] {
        return tok[1..tok.len() - 1].to_string();
    }
    tok.to_string()
}
