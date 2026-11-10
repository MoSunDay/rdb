//! FT.* command handlers, part 1: index lifecycle (CREATE/DROP),
//! document writes (ADD/DEL) and introspection (INFO). Query
//! execution lives in `ft_search`, the SPANN trainer (BUILD) in
//! `ft_build`. Same contract as every typed family: keys resolve via
//! `keys_core`, read-modify-write sequences hold the index-key
//! latch, mutations land in ONE batched fsync.

use rocksdb::WriteBatch;
use serde_json::Value;

use crate::command::hash_cmd::{arity, WRONGTYPE};
use crate::command::zset_util::eq_ignore_case;
use crate::command::{keys_core, Ctx};
use crate::ds::codec::KIND_SEARCH_META;
use crate::ds::{expire, latch};
use crate::resp::codec::{append_error, append_int, append_string};

use super::ft_index::{self, FieldType, IndexField, IndexMeta};
use super::index_codec::{DocRecord, TermFreq, NO_CENTROID};
use super::tokenize::tokenize;

/// What the index key currently is (the `vectorset_state` pattern).
pub(crate) enum IndexState {
    Missing,
    WrongType,
    Present(IndexMeta),
}

pub(crate) fn index_state(store: &crate::store::Store, prefix: &[u8], index: &[u8]) -> IndexState {
    match keys_core::resolve(store, prefix, index, expire::now_ms()) {
        keys_core::KeyState::Missing => IndexState::Missing,
        keys_core::KeyState::RawString { .. } => IndexState::WrongType,
        keys_core::KeyState::Enveloped { kind, .. } if kind != KIND_SEARCH_META => {
            IndexState::WrongType
        }
        keys_core::KeyState::Enveloped {
            expire_ms, payload, ..
        } => match ft_index::decode_index_meta(expire_ms, &payload) {
            Ok(meta) => IndexState::Present(meta),
            Err(_) => IndexState::Missing, // corrupt payload must not brick reads
        },
    }
}

