//! Data plane of `MIGRATE`: the per-key DUMP/RESTORE transport
//! (`MIGRATE host port key db timeout [COPY] [REPLACE] [KEYS key ...]`)
//! and `RESTORE`, the target side. Admin-plane orchestration lives in
//! [`super::task`]; dispatch in [`super::handle`].

use rocksdb::WriteBatch;

use crate::command::keys_core;
use crate::command::Ctx;
use crate::ds::{dump, expire, latch};
use crate::hash;
use crate::resp::client::{self, Reply};
use crate::resp::codec::{append_error, append_string};

/// `MIGRATE host port key db timeout [COPY] [REPLACE] [KEYS key ...]`.
///
/// Redis semantics: `db` is ignored (single logical DB), `timeout` is
/// milliseconds (0 = unbounded) and bounds each transport step. Keys are
/// migrated one at a time under the per-key latch; a transport or target
/// error aborts the remaining keys (already-moved keys stay moved, like
/// Redis). `COPY` keeps the source copy; `REPLACE` lets RESTORE overwrite
/// an existing target key.
pub(super) async fn migrate_data(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 6 {
        append_error(
            ctx.out,
            "ERR wrong number of arguments for 'migrate' command",
        );
        return;
    }
    let host = String::from_utf8_lossy(&ctx.args[0]).into_owned();
    let port = String::from_utf8_lossy(&ctx.args[1]).into_owned();
    let key = &ctx.args[2];
    let timeout_ms: u64 = std::str::from_utf8(&ctx.args[4])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut copy = false;
    let mut replace = false;
    let mut keys_mode = false;
    let mut extra_keys: Vec<Vec<u8>> = Vec::new();
    for a in &ctx.args[5..] {
        if keys_mode {
            extra_keys.push(a.clone());
            continue;
        }
        match a.to_ascii_uppercase().as_slice() {
            b"COPY" => copy = true,
            b"REPLACE" => replace = true,
            b"KEYS" => keys_mode = true,
            _ => {
                append_error(ctx.out, "ERR syntax error");
                return;
            }
        }
    }
    if keys_mode && !key.is_empty() {
        append_error(
            ctx.out,
            "ERR When using MIGRATE KEYS option, the key argument must be set to the empty string",
        );
        return;
    }
    let keys: Vec<Vec<u8>> = if keys_mode {
        extra_keys
    } else {
        vec![key.clone()]
    };
    let dst = format!("{host}:{port}");

    let mut stream = None;
    for k in &keys {
        let (_, prefix) = hash::slot_with_prefix(hash::hash_tag(k));
        let _guard = latch::lock(&ctx.shared.latch, &keys_core::latch_key(&prefix, k)).await;
        let now = expire::now_ms();
        let state = keys_core::resolve_arc(&ctx.shared.store, &prefix, k, now);
        if !state.is_present() {
            continue; // absent keys are skipped silently (Redis)
        }
        let Some(payload) = dump::dump_key(&ctx.shared.store, &prefix, k, now) else {
            continue;
        };
        let ttl = state.expire_ms();
        if stream.is_none() {
            stream = Some(
                match client::connect_authed(&dst, &ctx.shared.conf.raft_token, timeout_ms).await {
                    Ok(s) => s,
                    Err(e) => {
                        append_error(
                            ctx.out,
                            &format!("IOERR error or timeout writing to target instance ({e})"),
                        );
                        return;
                    }
                },
            );
        }
        let s = stream.as_mut().expect("stream set above");
        // ASKING is a connection-level one-shot on the target: it must be
        // its OWN frame, or the target executes only it (replies +OK) and
        // the RESTORE below never runs. Then the RESTORE frame follows.
        if let Err(e) = client::send_command(s, &[b"ASKING"]).await {
            append_error(
                ctx.out,
                &format!("IOERR error or timeout writing to target instance ({e})"),
            );
            return;
        }
        match client::read_reply_timed(s, timeout_ms).await {
            Ok(Reply::Simple(_)) => {}
            Ok(Reply::Error(e)) => {
                append_error(ctx.out, &e);
                return;
            }
            _ => {
                append_error(ctx.out, "ERR unexpected reply from target instance");
                return;
            }
        }
        let mut argv: Vec<Vec<u8>> = vec![
            b"RESTORE".to_vec(),
            k.clone(),
            ttl.to_string().into_bytes(),
            payload,
        ];
        if replace {
            argv.push(b"REPLACE".to_vec());
        }
        let argv_refs: Vec<&[u8]> = argv.iter().map(|a| a.as_slice()).collect();
        if let Err(e) = client::send_command(s, &argv_refs).await {
            append_error(
                ctx.out,
                &format!("IOERR error or timeout writing to target instance ({e})"),
            );
            return;
        }
        match client::read_reply_timed(s, timeout_ms).await {
            Ok(Reply::Simple(_)) => {}
            Ok(Reply::Error(e)) => {
                // BUSYKEY (no REPLACE) or any target-side error aborts.
                append_error(ctx.out, &e);
                return;
            }
            Ok(_) => {
                append_error(ctx.out, "ERR unexpected reply from target instance");
                return;
            }
            Err(e) => {
                append_error(
                    ctx.out,
                    &format!("IOERR error or timeout writing to target instance ({e})"),
                );
                return;
            }
        }
        if !copy {
            let mut batch = WriteBatch::default();
            crate::command::string::clear_key_family(&mut batch, &prefix, k, &state);
            if let Err(e) = ctx.commit(batch).await {
                append_error(ctx.out, &format!("ERR: migrate key failed ({e})"));
                return;
            }
        }
    }
    append_string(ctx.out, "OK");
}

/// `RESTORE key ttl serialized-value [REPLACE] [ABSTTL]` -- the target
/// side of MIGRATE. `ttl` is an ABSOLUTE ms deadline (0 = keep the
/// deadline embedded in the payload); ABSTTL is accepted as a no-op
/// because our TTLs are always absolute.
pub async fn restore(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        append_error(
            ctx.out,
            "ERR wrong number of arguments for 'restore' command",
        );
        return;
    }
    let ttl: i64 = match std::str::from_utf8(&ctx.args[1])
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(t) if t >= 0 => t,
        _ => {
            append_error(ctx.out, "ERR Invalid TTL value, must be >= 0");
            return;
        }
    };
    let mut replace = false;
    for a in &ctx.args[3..] {
        match a.to_ascii_uppercase().as_slice() {
            b"REPLACE" => replace = true,
            b"ABSTTL" => {}
            _ => {
                append_error(ctx.out, "ERR syntax error");
                return;
            }
        }
    }
    let now = expire::now_ms();
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0], now);
    if state.is_present() && !replace {
        append_error(ctx.out, "BUSYKEY Target key name already exists.");
        return;
    }
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &ctx.args[0]),
    )
    .await;
    let mut batch = WriteBatch::default();
    match dump::restore_key(
        &mut batch,
        &ctx.prefix_key,
        &ctx.args[0],
        &ctx.args[2],
        ttl as u64,
    ) {
        Ok(_) => match ctx.commit(batch).await {
            Ok(()) => append_string(ctx.out, "OK"),
            Err(_) => append_error(ctx.out, "ERR: restore key failed"),
        },
        Err(e) => append_error(ctx.out, &e),
    }
}
