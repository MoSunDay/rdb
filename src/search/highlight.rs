//! FT.SEARCH `HIGHLIGHT` support: pure functions that wrap the
//! matched query terms of the REPLY's doc JSON in open/close tags --
//! nothing is ever written back to the store.
//!
//! - `token_spans`: the span (byte-offset) mirror of `tokenize` --
//!   same run split (Han/non-Han), same alphanumeric filtering, same
//!   jieba word/bigram emission, so every span slices out exactly
//!   the term `tokenize` emits (the golden equivalence
//!   `spans.map(lowercase slice) == tokenize` is unit-tested).
//! - `highlight_terms`: the POSITIVE (non-NOT) Term/Phrase/Prefix
//!   words of the parsed query tree, stopword-filtered and deduped.
//! - `highlight_doc`: for every named field whose JSON value is a
//!   string, wrap each token span whose lowercased text equals an
//!   Exact term or starts with a Prefix stem. Spans are disjoint by
//!   construction (tags never nest) and are inserted back-to-front
//!   so earlier offsets stay valid.

use super::ft_index::IndexMeta;
use super::query_tree::QueryNode;
use super::tokenize::{is_han, jieba, runs, Run};

/// Byte spans of `text`, one per token `tokenize(text)` emits: the
/// golden invariant is `t[s..e].to_lowercase() == term` for every
/// span/term pair. Mirrors `tokenize::runs` + `latin_tokens` +
/// `han_tokens`; keep the two in lockstep (see each for the rules).
pub(crate) fn token_spans(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut base = 0usize;
    for run in runs(text) {
        match run {
            Run::Han(s) => {
                han_spans(base, s, &mut out);
                base += s.len();
            }
            Run::Other(s) => {
                latin_spans(base, s, &mut out);
                base += s.len();
            }
        }
    }
    out
}

/// Span mirror of `tokenize::latin_tokens`: one span per maximal
/// alphanumeric substring of a non-Han run (noise gaps emit nothing).
fn latin_spans(base: usize, run: &str, out: &mut Vec<(usize, usize)>) {
    let mut start: Option<usize> = None;
    for (i, c) in run.char_indices() {
        if c.is_alphanumeric() {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push((base + s, base + i));
        }
    }
    if let Some(s) = start {
        out.push((base + s, base + run.len()));
    }
}

/// Span mirror of `tokenize::han_tokens`: jieba words in cut order
/// tile the run, so offsets rebuild by accumulating word lengths; a
/// multi-char word is its own span, consecutive single Han chars
/// flush as overlapping bigrams (a lone one emits itself). The word
/// must equal the run slice at the accumulated offset (jieba tiles
/// its input; a mismatch skips that one span, defensive only).
fn han_spans(base: usize, run: &str, out: &mut Vec<(usize, usize)>) {
    let words = jieba().cut(run, true);
    let mut off = 0usize;
    let mut singles: Vec<(usize, usize)> = Vec::new();
    let flush =
        |singles: &mut Vec<(usize, usize)>, out: &mut Vec<(usize, usize)>| match singles.len() {
            0 => {}
            1 => out.push(singles.pop().expect("len checked above")),
            _ => {
                for pair in singles.windows(2) {
                    out.push((pair[0].0, pair[1].1));
                }
                singles.clear();
            }
        };
    for token in words {
        let w = token.word;
        let len = w.len();
        let span = (base + off, base + off + len);
        let aligned = run.get(off..off + len) == Some(w);
        off += len;
        let mut chars = w.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if is_han(c) {
                if aligned {
                    singles.push(span);
                }
                continue;
            }
        }
        flush(&mut singles, out);
        if aligned && !w.trim().is_empty() {
            out.push(span);
        }
    }
    flush(&mut singles, out);
}

/// A highlightable query word: an exact posting term or a prefix stem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HLToken {
    Exact(Vec<u8>),
    Prefix(Vec<u8>),
}