/// `FT.CREATE <index> [STOPWORDS <n> <w...>] [K1 <f>] [B <f>]
/// SCHEMA <field> TEXT | KEYWORD | NUMERIC | <field> VECTOR DIM <n>
/// ...` (SCHEMA keyword required, options before it, case-insensitive,
/// repeatable with last-wins, at least one field, one VECTOR field
/// max at v1). STOPWORDS filters TEXT tokens at index AND query time
/// (positions compact after the drop, a documented deviation); K1/B
/// retune the BM25 scorer for this index.
pub async fn ft_create(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        arity(ctx.out, "ft.create");
        return;
    }
    let Some(schema_at) = (1..ctx.args.len()).find(|&i| eq_ignore_case(&ctx.args[i], b"SCHEMA"))
    else {
        arity(ctx.out, "ft.create");
        return;
    };
    let index = ctx.args[0].clone();
    let mut stopwords: Vec<Vec<u8>> = Vec::new();
    let mut k1 = super::bm25::K1;
    let mut b = super::bm25::B;
    let mut i = 1;
    while i < schema_at {
        let opt = &ctx.args[i];
        if eq_ignore_case(opt, b"STOPWORDS") {
            let Some(n) = ctx
                .args
                .get(i + 1)
                .and_then(|a| std::str::from_utf8(a).ok())
                .and_then(|t| t.parse::<u64>().ok())
            else {
                append_error(ctx.out, "ERR invalid STOPWORDS count");
                return;
            };
            if !(0..=10000).contains(&n) || i + 2 + n as usize > schema_at {
                append_error(ctx.out, "ERR invalid STOPWORDS count");
                return;
            }
            stopwords = ctx.args[i + 2..i + 2 + n as usize].to_vec();
            i += 2 + n as usize;
        } else if eq_ignore_case(opt, b"K1") || eq_ignore_case(opt, b"B") {
            let is_k1 = eq_ignore_case(opt, b"K1");
            let val = ctx
                .args
                .get(i + 1)
                .and_then(|a| std::str::from_utf8(a).ok())
                .and_then(|t| t.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0);
            match val {
                Some(v) if is_k1 => k1 = v,
                Some(v) => b = v,
                None => {
                    append_error(
                        ctx.out,
                        if is_k1 {
                            "ERR invalid K1"
                        } else {
                            "ERR invalid B"
                        },
                    );
                    return;
                }
            }
            i += 2;
        } else {
            append_error(ctx.out, "ERR unknown option");
            return;
        }
    }
    let mut fields: Vec<IndexField> = Vec::new();
    let mut i = schema_at + 1;
    while i < ctx.args.len() {
        let name = ctx.args[i].clone();
        i += 1;
        let Some(ty) = ctx.args.get(i) else {
            arity(ctx.out, "ft.create");
            return;
        };
        i += 1;
        let field = if eq_ignore_case(ty, b"TEXT") {
            IndexField {
                name,
                ftype: FieldType::Text,
            }
        } else if eq_ignore_case(ty, b"KEYWORD") {
            IndexField {
                name,
                ftype: FieldType::Keyword,
            }
        } else if eq_ignore_case(ty, b"NUMERIC") {
            IndexField {
                name,
                ftype: FieldType::Numeric,
            }
        } else if eq_ignore_case(ty, b"VECTOR") {
            let (Some(kw), Some(dim)) = (ctx.args.get(i), ctx.args.get(i + 1)) else {
                arity(ctx.out, "ft.create");
                return;
            };
            if !eq_ignore_case(kw, b"DIM") {
                append_error(ctx.out, "ERR expected DIM after VECTOR");
                return;
            }
            let Some(dim) = std::str::from_utf8(dim)
                .ok()
                .and_then(|t| t.parse::<u64>().ok())
            else {
                append_error(ctx.out, "ERR invalid dim");
                return;
            };
            if !(1..=4096).contains(&dim) {
                append_error(ctx.out, "ERR invalid dim");
                return;
            }
            i += 2;
            IndexField {
                name,
                ftype: FieldType::Vector { dim },
            }
        } else {
            append_error(
                ctx.out,
                "ERR unknown field type (use TEXT, KEYWORD, NUMERIC or VECTOR DIM n)",
            );
            return;
        };
        if fields.iter().any(|f| f.name == field.name) {
            append_error(ctx.out, "ERR duplicate field");
            return;
        }
        fields.push(field);
    }
    if fields
        .iter()
        .filter(|f| matches!(f.ftype, FieldType::Vector { .. }))
        .count()
        > 1
    {
        append_error(ctx.out, "ERR only one VECTOR field is supported");
        return;
    }
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &index),
    )
    .await;
    match index_state(&ctx.shared.store, &ctx.prefix_key, &index) {
        IndexState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
        IndexState::Present(_) => {
            append_error(ctx.out, "ERR index already exists");
            return;
        }
        IndexState::Missing => {}
    }
    let meta = IndexMeta {
        expire_ms: 0,
        num_docs: 0,
        sum_doclen: 0,
        fields,
        stopwords,
        k1,
        b,
    };
    let mut batch = WriteBatch::default();
    ft_index::put_meta(&mut batch, &ctx.prefix_key, &index, &meta);
    match ctx.commit(batch).await {
        Ok(()) => append_string(ctx.out, "OK"),
        Err(_) => append_error(ctx.out, "ERR: ft.create failed"),
    }
}

/// Stringify one JSON value for a TEXT field: strings verbatim,
/// numbers/bools via serde_json, everything else is a type error.
pub(crate) fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

type DocRecordWithNumFields = Result<
    (
        DocRecord,
        Vec<(Vec<u8>, f64)>,
        Vec<(Vec<u8>, Vec<u8>, Vec<u64>)>,
    ),
    &'static str,
>;

