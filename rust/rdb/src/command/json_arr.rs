//! JSON array edits (JSON.ARRAPPEND/ARRPOP/ARRINDEX/ARRINSERT/ARRLEN/
//! ARRTRIM): the shared load-mutate-persist flow from `json_cmd` plus
//! Python-style negative index resolution. Indexes address from 0;
//! negative counts from the end; every mutation rewrites the single
//! KIND_JSON record in one fsync under the per-key latch (reads take
//! none). ARRINDEX/ARRTRIM range semantics are documented deviations
//! (see COMPAT.md): stop is EXCLUSIVE in ARRINDEX (-1 = through end)
//! and INCLUSIVE in ARRTRIM (-1 = last element).

use serde_json::Value;

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::json_cmd::{
    decode_doc, doc_bytes, json_state, save_doc, JsonState, ERR_INVALID_JSON, ERR_PATH_MISSING,
    ERR_PATH_SYNTAX, ERR_WRONG_PATH_TYPE,
};
use crate::command::json_path;
use crate::command::{keys_core, Ctx};
use crate::ds::latch;
use crate::resp::codec::{append_bulk, append_error, append_int, append_null};

/// An index argument fell outside the addressable window.
const ERR_INDEX: &str = "ERR index out of range";
/// An index argument did not parse as an integer at all.
const ERR_NOT_INT: &str = "ERR value is not an integer or out of range";

/// Resolve a (possibly negative) index against `len`; `None` outside
/// `[0, len)` (negative indexes count from the end, -1 = last).
fn resolve_index(idx: i64, len: usize) -> Option<usize> {
    let idx = if idx < 0 { idx + len as i64 } else { idx };
    (0..len as i64).contains(&idx).then_some(idx as usize)
}

/// One optional index argument (`default` when absent).
fn index_arg(arg: Option<&[u8]>, default: i64) -> Option<i64> {
    match arg {
        None => Some(default),
        Some(a) => parse_i64(a),
    }
}

/// JSON.ARRAPPEND key path value [value ...] -> new array length. Every
/// value must parse as JSON; the path must exist and hold an array.
pub async fn json_arrappend(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        arity(ctx.out, "json.arrappend");
        return;
    }
    let key = ctx.args[0].clone();
    let Some(segs) = json_path::parse_path(&ctx.args[1]) else {
        append_error(ctx.out, ERR_PATH_SYNTAX);
        return;
    };
    let mut add: Vec<Value> = Vec::with_capacity(ctx.args.len() - 2);
    for arg in &ctx.args[2..] {
        match serde_json::from_slice(arg) {
            Ok(v) => add.push(v),
            Err(_) => {
                append_error(ctx.out, ERR_INVALID_JSON);
                return;
            }
        }
    }
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    );
    match json_state(ctx, &key) {
        JsonState::WrongType => append_error(ctx.out, WRONGTYPE),
        JsonState::Missing => append_error(ctx.out, ERR_PATH_MISSING),
        JsonState::Doc { expire_ms, doc } => {
            let Some(mut doc) = decode_doc(ctx, &doc) else {
                return;
            };
            let Some(slot) = json_path::get_at_mut(&mut doc, &segs) else {
                append_error(ctx.out, ERR_PATH_MISSING);
                return;
            };
            let Value::Array(arr) = slot else {
                append_error(ctx.out, ERR_WRONG_PATH_TYPE);
                return;
            };
            arr.append(&mut add);
            let new_len = arr.len();
            if save_doc(ctx, &key, expire_ms, &doc, "json.arrappend").await {
                append_int(ctx.out, new_len as i64);
            }
        }
    }
}

