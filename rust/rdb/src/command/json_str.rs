//! JSON string and number edits (JSON.STRAPPEND/STRLEN/NUMINCRBY): the
//! shared load-mutate-persist flow from `json_cmd` -- resolve the key
//! under the per-key latch, decode the whole document, navigate the
//! legacy path, edit the serde_json tree in place, rewrite the single
//! KIND_JSON record in one fsync. Reads (STRLEN) take no latch.

use serde_json::Value;

use crate::command::hash_cmd::{arity, parse_f64, WRONGTYPE};
use crate::command::json_cmd::{
    decode_doc, doc_bytes, json_state, save_doc, JsonState, ERR_PATH_MISSING, ERR_PATH_SYNTAX,
    ERR_WRONG_PATH_TYPE,
};
use crate::command::json_path::{self, PathSeg};
use crate::command::{keys_core, Ctx};
use crate::ds::latch;
use crate::resp::codec::{append_bulk, append_error, append_int, append_null};

/// The value argument must be a JSON string.
const ERR_VALUE_NOT_STRING: &str = "ERR wrong value type: expected string";
/// The increment argument must parse as a number.
const ERR_VALUE_NOT_FLOAT: &str = "ERR value is not a float";
/// The arithmetic produced NaN/inf.
const ERR_NOT_NUMBER: &str = "ERR result is not a number or out of range";

/// Root path when the optional path argument is absent.
fn root_or_path(arg: Option<&[u8]>) -> Option<Vec<PathSeg>> {
    match arg {
        None => Some(Vec::new()),
        Some(a) => json_path::parse_path(a),
    }
}

/// JSON.STRAPPEND key path value -> new byte length of the resulting
/// string. The value must itself be a JSON string; the path must exist
/// and hold a string.
pub async fn json_strappend(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "json.strappend");
        return;
    }
    let key = ctx.args[0].clone();
    let Some(segs) = json_path::parse_path(&ctx.args[1]) else {
        append_error(ctx.out, ERR_PATH_SYNTAX);
        return;
    };
    let Ok(Value::String(suffix)) = serde_json::from_slice::<Value>(&ctx.args[2]) else {
        append_error(ctx.out, ERR_VALUE_NOT_STRING);
        return;
    };
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
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
            let Value::String(s) = slot else {
                append_error(ctx.out, ERR_WRONG_PATH_TYPE);
                return;
            };
            s.push_str(&suffix);
            let new_len = s.len();
            if save_doc(ctx, &key, expire_ms, &doc, "json.strappend").await {
                append_int(ctx.out, new_len as i64);
            }
        }
    }
}

/// JSON.STRLEN key [path] -> byte length of the string at the path
/// (root by default); missing key/path -> null bulk, non-string ->
/// wrong-type-of-path error.
pub async fn json_strlen(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 2 {
        arity(ctx.out, "json.strlen");
        return;
    }
    let Some(segs) = root_or_path(ctx.args.get(1).map(|v| v.as_slice())) else {
        append_error(ctx.out, ERR_PATH_SYNTAX);
        return;
    };
    match json_state(ctx, &ctx.args[0]) {
        JsonState::WrongType => append_error(ctx.out, WRONGTYPE),
        JsonState::Missing => append_null(ctx.out),
        JsonState::Doc { doc, .. } => {
            let Some(doc) = decode_doc(ctx, &doc) else {
                return;
            };
            match json_path::get_at(&doc, &segs) {
                Some(Value::String(s)) => append_int(ctx.out, s.len() as i64),
                Some(_) => append_error(ctx.out, ERR_WRONG_PATH_TYPE),
                None => append_null(ctx.out),
            }
        }
    }
}

/// JSON.NUMINCRBY key path value -> bulk of the new number. Integral
/// results below 2^53 are stored (and printed) as integers, everything
/// else as f64 in serde_json's shortest-roundtrip form; a NaN/infinite
/// result is an error.
pub async fn json_numincrby(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "json.numincrby");
        return;
    }
    let key = ctx.args[0].clone();
    let Some(segs) = json_path::parse_path(&ctx.args[1]) else {
        append_error(ctx.out, ERR_PATH_SYNTAX);
        return;
    };
    let Some(delta) = parse_f64(&ctx.args[2]) else {
        append_error(ctx.out, ERR_VALUE_NOT_FLOAT);
        return;
    };
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
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
            let Value::Number(num) = slot else {
                append_error(ctx.out, ERR_WRONG_PATH_TYPE);
                return;
            };
            let sum = num.as_f64().unwrap_or(0.0) + delta;
            if !sum.is_finite() {
                append_error(ctx.out, ERR_NOT_NUMBER);
                return;
            }
            let new_num = if sum.fract() == 0.0 && sum.abs() < 9_007_199_254_740_992.0 {
                serde_json::Number::from(sum as i64)
            } else {
                serde_json::Number::from_f64(sum).expect("finite checked above")
            };
            let reply = doc_bytes(&Value::Number(new_num.clone()));
            *slot = Value::Number(new_num);
            if save_doc(ctx, &key, expire_ms, &doc, "json.numincrby").await {
                append_bulk(ctx.out, &reply);
            }
        }
    }
}
