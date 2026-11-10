//! FT.SEARCH `HIGHLIGHT` e2e: tag wrapping of matched query terms in
//! the reply doc JSON -- default `<b>` tags on every TEXT field
//! (KEYWORD/NUMERIC untouched), custom TAGS, FIELDS scoping, prefix
//! and stopword rules, the NOCONTENT mutual exclusion in BOTH orders,
//! and coexistence with RETURN / LIMIT / match-all. Separate file
//! from search_e2e.rs (at its 748-line budget); helpers follow the
//! same per-binary test-mount convention.

mod common;

use common::contains_bytes;
use common::lite::{call, shared_at};

fn ok(reply: &[u8], what: &str) {
    assert!(reply.starts_with(b"+OK"), "{what}: {reply:?}");
}

fn err_has(reply: &[u8], needle: &str, what: &str) {
    assert!(
        reply.starts_with(b"-") && contains_bytes(reply, needle.as_bytes()),
        "{what}: {reply:?}"
    );
}

/// N-th bulk payload of a reply, skipping the leading integer count
/// (FT.SEARCH: docid, [score], [content] per hit -- all bulk typed).
fn bulk_at(reply: &[u8], n: usize) -> String {
    let s = String::from_utf8_lossy(reply);
    let mut rest: &str = &s;
    if rest.starts_with(':') {
        rest = rest.split_once("\r\n").map(|(_, r)| r).unwrap_or("");
    }
    let mut items = 0usize;
    let mut it = rest.split("\r\n");
    while let Some(head) = it.next() {
        if let Some(len) = head.strip_prefix('$') {
            let len: usize = len.parse().unwrap();
            let body = it.next().unwrap_or("");
            if items == n {
                return String::from_utf8_lossy(&body.as_bytes()[..len.min(body.len())])
                    .into_owned();
            }
            items += 1;
        }
    }
    panic!("no bulk #{n} in {reply:?}");
}

/// d2 carries non-ASCII text (CJK title/body): kept as &str, added
/// via `.as_bytes()`.
const D1: &[u8] =
    br#"{"title":"Quantum Computing","body":"The quantum leap","tag":"quantum","price":10}"#;
const D2: &str =
    r#"{"title":"quantum 中文检索","body":"<b>raw</b> 龘爨 quantum engine","tag":"zh","price":20}"#;

#[test]
fn ft_search_highlight_option() {
    let (shared, _dir) = shared_at("45033");
    ok(
        &call(
            &shared,
            "ft.create",
            &[
                b"hl",
                b"STOPWORDS",
                b"1",
                b"the",
                b"SCHEMA",
                b"title",
                b"TEXT",
                b"body",
                b"TEXT",
                b"tag",
                b"KEYWORD",
                b"price",
                b"NUMERIC",
            ],
        ),
        "create",
    );
    assert_eq!(
        call(&shared, "ft.add", &[b"hl", b"d1", D1]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "ft.add", &[b"hl", b"d2", D2.as_bytes()]),
        b":1\r\n".to_vec()
    );

    // 1) bare HIGHLIGHT: default <b> tags, TEXT fields only -- the
    // uppercase original matches the lowercase query term, the
    // stopword "The" never wraps, KEYWORD/NUMERIC stay untouched.
    let r = call(&shared, "ft.search", &[b"hl", b"quantum", b"HIGHLIGHT"]);
    assert!(
        contains_bytes(&r, b"<b>Quantum</b> Computing"),
        "title: {r:?}"
    );
    assert!(
        contains_bytes(&r, b"The <b>quantum</b> leap"),
        "body: {r:?}"
    );
    assert!(!contains_bytes(&r, b"<b>The"), "stopword wrapped: {r:?}");
    assert!(
        contains_bytes(&r, br#""tag":"quantum""#),
        "keyword kept: {r:?}"
    );
    assert!(
        !contains_bytes(&r, br#""tag":"<b>"#),
        "keyword wrapped: {r:?}"
    );
    assert!(contains_bytes(&r, br#""price":10"#), "numeric kept: {r:?}");

    // 2) TAGS <em> </em>: custom open/close.
    let r = call(
        &shared,
        "ft.search",
        &[b"hl", b"quantum", b"HIGHLIGHT", b"TAGS", b"<em>", b"</em>"],
    );
    assert!(contains_bytes(&r, b"<em>Quantum</em> Computing"), "{r:?}");

    // 3) FIELDS 1 title: only the named field wraps (body stays raw).
    let r = call(
        &shared,
        "ft.search",
        &[b"hl", b"quantum", b"HIGHLIGHT", b"FIELDS", b"1", b"title"],
    );
    assert!(contains_bytes(&r, b"<b>Quantum</b> Computing"), "{r:?}");
    assert!(
        contains_bytes(&r, br#""body":"The quantum leap""#),
        "body raw: {r:?}"
    );

    // 4) prefix query: every token starting with the stem wraps.
    let r = call(&shared, "ft.search", &[b"hl", b"quant*", b"HIGHLIGHT"]);
    assert!(contains_bytes(&r, b"<b>Quantum</b> Computing"), "{r:?}");
    assert!(contains_bytes(&r, b"<b>quantum</b> engine"), "{r:?}");

    // 5) a stopword TERM in the query (via OR union) is filtered out
    // of the highlighter even though the doc contains it.
    let r = call(
        &shared,
        "ft.search",
        &[b"hl", b"the | quantum", b"HIGHLIGHT"],
    );
    assert!(contains_bytes(&r, b"The <b>quantum</b> leap"), "{r:?}");
    assert!(!contains_bytes(&r, b"<b>The"), "stopword wrapped: {r:?}");

    // 6) NOCONTENT is refused in BOTH orders.
    err_has(
        &call(
            &shared,
            "ft.search",
            &[b"hl", b"quantum", b"HIGHLIGHT", b"NOCONTENT"],
        ),
        "HIGHLIGHT cannot be used with NOCONTENT",
        "hl then noc",
    );
    err_has(
        &call(
            &shared,
            "ft.search",
            &[b"hl", b"quantum", b"NOCONTENT", b"HIGHLIGHT"],
        ),
        "HIGHLIGHT cannot be used with NOCONTENT",
        "noc then hl",
    );

    // 7) RETURN + HIGHLIGHT: projection keeps only body, still wrapped.
    let r = call(
        &shared,
        "ft.search",
        &[b"hl", b"quantum", b"RETURN", b"1", b"body", b"HIGHLIGHT"],
    );
    assert!(
        contains_bytes(&r, &br#"{"body":"The <b>quantum</b> leap"}"#[..]),
        "{r:?}"
    );
    assert!(!contains_bytes(&r, b"title"), "title leaked: {r:?}");

    // 8) LIMIT 1 1 + HIGHLIGHT: the page (d2 only) is highlighted.
    let r = call(
        &shared,
        "ft.search",
        &[b"hl", b"quantum", b"LIMIT", b"1", b"1", b"HIGHLIGHT"],
    );
    assert!(contains_bytes(&r, b"<b>quantum</b> engine"), "{r:?}");
    assert!(!contains_bytes(&r, b"Computing"), "d1 leaked: {r:?}");

    // 9) match-all `*` + HIGHLIGHT: no query terms -> docs round-trip
    // byte-identical (the literal <b>raw</b> gains nothing).
    let r = call(&shared, "ft.search", &[b"hl", b"*", b"HIGHLIGHT"]);
    assert_eq!(bulk_at(&r, 1), String::from_utf8_lossy(D1).into_owned());
    assert_eq!(bulk_at(&r, 3), D2.to_string());
}
