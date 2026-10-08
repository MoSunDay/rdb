//! Pre-parse TRIM default-remstr expansion.
//!
//! sqlparser 0.62's TRIM grammar requires a remstr expression before
//! `FROM`, so the legal MySQL forms `TRIM([BOTH|LEADING|TRAILING] FROM
//! x)` (remstr defaults to a single space) do not reach the
//! translator. The raw text is rewritten BEFORE the sqlparser pass --
//! the implicit `' '` made explicit -- so the grammar stays stock.
//! Same approach as the StarRocks clause lifting in `parse::starrocks`
//! (pure byte scan, no regex; literals and comments are skipped).
//! Statements without the shape return unchanged, byte-identical.

/// Expand every `TRIM(KW FROM ..)` into `TRIM(KW ' ' FROM ..)`.
pub(crate) fn expand(sql: &str) -> String {
    match injection_points(sql.as_bytes()) {
        Some(points) if !points.is_empty() => {
            let mut out = String::with_capacity(sql.len() + points.len() * 4);
            let mut at = 0usize;
            for p in points {
                out.push_str(&sql[at..p]);
                out.push_str("' ' ");
                at = p;
            }
            out.push_str(&sql[at..]);
            out
        }
        // No match, or a bail-out (unterminated literal): the
        // sqlparser pass owns those errors.
        _ => sql.to_string(),
    }
}

/// Byte offset of each `FROM` that needs the default injected (the
/// scan position right at `FROM`, so the stitch inserts before it).
/// `None` = unterminated literal/comment; caller keeps the original.
fn injection_points(b: &[u8]) -> Option<Vec<usize>> {
    let mut points = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => i = skip_quoted(b, i)?,
            b'-' | b'#' if b[i] == b'#' || b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = b[i + 2..]
                    .windows(2)
                    .position(|w| w == b"*/")
                    .map(|p| i + 2 + p + 2)?;
            }
            _ if word_byte(b[i]) => {
                let start = i;
                while i < b.len() && word_byte(b[i]) {
                    i += 1;
                }
                // Only a TRIM whose keyword runs straight into FROM
                // needs help; `TRIM(x)` / `TRIM(KW remstr FROM x)`
                // parse fine and stay untouched.
                if eq_word(&b[start..i], b"TRIM") {
                    if let Some(from_at) = trim_from_offset(b, i) {
                        points.push(from_at);
                        i = from_at;
                    }
                }
            }
            _ => i += 1,
        }
    }
    Some(points)
}

/// Offset of the `FROM` when `i` (just past `TRIM`) opens
/// `( [BOTH|LEADING|TRAILING] FROM`; `None` = any other shape.
fn trim_from_offset(b: &[u8], mut i: usize) -> Option<usize> {
    i = skip_gap(b, i)?;
    if b.get(i) != Some(&b'(') {
        return None;
    }
    i = skip_gap(b, i + 1)?;
    let kw_start = i;
    while i < b.len() && word_byte(b[i]) {
        i += 1;
    }
    let kw = &b[kw_start..i];
    if !(eq_word(kw, b"BOTH") || eq_word(kw, b"LEADING") || eq_word(kw, b"TRAILING")) {
        return None;
    }
    i = skip_gap(b, i)?;
    // `FROM` must be a whole word (FROMX is a plain remstr name) and
    // case-insensitive like every keyword here.
    let from = b
        .get(i..i + 4)
        .map(|w| w.eq_ignore_ascii_case(b"FROM"))
        .unwrap_or(false);
    if !from || b.get(i + 4).is_some_and(|&c| word_byte(c)) {
        return None;
    }
    Some(i)
}

/// Skip whitespace and comments (`--`, `#`, `/* */`); `None` on an
/// unterminated block comment.
fn skip_gap(b: &[u8], mut i: usize) -> Option<usize> {
    loop {
        match b.get(i) {
            Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') => i += 1,
            Some(b'-') if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            Some(b'#') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            Some(b'/') if b.get(i + 1) == Some(&b'*') => {
                i = b[i + 2..]
                    .windows(2)
                    .position(|w| w == b"*/")
                    .map(|p| i + 2 + p + 2)?;
            }
            _ => return Some(i),
        }
    }
}

/// Scan one quoted span starting at the quote byte; returns the index
/// just past the closing quote. Doubled quotes escape; backslash also
/// escapes inside '...' and "..." (MySQL default).
fn skip_quoted(b: &[u8], start: usize) -> Option<usize> {
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
                    return Some(i + 1);
                }
            }
            _ => i += 1,
        }
    }
    None
}

fn word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// ASCII case-insensitive equality of a source word against a keyword.
fn eq_word(word: &[u8], kw: &[u8]) -> bool {
    word.len() == kw.len() && word.eq_ignore_ascii_case(kw)
}

#[cfg(test)]
#[path = "trim_default_tests.rs"]
mod tests;
