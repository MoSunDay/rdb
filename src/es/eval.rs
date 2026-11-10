//! Candidate-set evaluation for `_search` plans: every `dsl::Plan`
//! clause maps to a (docid -> score) set over the SAME kernel FT.*
//! uses -- postings + BM25 for TEXT, exact postings for KEYWORD,
//! columnar f64 doc-values for NUMERIC, SPANN/exact-L2 for knn.
//!
//! scoring deviations from real ES (kept local until COMPAT
//! catches up): KEYWORD `term` and NUMERIC `term`/`range` score a
//! CONSTANT 1.0 (no BM25, constant_score); unknown field names match
//! NOTHING instead of erroring "no field mapping"; `bool` honors
//! `minimum_should_match` (count / 0-100 percent, default 1 when
//! only shoulds are present; beside must/filter should stays additive
//! until an explicit msm>0 turns it into a gate); `match_phrase`/
//! `prefix`/`query_string` delegate to the SHARED `eval_tree`
//! kernel; `bool` with only must_not matches everything else, like
//! ES.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::search::ann;
use crate::search::bm25::{term_score, TopK};
use crate::search::ft_index::{
    read_doc, read_posting, read_termstat, vector_field, FieldType, IndexMeta,
};
use crate::search::index_codec::doc_range;
use crate::search::tokenize::tokenize;
use crate::search::vecmath;
use crate::store::{ops, Store};

use super::dsl::{BoolOp, KnnPlan, MinShouldMatch, Plan, TermValue};
use crate::search::eval_tree::{eval_node, new_tree_ctx, scan_numvals};
use crate::search::query_tree::QueryNode;

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
        Plan::MatchPhrase { field, terms } => eval_tree_node(
            ctx,
            &QueryNode::Phrase {
                field: Some(field.as_bytes().to_vec()),
                terms: terms.clone(),
            },
        ),
        Plan::Prefix { field, prefix } => eval_tree_node(
            ctx,
            &QueryNode::Prefix {
                field: Some(field.as_bytes().to_vec()),
                prefix: prefix.clone(),
            },
        ),
        Plan::QueryString { node } => match node {
            // bare `*` parses to None: match-all (ES alignment)
            None => eval_match_all(ctx),
            Some(n) => eval_tree_node(ctx, n),
        },
        Plan::Bool {
            must,
            filter,
            should,
            must_not,
            msm,
        } => eval_bool(ctx, must, filter, should, must_not, msm.as_ref()),
    }
}

/// Shared-kernel bridge: the tree clauses (phrase/prefix/query_string)
/// evaluate over the SAME records via `search::eval_tree`, in their
/// own stats/doclen context (identical inputs).
fn eval_tree_node(ctx: &mut EvalCtx, node: &QueryNode) -> Result<Scored, String> {
    let mut tctx = new_tree_ctx(ctx.store, ctx.prefix, ctx.index, ctx.meta);
    Ok(Scored {
        docs: eval_node(&mut tctx, node)?,
    })
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
    // full-text clause: query-side stopword drop (the plan-level
    // term/terms/range queries stay unfiltered by design -- a
    // stopword term simply has no posting)
    let terms: Vec<&Vec<u8>> = terms
        .iter()
        .filter(|t| !ctx.meta.stopwords.contains(t))
        .collect();
    if terms.is_empty() {
        return Ok(Scored::default());
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
                .map(|(df, tfm)| {
                    term_score(tfm[&d], *df, dl, ctx.avgdl, ctx.n, ctx.meta.k1, ctx.meta.b)
                })
                .sum();
            docs.insert(d, sc);
        }
    } else {
        for (df, tfm) in &per_term {
            for (d, tf) in tfm {
                let dl = doclen_of(ctx, d);
                *docs.entry(d.clone()).or_insert(0.0) +=
                    term_score(*tf, *df, dl, ctx.avgdl, ctx.n, ctx.meta.k1, ctx.meta.b);
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
        (Some(FieldType::Numeric), TermValue::Number(x)) => {
            let docs = scan_numvals(ctx.store, ctx.prefix, ctx.index, field.as_bytes(), &|v| {
                v == *x
            })
            .into_keys()
            .map(|d| (d, 1.0))
            .collect();
            Ok(Scored { docs })
        }
        // Unknown field / type mismatch: empty set (ES would 404 the
        // field mapping; v1 documents this as match-nothing).
        _ => Ok(Scored::default()),
    }
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
    // canonical scan_numvals: keep 谓词组合四个界，值 -> 1.0 进 Scored
    let docs = scan_numvals(ctx.store, ctx.prefix, ctx.index, field.as_bytes(), &|v| {
        gte.is_none_or(|b| v >= b)
            && gt.is_none_or(|b| v > b)
            && lte.is_none_or(|b| v <= b)
            && lt.is_none_or(|b| v < b)
    })
    .into_keys()
    .map(|d| (d, 1.0))
    .collect();
    Ok(Scored { docs })
}

/// How many should clauses a doc must match. Default: 1 when only
/// shoulds are present (union semantics), 0 beside must/filter
/// (additive). A percent rounds to nearest, floored at 1 (ES).
fn required_shoulds(msm: Option<&MinShouldMatch>, n_should: usize, only_should: bool) -> usize {
    let n = n_should as u64;
    match msm {
        None => usize::from(only_should),
        Some(MinShouldMatch::Count(k)) => (*k).min(n) as usize,
        Some(MinShouldMatch::Percent(p)) => ((n * *p as u64 + 50) / 100).max(1).min(n) as usize,
    }
}

/// Per-doc matched-should count and summed score over the should
/// clause results.
fn should_stats(should_s: &[Scored]) -> HashMap<Vec<u8>, (u32, f64)> {
    let mut stats: HashMap<Vec<u8>, (u32, f64)> = HashMap::new();
    for s in should_s {
        for (d, x) in &s.docs {
            let e = stats.entry(d.clone()).or_insert((0, 0.0));
            e.0 += 1;
            e.1 += *x;
        }
    }
    stats
}

fn eval_bool(
    ctx: &mut EvalCtx,
    must: &[Plan],
    filter: &[Plan],
    should: &[Plan],
    must_not: &[Plan],
    msm: Option<&MinShouldMatch>,
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
            // should-only: match >= required shoulds (default 1: the
            // ES union); score = sum of the MATCHED shoulds
            let required = required_shoulds(msm, should.len(), true);
            docs = should_stats(&should_s)
                .into_iter()
                .filter(|(_, (c, _))| *c as usize >= required)
                .map(|(d, (_, sc))| (d, sc))
                .collect();
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
        // explicit msm>0 beside must/filter turns should into a gate
        let gate = required_shoulds(msm, should.len(), false);
        let stats = if gate > 0 {
            Some(should_stats(&should_s))
        } else {
            None
        };
        for d in member.unwrap_or_default().into_keys() {
            match stats.as_ref().and_then(|m| m.get(&d)) {
                Some(req) if (req.0 as usize) >= gate => {}
                // zero matched shoulds also fails an explicit gate
                _ if gate == 0 => {}
                _ => continue,
            }
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
        // should beside must/filter only adds score (default msm 0)
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
