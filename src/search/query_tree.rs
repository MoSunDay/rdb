//! Shared query-tree parser: ONE grammar serving both frontends --
//! RESP `FT.SEARCH` (`QueryDialect::Ft`) and ES `query_string`
//! (`QueryDialect::QueryString`) -- so the two protocols lower onto
//! the SAME `QueryNode` AST and the same RocksDB records.
//!
//! Dialect v2 grammar (superset of the v1 whitespace-AND dialect):
//! - whitespace-separated clauses, implicit AND;
//! - `@field:unit` scopes (Ft); `field:unit` bare scopes with at
//!   least one letter in the name (QueryString -- Lucene-style;
//!   `12:30` stays a plain term list, not field `12`);
//! - `"quoted phrase"`: ordered adjacent tokens (positions come from
//!   the v2 postings; single-token phrases fold to a Term);
//! - trailing `*` on a single-token stem = prefix scan (`redi*`);
//!   a multi-token (CJK-segmented) stem degrades to its AND terms,
//!   star ignored (documented deviation);
//! - `@field:[min max]` = NUMERIC range over the columnar doc-values
//!   (closed interval, exactly two bounds; `*`/`-inf`/`+inf`, case
//!   insensitive, = open bound). Malformed bodies are a HARD error
//!   (`ERR bad numeric range`), never a silently dropped clause. An
//!   UNSCOPED `[1 5]` keeps the legacy word path (`[` is not special
//!   without a scope);
//! - `|` or uppercase `OR` between clauses = union of the two;
//!   uppercase `AND` is accepted and implicit; uppercase `NOT` (or a
//!   leading `-`) negates the NEXT clause. Lowercase and/or/not are
//!   ordinary terms. No parentheses (v1 subset).
//! - `*` alone = match-all (parser returns `None`); an EMPTY query
//!   returns `Some(And([]))` which matches NOTHING (v1 semantics).
//!
//! Tokenization is the SAME `tokenize` as indexing on both sides, so
//! every leaf term lines up with a posting key.

use super::tokenize::tokenize;

/// Pure query AST shared by `ft_search` and the ES `query_string`
/// frontend; evaluation lives in `eval_tree`.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryNode {
    /// Exact posting lookup; `field: None` = every TEXT field.
    /// `raw` = the clause body as typed (KEYWORD exact lookups).
    Term {
        field: Option<Vec<u8>>,
        term: Vec<u8>,
        raw: Vec<u8>,
    },
    /// Ordered adjacent tokens (v2 postings supply positions; a term
    /// with unknown positions degrades to AND, see `eval_tree`).
    /// `raw` = the quoted body as typed (KEYWORD exact lookups).
    Phrase {
        field: Option<Vec<u8>>,
        terms: Vec<Vec<u8>>,
        raw: Vec<u8>,
    },
    /// Posting-key prefix iteration (`term*`). `raw` = the stem as
    /// typed, star excluded (KEYWORD exact lookups).
    Prefix {
        field: Option<Vec<u8>>,
        prefix: Vec<u8>,
        raw: Vec<u8>,
    },
    /// Closed-interval walk of one NUMERIC field's columnar
    /// doc-values (`@price:[10 20]`; `None` bound = open). The
    /// syntax only arises under a scope, so the field is mandatory.
    NumRange {
        field: Vec<u8>,
        gte: Option<f64>,
        lte: Option<f64>,
    },
    /// Intersection, scores summed for surviving docs; `And([])`
    /// matches nothing (the v1 empty-query contract).
    And(Vec<QueryNode>),
    /// Union, scores summed.
    Or(Vec<QueryNode>),
    /// Complement against all docs of the index, constant 1.0.
    Not(Box<QueryNode>),
}

/// Which frontend is parsing; only the scope syntax differs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QueryDialect {
    /// `FT.SEARCH`: `@field:` scopes, `-` negation, `|` OR.
    Ft,
    /// ES `query_string`: bare `field:` scopes, AND/OR/NOT keywords.
    QueryString,
}

enum Body {
    Phrase(String),
    Word {
        stem: String,
        prefix: bool,
    },
    /// Inner text of a scoped `[...]` numeric range body.
    NumRange(String),
}