/// JSON.ARRPOP key [path [index]] -> bulk of the popped element. Default
/// path is the root, default index -1 (last); negative counts from the
/// end; out of range is an ERROR (documented deviation, Redis answers
/// null); popping an empty array answers null.
pub async fn json_arrpop(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 3 {
        arity(ctx.out, "json.arrpop");
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
    let Some(idx) = index_arg(ctx.args.get(2).map(|v| v.as_slice()), -1) else {
        append_error(ctx.out, ERR_NOT_INT);
        return;
    };
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    );
    match json_state(ctx, &key) {
        JsonState::WrongType => append_error(ctx.out, WRONGTYPE),
        JsonState::Missing => append_null(ctx.out),
        JsonState::Doc { expire_ms, doc } => {
            let Some(mut doc) = decode_doc(ctx, &doc) else {
                return;
            };
            let Some(slot) = json_path::get_at_mut(&mut doc, &segs) else {
                append_null(ctx.out);
                return;
            };
            let Value::Array(arr) = slot else {
                append_error(ctx.out, ERR_WRONG_PATH_TYPE);
                return;
            };
            if arr.is_empty() {
                append_null(ctx.out); // nothing to pop in any position
                return;
            }
            let Some(pos) = resolve_index(idx, arr.len()) else {
                append_error(ctx.out, ERR_INDEX);
                return;
            };
            let popped = doc_bytes(&arr.remove(pos));
            if save_doc(ctx, &key, expire_ms, &doc, "json.arrpop").await {
                append_bulk(ctx.out, &popped);
            }
        }
    }
}

/// JSON.ARRINDEX key path value [start [stop]] -> first position of an
/// exact serde_json-equal element inside `[start, stop)` (stop is
/// EXCLUSIVE, -1 = through end; negative start counts from the end and
/// clamps to 0), or -1 when absent / key missing / path missing.
pub async fn json_arrindex(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 || ctx.args.len() > 5 {
        arity(ctx.out, "json.arrindex");
        return;
    }
    let key = ctx.args[0].clone();
    let Some(segs) = json_path::parse_path(&ctx.args[1]) else {
        append_error(ctx.out, ERR_PATH_SYNTAX);
        return;
    };
    let Ok(want) = serde_json::from_slice::<Value>(&ctx.args[2]) else {
        append_error(ctx.out, ERR_INVALID_JSON);
        return;
    };
    let Some(start) = index_arg(ctx.args.get(3).map(|v| v.as_slice()), 0) else {
        append_error(ctx.out, ERR_NOT_INT);
        return;
    };
    let Some(stop) = index_arg(ctx.args.get(4).map(|v| v.as_slice()), -1) else {
        append_error(ctx.out, ERR_NOT_INT);
        return;
    };
    match json_state(ctx, &key) {
        JsonState::WrongType => append_error(ctx.out, WRONGTYPE),
        JsonState::Missing => append_int(ctx.out, -1),
        JsonState::Doc { doc, .. } => {
            let Some(doc) = decode_doc(ctx, &doc) else {
                return;
            };
            let Some(target) = json_path::get_at(&doc, &segs) else {
                append_int(ctx.out, -1);
                return;
            };
            let Value::Array(items) = target else {
                append_error(ctx.out, ERR_WRONG_PATH_TYPE);
                return;
            };
            let len = items.len() as i64;
            let start = if start < 0 {
                (len + start).max(0)
            } else {
                start
            };
            // -1 is special-cased BEFORE negation: "through the end".
            let stop = if stop == -1 {
                len
            } else if stop < 0 {
                (len + stop).max(0)
            } else {
                stop.min(len)
            };
            let mut found = -1i64;
            if start < stop {
                for (i, item) in items.iter().enumerate().skip(start as usize) {
                    if i as i64 >= stop {
                        break;
                    }
                    if *item == want {
                        found = i as i64;
                        break;
                    }
                }
            }
            append_int(ctx.out, found);
        }
    }
}

