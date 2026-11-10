//! `QueryNode` evaluation over the kernel's records: Term = posting
//! lookup (BM25, summed over every scoped field), Phrase = posting
//! intersection + ADJACENT-position chain via the v2 postings
//! (position-less v1 records degrade the phrase to AND -- documented
//! deviation), Prefix = posting-key range walk over both posting
//! kinds (callback filters the term prefix), NumRange = closed-
//! interval walk of the NUMERIC columnar doc-values (constant 1.0
//! per hit, the ES numeric-range convention: no BM25 over
//! doc-values), And/Or = set ops with
//! summed scores, Not = complement against the index's doc records
//! (constant 1.0, the match_all convention).
//!
//! Pure functions over `(TreeCtx, &QueryNode)`; `TreeCtx` carries the
//! corpus stats and the per-query doclen cache (one doc read per doc
//! max, the `ft_search`/`es::eval` pattern).

use std::collections::HashMap;

use crate::store::{ops, Store};

use super::ann;
use super::bm25::term_score;
use super::ft_index::{read_doc, read_posting, read_termstat, FieldType, IndexMeta};
use super::index_codec::{
    decode_numval, decode_term_suffix, doc_range, numval_range, posting_field_range, PostEntry,
};
use super::query_tree::QueryNode;
use crate::ds::codec::{KIND_SEARCH_POSTING, KIND_SEARCH_POSTING2};

/// docid -> summed BM25-ish score.
pub type TreeHits = HashMap<Vec<u8>, f64>;

/// Per-query evaluation context: corpus stats + doclen cache.
pub struct TreeCtx<'a> {
    pub store: &'a Store,
    pub prefix: &'a [u8],
    pub index: &'a [u8],
    pub meta: &'a IndexMeta,
    n: u64,
    avgdl: f64,
    doclens: HashMap<Vec<u8>, u64>,
}

pub fn new_tree_ctx<'a>(
    store: &'a Store,
    prefix: &'a [u8],
    index: &'a [u8],
    meta: &'a IndexMeta,
) -> TreeCtx<'a> {
    let n = meta.num_docs.max(1);
    TreeCtx {
        store,
        prefix,
        index,
        meta,
        n,
        avgdl: meta.sum_doclen as f64 / n as f64,
        doclens: HashMap::new(),
    }
}

/// Fields a leaf searches: a scope is honored as-is (KEYWORD fields
/// are legitimately term-scoped), no scope = every TEXT field.
fn scope_fields<'a>(meta: &'a IndexMeta, scope: &'a Option<Vec<u8>>) -> Vec<&'a [u8]> {
    match scope {
        Some(f) => vec![f.as_slice()],
        None => meta
            .fields
            .iter()
            .filter(|f| matches!(f.ftype, FieldType::Text))
            .map(|f| f.name.as_slice())
            .collect(),
    }
}

/// Cached doclen (defaults 1 on a missing/short record).
fn doclen_of(ctx: &mut TreeCtx, docid: &[u8]) -> u64 {
    if let Some(&l) = ctx.doclens.get(docid) {
        return l;
    }
    let l = read_doc(ctx.store, ctx.prefix, ctx.index, docid)
        .ok()
        .flatten()
        .map_or(1, |r| r.doclen.max(1));
    ctx.doclens.insert(docid.to_vec(), l);
    l
}

/// Evaluate one node; `And([])` and `Or([])` match nothing (the v1
/// empty-query contract).
pub fn eval_node(ctx: &mut TreeCtx, node: &QueryNode) -> Result<TreeHits, String> {
    match node {
        QueryNode::Term { field, term } => eval_term(ctx, field, term),
        QueryNode::Phrase { field, terms } => eval_phrase(ctx, field, terms),
        QueryNode::Prefix { field, prefix } => eval_prefix(ctx, field, prefix),
        QueryNode::NumRange { field, gte, lte } => eval_numrange(ctx, field, *gte, *lte),
        QueryNode::And(children) => {
            let mut acc: Option<TreeHits> = None;
            for c in children {
                let h = eval_node(ctx, c)?;
                acc = Some(match acc {
                    None => h,
                    Some(mut a) => {
                        a.retain(|d, s| match h.get(d) {
                            Some(x) => {
                                *s += x;
                                true
                            }
                            None => false,
                        });
                        a
                    }
                });
            }
            Ok(acc.unwrap_or_default())
        }
        QueryNode::Or(children) => {
            let mut docs = TreeHits::new();
            for c in children {
                for (d, s) in eval_node(ctx, c)? {
                    *docs.entry(d).or_insert(0.0) += s;
                }
            }
            Ok(docs)
        }
        QueryNode::Not(inner) => {
            let excluded = eval_node(ctx, inner)?;
            let mut docs = TreeHits::new();
            all_docids(ctx, &mut |d| {
                if !excluded.contains_key(d) {
                    docs.insert(d.to_vec(), 1.0);
                }
            });
            Ok(docs)
        }
    }
}

