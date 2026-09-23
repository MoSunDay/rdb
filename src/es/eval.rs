//! Candidate-set evaluation for `_search` plans: every `dsl::Plan`
//! clause maps to a (docid -> score) set over the SAME kernel FT.*
//! uses -- postings + BM25 for TEXT, exact postings for KEYWORD,
//! columnar f64 doc-values for NUMERIC, SPANN/exact-L2 for knn.
//!
//! v1 scoring deviations from real ES (kept local until COMPAT
//! catches up): KEYWORD `term` and NUMERIC `term`/`range` score a
//! CONSTANT 1.0 (no BM25, constant_score); unknown field names match
//! NOTHING instead of erroring "no field mapping"; `should` beside
//! must/filter only ADDS score (minimum_should_match unsupported);
//! `bool` with only must_not matches everything else, like ES.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::search::ann;
use crate::search::bm25::{term_score, TopK};
use crate::search::ft_index::{
    read_doc, read_posting, read_termstat, vector_field, FieldType, IndexMeta,
};
use crate::search::index_codec::{decode_numval, doc_range, numval_range};
use crate::search::tokenize::tokenize;
use crate::search::vecmath;
use crate::store::{ops, Store};

use super::dsl::{BoolOp, KnnPlan, Plan, TermValue};

/// (docid -> score) candidate set every clause evaluates to; bool
/// clauses combine these with set ops.
#[derive(Default)]
pub(super) struct Scored {
    pub(super) docs: HashMap<Vec<u8>, f64>,
}

/// Per-execute evaluation context: corpus stats + the doclen cache
/// `ft_search::run_query` also keeps (one read per doc max).
pub(super) struct EvalCtx<'a> {
    store: &'a Store,
    prefix: &'a [u8],
    index: &'a [u8],
    meta: &'a IndexMeta,
    n: u64,
    avgdl: f64,
    doclens: HashMap<Vec<u8>, u64>,
}

pub(super) fn new_ctx<'a>(
    store: &'a Store,
    prefix: &'a [u8],
    index: &'a [u8],
    meta: &'a IndexMeta,
) -> EvalCtx<'a> {
    let n = meta.num_docs.max(1);
    EvalCtx {
        store,
        prefix,
        index,
        meta,
        n,
        avgdl: meta.sum_doclen as f64 / n as f64,
        doclens: HashMap::new(),
    }
}

/// b += a's scores (union with sum).
fn union_sum(a: &HashMap<Vec<u8>, f64>, b: &mut HashMap<Vec<u8>, f64>) {
    for (d, s) in a {
        *b.entry(d.clone()).or_insert(0.0) += s;
    }
}

/// Keep only keys present in both (a mutated in place).
fn retain_common(a: &mut HashMap<Vec<u8>, f64>, b: &HashMap<Vec<u8>, f64>) {
    a.retain(|d, _| b.contains_key(d));
}

pub(super) fn eval(ctx: &mut EvalCtx, plan: &Plan) -> Result<Scored, String> {
    match plan {
        Plan::MatchAll => eval_match_all(ctx),
        Plan::Match {
            field,
            terms,
            operator,
        } => eval_match(ctx, field, terms, operator),
        Plan::Term { field, value } => eval_term(ctx, field, value),
        Plan::Terms { field, values } => {
            let mut docs = HashMap::new();
            for v in values {
                union_sum(&eval_term(ctx, field, v)?.docs, &mut docs);
            }
            Ok(Scored { docs })
        }
        Plan::Range {
            field,
            gte,
            gt,
            lte,
            lt,
        } => eval_range(ctx, field, *gte, *gt, *lte, *lt),
        Plan::Bool {
            must,
            filter,
            should,
            must_not,
        } => eval_bool(ctx, must, filter, should, must_not),
    }
}

/// Every doc record in the index, score 1.0 (ES match_all constant).
fn eval_match_all(ctx: &EvalCtx) -> Result<Scored, String> {
    let (lower, upper) = doc_range(ctx.prefix, ctx.index);
    let mut docs = HashMap::new();
    let _ = ops::for_each_from(ctx.store, &lower, false, &mut |k, _| {
        if !upper.is_empty() && k >= upper.as_slice() {
            return false;
        }
        if let Some(docid) = ann::docid_of_key(k, ctx.prefix.len()) {
            docs.insert(docid, 1.0);
        }
        true
    });
    Ok(Scored { docs })
}

