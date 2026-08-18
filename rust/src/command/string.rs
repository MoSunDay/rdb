//! String / basic commands (Go `internal/command/string.go` + `othes.go`).

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::command::hash_cmd;
use crate::command::keys_core::{self, KeyState};
use crate::command::string_opts;
use crate::command::Ctx;
use crate::ds::codec;
use crate::ds::expire;
use crate::ds::latch;
use crate::ds::setops;
use crate::resp::codec::{
    append_array, append_bulk, append_bulk_string, append_error, append_int, append_null,
    append_string,
};
use crate::store::{self, ops};

fn arity(out: &mut Vec<u8>, cmd: &str) {
    append_error(
        out,
        &format!("ERR wrong number of arguments for '{}' command", cmd),
    );
}

/// What one key holds from a string reader's point of view.
enum OldValue {
    Missing,
    Str(Vec<u8>),
    /// Present, but not a string (hash/list/set/...): WRONGTYPE.
    WrongType,
}

/// Read a resolved key as a string: raw records and STRING_TTL envelopes
/// carry one; other kinds do not.
fn old_string_value(state: &KeyState) -> OldValue {
    match state {
        KeyState::Missing => OldValue::Missing,
        KeyState::RawString { value } => OldValue::Str(value.clone()),
        KeyState::Enveloped { kind, payload, .. } if *kind == codec::KIND_STRING_TTL => {
            OldValue::Str(payload.clone())
        }
        KeyState::Enveloped { .. } => OldValue::WrongType,
    }
}

/// GET-option reply shape: old value as a bulk, absence as a null bulk.
fn reply_old_or_null(out: &mut Vec<u8>, old: OldValue) {
    match old {
        OldValue::Str(v) => append_bulk(out, &v),
        OldValue::Missing => append_null(out),
        // Callers reject wrong types before veto/set replies are built.
        OldValue::WrongType => unreachable!("wrong type must be rejected earlier"),
    }
}

/// Batch entries removing `key`'s current family (any type), so a string
/// overwrite crosses types exactly like Redis SET/MSET.
fn clear_key_family(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], state: &KeyState) {
    match state {
        KeyState::Missing => {}
        KeyState::RawString { .. } => {
            batch.delete(codec::string_key(prefix, key));
        }
        KeyState::Enveloped {
            kind, expire_ms, ..
        } => {
            let family = codec::family_of(*kind).unwrap_or(codec::STRING_FAMILY);
            expire::family_delete_entries(batch, prefix, family, key, *expire_ms);
        }
    }
}

/// Write the new string: a STRING_TTL envelope when `deadline` > 0
/// (expire index entry included), else the bare legacy record -- the same
/// shapes `keys_core::apply_ttl` writes.
fn write_string_record(
    batch: &mut WriteBatch,
    prefix: &[u8],
    key: &[u8],
    val: &[u8],
    deadline: u64,
) {
    if deadline > 0 {
        let root = codec::data_key(prefix, codec::KIND_STRING_TTL, key);
        batch.put(&root, codec::encode_envelope(deadline, val));
        expire::set_ttl_entries(batch, prefix, root, 0, deadline);
    } else {
        batch.put(codec::string_key(prefix, key), val);
    }
}

/// `PING` -> `+PONG`.
pub async fn ping(ctx: &mut Ctx<'_>) {
    append_string(ctx.out, "PONG");
}

/// `QUIT`: reply a single `+OK` and ask for the connection to close
/// (BREAKING, approved: Redis replies exactly one +OK; the Go fork's
/// leading +PONG is dropped).
pub async fn quit(ctx: &mut Ctx<'_>) {
    append_string(ctx.out, "OK");
    ctx.close_conn = true;
}

/// `CONFIG ...`: all arguments ignored; always the same two bulk strings.
pub async fn config(ctx: &mut Ctx<'_>) {
    append_array(ctx.out, 2);
    append_bulk_string(ctx.out, "cluster-require-full-coverage");
    append_bulk_string(ctx.out, "no");
}

/// `GET key`: raw string first, then the enveloped STRING_TTL record
/// (written by EXPIRE) with lazy expiry; any store error replies null bulk.
pub async fn get(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "get");
        return;
    }
    if let Ok(Some(val)) = store::get(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0]) {
        append_bulk(ctx.out, &val);
        return;
    }
    match crate::ds::expire::read_enveloped(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0]) {
        Ok(Some((_, payload))) => append_bulk(ctx.out, &payload),
        _ => append_null(ctx.out),
    }
}