/// The canonical NUMERIC doc-value column scan shared by the tree
/// evaluator, the ES `eval`/`exec` fronts and FT.SEARCH SORTBY: walk
/// one field's kind-0x19 records keeping `keep` values -> docid -> f64
/// (the SCORE is the caller's concern). Infallible by the family scan
/// convention: store errors degrade to a partial column. Docids ride
/// the key suffix after the range's lower bound (the field prefix).
pub fn scan_numvals(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    field: &[u8],
    keep: &dyn Fn(f64) -> bool,
) -> HashMap<Vec<u8>, f64> {
    let (lower, upper) = numval_range(prefix, index, field);
    let mut out = HashMap::new();
    let _ = ops::for_each_from(store, &lower, false, &mut |k, v| {
        if !upper.is_empty() && k >= upper.as_slice() {
            return false;
        }
        if let (Some(docid), Some(x)) = (k.get(lower.len()..), decode_numval(v)) {
            if keep(x) {
                out.insert(docid.to_vec(), x);
            }
        }
        true
    });
    out
}

/// Closed-interval range over one NUMERIC field's doc-values (a
/// `None` bound is open); constant score 1.0 per hit -- the ES
/// NUMERIC range/match convention, no BM25 over doc-values.
fn eval_numrange(
    ctx: &TreeCtx,
    field: &[u8],
    gte: Option<f64>,
    lte: Option<f64>,
) -> Result<TreeHits, String> {
    let col = scan_numvals(ctx.store, ctx.prefix, ctx.index, field, &|v| {
        gte.is_none_or(|b| v >= b) && lte.is_none_or(|b| v <= b)
    });
    Ok(col.into_keys().map(|d| (d, 1.0)).collect())
}

/// Every docid of the index, in physical order.
fn all_docids(ctx: &TreeCtx, f: &mut dyn FnMut(&[u8])) {
    let (lower, upper) = doc_range(ctx.prefix, ctx.index);
    let _ = ops::for_each_from(ctx.store, &lower, false, &mut |k, _| {
        if !upper.is_empty() && k >= upper.as_slice() {
            return false;
        }
        if let Some(docid) = ann::docid_of_key(k, ctx.prefix.len()) {
            f(&docid);
        }
        true
    });
}

/// BM25 posting lookup summed over the scoped fields.
fn eval_term(ctx: &mut TreeCtx, field: &Option<Vec<u8>>, term: &[u8]) -> Result<TreeHits, String> {
    let mut docs = TreeHits::new();
    if is_stopword(ctx.meta, term) {
        return Ok(docs); // stopword never entered the index either
    }
    for f in scope_fields(ctx.meta, field) {
        let stat = read_termstat(ctx.store, ctx.prefix, ctx.index, f, term)?;
        if stat.df == 0 {
            continue;
        }
        for e in read_posting(ctx.store, ctx.prefix, ctx.index, f, term)? {
            let dl = doclen_of(ctx, &e.docid);
            *docs.entry(e.docid).or_insert(0.0) +=
                term_score(e.tf, stat.df, dl, ctx.avgdl, ctx.n, ctx.meta.k1, ctx.meta.b);
        }
    }
    Ok(docs)
}

/// Query-side stopword check (Term/Phrase leaves; Prefix stays
/// unfiltered -- a stopword string can still be a legal prefix).
fn is_stopword(meta: &IndexMeta, term: &[u8]) -> bool {
    meta.stopwords.iter().any(|s| s.as_slice() == term)
}