/// Cached doclen (defaults 1 on a missing/short record, like
/// `ft_search::run_query`).
fn doclen_of(ctx: &mut EvalCtx, docid: &[u8]) -> u64 {
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

/// BM25 over one field's postings: Or unions and sums, And intersects
/// the postings and sums each term's score for the surviving docs.
fn eval_match(
    ctx: &mut EvalCtx,
    field: &str,
    terms: &[Vec<u8>],
    op: &BoolOp,
) -> Result<Scored, String> {
    if terms.is_empty() {
        return Ok(Scored::default()); // empty token list matches nothing
    }
    let fb = field.as_bytes();
    let mut per_term: Vec<(u64, HashMap<Vec<u8>, u64>)> = Vec::with_capacity(terms.len());
    for t in terms {
        let stat = read_termstat(ctx.store, ctx.prefix, ctx.index, fb, t)?;
        let posting = read_posting(ctx.store, ctx.prefix, ctx.index, fb, t)?;
        if stat.df == 0 || posting.is_empty() {
            if *op == BoolOp::And {
                return Ok(Scored::default());
            }
            continue;
        }
        let tfm = posting.into_iter().map(|e| (e.docid, e.tf)).collect();
        per_term.push((stat.df, tfm));
    }
    if per_term.is_empty() {
        return Ok(Scored::default());
    }
    let mut docs = HashMap::new();
    if *op == BoolOp::And {
        let mut ids: HashSet<Vec<u8>> = per_term[0].1.keys().cloned().collect();
        for (_, tfm) in &per_term[1..] {
            ids.retain(|d| tfm.contains_key(d));
        }
        for d in ids {
            let dl = doclen_of(ctx, &d);
            let sc = per_term
                .iter()
                .map(|(df, tfm)| term_score(tfm[&d], *df, dl, ctx.avgdl, ctx.n))
                .sum();
            docs.insert(d, sc);
        }
    } else {
        for (df, tfm) in &per_term {
            for (d, tf) in tfm {
                let dl = doclen_of(ctx, d);
                *docs.entry(d.clone()).or_insert(0.0) += term_score(*tf, *df, dl, ctx.avgdl, ctx.n);
            }
        }
    }
    Ok(Scored { docs })
}

/// Normalize a `term` value for a TEXT field: tokenize; a single token
/// (the tokenizer lowercases latin) is the posting key, anything else
/// falls back to the lowercased raw bytes.
fn text_term_bytes(raw: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(raw) {
        Ok(s) => {
            let toks = tokenize(s);
            if toks.len() == 1 {
                toks.into_iter().next().unwrap().into_bytes()
            } else {
                s.to_lowercase().into_bytes()
            }
        }
        Err(_) => raw.to_vec(),
    }
}

fn eval_term(ctx: &mut EvalCtx, field: &str, value: &TermValue) -> Result<Scored, String> {
    let ftype = ctx
        .meta
        .fields
        .iter()
        .find(|f| f.name == field.as_bytes())
        .map(|f| &f.ftype);
    match (ftype, value) {
        // TEXT: BM25-scored posting of the normalized single term.
        (Some(FieldType::Text), TermValue::Bytes(b)) => {
            let term = text_term_bytes(b);
            if term.is_empty() {
                Ok(Scored::default())
            } else {
                eval_match(ctx, field, std::slice::from_ref(&term), &BoolOp::Or)
            }
        }
        // KEYWORD: exact bytes, constant_score 1.0 (v1 deviation).
        (Some(FieldType::Keyword), TermValue::Bytes(b)) => {
            let docs = read_posting(ctx.store, ctx.prefix, ctx.index, field.as_bytes(), b)?
                .into_iter()
                .map(|e| (e.docid, 1.0))
                .collect();
            Ok(Scored { docs })
        }
        // NUMERIC: f64 doc-value equality (exact LE roundtrip).
        (Some(FieldType::Numeric), TermValue::Number(x)) => scan_numvals(ctx, field, &|v| v == *x),
        // Unknown field / type mismatch: empty set (ES would 404 the
        // field mapping; v1 documents this as match-nothing).
        _ => Ok(Scored::default()),
    }
}

/// Walk one NUMERIC field's doc-value records keeping `keep` values
/// (constant score 1.0). Docids ride the key suffix after the range's
/// lower bound (the `numval_range` field prefix).
fn scan_numvals(ctx: &EvalCtx, field: &str, keep: &dyn Fn(f64) -> bool) -> Result<Scored, String> {
    let (lower, upper) = numval_range(ctx.prefix, ctx.index, field.as_bytes());
    let mut docs = HashMap::new();
    let _ = ops::for_each_from(ctx.store, &lower, false, &mut |k, v| {
        if !upper.is_empty() && k >= upper.as_slice() {
            return false;
        }
        if let (Some(docid), Some(x)) = (k.get(lower.len()..), decode_numval(v)) {
            if keep(x) {
                docs.insert(docid.to_vec(), 1.0);
            }
        }
        true
    });
    Ok(Scored { docs })
}

/// Range over a NUMERIC field's doc-values; non-numeric fields (or
/// missing mappings) match nothing (v1 deviation).
fn eval_range(
    ctx: &EvalCtx,
    field: &str,
    gte: Option<f64>,
    gt: Option<f64>,
    lte: Option<f64>,
    lt: Option<f64>,
) -> Result<Scored, String> {
    let numeric = ctx
        .meta
        .fields
        .iter()
        .any(|f| f.name == field.as_bytes() && matches!(f.ftype, FieldType::Numeric));
    if !numeric {
        return Ok(Scored::default());
    }
    scan_numvals(ctx, field, &|v| {
        gte.is_none_or(|b| v >= b)
            && gt.is_none_or(|b| v > b)
            && lte.is_none_or(|b| v <= b)
            && lt.is_none_or(|b| v < b)
    })
}

fn eval_bool(
    ctx: &mut EvalCtx,
    must: &[Plan],
    filter: &[Plan],
    should: &[Plan],
    must_not: &[Plan],
) -> Result<Scored, String> {
    let must_s = eval_all(ctx, must)?;
    let filter_s = eval_all(ctx, filter)?;
    let should_s = eval_all(ctx, should)?;
    let not_s = eval_all(ctx, must_not)?;
    let mut docs: HashMap<Vec<u8>, f64> = HashMap::new();
    if must.is_empty() && filter.is_empty() {
        if should.is_empty() {
            // bool{must_not} / bool{}: everything minus must_not
            docs = eval(ctx, &Plan::MatchAll)?.docs;
        } else {
            // should is REQUIRED only when must+filter are empty: Or
            for s in &should_s {
                union_sum(&s.docs, &mut docs);
            }
        }
    } else {
        // membership = must INTERSECT filter (docs-only from filter)
        let mut member: Option<HashMap<Vec<u8>, f64>> = None;
        for s in must_s.iter().chain(&filter_s) {
            match member.as_mut() {
                None => member = Some(s.docs.clone()),
                Some(m) => retain_common(m, &s.docs),
            }
        }
        for d in member.unwrap_or_default().into_keys() {
            // must scores sum; filter-only matches keep constant 1.0
            let parts: Vec<f64> = must_s
                .iter()
                .filter_map(|s| s.docs.get(&d))
                .copied()
                .collect();
            let sc = if parts.is_empty() {
                1.0
            } else {
                parts.iter().sum()
            };
            docs.insert(d, sc);
        }
        // should beside must/filter only adds score (msm 0 default)
        for s in &should_s {
            for (d, x) in &s.docs {
                if let Some(e) = docs.get_mut(d) {
                    *e += x;
                }
            }
        }
    }
    for s in &not_s {
        for d in s.docs.keys() {
            docs.remove(d);
        }
    }
    Ok(Scored { docs })
}

fn eval_all(ctx: &mut EvalCtx, plans: &[Plan]) -> Result<Vec<Scored>, String> {
    plans.iter().map(|p| eval(ctx, p)).collect()
}

/// nprobe mapping for unfiltered knn (v1: num_candidates/16 clamped
/// to SPANN's useful probe range).
fn nprobe_of(num_candidates: usize) -> usize {
    (num_candidates / 16).clamp(1, 64)
}

/// knn: exact L2 over the filtered candidate set, or `ann::knn` when
/// unfiltered. Scores are 1/(1+L2) so higher sorts nearer.
pub(super) fn run_knn(
    ctx: &mut EvalCtx,
    knn: &KnnPlan,
    filter: Option<&Plan>,
) -> Result<Vec<(Vec<u8>, f64)>, String> {
    let Some((vfield, dim)) = vector_field(ctx.meta) else {
        return Err(format!("knn: index has no VECTOR field '{}'", knn.field));
    };
    if knn.field.as_bytes() != vfield {
        return Err(format!("knn: index has no VECTOR field '{}'", knn.field));
    }
    if knn.vector.len() != dim as usize {
        return Err(format!(
            "knn: query_vector dim {} does not match index dim {}",
            knn.vector.len(),
            dim
        ));
    }
    match filter {
        None => Ok(ann::knn(
            ctx.store,
            ctx.prefix,
            ctx.index,
            &vfield,
            dim,
            &knn.vector,
            knn.k,
            nprobe_of(knn.num_candidates),
        )?
        .into_iter()
        .map(|(docid, l2)| (docid, 1.0 / (1.0 + l2)))
        .collect()),
        Some(p) => {
            let candidates = eval(ctx, p)?.docs;
            let mut top = TopK::new(knn.k);
            for docid in candidates.keys() {
                if let Ok(Some(rec)) = read_doc(ctx.store, ctx.prefix, ctx.index, docid) {
                    if rec.vector.len() == dim as usize {
                        let l2 = vecmath::l2(&rec.vector, &knn.vector);
                        top.push(docid, 1.0 / (1.0 + l2));
                    }
                }
            }
            Ok(top
                .finish()
                .into_iter()
                .map(|h| (h.docid, h.score))
                .collect())
        }
    }
}

/// Parsed source of a doc record; invalid JSON -> None (never fails
/// the query).
pub(super) fn load_source(ctx: &EvalCtx, docid: &[u8]) -> Option<Value> {
    let rec = read_doc(ctx.store, ctx.prefix, ctx.index, docid)
        .ok()
        .flatten()?;
    serde_json::from_slice(&rec.doc).ok()
}

#[cfg(test)]
#[path = "eval_tests.rs"]
mod eval_tests;
