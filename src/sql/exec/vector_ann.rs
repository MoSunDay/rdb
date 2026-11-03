//! ANN probe for the D11 KNN shape over a SPANN-style vector index
//! (02 doc §4.3): coarse nprobe partitions ranked by SQ8 codes, then
//! the surviving pks are re-read as visible rows (`visible_row_at_pk`
//! adjudicates MVCC, prepared versions and tombstones) and handed to
//! the normal pipeline -- the exact DISTANCE sort then runs over the
//! candidates, so output ordering/semantics stay the executor's.
//!
//! The result is APPROXIMATE: a stale snapshot may miss rows whose
//! vector moved partitions after the snapshot (documented deviation,
//! same magnitude as ANN's inherent approximation). Everything the
//! probe declines (no index, metric mismatch, another metric spelling,
//! joins, non-constant query) falls back to the exact scan unchanged.

use std::cmp::Reverse;

use crate::sql::exec::func::Metric as ExecMetric;
use crate::sql::exec::scan::{self, Source};
use crate::sql::exec::vector_knn;
use crate::sql::index::vector::{self, Metric};
use crate::sql::index::vector_build::{self, to_domain};
use crate::sql::parse::ast::{Query, TableRef};
use crate::sql::storage::catalog;
use crate::state::Shared;

/// Candidate multiplier over the requested window (02 doc §4.3: the
/// shortlist is max(4k, k+16) SQ8-ranked rows before the exact pass).
const SHORTLIST: usize = 4;

pub(crate) fn ann_source(shared: &Shared, q: &Query, read_ts: u64) -> Option<Source> {
    let shape = vector_knn::match_knn(q)?;
    // The probe reuses the exact path's constant evaluation: query and
    // metric must be row-constant, and the column operand a bare
    // column of the single FROM table.
    let TableRef::Table { name, alias } = &q.from else {
        return None;
    };
    let col = match shape.col {
        crate::sql::parse::ast::Expr::Col { name, .. } => name,
        _ => return None,
    };
    let schema = catalog::lookup(shared, name).ok().flatten()?;
    let col_pos = schema.column_index(col)?;
    let def = schema
        .indexes
        .iter()
        .find(|d| d.vector.is_some() && schema.column_index(&d.column) == Some(col_pos))?;
    let params = def.vector.as_ref()?;
    // metric literal must match the index's metric (another spelling
    // or another metric = coarse space mismatch -> exact scan)
    let scope = crate::sql::exec::scan::FromScope {
        sides: vec![scan::table_side(&schema, alias)],
    };
    let metric_expr = crate::sql::exec::expr::eval(shape.metric, &scope, &[]).ok()?;
    let exec_metric = crate::sql::exec::func::metric_arg(&metric_expr).ok()??;
    if exec_metric != to_exec(params.metric) {
        return None;
    }
    let query_val = crate::sql::exec::expr::eval(shape.query, &scope, &[]).ok()?;
    let query_vec = crate::sql::exec::func::vector_arg("distance", &query_val).ok()?;
    let loaded = vector_build::load(&shared.store, schema.id, col_pos as u32).ok()??;
    if loaded.params.metric != params.metric {
        return None;
    }
    let take = crate::sql::parse::order_limit::limit_u64(shape.limit).ok()? as usize;
    let offset = crate::sql::parse::order_limit::limit_u64(&q.offset).ok()? as usize;
    let window = offset.saturating_add(take).max(1);
    let cap = (SHORTLIST * window).max(window + 16);

    // Coarse pass: rank partitions by L2 to the (domain-projected)
    // query, keep nprobe, SQ8-rank their entries.
    let domain_q = to_domain(&query_vec, params.metric);
    let dim = params.dim as usize;
    let mut cids: Vec<(usize, f64)> = loaded
        .centroids
        .iter()
        .enumerate()
        .map(|(i, c)| (i, crate::search::vecmath::l2(c, &domain_q)))
        .collect();
    cids.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut shortlist: Vec<(Reverse<f64>, Vec<u8>)> = Vec::with_capacity(cap);
    for (cid, _) in cids.into_iter().take(loaded.params.nprobe.max(1) as usize) {
        let blob = crate::store::ops::get_physical(
            &shared.store,
            &vector::posting_key(schema.id, col_pos as u32, cid as u32),
        )
        .ok()
        .flatten()?;
        let entries = vector::decode_postings(&blob, dim)?;
        for e in entries {
            let score = crate::search::quant::l2_dequant(&loaded.cal, &e.sq8, &domain_q);
            push_bounded(&mut shortlist, cap, (Reverse(score), e.pk));
        }
    }
    // Exact + visibility pass: rows_by_pks drops everything the
    // snapshot cannot see. Fewer candidates than requested is the
    // accepted approximate outcome (recall < 1).
    let pks: Vec<Vec<u8>> = shortlist.into_iter().map(|(_, pk)| pk).collect();
    let rows = scan::rows_by_pks(&shared.store, &schema, &pks, read_ts).ok()?;
    Some(Source { scope, rows })
}

fn to_exec(m: Metric) -> ExecMetric {
    match m {
        Metric::Cosine => ExecMetric::Cosine,
        Metric::Euclidean => ExecMetric::Euclidean,
        Metric::Dot => ExecMetric::Dot,
    }
}

/// Bounded min-heap insert: keep the `cap` smallest (Reverse makes the
/// BinaryHeap a min-heap on the score); ties drop the later pk.
fn push_bounded(
    heap: &mut Vec<(Reverse<f64>, Vec<u8>)>,
    cap: usize,
    item: (Reverse<f64>, Vec<u8>),
) {
    if heap.len() < cap {
        heap.push(item);
        heap.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
    } else if let Some(worst) = heap.last() {
        if item.0.partial_cmp(&worst.0) == Some(std::cmp::Ordering::Less) {
            heap.pop();
            heap.push(item);
            heap.sort_by(|a, b| {
                a.0.partial_cmp(&b.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.1.cmp(&b.1))
            });
        }
    }
}
