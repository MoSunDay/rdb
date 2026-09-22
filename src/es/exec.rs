//! Executor glue for parsed `_search` plans: run the query/knn
//! evaluation (`eval`), sort (default `_score` desc, docid asc
//! tiebreak), apply the from/size window, then shape `_source`.
//! Pure functions of (Store, prefix, index, meta, plan).
//!
//! Reply-shaping deviations from real ES: `_score` is null when the
//! FIRST sort key is not `_score` (field sorts); `max_score` covers
//! all matches pre-windowing and is None for scoreless sorts; `_doc`
//! sort ignores its direction (natural docid order); sources that
//! fail to parse JSON become Null instead of failing the query.

use std::cmp::Ordering;

use serde_json::Value;

use crate::search::ft_index::IndexMeta;
use crate::store::Store;

use super::dsl::{Plan, SearchPlan, SortDir, SortKey};
use super::eval::{eval, load_source, new_ctx, run_knn};
use super::source::{cmp_sortval, filter_source, path_get, sortval_of, SortVal};

/// One reply hit: `_id` bytes, `_score` (None when the first sort key
/// is not `_score`) and the shaped `_source`.
pub struct ExecHit {
    pub docid: Vec<u8>,
    pub score: Option<f64>,
    /// Parsed `DocRecord.doc` source; Null when `_source` is off.
    pub source: Value,
}

pub struct ExecResult {
    /// Full match count BEFORE windowing.
    pub total: usize,
    pub max_score: Option<f64>,
    pub hits: Vec<ExecHit>,
}

/// One hit's sort inputs: the doc's extracted sort keys (aligned with
/// the plan's sort vec; Doc/Score slots carry placeholders), plus the
/// source when a field sort forced the read (reused for the reply).
struct Row {
    docid: Vec<u8>,
    score: f64,
    keys: Vec<SortVal>,
    source: Option<Value>,
}

static DEFAULT_SORT: [SortKey; 1] = [SortKey::Score(SortDir::Desc)];

/// Deterministic ordering: each sort key in turn, docid asc as the
/// final tiebreak. `_doc` is the natural (docid) order; its `dir` is
/// ignored (v1 deviation).
fn cmp_rows(a: &Row, b: &Row, sort: &[SortKey]) -> Ordering {
    for (i, key) in sort.iter().enumerate() {
        let o = match key {
            SortKey::Doc => a.docid.cmp(&b.docid),
            SortKey::Score(dir) => flip(a.score.total_cmp(&b.score), *dir),
            SortKey::Field { dir, .. } => cmp_sortval(&a.keys[i], &b.keys[i], *dir),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.docid.cmp(&b.docid)
}

fn flip(o: Ordering, dir: SortDir) -> Ordering {
    if dir == SortDir::Desc {
        o.reverse()
    } else {
        o
    }
}

/// Evaluate, sort, window, then shape sources.
pub fn execute(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
    plan: &SearchPlan,
) -> Result<ExecResult, String> {
    let mut ctx = new_ctx(store, prefix, index, meta);
    let trivial_query = matches!(plan.query, Plan::MatchAll);
    let scored: Vec<(Vec<u8>, f64)> = if let Some(knn) = &plan.knn {
        let filter = knn
            .filter
            .as_deref()
            .or_else(|| if trivial_query { None } else { Some(&plan.query) });
        run_knn(&mut ctx, knn, filter)?
    } else {
        eval(&mut ctx, &plan.query)?.docs.into_iter().collect()
    };
    let sort: &[SortKey] = if plan.sort.is_empty() {
        &DEFAULT_SORT
    } else {
        &plan.sort
    };
    let by_field = sort.iter().any(|s| matches!(s, SortKey::Field { .. }));
    let mut rows: Vec<Row> = Vec::with_capacity(scored.len());
    for (docid, score) in scored {
        let source = if by_field { load_source(&ctx, &docid) } else { None };
        let keys = sort
            .iter()
            .map(|s| match s {
                SortKey::Field { field, .. } => source
                    .as_ref()
                    .and_then(|v| path_get(v, field))
                    .map(sortval_of)
                    .unwrap_or(SortVal::Missing),
                _ => SortVal::Missing, // Doc/Score compare from the row
            })
            .collect();
        rows.push(Row { docid, score, keys, source });
    }
    rows.sort_by(|a, b| cmp_rows(a, b, sort));
    let total = rows.len();
    // _score is null in the reply when the FIRST sort key is not
    // _score (ES field-sort behavior); max_score follows the same rule.
    let scored_reply = matches!(sort.first(), Some(SortKey::Score(_)));
    let max_score = if scored_reply && total > 0 {
        Some(rows.iter().map(|r| r.score).fold(f64::NEG_INFINITY, f64::max))
    } else {
        None
    };
    let mut hits = Vec::new();
    for row in rows.into_iter().skip(plan.from).take(plan.size) {
        let source = match row.source {
            Some(v) => filter_source(&v, &plan.source),
            None => filter_source(&load_source(&ctx, &row.docid).unwrap_or(Value::Null), &plan.source),
        };
        hits.push(ExecHit {
            docid: row.docid,
            score: if scored_reply { Some(row.score) } else { None },
            source,
        });
    }
    Ok(ExecResult { total, max_score, hits })
}

/// `_count` endpoint: the query tree's match count, docs unread, knn
/// ignored (v1 deviation).
pub fn count(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
    plan: &SearchPlan,
) -> Result<usize, String> {
    let mut ctx = new_ctx(store, prefix, index, meta);
    Ok(eval(&mut ctx, &plan.query)?.docs.len())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sort_is_score_desc() {
        let low = Row { docid: b"a".to_vec(), score: 1.0, keys: vec![], source: None };
        let high = Row { docid: b"z".to_vec(), score: 2.0, keys: vec![], source: None };
        assert_eq!(cmp_rows(&high, &low, &DEFAULT_SORT), Ordering::Less);
        // score ties fall through to docid asc
        let tie = Row { docid: b"b".to_vec(), score: 1.0, keys: vec![], source: None };
        let tie2 = Row { docid: b"a".to_vec(), score: 1.0, keys: vec![], source: None };
        assert_eq!(cmp_rows(&tie, &tie2, &DEFAULT_SORT), Ordering::Greater);
    }
}