enum LexUnit {
    Clause {
        field: Option<Vec<u8>>,
        body: Body,
        negated: bool,
    },
    Or,
    And,
    Not,
}

/// Parse a query blob; `None` = match-all `*`.
pub fn parse_query_tree(
    query: &[u8],
    dialect: QueryDialect,
) -> Result<Option<QueryNode>, &'static str> {
    let text = std::str::from_utf8(query).map_err(|_| "ERR query must be UTF-8")?;
    if text.trim() == "*" {
        return Ok(None);
    }
    let units = lex(text, dialect);
    let mut acc: Vec<QueryNode> = Vec::new();
    let mut or_pending = false;
    let mut negate_next = false;
    for unit in units {
        match unit {
            LexUnit::Or => or_pending = true,
            LexUnit::And => {} // implicit between clauses
            LexUnit::Not => negate_next = true,
            LexUnit::Clause {
                field,
                body,
                negated,
            } => {
                // scoped `[min max]`: a malformed range is a HARD
                // error (never a silently dropped clause). Unscoped
                // `[..]` never lexes as NumRange (lex gates on the
                // scope), so the field is always present here.
                let node = match body {
                    Body::NumRange(inner) => {
                        let (gte, lte) = parse_num_range(&inner).ok_or("ERR bad numeric range")?;
                        Some(QueryNode::NumRange {
                            field: field.unwrap_or_default(),
                            gte,
                            lte,
                        })
                    }
                    body => body_to_node(field, body),
                };
                let Some(node) = node else {
                    negate_next = false; // empty clause: nothing to negate
                    continue;
                };
                let node = if negate_next || negated {
                    QueryNode::Not(Box::new(node))
                } else {
                    node
                };
                negate_next = false;
                match (or_pending, acc.last_mut()) {
                    (true, Some(QueryNode::Or(v))) => v.push(node),
                    (true, Some(_)) => {
                        let prev = acc.pop().unwrap();
                        acc.push(QueryNode::Or(vec![prev, node]));
                    }
                    _ => acc.push(node), // leading `OR` with no left side: lenient
                }
                or_pending = false;
            }
        }
    }
    Ok(Some(match acc.len() {
        0 => QueryNode::And(Vec::new()), // empty query: matches nothing (v1)
        1 => acc.pop().unwrap(),
        _ => QueryNode::And(acc),
    }))
}

/// Scan the query into lex units. Pure; whitespace/punctuation only
/// shapes tokens, every error case degrades (empty stems vanish).
fn lex(text: &str, dialect: QueryDialect) -> Vec<LexUnit> {
    let chars: Vec<char> = text.chars().collect();
    let mut units = Vec::new();
    let (mut i, len) = (0usize, chars.len());
    while i < len {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // leading '-' attached to a clause negates it (-term, -@f:x, -"p")
        let mut negated = false;
        if chars[i] == '-' && chars.get(i + 1).is_some_and(|c| !c.is_whitespace()) {
            negated = true;
            i += 1;
        }
        // -- optional field scope --
        let mut field: Option<Vec<u8>> = None;
        if chars[i] == '@' {
            let (start, mut j) = (i + 1, i + 1);
            while j < len && chars[j] != ':' && !chars[j].is_whitespace() && chars[j] != '"' {
                j += 1;
            }
            if j > start && j < len && chars[j] == ':' {
                field = Some(chars[start..j].iter().collect::<String>().into_bytes());
                i = j + 1;
            }
            // no colon: '@word' falls through and parses as a plain word
        } else if dialect == QueryDialect::QueryString && !negated {
            let (start, mut j) = (i, i);
            while j < len
                && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '.')
                && chars[j] != ':'
            {
                j += 1;
            }
            let named = j > start
                && chars[j..].first() == Some(&':')
                && chars[start..j].iter().any(|c| c.is_alphabetic());
            if named {
                field = Some(chars[start..j].iter().collect::<String>().into_bytes());
                i = j + 1;
            }
        }
        // -- clause body --
        // a scoped `[...]` opens a numeric range (whitespace inside is
        // part of the body, so the scan runs to `]`, or to the end of
        // the input when unterminated -- the phrase convention);
        // without a scope `[` keeps the legacy word path
        if field.is_some() && chars.get(i) == Some(&'[') {
            let start = i + 1;
            i += 1;
            while i < len && chars[i] != ']' {
                i += 1;
            }
            let inner: String = chars[start..i].iter().collect();
            i += 1; // consume ']' (or past end when unterminated)
            units.push(LexUnit::Clause {
                field,
                body: Body::NumRange(inner),
                negated,
            });
        } else if chars.get(i) == Some(&'"') {
            i += 1;
            let start = i;
            while i < len && chars[i] != '"' {
                i += 1;
            }
            let phrase: String = chars[start..i].iter().collect();
            i += 1; // consume closing quote (or past end when unterminated)
            units.push(LexUnit::Clause {
                field,
                body: Body::Phrase(phrase),
                negated,
            });
        } else {
            let start = i;
            while i < len && !chars[i].is_whitespace() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if field.is_none() && !negated {
                match word.as_str() {
                    "OR" | "|" => {
                        units.push(LexUnit::Or);
                        continue;
                    }
                    "AND" => {
                        units.push(LexUnit::And);
                        continue;
                    }
                    "NOT" => {
                        units.push(LexUnit::Not);
                        continue;
                    }
                    _ => {}
                }
            }
            let prefix = word.ends_with('*');
            let stem = match word.strip_suffix('*') {
                Some(s) if prefix => s.to_string(),
                _ => word,
            };
            units.push(LexUnit::Clause {
                field,
                body: Body::Word { stem, prefix },
                negated,
            });
        }
    }
    units
}