/// `SET key value [EX s|PX ms|EXAT s|PXAT ms] [NX|XX] [KEEPTTL] [GET]`
/// (Redis 7 semantics; option parsing lives in `string_opts`).
///
/// NX/XX vetoes reply a null bulk -- or the old value with GET. The GET
/// option turns success replies into the previous value (null when the
/// key was absent) and refuses non-string keys with WRONGTYPE. Unless
/// KEEPTTL holds it back, the write clears whatever family the key had
/// and rewrites it as a bare record (no deadline) or a STRING_TTL
/// envelope. Past EXAT/PXAT deadlines still write: the record is simply
/// due immediately.
pub async fn set(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "set");
        return;
    }
    let opts = match string_opts::parse(&ctx.args[2..]) {
        Ok(opts) => opts,
        Err(err) => {
            append_error(ctx.out, string_opts::error_text(&err));
            return;
        }
    };
    let key = ctx.args[0].clone();
    let val = ctx.args[1].clone();
    let now = expire::now_ms();
    let expire_ms = string_opts::resolve_ttl(opts.ttl, now);
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let state = keys_core::resolve(&ctx.shared.store, &ctx.prefix_key, &key, now);

    let old = old_string_value(&state);
    if matches!(old, OldValue::WrongType) && opts.get {
        append_error(ctx.out, hash_cmd::WRONGTYPE);
        return;
    }
    if (opts.nx && state.is_present()) || (opts.xx && !state.is_present()) {
        // Veto: GET still reports the old value, plain SET replies null.
        if opts.get {
            reply_old_or_null(ctx.out, old);
        } else {
            append_null(ctx.out);
        }
        return;
    }
    // KEEPTTL carries the previous deadline over (0 = raw/missing key).
    let deadline = if opts.keepttl {
        state.expire_ms()
    } else {
        expire_ms.unwrap_or(0)
    };
    let mut batch = WriteBatch::default();
    clear_key_family(&mut batch, &ctx.prefix_key, &key, &state);
    write_string_record(&mut batch, &ctx.prefix_key, &key, &val, deadline);
    match ops::batch_write_async(Arc::clone(&ctx.shared.store), batch).await {
        Ok(()) => {
            if opts.get {
                reply_old_or_null(ctx.out, old);
            } else {
                append_string(ctx.out, "OK");
            }
        }
        Err(_) => append_error(ctx.out, "ERR: set key failed"),
    }
}

/// `DEL key`.
///
/// AGREED BUG FIX: Go discarded pebble's Del error and always replied `:1`.
/// Rust reports the truth: `:1` only when the key existed and was removed,
/// `:0` otherwise (missing key or store error).
pub async fn del(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        append_error(ctx.out, "ERR wrong number of arguments for del command");
        return;
    }
    let key = std::mem::take(&mut ctx.args[0]);
    let res = store::del_async(Arc::clone(&ctx.shared.store), ctx.prefix_key.clone(), key).await;
    append_int(ctx.out, i64::from(matches!(res, Ok(true))));
}

/// `MGET key [key ...]`: one bulk per key. Missing keys render as null
/// bulks, stored empty strings as `$0` empty bulks, and ANY key holding
/// a non-string type fails the whole command with WRONGTYPE (Redis).
/// All keys must hash to one slot.
pub async fn mget(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        arity(ctx.out, "mget");
        return;
    }
    if !setops::require_same_slot(ctx.out, &ctx.args) {
        return;
    }
    let now = expire::now_ms();
    let mut rows = Vec::with_capacity(ctx.args.len());
    for key in &ctx.args {
        let state = keys_core::resolve(&ctx.shared.store, &ctx.prefix_key, key, now);
        match old_string_value(&state) {
            OldValue::WrongType => {
                append_error(ctx.out, hash_cmd::WRONGTYPE);
                return;
            }
            row => rows.push(row),
        }
    }
    append_array(ctx.out, rows.len());
    for row in &rows {
        match row {
            OldValue::Str(v) => append_bulk(ctx.out, v),
            _ => append_null(ctx.out),
        }
    }
}

/// `MSET key value [key value ...]`: every pair lands in ONE WriteBatch.
///
/// AGREED BUG FIX: zero arguments used to reply `+OK` (and a lone value
/// panicked in Go); both are arity errors now, the odd tail keeps its
/// dedicated message. Each destination's previous family is cleared in
/// the same batch, so MSET overwrites keys of any type, like Redis.
/// All keys (even indices) must hash to one slot.
pub async fn mset(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        arity(ctx.out, "mset");
        return;
    }
    if !ctx.args.len().is_multiple_of(2) {
        append_error(
            ctx.out,
            &format!("ERR wrong number of arguments: {}", ctx.args.len()),
        );
        return;
    }
    let keys: Vec<Vec<u8>> = ctx.args.chunks(2).map(|c| c[0].clone()).collect();
    if !setops::require_same_slot(ctx.out, &keys) {
        return;
    }
    let pairs = std::mem::take(&mut ctx.args);
    let now = expire::now_ms();
    // Every distinct destination under its latch (byte order, ABBA rule).
    let mut latches: Vec<Vec<u8>> = keys
        .iter()
        .map(|k| keys_core::latch_key(&ctx.prefix_key, k))
        .collect();
    latches.sort();
    latches.dedup();
    let mut guards = Vec::with_capacity(latches.len());
    for k in &latches {
        guards.push(latch::lock(&ctx.shared.latch, k).await);
    }
    let _guards = guards;
    let mut batch = WriteBatch::default();
    for pair in pairs.chunks_exact(2) {
        let state = keys_core::resolve(&ctx.shared.store, &ctx.prefix_key, &pair[0], now);
        clear_key_family(&mut batch, &ctx.prefix_key, &pair[0], &state);
        write_string_record(&mut batch, &ctx.prefix_key, &pair[0], &pair[1], 0);
    }
    match ops::batch_write_async(Arc::clone(&ctx.shared.store), batch).await {
        Ok(()) => append_string(ctx.out, "OK"),
        Err(_) => append_error(ctx.out, "ERR: set key failed"),
    }
}

#[cfg(test)]
pub(crate) static TEST_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
#[path = "string/test_util.rs"]
pub(crate) mod test_util;

#[cfg(test)]
#[path = "string/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "string/set_opts_tests.rs"]
mod set_opts_tests;
