//! Generic key-space commands (Go: internal/command/keys.go):
//! TYPE, EXISTS, DEL/UNLINK, EXPIRE family, TTL/PTTL, PERSIST, SCAN,
//! KEYS, RANDOMKEY, RENAME/RENAMENX.
//!
//! Handlers parse RESP args, delegate to `keys_core`/`keys_scan` storage
//! functions and append replies. Writes run under the per-key latch and
//! await off-worker fsyncs (`ops::batch_write_async`).

use crate::command::keys_core::{self, RenameOutcome, TtlFlag};
use crate::command::keys_scan;
use crate::command::Ctx;
use crate::ds;
use crate::resp::codec::{
    append_array, append_bulk, append_bulk_string, append_error, append_int, append_null,
    append_string,
};

fn arity(out: &mut Vec<u8>, cmd: &str) {
    append_error(
        out,
        &format!("ERR wrong number of arguments for '{}' command", cmd),
    );
}

fn parse_i64(arg: &[u8]) -> Option<i64> {
    std::str::from_utf8(arg).ok()?.parse().ok()
}

/// `TYPE key` -> simple string; raw strings and STRING_TTL both "string".
pub async fn type_(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "type");
        return;
    }
    let state = keys_core::resolve(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        ds::expire::now_ms(),
    );
    let name = match state {
        keys_core::KeyState::Missing => "none",
        keys_core::KeyState::RawString { .. } => "string",
        keys_core::KeyState::Enveloped { kind, .. } => ds::type_name(kind),
    };
    append_string(ctx.out, name);
}

/// `EXISTS key [key ...]` -> count of existing keys.
pub async fn exists(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        arity(ctx.out, "exists");
        return;
    }
    if !ds::setops::require_same_slot(ctx.out, &ctx.args) {
        return;
    }
    let now = ds::expire::now_ms();
    let mut n = 0i64;
    for key in &ctx.args {
        if keys_core::resolve(&ctx.shared.store, &ctx.prefix_key, key, now).is_present() {
            n += 1;
        }
    }
    append_int(ctx.out, n);
}

/// `DEL key [key ...]` / `UNLINK ...` -> count actually removed.
pub async fn del(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        arity(ctx.out, "del");
        return;
    }
    if !ds::setops::require_same_slot(ctx.out, &ctx.args) {
        return;
    }
    let now = ds::expire::now_ms();
    let mut n = 0i64;
    // DEL is a write command: implicitly UNWATCHes outside MULTI even when
    // it removes nothing (a no-op cannot change any watched key's hash).
    ctx.wrote = true;
    for key in &ctx.args {
        match keys_core::delete_records(ctx.shared, &ctx.prefix_key, key, now).await {
            Ok(true) => n += 1,
            Ok(false) => {}
            // DEL is not transactional: keys already removed stay removed,
            // but the count is NOT reported after a storage error.
            Err(_) => {
                append_error(ctx.out, "ERR: del failed");
                return;
            }
        }
    }
    append_int(ctx.out, n);
}

/// Shared EXPIRE/PEXPIRE/EXPIREAT/PEXPIREAT body. `unit_ms` converts the
/// integer argument to milliseconds; `absolute` for the *AT variants.
async fn expire_common(ctx: &mut Ctx<'_>, cmd: &str, unit_ms: i64, absolute: bool) {
    if ctx.args.len() < 2 || ctx.args.len() > 3 {
        arity(ctx.out, cmd);
        return;
    }
    let flag = match ctx.args.len() {
        3 => match keys_core::parse_ttl_flag(&ctx.args[2]) {
            Some(f) => f,
            None => {
                append_error(
                    ctx.out,
                    "ERR Unsupported option: supported options are NX, XX, GT and LT",
                );
                return;
            }
        },
        _ => TtlFlag::None,
    };
    let Some(n) = parse_i64(&ctx.args[1]) else {
        append_error(ctx.out, "ERR value is not an integer or out of range");
        return;
    };
    let now = ds::expire::now_ms();
    // Absolute deadlines use the raw value; relative ones add `now`.
    // Negative/past results clamp to 0, which apply_ttl treats as "delete".
    let new_ms = if absolute {
        n.saturating_mul(unit_ms)
    } else {
        (now as i64).saturating_add(n.saturating_mul(unit_ms))
    }
    .max(0) as u64;
    ctx.wrote = true; // TTL change is a key modification (implicit UNWATCH)
    match keys_core::apply_ttl(ctx.shared, &ctx.prefix_key, &ctx.args[0], new_ms, flag, now).await {
        Ok(changed) => append_int(ctx.out, i64::from(changed)),
        Err(_) => append_error(ctx.out, "ERR: expire failed"),
    }
}