/// One `[min max]` bound: `*`/`-inf`/`+inf` (case-insensitive) = open
/// end (`None`), anything else must parse as f64 (`None` = malformed).
fn num_bound(text: &str) -> Option<Option<f64>> {
    match text.to_ascii_lowercase().as_str() {
        "*" | "-inf" | "+inf" => Some(None),
        _ => text.parse::<f64>().map(Some).ok(),
    }
}

/// `[min max]` inner text -> `(gte, lte)`; exactly two
/// whitespace-split bounds or `None` (malformed, the caller's hard
/// error). Pure.
pub fn parse_num_range(text: &str) -> Option<(Option<f64>, Option<f64>)> {
    let mut parts = text.split_whitespace();
    let gte = num_bound(parts.next()?)?;
    let lte = num_bound(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some((gte, lte))
}

/// One clause body -> a leaf/AND subtree under the parsed scope.
/// Every leaf carries `raw` (the body bytes AS TYPED) so the
/// evaluator can do exact-bytes lookups on KEYWORD fields.
fn body_to_node(field: Option<Vec<u8>>, body: Body) -> Option<QueryNode> {
    match body {
        Body::Phrase(s) => {
            let toks = tokenize(&s);
            match toks.len() {
                0 => None, // noise-only phrase contributes nothing (v1 skip)
                1 => Some(QueryNode::Term {
                    field,
                    term: toks.into_iter().next().unwrap().into_bytes(),
                    raw: s.into_bytes(),
                }),
                _ => Some(QueryNode::Phrase {
                    field,
                    terms: toks.into_iter().map(|t| t.into_bytes()).collect(),
                    raw: s.into_bytes(),
                }),
            }
        }
        Body::Word { stem, prefix } => {
            let toks = tokenize(&stem);
            if toks.is_empty() {
                return None;
            }
            if prefix && toks.len() == 1 {
                return Some(QueryNode::Prefix {
                    field,
                    prefix: toks.into_iter().next().unwrap().into_bytes(),
                    raw: stem.into_bytes(),
                });
            }
            // multi-token stem (CJK segmentation): AND of the terms;
            // a trailing star cannot survive segmentation and is dropped
            let mut terms = toks.into_iter().map(|t| QueryNode::Term {
                field: field.clone(),
                term: t.into_bytes(),
                raw: stem.clone().into_bytes(),
            });
            let first = terms.next().unwrap();
            Some(match terms.next() {
                None => first,
                Some(second) => {
                    QueryNode::And(vec![first, second].into_iter().chain(terms).collect())
                }
            })
        }
        // unreachable from `lex` (NumRange only lexes under a scope
        // and the parse loop intercepts it first); degrade to nothing
        Body::NumRange(_) => None,
    }
}

#[cfg(test)]
#[path = "query_tree_tests.rs"]
mod tests;
