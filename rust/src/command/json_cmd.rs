//! JSON document commands, part 1 (JSON.SET/GET/DEL/FORGET/TYPE/MGET)
//! plus the plumbing every json handler shares (mirrors how `list_cmd`
//! hosts the commit used by `list_ops`/`list_move`): keys resolve via
//! `keys_core::resolve` (lazy expiry + wrong-type detection), the whole
//! document is the single KIND_JSON record (`ds::json_ds`), and every
//! mutation lands in ONE batched fsync under the per-key latch. String
//! and number edits live in `json_str`, array edits in `json_arr`,
//! object reads in `json_obj`.

use rocksdb::WriteBatch;
use serde_json::Value;

use crate::command::hash_cmd::{arity, WRONGTYPE};
use crate::command::json_path::{self, PathSeg};
use crate::command::zset_util::eq_ignore_case;
use crate::command::{keys_core, Ctx};
use crate::ds::codec::KIND_JSON;
use crate::ds::{expire, json_ds, latch, setops};
use crate::resp::codec::{
    append_array, append_bulk, append_error, append_int, append_null, append_string,
};

/// A path argument that is not legacy deterministic syntax.
pub(crate) const ERR_PATH_SYNTAX: &str = "ERR wrong static path";
/// Navigation reached nothing (missing key/path).
pub(crate) const ERR_PATH_MISSING: &str = "ERR path does not exist";
/// The value at the path is of the wrong JSON type for the operation.
pub(crate) const ERR_WRONG_PATH_TYPE: &str = "ERR wrong type of path value";
/// A value argument did not parse as JSON.
pub(crate) const ERR_INVALID_JSON: &str = "ERR invalid JSON";

/// What one key is from the json commands' point of view.
#[derive(Debug, PartialEq)]
pub(crate) enum JsonState {
    Missing,
    WrongType,
    Doc { expire_ms: u64, doc: Vec<u8> },
}

/// Resolve via keys_core (raw strings and foreign kinds -> WrongType);
/// an expired document purges and reads as Missing.
pub(crate) fn json_state(ctx: &Ctx<'_>, key: &[u8]) -> JsonState {
    match keys_core::resolve(&ctx.shared.store, &ctx.prefix_key, key, expire::now_ms()) {
        keys_core::KeyState::Missing => JsonState::Missing,
        keys_core::KeyState::RawString { .. } => JsonState::WrongType,
        keys_core::KeyState::Enveloped { kind, .. } if kind != KIND_JSON => JsonState::WrongType,
        keys_core::KeyState::Enveloped {
            expire_ms, payload, ..
        } => JsonState::Doc {
            expire_ms,
            doc: payload,
        },
    }
}

/// Decode stored bytes into the tree; corrupt payloads surface as the
/// invalid-JSON error (writers here only ever store serde_json output).
pub(crate) fn decode_doc(ctx: &mut Ctx<'_>, payload: &[u8]) -> Option<Value> {
    let doc = serde_json::from_slice(payload).ok();
    if doc.is_none() {
        append_error(ctx.out, ERR_INVALID_JSON);
    }
    doc
}

/// Compact serialization of a tree (serde_json shortest-roundtrip
/// numbers, insertion-ordered object keys).
pub(crate) fn doc_bytes(doc: &Value) -> Vec<u8> {
    serde_json::to_vec(doc).unwrap_or_default()
}

/// Rewrite the whole document record in ONE fsync, keeping the TTL
/// envelope (unchanged expiry only re-asserts the index entry).
pub(crate) async fn save_doc(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    expire_ms: u64,
    doc: &Value,
    cmd: &str,
) -> bool {
    let mut batch = WriteBatch::default();
    json_ds::write_doc(
        &mut batch,
        &ctx.prefix_key,
        key,
        expire_ms,
        expire_ms,
        &doc_bytes(doc),
    );
    ctx.commit(batch).await.map(|_| true).unwrap_or_else(|_| {
        append_error(ctx.out, &format!("ERR: {cmd} failed"));
        false
    })
}

