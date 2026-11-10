//! FT.SEARCH execution + the FT.INFO reply. Query dialect (v2, all
//! node-local, parsed by `query_tree`): `*` matches every doc;
//! otherwise whitespace-separated clauses -- implicit AND, optional
//! `@field:term` scopes, `"quoted phrases"` (adjacent positions),
//! trailing-`*` prefix scans, `|`/OR union, `-`/NOT complement. Terms
//! run through the SAME tokenizer as indexing (a CJK query tokenizes
//! into its bigrams, so substring recall lines up with the postings).
//! NOTE: whitespace clauses are now a STRICT AND (the v1 code unioned
//! multi-term queries despite its docs; aligned with the documented
//! contract and the KNN prefilter).
//! Options: `LIMIT o n`, `WITHSCORES`, `NOCONTENT`, `SORTBY <field>
//! [ASC|DESC]` (NUMERIC doc-value sort: full match set ordered by
//! (value, docid), MISSING values last in BOTH directions, total =
//! the FULL match count), `RETURN <n> <f...>` (project the reply's
//! doc JSON to those keys, in list order; missing keys skip, `0`
//! gives `{}`), `HIGHLIGHT [FIELDS <n> <f...>] [TAGS <open>
//! <close>]` (wrap the reply docs' matched query terms in tags;
//! default fields = every TEXT field, default tags `<b>`/`</b>`,
//! incompatible with NOCONTENT), and vector search
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

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::command::hash_cmd::{arity, WRONGTYPE};
use crate::command::Ctx;
use crate::resp::codec::{append_array, append_bulk, append_bulk_string, append_error, append_int};
use crate::store::{ops, Store};

use super::ann;
use super::bm25::{Hit, TopK};
use super::eval_tree::{eval_node, new_tree_ctx, scan_numvals, TreeHits};
use super::ft_cmd::{index_state, IndexState};
use super::ft_index::{read_doc, text_fields, FieldType, IndexMeta};
use super::ft_query::{knn_vector, parse_opts, KnnOpts, SearchOpts};
use super::highlight::{highlight_doc, highlight_terms};
use super::index_codec::doc_range;
use super::query_tree::QueryNode;
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
    // SORTBY 只支持 NUMERIC 字段（列存 doc-value 排序），schema 校验
    if let Some((f, _)) = &opts.sortby {
        let numeric = meta
            .fields
            .iter()
            .any(|fl| &fl.name == f && matches!(fl.ftype, FieldType::Numeric));
        if !numeric {
            append_error(ctx.out, "ERR SORTBY field is not NUMERIC");
            return;
        }
    }
    // HIGHLIGHT：FIELDS 未给定 -> 全部 TEXT 字段；词 = 正向查询词
    // （match-all 的 `*` 无查询树 -> 空词表 -> 内容原样）
    let hl_terms = terms
        .as_ref()
        .map(|n| highlight_terms(n, &meta))
        .unwrap_or_default();
    let hl_fields = opts
        .highlight
        .as_ref()
        .map(|h| h.fields.clone().unwrap_or_else(|| text_fields(&meta)));
    // (total, page): total = 全匹配数（SORTBY 路径），否则为页大小
    let result = match (&opts.knn, &terms) {
        (Some(knn), None) => run_knn(store, prefix, &index, &meta, knn, None).map(|h| (h.len(), h)),
        (Some(knn), Some(terms)) => {
            matched_docids(store, prefix, &index, &meta, terms).and_then(|filter| {
                run_knn(store, prefix, &index, &meta, knn, Some(filter)).map(|h| (h.len(), h))
            })
        }
        (None, None) => Ok(scan_all(store, prefix, &index, &opts)),
        (None, Some(terms)) => text_search(store, prefix, &index, &meta, terms, &opts),
    };
    let (total, hits) = match result {
        Ok(h) => h,
        Err(e) => {
            append_error(ctx.out, &e);
            return;
        }
    };
    // One flat ARRAY reply: integer header (total), then per hit the
    // docid, [score], [content]. Without the array header every
    // element becomes a standalone RESP value and clients (redis-cli,
    // iredis, python) stop after the total -- regression-tested in
    // tests/search/search_e2e.rs (ft_search_reply_is_a_flat_array).
    let per_hit = 1 + opts.with_scores as usize + (!opts.no_content) as usize;
    append_array(ctx.out, 1 + hits.len() * per_hit);
    append_int(ctx.out, total as i64);
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
            // RETURN 投影：按列表顺序保留存在的键
            let content = match &opts.ret {
                Some(list) => project_return(&content, list),
                None => content,
            };
            // HIGHLIGHT：投影之后的命中词包裹（KNN/过滤路径共用本
            // 循环，同样生效；无命中词则原样）
            let content = match (&hl_fields, opts.highlight.as_ref()) {
                (Some(fields), Some(h)) if !hl_terms.is_empty() => {
                    highlight_doc(&content, fields, &hl_terms, &h.open, &h.close)
                }
                _ => content,
            };
            append_bulk(ctx.out, &content);
        }
    }
}

/// Tree search (strict AND of clauses; phrase/prefix/bool leaves per
/// `eval_tree`); TopK bounded by offset+count (the node-local page
/// contract), sliced to the requested page. SORTBY replaces TopK
/// with the full-match doc-value sort (needs the whole set before
/// the window). Returns (total, page).
fn text_search(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
    node: &QueryNode,
    opts: &SearchOpts,
) -> Result<(usize, Vec<Hit>), String> {
    let mut ctx = new_tree_ctx(store, prefix, index, meta);
    let hits = eval_node(&mut ctx, node)?;
    if let Some((field, desc)) = &opts.sortby {
        let col = scan_numvals(store, prefix, index, field, &|_| true);
        return Ok(sort_by_numval(hits, &col, *desc, opts));
    }
    let mut top = TopK::new(opts.offset + opts.count);
    for (docid, score) in &hits {
        top.push(docid, *score);
    }
    let page: Vec<Hit> = top
        .finish()
        .into_iter()
        .skip(opts.offset)
        .take(opts.count)
        .collect();
    Ok((page.len(), page))
}

