//! JSON object reads (JSON.OBJKEYS/OBJLEN): plain navigation over the
//! decoded document, no latch, no write. Replies follow the missing =
//! null bulk convention; a non-object at the path is the wrong-type-of-
//! path error. Key order is the stored insertion order (serde_json
//! `preserve_order`).

use serde_json::Value;

use crate::command::hash_cmd::{arity, WRONGTYPE};
use crate::command::json_cmd::{
    decode_doc, json_state, JsonState, ERR_PATH_SYNTAX, ERR_WRONG_PATH_TYPE,
};
use crate::command::json_path::{self, PathSeg};
use crate::command::Ctx;
use crate::resp::codec::{append_array, append_bulk, append_error, append_int, append_null};

/// JSON.OBJKEYS key [path] -> RESP array of the object's keys in stored
/// order; missing key/path -> null bulk, non-object -> error.
pub async fn json_objkeys(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 2 {
        arity(ctx.out, "json.objkeys");
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
                Some(Value::Object(m)) => {
                    append_array(ctx.out, m.len());
                    for k in m.keys() {
                        append_bulk(ctx.out, k.as_bytes());
                    }
                }
                Some(_) => append_error(ctx.out, ERR_WRONG_PATH_TYPE),
                None => append_null(ctx.out),
            }
        }
    }
}

/// JSON.OBJLEN key [path] -> number of keys of the object at the path;
/// missing key/path -> null bulk, non-object -> error.
pub async fn json_objlen(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 2 {
        arity(ctx.out, "json.objlen");
        return;
    }
    let segs: Vec<PathSeg> = match ctx.args.get(1) {
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
                Some(Value::Object(m)) => append_int(ctx.out, m.len() as i64),
                Some(_) => append_error(ctx.out, ERR_WRONG_PATH_TYPE),
                None => append_null(ctx.out),
            }
        }
    }
}