/// JSON.SET key path value [NX|XX] -> +OK, nil bulk when the condition
/// fails. NX/XX are key-level (legacy RedisJSON); overwrites preserve
/// the TTL, new keys start with none.
pub async fn json_set(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 || ctx.args.len() > 4 {
        arity(ctx.out, "json.set");
        return;
    }
    let key = ctx.args[0].clone();
    let (nx, xx) = match ctx.args.len() {
        4 => {
            if eq_ignore_case(&ctx.args[3], b"NX") {
                (true, false)
            } else if eq_ignore_case(&ctx.args[3], b"XX") {
                (false, true)
            } else {
                append_error(ctx.out, "ERR syntax error");
                return;
            }
        }
        _ => (false, false),
    };
    let Some(segs) = json_path::parse_path(&ctx.args[1]) else {
        append_error(ctx.out, ERR_PATH_SYNTAX);
        return;
    };
    let Ok(new) = serde_json::from_slice::<Value>(&ctx.args[2]) else {
        append_error(ctx.out, ERR_INVALID_JSON);
        return;
    };
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let (expire_ms, mut doc) = match json_state(ctx, &key) {
        JsonState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
        JsonState::Missing if xx => {
            append_null(ctx.out); // XX: only when the key exists
            return;
        }
        JsonState::Missing => (0, Value::Null),
        JsonState::Doc { expire_ms, doc } => {
            if nx {
                append_null(ctx.out); // NX: only when the key is absent
                return;
            }
            let Some(doc) = decode_doc(ctx, &doc) else {
                return;
            };
            (expire_ms, doc)
        }
    };
    // A brand-new document starts as a Null root: non-root paths have no
    // object to descend into (RedisJSON v1 also refuses: new objects are
    // created at root only), so set_at answers WrongType.
    if let Some(text) = json_path::set_at(&mut doc, &segs, new)
        .err()
        .map(|e| match e {
            // RedisJSON embeds the offending path in this message.
            json_path::SetErr::NotFound => {
                format!("ERR path {} does not exist", json_path::path_display(&segs))
            }
            json_path::SetErr::WrongType => ERR_WRONG_PATH_TYPE.to_string(),
        })
    {
        append_error(ctx.out, &text);
        return;
    }
    if save_doc(ctx, &key, expire_ms, &doc, "json.set").await {
        append_string(ctx.out, "OK");
    }
}

/// JSON.GET key [path [path ...]]: no path = the root bytes exactly as
/// stored (byte-stable roundtrip); one path = bulk at that path (null
/// when missing); several paths = a flat RESP array with one entry per
/// path (documented deviation: Redis merges them into an object).
pub async fn json_get(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        arity(ctx.out, "json.get");
        return;
    }
    let key = ctx.args[0].clone();
    let mut paths: Vec<Vec<PathSeg>> = Vec::with_capacity(ctx.args.len() - 1);
    for arg in &ctx.args[1..] {
        match json_path::parse_path(arg) {
            Some(segs) => paths.push(segs),
            None => {
                append_error(ctx.out, ERR_PATH_SYNTAX);
                return;
            }
        }
    }
    match json_state(ctx, &key) {
        JsonState::WrongType => append_error(ctx.out, WRONGTYPE),
        JsonState::Missing => append_null(ctx.out),
        JsonState::Doc { doc, .. } => {
            if paths.is_empty() {
                append_bulk(ctx.out, &doc);
            } else if paths.len() == 1 {
                let Some(doc) = decode_doc(ctx, &doc) else {
                    return;
                };
                match json_path::get_at(&doc, &paths[0]) {
                    Some(v) => append_bulk(ctx.out, &doc_bytes(v)),
                    None => append_null(ctx.out),
                }
            } else {
                let Some(doc) = decode_doc(ctx, &doc) else {
                    return;
                };
                append_array(ctx.out, paths.len());
                for segs in &paths {
                    match json_path::get_at(&doc, segs) {
                        Some(v) => append_bulk(ctx.out, &doc_bytes(v)),
                        None => append_null(ctx.out),
                    }
                }
            }
        }
    }
}

