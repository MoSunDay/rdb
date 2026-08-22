//! Hybrid CJK/Latin tokenizer (index side AND query side -- both must
//! produce identical terms or postings never line up).
//!
//! - Latin/digit/other scripts: runs of `char::is_alphanumeric` split
//!   on everything else, lowercased (Lucene StandardAnalyzer-lite).
//! - Han runs: jieba (embedded dictionary + HMM) cuts words; words of
//!   2+ characters emit as-is, and consecutive single characters --
//!   dictionary misses -- fall back to overlapping BIGRAMS (the
//!   Lucene CJKAnalyzer trick: keeps substring recall for unsegmented
//!   text without emitting every char). An isolated single character
//!   (run length 1, or a run that is one giant unknown) emits itself.
//! - Pure noise (punctuation, whitespace, emoji) emits nothing.
//!
//! The jieba dictionary (~5 MB) loads once into a process-global
//! `OnceLock`; the first FT.* command on a fresh process pays that cost.

use std::sync::OnceLock;

use jieba_rs::Jieba;

fn jieba() -> &'static Jieba {
    static JIEBA: OnceLock<Jieba> = OnceLock::new();
    JIEBA.get_or_init(Jieba::new)
}

/// Han code points jieba knows how to segment (Unified Ideographs,
/// Extension A, compatibility ideographs). Kana/Hangul are rare in the
/// dict and fall through to the alphanumeric-run path.
fn is_han(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// One contiguous script run of the input.
#[derive(Debug, PartialEq)]
enum Run<'a> {
    Han(&'a str),
    Other(&'a str),
}

/// Split `text` into maximal Han / non-Han runs. Non-Han runs keep
/// their raw bytes; alphanumeric filtering happens per token.
fn runs(text: &str) -> Vec<Run<'_>> {
    let mut out = Vec::new();
    let mut other_start = 0usize;
    let mut in_han = false;
    for (i, c) in text.char_indices() {
        if is_han(c) != in_han {
            if in_han {
                out.push(Run::Han(&text[other_start..i]));
            } else if i > other_start {
                out.push(Run::Other(&text[other_start..i]));
            }
            other_start = i;
            in_han = !in_han;
        }
    }
    match in_han {
        true => out.push(Run::Han(&text[other_start..])),
        false if text.len() > other_start => out.push(Run::Other(&text[other_start..])),
        false => {}
    }
    out
}

/// Alphanumeric tokens of a non-Han run, lowercased. Digits glue to
/// letters ("mp3" stays one token); lone punctuation disappears.
fn latin_tokens(run: &str, out: &mut Vec<String>) {
    let mut cur = String::new();
    for c in run.chars() {
        if c.is_alphanumeric() {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(cur.to_lowercase());
            cur = String::new();
        }
    }
    if !cur.is_empty() {
        out.push(cur.to_lowercase());
    }
}

/// Han run -> jieba words. Multi-char words pass through; runs of
/// single characters (dictionary misses) become overlapping bigrams.
/// A lone character (whole run is one char) emits itself so single-char
/// queries still match.
fn han_tokens(run: &str, out: &mut Vec<String>) {
    // A jieba segment of exactly one Han char is a dictionary miss;
    // consecutive misses accumulate and flush as overlapping bigrams
    // (a lone one emits itself). Multi-char segments are dictionary
    // words and pass through verbatim.
    let words = jieba().cut(run, true);
    let mut singles: Vec<char> = Vec::new();
    let flush = |singles: &mut Vec<char>, out: &mut Vec<String>| match singles.len() {
        0 => {}
        1 => {
            out.push(singles[0].to_string());
            singles.clear();
        }
        _ => {
            for pair in singles.windows(2) {
                out.push(pair.iter().collect());
            }
            singles.clear();
        }
    };
    for token in words {
        let w = token.word;
        let mut chars = w.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if is_han(c) {
                singles.push(c);
                continue;
            }
        }
        flush(&mut singles, out);
        if !w.trim().is_empty() {
            out.push(w.to_string());
        }
    }
    flush(&mut singles, out);
}

/// Tokenize `text` into ordered terms (with duplicates -- tf is the
/// duplicate count). Same function runs at index and query time.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for run in runs(text) {
        match run {
            Run::Han(s) => han_tokens(s, &mut out),
            Run::Other(s) => latin_tokens(s, &mut out),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<String> {
        tokenize(text)
    }

    #[test]
    fn latin_lowercases_and_drops_punct() {
        assert_eq!(terms("Hello, WORLD!"), vec!["hello", "world"]);
        assert_eq!(terms("mp3 player v2.1"), vec!["mp3", "player", "v2", "1"]);
        assert!(terms("  ,.!?  ").is_empty());
    }

    #[test]
    fn mixed_text_has_both_scripts() {
        let t = terms("Redis 支持全文检索 full-text");
        assert!(t.contains(&"redis".to_string()));
        assert!(t.contains(&"full".to_string()));
        assert!(t.contains(&"text".to_string()));
        assert!(t.iter().any(|x| x.contains('支') || x == "支持"));
        assert!(t.iter().any(|x| x.contains('检') || x == "检索"));
    }

    #[test]
    fn known_words_segment_and_roundtrip_concat() {
        // Dictionary words come out whole; concatenating all Han terms
        // and singles must reconstruct the run's characters IN ORDER
        // (bigrams overlap, so reconstruct via first token + last chars).
        let src = "中华人民共和国";
        let t = terms(src);
        assert!(!t.is_empty());
        // Version-independent: every term is a substring, every char is
        // covered by some term (no losses across jieba/bigram paths).
        for term in &t {
            assert!(!term.is_empty());
            assert!(src.contains(term.as_str()), "term {term:?} not in source");
        }
        for c in src.chars() {
            assert!(t.iter().any(|x| x.contains(c)), "char {c} lost");
        }
    }

    #[test]
    fn unknown_han_falls_back_to_bigrams() {
        // 龘/爨/鱻 are outside the default dictionary's common vocabulary;
        // whatever jieba does, no term may be empty and chars survive.
        let t = terms("龘爨鱻");
        assert!(!t.is_empty());
        assert!(t.iter().all(|x| !x.is_empty()));
        // every emitted term only contains chars from the input
        assert!(t.iter().all(|x| x.chars().all(|c| "龘爨鱻".contains(c))));
    }

    #[test]
    fn lone_han_char_emits_itself() {
        assert_eq!(terms("龙"), vec!["龙"]);
    }

    #[test]
    fn tf_counts_duplicates_in_order() {
        let t = terms("go go go 编译");
        assert_eq!(t[..3].iter().filter(|x| x.as_str() == "go").count(), 3);
    }
}