pub async fn expire(ctx: &mut Ctx<'_>) {
    expire_common(ctx, "expire", 1_000, false).await;
}

pub async fn pexpire(ctx: &mut Ctx<'_>) {
    expire_common(ctx, "pexpire", 1, false).await;
}

pub async fn expireat(ctx: &mut Ctx<'_>) {
    expire_common(ctx, "expireat", 1_000, true).await;
}

pub async fn pexpireat(ctx: &mut Ctx<'_>) {
    expire_common(ctx, "pexpireat", 1, true).await;
}

/// Shared TTL/PTTL body; `ms` keeps milliseconds, else round HALF UP to
/// seconds like Redis's `(ttl_ms+500)/1000` (bug fix: was floored).
async fn ttl_common(ctx: &mut Ctx<'_>, cmd: &str, ms: bool) {
    if ctx.args.len() != 1 {
        arity(ctx.out, cmd);
        return;
    }
    let now = ds::expire::now_ms();
    let state = keys_core::resolve(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0], now);
    let answer = match keys_core::read_ttl(&state, now) {
        keys_core::TtlAnswer::Missing => -2,
        keys_core::TtlAnswer::NoExpiry => -1,
        keys_core::TtlAnswer::Millis(remaining) => {
            if ms {
                remaining as i64
            } else {
                ((remaining + 500) / 1_000) as i64
            }
        }
    };
    append_int(ctx.out, answer);
}

pub async fn ttl(ctx: &mut Ctx<'_>) {
    ttl_common(ctx, "ttl", false).await;
}

pub async fn pttl(ctx: &mut Ctx<'_>) {
    ttl_common(ctx, "pttl", true).await;
}

/// `PERSIST key` -> `:1` when a TTL was cleared.
pub async fn persist(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "persist");
        return;
    }
    let now = ds::expire::now_ms();
    ctx.wrote = true; // PERSIST removes a TTL: same modification semantics
    match keys_core::persist_key(ctx.shared, &ctx.prefix_key, &ctx.args[0], now).await {
        Ok(cleared) => append_int(ctx.out, i64::from(cleared)),
        Err(_) => append_error(ctx.out, "ERR: persist failed"),
    }
}