/// JSON.DEL key [path] (JSON.FORGET is the same handler): no path / root
/// path deletes the whole key (family delete incl. TTL index) -> 1 when
/// it existed; otherwise the element at the path is removed from its
/// parent (array removal shifts) -> 0/1. A document that shrinks to null
/// STAYS (only a root delete removes the key).
pub async fn json_del(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 2 {
        arity(ctx.out, "json.del");
        return;
    }
    let key = ctx.args[0].clone();
    let segs = match ctx.args.get(1) {
        Some(arg) => match json_path::parse_path(arg) {
            Some(segs) => segs,
            None => {
                append_error(ctx.out, ERR_PATH_SYNTAX);
                return;
            }
        },
        None => Vec::new(),
    };
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    match json_state(ctx, &key) {
        JsonState::WrongType => append_error(ctx.out, WRONGTYPE),
        JsonState::Missing => append_int(ctx.out, 0),
        JsonState::Doc { expire_ms, doc } => {
            if segs.is_empty() {
                let mut batch = WriteBatch::default();
                json_ds::delete_family(&mut batch, &ctx.prefix_key, &key, expire_ms);
                if ctx.commit(batch).await.is_ok() {
                    append_int(ctx.out, 1);
                } else {
                    append_error(ctx.out, "ERR: json.del failed");
                }
                return;
            }
            let Some(mut doc) = decode_doc(ctx, &doc) else {
                return;
            };
            if !json_path::remove_at(&mut doc, &segs) {
                append_int(ctx.out, 0);
                return;
            }
            if save_doc(ctx, &key, expire_ms, &doc, "json.del").await {
                append_int(ctx.out, 1);
            }
        }
    }
}

/// JSON.FORGET: exact alias of JSON.DEL (same reply, different name).
pub async fn json_forget(ctx: &mut Ctx<'_>) {
    json_del(ctx).await;
}

/// JSON.TYPE key [path] -> simple string object|array|string|integer|
/// number|boolean|null; missing key/path -> null bulk. Integers are
/// i64/u64 numbers, everything else with a decimal point is "number".
pub async fn json_type(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 2 {
        arity(ctx.out, "json.type");
        return;
    }
    let segs = match ctx.args.get(1) {
        Some(arg) => match json_path::parse_path(arg) {
            Some(segs) => segs,
            None => {
                append_error(ctx.out, ERR_PATH_SYNTAX);
                return;
            }
        },
        None => Vec::new(),
    };
    match json_state(ctx, &ctx.args[0]) {
        JsonState::WrongType => append_error(ctx.out, WRONGTYPE),
        JsonState::Missing => append_null(ctx.out),
        JsonState::Doc { doc, .. } => {
            let Some(doc) = decode_doc(ctx, &doc) else {
                return;
            };
            match json_path::get_at(&doc, &segs) {
                Some(v) => append_string(ctx.out, type_name(v)),
                None => append_null(ctx.out),
            }
        }
    }
}

/// RedisJSON v1 type name of one JSON value.
pub(crate) fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// JSON.MGET key [key ...] path: every key must hash to the same slot
/// (CROSSSLOT otherwise); reply is a flat array of one bulk per key (the
/// value at the path, null bulk for missing key/path). A wrong-typed key
/// fails the WHOLE command with WRONGTYPE (documented deviation).
pub async fn json_mget(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "json.mget");
        return;
    }
    let (keys, path_arg) = ctx.args.split_at(ctx.args.len() - 1);
    let Some(segs) = json_path::parse_path(&path_arg[0]) else {
        append_error(ctx.out, ERR_PATH_SYNTAX);
        return;
    };
    if !setops::same_slot(keys) {
        append_error(ctx.out, setops::CROSSSLOT_ERROR);
        return;
    }
    let now = expire::now_ms();
    let mut items: Vec<Option<Vec<u8>>> = Vec::with_capacity(keys.len());
    for key in keys {
        match keys_core::resolve(&ctx.shared.store, &ctx.prefix_key, key, now) {
            keys_core::KeyState::Missing => items.push(None),
            keys_core::KeyState::Enveloped { kind, payload, .. } if kind == KIND_JSON => {
                let Ok(doc) = serde_json::from_slice::<Value>(&payload) else {
                    append_error(ctx.out, ERR_INVALID_JSON);
                    return;
                };
                items.push(json_path::get_at(&doc, &segs).map(doc_bytes));
            }
            _ => {
                append_error(ctx.out, WRONGTYPE);
                return;
            }
        }
    }
    append_array(ctx.out, items.len());
    for item in &items {
        match item {
            Some(bytes) => append_bulk(ctx.out, bytes),
            None => append_null(ctx.out),
        }
    }
}