/// Phrase: per field, candidate docs live in EVERY term's posting;
/// docs whose every entry carries positions are kept only when the
/// positions chain adjacently (p, p+1, p+2, ...); a position-less
/// entry (v1 record) degrades that doc to a plain AND member --
/// documented deviation. Score = sum of the constituent term scores
/// (a BM25-flavored phrase score).
fn eval_phrase(
    ctx: &mut TreeCtx,
    field: &Option<Vec<u8>>,
    terms: &[Vec<u8>],
) -> Result<TreeHits, String> {
    let mut docs = TreeHits::new();
    // stopwords drop out of the chain (positions were compacted at
    // index time); every term a stopword -> nothing to match
    let terms: Vec<&[u8]> = terms
        .iter()
        .filter(|t| !is_stopword(ctx.meta, t))
        .map(|t| t.as_slice())
        .collect();
    if terms.is_empty() {
        return Ok(docs);
    }
    for f in scope_fields(ctx.meta, field) {
        // stats first (df per term), then postings; an unseen term
        // leaves no candidates for this field
        let mut stats = Vec::with_capacity(terms.len());
        let mut per_term: Vec<Vec<PostEntry>> = Vec::with_capacity(terms.len());
        let mut usable = true;
        for t in &terms {
            let stat = read_termstat(ctx.store, ctx.prefix, ctx.index, f, t)?;
            let entries = read_posting(ctx.store, ctx.prefix, ctx.index, f, t)?;
            if stat.df == 0 || entries.is_empty() {
                usable = false;
                break;
            }
            stats.push(stat);
            per_term.push(entries);
        }
        if !usable {
            continue;
        }
        'doc: for first in &per_term[0] {
            let docid = first.docid.clone();
            let mut all: Vec<&[u64]> = vec![&first.positions];
            for entries in &per_term[1..] {
                let Some(e) = entries.iter().find(|e| e.docid == docid) else {
                    continue 'doc; // a phrase term missing -> not a candidate
                };
                all.push(&e.positions);
            }
            // v1 record among the entries: positions unknown -> AND
            let matched = all.iter().any(|p| p.is_empty()) || verify_positions(&all);
            if matched {
                let dl = doclen_of(ctx, &docid);
                let mut score = 0.0;
                for (i, entries) in per_term.iter().enumerate() {
                    let e = entries.iter().find(|e| e.docid == docid).unwrap();
                    score += term_score(
                        e.tf,
                        stats[i].df,
                        dl,
                        ctx.avgdl,
                        ctx.n,
                        ctx.meta.k1,
                        ctx.meta.b,
                    );
                }
                *docs.entry(docid).or_insert(0.0) += score;
            }
        }
    }
    Ok(docs)
}

/// Adjacent-position chain: some p in positions[0] with p+1 in
/// positions[1], p+2 in positions[2], ... (positions strictly sorted).
pub fn verify_positions(positions: &[&[u64]]) -> bool {
    if positions.is_empty() {
        return false;
    }
    // cursors = the chain HEADS so far (positions in the current
    // level that extend a full chain); each next level keeps only
    // its q where q-1 was a head. q == 0 cannot extend anything.
    let mut cursors: Vec<u64> = positions[0].to_vec();
    for next in &positions[1..] {
        let mut advanced = Vec::new();
        for &q in *next {
            if q > 0 && cursors.binary_search(&(q - 1)).is_ok() {
                advanced.push(q);
            }
        }
        cursors = advanced;
        if cursors.is_empty() {
            return false;
        }
    }
    !cursors.is_empty()
}

/// Prefix scan: walk both posting kinds' field ranges, decode each
/// key's (field, term), keep `term.starts_with(prefix)`, BM25-sum the
/// matched terms' postings (ES prefix = term disjunction).
fn eval_prefix(
    ctx: &mut TreeCtx,
    field: &Option<Vec<u8>>,
    prefix: &[u8],
) -> Result<TreeHits, String> {
    let mut docs = TreeHits::new();
    if prefix.is_empty() {
        return Ok(docs);
    }
    for f in scope_fields(ctx.meta, field) {
        // collect matching terms first: read_posting inside the
        // for_each closure would borrow the store cursor and the ctx
        let mut terms: Vec<Vec<u8>> = Vec::new();
        for kind in [KIND_SEARCH_POSTING2, KIND_SEARCH_POSTING] {
            let (lower, upper) = posting_field_range(ctx.prefix, ctx.index, f, kind);
            let suffix_at = ctx.prefix.len() + 1 + 4 + ctx.index.len();
            let _ = ops::for_each_from(ctx.store, &lower, false, &mut |k, _| {
                if !upper.is_empty() && k >= upper.as_slice() {
                    return false;
                }
                if let Some((kf, term)) = k.get(suffix_at..).and_then(decode_term_suffix) {
                    if kf == f && term.starts_with(prefix) && !terms.contains(&term) {
                        terms.push(term);
                    }
                }
                true
            });
        }
        for term in terms {
            let stat = read_termstat(ctx.store, ctx.prefix, ctx.index, f, &term)?;
            if stat.df == 0 {
                continue;
            }
            for e in read_posting(ctx.store, ctx.prefix, ctx.index, f, &term)? {
                let dl = doclen_of(ctx, &e.docid);
                *docs.entry(e.docid).or_insert(0.0) +=
                    term_score(e.tf, stat.df, dl, ctx.avgdl, ctx.n, ctx.meta.k1, ctx.meta.b);
            }
        }
    }
    Ok(docs)
}

#[cfg(test)]
#[path = "eval_tree_tests.rs"]
mod tests;