/// JSON.ARRINSERT key path index value [value ...] -> new array length.
/// Negative index counts from the end; `0..=len` is valid (len appends);
/// everything else is out of range.
pub async fn json_arrinsert(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 4 {
        arity(ctx.out, "json.arrinsert");
        return;
    }
    let key = ctx.args[0].clone();
    let Some(segs) = json_path::parse_path(&ctx.args[1]) else {
        append_error(ctx.out, ERR_PATH_SYNTAX);
        return;
    };
    let Some(idx) = parse_i64(&ctx.args[2]) else {
        append_error(ctx.out, ERR_NOT_INT);
        return;
    };
    let mut add: Vec<Value> = Vec::with_capacity(ctx.args.len() - 3);
    for arg in &ctx.args[3..] {
        match serde_json::from_slice(arg) {
            Ok(v) => add.push(v),
            Err(_) => {
                append_error(ctx.out, ERR_INVALID_JSON);
                return;
            }
        }
    }
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    );
    match json_state(ctx, &key) {
        JsonState::WrongType => append_error(ctx.out, WRONGTYPE),
        JsonState::Missing => append_error(ctx.out, ERR_PATH_MISSING),
        JsonState::Doc { expire_ms, doc } => {
            let Some(mut doc) = decode_doc(ctx, &doc) else {
                return;
            };
            let Some(slot) = json_path::get_at_mut(&mut doc, &segs) else {
                append_error(ctx.out, ERR_PATH_MISSING);
                return;
            };
            let Value::Array(arr) = slot else {
                append_error(ctx.out, ERR_WRONG_PATH_TYPE);
                return;
            };
            let len = arr.len() as i64;
            let pos = if idx < 0 { idx + len } else { idx };
            if pos < 0 || pos > len {
                append_error(ctx.out, ERR_INDEX);
                return;
            }
            for (offset, v) in add.drain(..).enumerate() {
                arr.insert(pos as usize + offset, v);
            }
            let new_len = arr.len();
            if save_doc(ctx, &key, expire_ms, &doc, "json.arrinsert").await {
                append_int(ctx.out, new_len as i64);
            }
        }
    }
}

/// JSON.ARRLEN key [path] -> array length; missing key/path -> null
/// bulk, non-array -> wrong-type-of-path error.
pub async fn json_arrlen(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 2 {
        arity(ctx.out, "json.arrlen");
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
                Some(Value::Array(arr)) => append_int(ctx.out, arr.len() as i64),
                Some(_) => append_error(ctx.out, ERR_WRONG_PATH_TYPE),
                None => append_null(ctx.out),
            }
        }
    }
}

/// JSON.ARRTRIM key path start stop -> new length after keeping the
/// INCLUSIVE `[start, stop]` window (stop -1 = last element; negative
/// indexes count from the end; start > stop or start >= len empties the
/// array).
pub async fn json_arrtrim(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 4 {
        arity(ctx.out, "json.arrtrim");
        return;
    }
    let key = ctx.args[0].clone();
    let Some(segs) = json_path::parse_path(&ctx.args[1]) else {
        append_error(ctx.out, ERR_PATH_SYNTAX);
        return;
    };
    let (Some(start), Some(stop)) = (parse_i64(&ctx.args[2]), parse_i64(&ctx.args[3])) else {
        append_error(ctx.out, ERR_NOT_INT);
        return;
    };
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    );
    match json_state(ctx, &key) {
        JsonState::WrongType => append_error(ctx.out, WRONGTYPE),
        JsonState::Missing => append_null(ctx.out),
        JsonState::Doc { expire_ms, doc } => {
            let Some(mut doc) = decode_doc(ctx, &doc) else {
                return;
            };
            let Some(slot) = json_path::get_at_mut(&mut doc, &segs) else {
                append_error(ctx.out, ERR_PATH_MISSING);
                return;
            };
            let Value::Array(arr) = slot else {
                append_error(ctx.out, ERR_WRONG_PATH_TYPE);
                return;
            };
            let len = arr.len() as i64;
            let start = (if start < 0 { len + start } else { start }).max(0);
            let stop = if stop < 0 { len + stop } else { stop };
            let new_len = if start > stop || start >= len {
                arr.clear();
                0
            } else {
                let end = stop.min(len - 1); // inclusive
                let mut kept = arr.split_off(start as usize);
                kept.truncate((end - start + 1) as usize);
                let n = kept.len();
                *arr = kept;
                n
            };
            if save_doc(ctx, &key, expire_ms, &doc, "json.arrtrim").await {
                append_int(ctx.out, new_len as i64);
            }
        }
    }
}
