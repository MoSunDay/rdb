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

/// `FT.CREATE <index> SCHEMA <field> TEXT | <field> VECTOR DIM <n> ...`
/// (SCHEMA keyword required, options case-insensitive, at least one
/// field, one VECTOR field max at v1).
pub async fn ft_create(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 || !eq_ignore_case(&ctx.args[1], b"SCHEMA") {
        arity(ctx.out, "ft.create");
        return;
    }
    let index = ctx.args[0].clone();
    let mut fields: Vec<IndexField> = Vec::new();
    let mut i = 2;
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
            append_error(ctx.out, "ERR unknown field type (use TEXT or VECTOR DIM n)");
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
fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Build the DocRecord (terms + doclen + vector) from the JSON body.
fn doc_record_of(meta: &IndexMeta, body: &[u8]) -> Result<DocRecord, &'static str> {
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
    for f in &meta.fields {
        let name = String::from_utf8_lossy(&f.name);
        let Some(v) = map.get(name.as_ref()) else {
            continue; // absent field: contributes nothing
        };
        match &f.ftype {
            FieldType::Text => {
                let text = text_of(v).ok_or("ERR text field must be a JSON scalar")?;
                let toks = tokenize(&text);
                rec.doclen += toks.len() as u64;
                bump_terms(&mut rec.terms, &f.name, toks);
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
    Ok(rec)
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
    let rec = match doc_record_of(&meta, &body) {
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
