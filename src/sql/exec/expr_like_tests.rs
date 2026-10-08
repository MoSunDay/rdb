//! like_match unit tests: the wildcard language pinned against the
//! retired recursive matcher (an exhaustive small-corpus equivalence
//! run preceded the swap), the escape forms, and the long inputs the
//! recursion used to threaten the stack with.

use super::like_match;

#[test]
fn wildcard_runs_and_single_chars() {
    let like = |s: &str, p: &str| like_match(s, p);
    assert!(like("hello", "h%"));
    assert!(like("hello", "%l%"));
    assert!(like("hello", "h_llo"));
    assert!(!like("hello", "h_ll"));
    // `%a` anchors the tail: any run, then a.
    assert!(like("aaa", "%a"));
    assert!(like("cba", "%a"));
    assert!(!like("aab", "%a"));
    // `_b%`: one char, a b, then anything.
    assert!(like("abx", "_b%"));
    assert!(!like("axb", "_b%"));
    // Multi-run patterns walk every split.
    assert!(like("axcyb", "a%c%b"));
    assert!(like("acb", "a%c%b"));
    assert!(!like("abxb", "a%c%b"));
    assert!(like("hello", "%_el%"));
}

#[test]
fn escapes_bind_the_next_pattern_char() {
    let like = |s: &str, p: &str| like_match(s, p);
    // `\%` is a literal percent, not a run.
    assert!(like("a%b", r"a\%b"));
    assert!(like("%", r"\%"));
    assert!(!like("x", r"\%"));
    // `\_` is a literal underscore; `\\` a literal backslash.
    assert!(like("a_b", r"a\_b"));
    assert!(!like("axb", r"a\_b"));
    assert!(like("a\\b", r"a\\b"));
    assert!(like("\\", r"\\"));
    // An escape opener escapes exactly one char: after `\\` the `%`
    // is a wildcard again (left-to-right pairing).
    assert!(like("a\\xb", r"a\\%"));
    assert!(!like("a\\xb", r"a\\\%")); // that one needs a literal %
                                       // A trailing lone backslash is a literal.
    assert!(like("a\\", r"a\"));
    assert!(!like("a", r"a\"));
}

#[test]
fn empty_string_and_pattern() {
    let like = |s: &str, p: &str| like_match(s, p);
    assert!(like("", ""));
    assert!(like("", "%"));
    assert!(like("", "%%%"));
    assert!(!like("", "_"));
    assert!(!like("a", ""));
    assert!(!like("", "a"));
    // Case stays sensitive (binary collation).
    assert!(!like("A", "a"));
    assert!(like("A", "A"));
}

#[test]
fn long_inputs_walk_the_dp_without_recursion() {
    // Literal-heavy match: each char is a DP row, no per-char call.
    let s = "ab".repeat(2_000);
    assert!(like_match(&s, &("%".to_owned() + &s + "%")));
    assert!(!like_match(&s, &("%".to_owned() + &s + "x" + "%")));
    // Deep runs over a long subject stay linear in the pattern.
    let deep = "a".repeat(100_000);
    assert!(like_match(&deep, "%a%a%a%a%"));
    assert!(like_match(&deep, "__________a%"));
}
