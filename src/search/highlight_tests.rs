//! Unit tests for `highlight.rs`: the golden span/tokenize
//! equivalence, query-term collection (stopword/Not/prefix rules)
//! and the doc-JSON tag wrapping (tags, casing, CJK, non-strings).

use super::highlight_doc;
use super::highlight_terms;
use super::token_spans;
use super::HLToken;
use crate::search::ft_index::IndexMeta;
use crate::search::query_tree::{parse_query_tree, QueryDialect};

/// Golden invariant: every span slices out (lowercased) exactly the
/// token `tokenize` emits, same order, same count.
fn assert_span_equivalence(text: &str) {
    let spans = token_spans(text);
    let terms = crate::search::tokenize::tokenize(text);
    let sliced: Vec<String> = spans
        .iter()
        .map(|&(s, e)| text[s..e].to_lowercase())
        .collect();
    assert_eq!(sliced, terms, "span drift for {text:?}");
}

fn exact(w: &str) -> HLToken {
    HLToken::Exact(w.as_bytes().to_vec())
}

fn prefix(w: &str) -> HLToken {
    HLToken::Prefix(w.as_bytes().to_vec())
}

#[test]
fn spans_match_tokenize_on_latin_samples() {
    for t in [
        "Hello, WORLD!",
        "mp3 player v2.1",
        "  ,.!?  ",
        "",
        "quantum Quantum QUANTUM",
    ] {
        assert_span_equivalence(t);
    }
}

#[test]
fn spans_match_tokenize_on_mixed_and_cjk_samples() {
    for t in [
        "Redis 支持全文检索 full-text",
        "中华人民共和国",
        "龙",
        "龘爨鱻",
        "go go go 编译",
        "quantum 中文检索 龘爨 Quantum!",
        "ABC中文abc",
    ] {
        assert_span_equivalence(t);
    }
}

#[test]
fn spans_are_byte_offsets_into_the_whole_text() {
    // "量子" is a dictionary word: one span covering both chars,
    // offset by the latin prefix (not zero-based within the run).
    let t = "hi 量子 there";
    assert_span_equivalence(t);
    let spans = token_spans(t);
    assert!(spans.contains(&(3, 9)), "no 量子 span in {spans:?}");
}

#[test]
fn terms_collect_exact_prefix_and_skip_not_stopwords_dupes() {
    let tree = parse_query_tree(b"redis quant* -secret redis @f:[1 5]", QueryDialect::Ft)
        .unwrap()
        .expect("tree");
    let meta = IndexMeta {
        stopwords: vec![b"secret".to_vec()],
        ..Default::default()
    };
    assert_eq!(
        highlight_terms(&tree, &meta),
        vec![exact("redis"), prefix("quant")]
    );
}

#[test]
fn terms_skip_not_subtree_and_phrases_go_exact() {
    let tree = parse_query_tree(b"\"hello world\" -nope | other", QueryDialect::Ft)
        .unwrap()
        .expect("tree");
    assert_eq!(
        highlight_terms(&tree, &IndexMeta::default()),
        vec![exact("hello"), exact("world"), exact("other")]
    );
}

#[test]
fn doc_wraps_hits_with_default_tags() {
    let body = br#"{"title":"Quantum leap","n":10}"#;
    let out = highlight_doc(
        body,
        &[b"title".to_vec()],
        &[exact("quantum")],
        b"<b>",
        b"</b>",
    );
    assert_eq!(out, br#"{"title":"<b>Quantum</b> leap","n":10}"#.to_vec());
}

#[test]
fn doc_wraps_with_custom_tags_and_prefix_hits() {
    let body = br#"{"t":"quantum and quantifiers"}"#;
    let out = highlight_doc(
        body,
        &[b"t".to_vec()],
        &[prefix("quant")],
        b"<em>",
        b"</em>",
    );
    assert_eq!(
        out,
        br#"{"t":"<em>quantum</em> and <em>quantifiers</em>"}"#.to_vec()
    );
}

#[test]
fn doc_case_insensitive_matches_keel_original_text() {
    let body = br#"{"t":"The REDIS of Redis"}"#;
    let out = highlight_doc(body, &[b"t".to_vec()], &[exact("redis")], b"<b>", b"</b>");
    assert_eq!(out, br#"{"t":"The <b>REDIS</b> of <b>Redis</b>"}"#.to_vec());
}

#[test]
fn doc_wraps_cjk_word_and_bigram_spans() {
    let body = r#"{"t":"量子检索 龘爨 引擎"}"#.as_bytes().to_vec();
    let fields = &[b"t".to_vec()];
    // dictionary word
    let out = highlight_doc(&body, fields, &[exact("量子")], b"<b>", b"</b>");
    assert_eq!(
        String::from_utf8_lossy(&out),
        r#"{"t":"<b>量子</b>检索 龘爨 引擎"}"#
    );
    // rare-char run tokenizes to the bigram 龘爨 -- one span, both chars
    assert_span_equivalence("龘爨");
    let out = highlight_doc(&body, fields, &[exact("龘爨")], b"<b>", b"</b>");
    assert_eq!(
        String::from_utf8_lossy(&out),
        r#"{"t":"量子检索 <b>龘爨</b> 引擎"}"#
    );
}

#[test]
fn doc_multiple_hits_never_nest() {
    let body = br#"{"t":"redis redis redis"}"#;
    let out = highlight_doc(body, &[b"t".to_vec()], &[exact("redis")], b"<b>", b"</b>");
    assert_eq!(
        out,
        br#"{"t":"<b>redis</b> <b>redis</b> <b>redis</b>"}"#.to_vec()
    );
}

#[test]
fn doc_skips_non_string_values_missing_fields_and_bad_json() {
    let body = br#"{"t":"redis","n":42,"a":["redis"]}"#;
    // only the string field named "t" is considered
    let out = highlight_doc(body, &[b"t".to_vec()], &[exact("redis")], b"<b>", b"</b>");
    assert_eq!(
        out,
        br#"{"t":"<b>redis</b>","n":42,"a":["redis"]}"#.to_vec()
    );
    // a numeric-named field and a missing field fall through
    let out = highlight_doc(
        body,
        &[b"n".to_vec(), b"zzz".to_vec()],
        &[exact("42")],
        b"<b>",
        b"</b>",
    );
    assert_eq!(out, body.to_vec());
    // non-object body round-trips
    let raw = br#"[1,2,3]"#;
    assert_eq!(
        highlight_doc(raw, &[b"t".to_vec()], &[exact("1")], b"<b>", b"</b>"),
        raw.to_vec()
    );
}

#[test]
fn doc_without_terms_or_hits_round_trips_bytes() {
    let body = br#"{"t":"plain <b> literal","u":"redis"}"#;
    // no terms at all -> byte-identical
    assert_eq!(
        highlight_doc(body, &[b"t".to_vec()], &[], b"<b>", b"</b>"),
        body.to_vec()
    );
    // terms that hit nothing -> byte-identical (no re-serialization)
    assert_eq!(
        highlight_doc(body, &[b"t".to_vec()], &[exact("nope")], b"<b>", b"</b>"),
        body.to_vec()
    );
}