impl HLToken {
    fn word(&self) -> &[u8] {
        match self {
            HLToken::Exact(w) | HLToken::Prefix(w) => w,
        }
    }
}

/// The positive query words for HIGHLIGHT: Term/Phrase bodies become
/// Exact, Prefix stems become Prefix; And/Or union recursively, Not
/// subtrees are skipped (only positive hits wrap), NumRange carries
/// nothing highlightable, index stopwords drop out (they never
/// indexed), duplicates collapse keeping first occurrence.
pub(crate) fn highlight_terms(node: &QueryNode, meta: &IndexMeta) -> Vec<HLToken> {
    let mut raw = Vec::new();
    collect_terms(node, &mut raw);
    let mut out: Vec<HLToken> = Vec::new();
    for t in raw {
        if meta.stopwords.iter().any(|s| s.as_slice() == t.word()) {
            continue;
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

fn collect_terms(node: &QueryNode, out: &mut Vec<HLToken>) {
    match node {
        QueryNode::Term { term, .. } => out.push(HLToken::Exact(term.clone())),
        QueryNode::Phrase { terms, .. } => {
            out.extend(terms.iter().cloned().map(HLToken::Exact));
        }
        QueryNode::Prefix { prefix, .. } => out.push(HLToken::Prefix(prefix.clone())),
        QueryNode::NumRange { .. } => {}
        QueryNode::And(children) | QueryNode::Or(children) => {
            for child in children {
                collect_terms(child, out);
            }
        }
        QueryNode::Not(_) => {}
    }
}

/// Wrap matched spans of the doc JSON's string fields with the tags,
/// returning a NEW body (the stored doc is untouched). Field names
/// not present, non-string values and non-object bodies fall through
/// unchanged; a body with no hit round-trips byte-identical.
pub(crate) fn highlight_doc(
    body: &[u8],
    fields: &[Vec<u8>],
    terms: &[HLToken],
    open: &[u8],
    close: &[u8],
) -> Vec<u8> {
    if terms.is_empty() {
        return body.to_vec();
    }
    let Ok(obj) = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(body) else {
        return body.to_vec();
    };
    let mut marked = serde_json::Map::new();
    let mut changed = false;
    for (k, v) in obj {
        let hit = fields.iter().any(|f| f.as_slice() == k.as_bytes());
        if hit {
            if let serde_json::Value::String(s) = v {
                let wrapped = highlight_str(&s, terms, open, close);
                changed |= wrapped != s;
                marked.insert(k, serde_json::Value::String(wrapped));
                continue;
            }
        }
        marked.insert(k, v);
    }
    if !changed {
        return body.to_vec();
    }
    serde_json::to_vec(&serde_json::Value::Object(marked)).unwrap_or_else(|_| body.to_vec())
}

/// One JSON string: wrap every token span hitting a term, inserted
/// back-to-front so earlier spans keep their offsets. Token spans
/// are disjoint by construction, so tags never nest.
fn highlight_str(s: &str, terms: &[HLToken], open: &[u8], close: &[u8]) -> String {
    let hits: Vec<(usize, usize)> = token_spans(s)
        .into_iter()
        .filter(|&(st, e)| {
            let tok = s[st..e].to_lowercase();
            let tb = tok.as_bytes();
            terms.iter().any(|t| match t {
                HLToken::Exact(w) => tb == w.as_slice(),
                HLToken::Prefix(p) => !p.is_empty() && tb.starts_with(p.as_slice()),
            })
        })
        .collect();
    if hits.is_empty() {
        return s.to_string();
    }
    let mut out = s.as_bytes().to_vec();
    for &(st, e) in hits.iter().rev() {
        let mut seg = open.to_vec();
        seg.extend_from_slice(&out[st..e]);
        seg.extend_from_slice(close);
        out.splice(st..e, seg);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
#[path = "highlight_tests.rs"]
mod tests;