/// `SCAN cursor [MATCH pattern] [COUNT n] [TYPE type]` (per-slot; cursor =
/// hex of the last physical key, "0"/"" restarts). No lazy expiry
/// mid-iteration, per Redis SCAN guarantees.
pub async fn scan(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        arity(ctx.out, "scan");
        return;
    }
    let cursor_start = ctx.args[0].is_empty() || ctx.args[0] == b"0";
    let mut pattern: Option<Vec<u8>> = None;
    let mut type_filter: Option<Vec<u8>> = None;
    let mut count: usize = 10;
    let mut i = 1;
    while i < ctx.args.len() {
        let opt = ctx.args[i].to_ascii_uppercase();
        match opt.as_slice() {
            b"MATCH" if i + 1 < ctx.args.len() => {
                pattern = Some(ctx.args[i + 1].clone());
                i += 2;
            }
            b"TYPE" if i + 1 < ctx.args.len() => {
                // Value matches case-insensitively; unknown names are a
                // syntax error (Redis rejects them outright).
                if !keys_scan::is_scan_type_name(&ctx.args[i + 1]) {
                    append_error(ctx.out, "ERR syntax error");
                    return;
                }
                type_filter = Some(ctx.args[i + 1].clone());
                i += 2;
            }
            b"COUNT" if i + 1 < ctx.args.len() => {
                match parse_i64(&ctx.args[i + 1]) {
                    Some(n) if n > 0 => count = n as usize,
                    _ => {
                        append_error(ctx.out, "ERR value is not an integer or out of range");
                        return;
                    }
                }
                i += 2;
            }
            _ => {
                append_error(ctx.out, "ERR syntax error");
                return;
            }
        }
    }
    let from = if cursor_start {
        ctx.prefix_key.clone()
    } else {
        match hex::decode(&ctx.args[0]) {
            Ok(bytes) => bytes,
            Err(_) => {
                append_error(ctx.out, "ERR invalid cursor");
                return;
            }
        }
    };
    let page = match keys_scan::collect_user_keys(
        &ctx.shared.store,
        &ctx.prefix_key,
        &from,
        pattern.as_deref(),
        type_filter.as_deref(),
        count,
    ) {
        Ok(page) => page,
        // Storage failure: reply -ERR, never a "0" cursor (which would
        // silently truncate a client's iteration as "finished").
        Err(_) => {
            append_error(ctx.out, "ERR: scan failed");
            return;
        }
    };
    let cursor = if page.next.is_empty() {
        "0".to_string()
    } else {
        hex::encode(&page.next)
    };
    append_array(ctx.out, 2);
    append_bulk_string(ctx.out, &cursor);
    append_array(ctx.out, page.keys.len());
    for key in &page.keys {
        append_bulk(ctx.out, key);
    }
}

/// `KEYS pattern` -> all matching user keys in this slot.
pub async fn keys_cmd(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "keys");
        return;
    }
    let all = match keys_scan::all_user_keys(&ctx.shared.store, &ctx.prefix_key, Some(&ctx.args[0]))
    {
        Ok(all) => all,
        // Never reply a partial key list as if it were complete.
        Err(_) => {
            append_error(ctx.out, "ERR: keys failed");
            return;
        }
    };
    append_array(ctx.out, all.len());
    for key in &all {
        append_bulk(ctx.out, key);
    }
}

/// `RANDOMKEY` -> one user key from a random slot, or null when empty.
pub async fn randomkey(ctx: &mut Ctx<'_>) {
    if !ctx.args.is_empty() {
        arity(ctx.out, "randomkey");
        return;
    }
    match keys_scan::random_user_key(&ctx.shared.store) {
        Ok(Some(key)) => append_bulk(ctx.out, &key),
        Ok(None) => append_null(ctx.out),
        // A miss must mean "database empty", never "read failed".
        Err(_) => append_error(ctx.out, "ERR: randomkey failed"),
    }
}

/// Shared RENAME/RENAMENX body.
async fn rename_common(ctx: &mut Ctx<'_>, cmd: &str, nx: bool) {
    if ctx.args.len() != 2 {
        arity(ctx.out, cmd);
        return;
    }
    if !ds::setops::require_same_slot(ctx.out, &ctx.args) {
        return;
    }
    let now = ds::expire::now_ms();
    match keys_core::rename_key(
        ctx.shared,
        &ctx.prefix_key,
        &ctx.args[0],
        &ctx.args[1],
        nx,
        now,
    )
    .await
    {
        Ok(RenameOutcome::Moved) => {
            if nx {
                append_int(ctx.out, 1);
            } else {
                append_string(ctx.out, "OK");
            }
        }
        Ok(RenameOutcome::DstBlocked) => append_int(ctx.out, 0),
        Ok(RenameOutcome::SrcMissing) => append_error(ctx.out, "ERR no such key"),
        Err(e) => append_error(ctx.out, &format!("ERR: rename failed: {e}")),
    }
}

pub async fn rename(ctx: &mut Ctx<'_>) {
    rename_common(ctx, "rename", false).await;
}

pub async fn renamenx(ctx: &mut Ctx<'_>) {
    rename_common(ctx, "renamenx", true).await;
}

#[cfg(test)]
mod tests;