/// Build the DocRecord (terms + doclen + vector) from the JSON body,
/// the (field, value) pairs of NUMERIC fields for the kind 0x19
/// doc-value records, and the (field, term, positions) side-table for
/// TEXT fields (token ordinals, 0-based per field; tf == positions
/// count by construction). All three are written by
/// `build_add_batch`.
pub(crate) fn doc_record_of(meta: &IndexMeta, body: &[u8]) -> DocRecordWithNumFields {
    let json: Value = serde_json::from_slice(body).map_err(|_| "ERR invalid JSON document")?;
    let Value::Object(map) = json else {
        return Err("ERR document must be a JSON object");
    };
    let mut rec = DocRecord {
        doclen: 0,
        terms: Vec::new(),
        centroid: NO_CENTROID,
        vector: Vec::new(),
        doc: body.to_vec(),
    };
    let mut numvals = Vec::new();
    let mut positions: Vec<(Vec<u8>, Vec<u8>, Vec<u64>)> = Vec::new();
    for f in &meta.fields {
        let name = String::from_utf8_lossy(&f.name);
        let Some(v) = map.get(name.as_ref()) else {
            continue; // absent field: contributes nothing
        };
        match &f.ftype {
            FieldType::Text => {
                let text = text_of(v).ok_or("ERR text field must be a JSON scalar")?;
                let mut toks = tokenize(&text);
                // index-side stopword drop: positions and doclen stay
                // compact over the surviving tokens (RediSearch keeps
                // holes -- documented deviation, see COMPAT.md)
                toks.retain(|t| !meta.stopwords.iter().any(|s| s.as_slice() == t.as_bytes()));
                rec.doclen += toks.len() as u64;
                bump_positions(&mut positions, &f.name, &toks);
                bump_terms(&mut rec.terms, &f.name, toks);
            }
            FieldType::Keyword => {
                // exact bytes, NO tokenize/lowercase: one term per value
                let toks = match v {
                    Value::String(s) => vec![s.clone()],
                    Value::Number(n) => vec![n.to_string()],
                    Value::Bool(b) => vec![b.to_string()],
                    Value::Array(items) => {
                        let mut vals = Vec::with_capacity(items.len());
                        for it in items {
                            let s = match it {
                                Value::String(s) => s.clone(),
                                Value::Number(n) => n.to_string(),
                                Value::Bool(b) => b.to_string(),
                                _ => {
                                    return Err(
                                        "ERR keyword field must be a string or array of strings",
                                    );
                                }
                            };
                            vals.push(s);
                        }
                        vals
                    }
                    _ => return Err("ERR keyword field must be a string or array of strings"),
                };
                bump_terms(&mut rec.terms, &f.name, toks);
            }
            FieldType::Numeric => {
                let v = v
                    .as_f64()
                    .ok_or("ERR numeric field must be a JSON number")?;
                if !v.is_finite() {
                    return Err("ERR numeric field must be a JSON number");
                }
                numvals.push((f.name.clone(), v));
            }
            FieldType::Vector { dim } => {
                let Value::Array(items) = v else {
                    return Err("ERR vector field must be a JSON array");
                };
                let mut vector = Vec::with_capacity(items.len());
                for it in items {
                    let x = it.as_f64().ok_or("ERR vector values must be numbers")?;
                    vector.push(x);
                }
                if vector.len() != *dim as usize {
                    return Err("ERR vector dimension mismatch");
                }
                rec.vector = vector;
            }
        }
    }
    Ok((rec, numvals, positions))
}

/// Fold token ordinals into the (field, term, positions) side-table;
/// positions are strictly increasing by construction (enumerate
/// order), which is the v2 posting encode invariant.
fn bump_positions(
    positions: &mut Vec<(Vec<u8>, Vec<u8>, Vec<u64>)>,
    field: &[u8],
    toks: &[String],
) {
    for (i, t) in toks.iter().enumerate() {
        let bytes = t.as_bytes();
        match positions
            .iter_mut()
            .find(|(f, term, _)| f == field && term == bytes)
        {
            Some((_, _, pos)) => pos.push(i as u64),
            None => positions.push((field.to_vec(), bytes.to_vec(), vec![i as u64])),
        }
    }
}

