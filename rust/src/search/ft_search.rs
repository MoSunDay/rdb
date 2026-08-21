//! FT.SEARCH execution + the FT.INFO reply. Query dialect (v1, all
//! node-local): `*` matches every doc; otherwise whitespace-separated
//! AND terms, each optionally field-scoped `@field:term`. Terms run
//! through the SAME tokenizer as indexing (a CJK query tokenizes into
//! its bigrams, so substring recall lines up with the postings).
//! Options: `LIMIT o n`, `WITHSCORES`, `NOCONTENT`, and vector search
//! `KNN <k> @<field>` followed by `FP16 <blob>` or a `VALUES <f64...>`
//! tail (VSIM's convention: VALUES swallows the remaining args). A
//! text query combined with KNN pre-filters the candidate set, then
//! exact L2 brute-forces it (exact semantics for the filtered set);
//! KNN with `*` goes through the SPANN index (`ann::knn`).
//!
//! Reply: `:<total> docid [score] [content] ...` -- a FLAT array with
//! an integer header (node-local page size; RediSearch's nested
//! layout is a documented COMPAT deviation). KNN scores are
//! `1/(1+L2)` so every mode sorts descending.

use std::collections::{HashMap, HashSet};

use crate::command::hash_cmd::{arity, WRONGTYPE};
use crate::command::Ctx;
use crate::resp::codec::{append_array, append_bulk, append_bulk_string, append_error, append_int};
use crate::store::{ops, Store};

use super::ann;
use super::bm25::{term_score, Hit, TopK};
use super::ft_cmd::{index_state, IndexState};
use super::ft_index::{read_doc, read_posting, read_termstat, FieldType, IndexMeta};
use super::ft_query::{knn_vector, parse_opts, KnnOpts, QueryTerm, SearchOpts};
use super::index_codec::doc_range;
use super::vecmath;

pub async fn ft_search(ctx: &mut Ctx<'_>) {
    let (terms, opts) = match parse_opts(ctx) {
        Ok(parsed) => parsed,
        Err(e) => {
            if e.ends_with("for 'ft.search' command") {
                arity(ctx.out, "ft.search");
            } else {
                append_error(ctx.out, e);
            }
            return;
        }
    };
    let index = ctx.args[0].clone();
    let meta = match index_state(&ctx.shared.store, &ctx.prefix_key, &index) {
        IndexState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
        IndexState::Missing => {
            append_error(ctx.out, "ERR unknown index");
            return;
        }
        IndexState::Present(m) => m,
    };
    let store = &ctx.shared.store;
    let prefix = &ctx.prefix_key;
    let result = match (&opts.knn, &terms) {
        (Some(knn), None) => run_knn(store, prefix, &index, &meta, knn, None),
        (Some(knn), Some(terms)) => {
            let filter = matched_docids(store, prefix, &index, &meta, terms);
            run_knn(store, prefix, &index, &meta, knn, Some(filter))
        }
        (None, None) => Ok(scan_all(store, prefix, &index, &opts)),
        (None, Some(terms)) => text_search(store, prefix, &index, &meta, terms, &opts),
    };
    let hits = match result {
        Ok(h) => h,
        Err(e) => {
            append_error(ctx.out, &e);
            return;
        }
    };
    append_int(ctx.out, hits.len() as i64);
    for h in &hits {
        append_bulk(ctx.out, &h.docid);
        if opts.with_scores {
            append_bulk_string(ctx.out, &format!("{:.4}", h.score));
        }
        if !opts.no_content {
            let content = read_doc(store, prefix, &index, &h.docid)
                .ok()
                .flatten()
                .map(|r| r.doc)
                .unwrap_or_default();
            append_bulk(ctx.out, &content);
        }
    }
}

/// Text fields a bare term searches (all TEXT fields of the schema).
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

/// BM25 text search over AND-ed terms; TopK bounded by offset+count
/// (the node-local page contract), sliced to the requested page.
fn text_search(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
    terms: &[QueryTerm],
    opts: &SearchOpts,
) -> Result<Vec<Hit>, String> {
    let n = meta.num_docs.max(1);
    let avgdl = meta.sum_doclen as f64 / n as f64;
    let mut doclens: HashMap<Vec<u8>, u64> = HashMap::new();
    let mut scores: HashMap<Vec<u8>, f64> = HashMap::new();
    let mut doclen_of = |docid: &[u8]| -> u64 {
        *doclens.entry(docid.to_vec()).or_insert_with(|| {
            read_doc(store, prefix, index, docid)
                .ok()
                .flatten()
                .map_or(1, |r| r.doclen.max(1))
        })
    };
    for qt in terms {
        for field in scope_fields(meta, &qt.field) {
            for term in &qt.terms {
                let stat = read_termstat(store, prefix, index, field, term)?;
                if stat.df == 0 {
                    continue;
                }
                for e in read_posting(store, prefix, index, field, term)? {
                    let doclen = doclen_of(&e.docid);
                    let s = term_score(e.tf, stat.df, doclen, avgdl, n);
                    *scores.entry(e.docid).or_insert(0.0) += s;
                }
            }
        }
    }
    let mut top = TopK::new(opts.offset + opts.count);
    for (docid, score) in &scores {
        top.push(docid, *score);
    }
    Ok(top
        .finish()
        .into_iter()
        .skip(opts.offset)
        .take(opts.count)
        .collect())
}

