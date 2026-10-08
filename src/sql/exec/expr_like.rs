//! SQL LIKE wildcard matching (`%` any run, `_` one char, `\` escape)
//! shared by the expression executor's `Expr::Like` arm and the SHOW
//! metadata surface.
//!
//! Iterative bottom-up DP over (chars(s), chars(pattern)) with two
//! rolling rows -- the retired recursive matcher walked suffixes with
//! a call per char and could blow the stack (and exponential time) on
//! long inputs; this is O(len(s) * len(pattern)) time and
//! O(len(pattern)) memory with byte-for-byte identical semantics:
//! char-wise and case-sensitive (MySQL binary collation), the SHOW
//! surface case-folds its inputs BEFORE calling.

/// Does `pattern` match `s`? `%` matches any run of chars (including
/// empty), `_` exactly one char, `\` escapes the next pattern char
/// (`\%` and `\_` lose their wildcard meaning); a trailing lone `\`
/// is a literal backslash.
pub(crate) fn like_match(s: &str, pattern: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (n, m) = (s.len(), p.len());
    // Escape pairing runs left to right: an opener backslash consumes
    // the next char, so in `\\%` the `%` stays a wildcard.
    let mut escaped = vec![false; m];
    let mut k = 0;
    while k < m {
        if p[k] == '\\' && k + 1 < m {
            escaped[k + 1] = true;
            k += 2;
        } else {
            k += 1;
        }
    }
    // Rows over pattern prefixes: prev = s[..i-1] decided, cur = the
    // s[..i] row being filled.
    let mut prev = vec![false; m + 1];
    let mut cur = vec![false; m + 1];
    // Empty s: only a leading run of unescaped `%` matches.
    prev[0] = true;
    for j in 1..=m {
        prev[j] = !escaped[j - 1] && p[j - 1] == '%' && prev[j - 1];
    }
    for i in 1..=n {
        cur[0] = false;
        for j in 1..=m {
            let c = p[j - 1];
            cur[j] = if escaped[j - 1] {
                // Escape pair: one literal char, two pattern slots.
                prev[j - 2] && s[i - 1] == c
            } else if c == '%' {
                // Empty run, or the `%` eats one char and stays live.
                cur[j - 1] || prev[j]
            } else if c == '_' {
                prev[j - 1]
            } else if c == '\\' && j < m {
                // Prefix ends on an escape opener: no reachable state.
                false
            } else {
                prev[j - 1] && s[i - 1] == c
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

#[cfg(test)]
#[path = "expr_like_tests.rs"]
mod tests;