/// Fold tokens into the (field, term, tf) list (order-preserving merge
/// of duplicate terms).
fn bump_terms(terms: &mut Vec<TermFreq>, field: &[u8], toks: Vec<String>) {
    for t in toks {
        let bytes = t.into_bytes();
        match terms
            .iter_mut()
            .find(|tf| tf.field == field && tf.term == bytes)
        {
            Some(tf) => tf.tf += 1,
            None => terms.push(TermFreq {
                field: field.to_vec(),
                term: bytes,
                tf: 1,
            }),
        }
    }
}

/// `FT.ADD <index> <docid> <json>` -> :1 (added or replaced, one fsync).
pub async fn ft_add(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "ft.add");
        return;
    }
    let (index, docid, body) = (
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        ctx.args[2].clone(),
    );
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &index),
    )
    .await;
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
    let (rec, numvals, positions) = match doc_record_of(&meta, &body) {
        Ok(r) => r,
        Err(e) => {
            append_error(ctx.out, e);
            return;
        }
    };
    match ft_index::build_add_batch(
        &ctx.shared.store,
        &ctx.prefix_key,
        &index,
        &meta,
        &docid,
        rec,
        &numvals,
        &positions,
    ) {
        Ok(batch) => match ctx.commit(batch).await {
            Ok(()) => append_int(ctx.out, 1),
            Err(_) => append_error(ctx.out, "ERR: ft.add failed"),
        },
        Err(e) => append_error(ctx.out, &e),
    }
}

/// `FT.DEL <index> <docid>` -> :1 when the doc existed.
pub async fn ft_del(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "ft.del");
        return;
    }
    let (index, docid) = (ctx.args[0].clone(), ctx.args[1].clone());
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &index),
    )
    .await;
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
    match ft_index::build_del_batch(&ctx.shared.store, &ctx.prefix_key, &index, &meta, &docid) {
        Ok(Some(batch)) => match ctx.commit(batch).await {
            Ok(()) => append_int(ctx.out, 1),
            Err(_) => append_error(ctx.out, "ERR: ft.del failed"),
        },
        Ok(None) => append_int(ctx.out, 0),
        Err(e) => append_error(ctx.out, &e),
    }
}

/// `FT.DROP <index>` (alias FT.DROPINDEX): wipes the whole search
/// family (docs, postings, termstats, centroids, partitions) + TTL
/// index entry in one fsync.
pub async fn ft_drop(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "ft.drop");
        return;
    }
    let index = ctx.args[0].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &index),
    )
    .await;
    match index_state(&ctx.shared.store, &ctx.prefix_key, &index) {
        // The match is the tail expression: arms need no `return`.
        IndexState::WrongType => append_error(ctx.out, WRONGTYPE),
        IndexState::Missing => append_error(ctx.out, "ERR unknown index"),
        IndexState::Present(meta) => {
            let mut batch = WriteBatch::default();
            ft_index::delete_family(&mut batch, &ctx.prefix_key, &index, meta.expire_ms);
            match ctx.commit(batch).await {
                Ok(()) => append_int(ctx.out, 1),
                Err(_) => append_error(ctx.out, "ERR: ft.drop failed"),
            }
        }
    }
}

/// `FT.INFO <index>` -> flat array (index_name, num_docs, sum_doclen,
/// avg_doclen, fields [...], ann (built/not) ...).
pub async fn ft_info(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "ft.info");
        return;
    }
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
    super::ft_search::reply_info(ctx.out, &ctx.shared.store, &ctx.prefix_key, &index, &meta);
}

pub use super::ft_build::ft_build;