/// Docids matching ALL terms (KNN prefilter; empty set = no matches).
fn matched_docids(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
    terms: &[QueryTerm],
) -> HashSet<Vec<u8>> {
    let mut acc: Option<HashSet<Vec<u8>>> = None;
    for qt in terms {
        for field in scope_fields(meta, &qt.field) {
            for term in &qt.terms {
                let docs: HashSet<Vec<u8>> = read_posting(store, prefix, index, field, term)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| e.docid)
                    .collect();
                acc = Some(match acc {
                    Some(a) => a.intersection(&docs).cloned().collect(),
                    None => docs,
                });
            }
        }
    }
    acc.unwrap_or_default()
}

/// `*` without KNN: docids in physical order, page applied, score 1.
fn scan_all(store: &Store, prefix: &[u8], index: &[u8], opts: &SearchOpts) -> Vec<Hit> {
    let (lower, upper) = doc_range(prefix, index);
    let mut docids = Vec::new();
    let _ = ops::for_each_from(store, &lower, false, &mut |k, _| {
        if !upper.is_empty() && k >= upper.as_slice() {
            return false;
        }
        docids.push(k.to_vec());
        true
    });
    let mut out = Vec::new();
    for key in docids.into_iter().skip(opts.offset).take(opts.count) {
        out.push(Hit {
            docid: ann::docid_of_key(&key, prefix.len()).unwrap_or_default(),
            score: 1.0,
        });
    }
    out
}

/// KNN path: SPANN probe+rerank via `ann::knn`, or an exact scan over
/// a text-filtered candidate set. Score = 1/(1+L2).
fn run_knn(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
    knn: &KnnOpts,
    filter: Option<HashSet<Vec<u8>>>,
) -> Result<Vec<Hit>, String> {
    let Some((vfield, dim)) = meta.fields.iter().find_map(|f| match &f.ftype {
        FieldType::Vector { dim } => Some((f.name.clone(), dim)),
        FieldType::Text => None,
    }) else {
        return Err("ERR index has no VECTOR field".to_string());
    };
    if knn.field != vfield {
        return Err("ERR unknown vector field".to_string());
    }
    let query = knn_vector(knn, *dim).map_err(String::from)?;
    let ranked = match filter {
        Some(candidates) => {
            let mut top = TopK::new(knn.k);
            for docid in &candidates {
                if let Ok(Some(rec)) = read_doc(store, prefix, index, docid) {
                    if rec.vector.len() == *dim as usize {
                        top.push(docid, 1.0 / (1.0 + vecmath::l2(&rec.vector, &query)));
                    }
                }
            }
            top.finish()
        }
        None => ann::knn(
            store, prefix, index, &vfield, *dim, &query, knn.k, knn.nprobe,
        )?
        .into_iter()
        .map(|(docid, l2)| Hit {
            docid,
            score: 1.0 / (1.0 + l2),
        })
        .collect(),
    };
    Ok(ranked)
}

/// FT.INFO reply: flat key/value array (index_name, num_docs,
/// sum_doclen, avg_doclen, fields [...], ann_built, ann_centroids).
pub(super) fn reply_info(
    out: &mut Vec<u8>,
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
) {
    let avg = if meta.num_docs > 0 {
        format!("{:.4}", meta.sum_doclen as f64 / meta.num_docs as f64)
    } else {
        "0".to_string()
    };
    append_array(out, 14);
    append_bulk_string(out, "index_name");
    append_bulk(out, index);
    append_bulk_string(out, "num_docs");
    append_int(out, meta.num_docs as i64);
    append_bulk_string(out, "sum_doclen");
    append_int(out, meta.sum_doclen as i64);
    append_bulk_string(out, "avg_doclen");
    append_bulk_string(out, &avg);
    append_bulk_string(out, "fields");
    append_array(out, meta.fields.len());
    for f in &meta.fields {
        append_array(out, 3);
        append_bulk(out, &f.name);
        match &f.ftype {
            FieldType::Text => {
                append_bulk_string(out, "TEXT");
                append_int(out, 0);
            }
            FieldType::Vector { dim } => {
                append_bulk_string(out, "VECTOR");
                append_int(out, *dim as i64);
            }
        }
    }
    let (built, centroids) = ann_built(store, prefix, index, meta);
    append_bulk_string(out, "ann_built");
    append_int(out, built);
    append_bulk_string(out, "ann_centroids");
    append_int(out, centroids);
}

/// (built?, centroid count) -- reads the centroid record when the
/// schema has a VECTOR field; informational only.
fn ann_built(store: &Store, prefix: &[u8], index: &[u8], meta: &IndexMeta) -> (i64, i64) {
    let Some((field, _)) = meta.fields.iter().find_map(|f| match &f.ftype {
        FieldType::Vector { dim } => Some((f.name.clone(), dim)),
        FieldType::Text => None,
    }) else {
        return (0, 0);
    };
    match ops::get_physical(
        store,
        &super::index_codec::centroid_key(prefix, index, &field),
    ) {
        Ok(Some(raw)) => match super::index_codec::decode_centroids(&raw) {
            Some(t) => (1, t.centroids.len() as i64),
            None => (0, 0),
        },
        _ => (0, 0),
    }
}