/// Docids matching the whole tree (KNN prefilter; empty set = no
/// matches). Errors PROPAGATE -- a store/decode failure used to
/// collapse to an empty set, turning a bad posting into a silent
/// 0-hit KNN answer.
fn matched_docids(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
    node: &QueryNode,
) -> Result<HashSet<Vec<u8>>, String> {
    let mut ctx = new_tree_ctx(store, prefix, index, meta);
    Ok(eval_node(&mut ctx, node)?.into_keys().collect())
}

/// `*` without KNN: docids in physical order, page applied, score 1.
/// SORTBY switches to the full-match doc-value sort. Returns
/// (total, page).
fn scan_all(store: &Store, prefix: &[u8], index: &[u8], opts: &SearchOpts) -> (usize, Vec<Hit>) {
    let (lower, upper) = doc_range(prefix, index);
    let mut docids = Vec::new();
    let _ = ops::for_each_from(store, &lower, false, &mut |k, _| {
        if !upper.is_empty() && k >= upper.as_slice() {
            return false;
        }
        docids.push(k.to_vec());
        true
    });
    let docid_of = |k: &[u8]| ann::docid_of_key(k, prefix.len()).unwrap_or_default();
    if let Some((field, desc)) = &opts.sortby {
        let hits: TreeHits = docids.iter().map(|k| (docid_of(k), 1.0)).collect();
        let col = scan_numvals(store, prefix, index, field, &|_| true);
        return sort_by_numval(hits, &col, *desc, opts);
    }
    let mut out = Vec::new();
    for key in docids.into_iter().skip(opts.offset).take(opts.count) {
        out.push(Hit {
            docid: docid_of(&key),
            score: 1.0,
        });
    }
    (out.len(), out)
}

/// SORTBY 排序共用路径：全量 (docid, score) 命中按数值列排序后
/// 分页。缺失值两个方向都排最后（先按 missing 分桶再桶内比较，
/// 与 es/source.rs cmp_sortval 语义一致），同值 docid 升序，DESC
/// 只反转数值序。返回 (全匹配数, 分页结果)。
fn sort_by_numval(
    hits: TreeHits,
    col: &HashMap<Vec<u8>, f64>,
    desc: bool,
    opts: &SearchOpts,
) -> (usize, Vec<Hit>) {
    let total = hits.len();
    let mut keyed: Vec<(Option<f64>, Vec<u8>, f64)> = hits
        .into_iter()
        .map(|(docid, score)| (col.get(&docid).copied(), docid, score))
        .collect();
    keyed.sort_by(|a, b| match (&a.0, &b.0) {
        (None, None) => a.1.cmp(&b.1),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let ord = x.total_cmp(y);
            (if desc { ord.reverse() } else { ord }).then_with(|| a.1.cmp(&b.1))
        }
    });
    let page = keyed
        .into_iter()
        .skip(opts.offset)
        .take(opts.count)
        .map(|(_, docid, score)| Hit { docid, score })
        .collect();
    (total, page)
}

/// RETURN 投影：文档 JSON 只保留列表中存在键（顺序跟随列表），
/// 缺键跳过；列表为空或文档非 object（含解析失败）退化为 `{}`。
fn project_return(doc: &[u8], list: &[Vec<u8>]) -> Vec<u8> {
    let kept = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(doc)
        .ok()
        .map(|obj| {
            let mut kept = serde_json::Map::new();
            for f in list {
                let key = String::from_utf8_lossy(f);
                if let Some(v) = obj.get(key.as_ref()) {
                    kept.insert(key.into_owned(), v.clone());
                }
            }
            kept
        })
        .unwrap_or_default();
    serde_json::to_vec(&serde_json::Value::Object(kept)).unwrap_or_else(|_| b"{}".to_vec())
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
        FieldType::Text | FieldType::Keyword | FieldType::Numeric => None,
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
/// sum_doclen, avg_doclen, fields [...], ann_built, ann_centroids,
/// stopwords [...], k1, b).
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
    append_array(out, 20);
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
            FieldType::Keyword => {
                append_bulk_string(out, "KEYWORD");
                append_int(out, 0);
            }
            FieldType::Numeric => {
                append_bulk_string(out, "NUMERIC");
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
    append_bulk_string(out, "stopwords");
    append_array(out, meta.stopwords.len());
    for s in &meta.stopwords {
        append_bulk(out, s);
    }
    append_bulk_string(out, "k1");
    append_bulk(out, format!("{}", meta.k1).as_bytes());
    append_bulk_string(out, "b");
    append_bulk(out, format!("{}", meta.b).as_bytes());
}

/// (built?, centroid count) -- reads the centroid record when the
/// schema has a VECTOR field; informational only.
fn ann_built(store: &Store, prefix: &[u8], index: &[u8], meta: &IndexMeta) -> (i64, i64) {
    let Some((field, _)) = meta.fields.iter().find_map(|f| match &f.ftype {
        FieldType::Vector { dim } => Some((f.name.clone(), dim)),
        FieldType::Text | FieldType::Keyword | FieldType::Numeric => None,
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
