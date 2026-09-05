//! Server-surface commands: `COMMAND` introspection, `INFO`, `DBSIZE`,
//! `ECHO`, `SELECT`, `FLUSHDB` (Go `othes.go` surface, Redis-compatible).

use std::sync::OnceLock;

use crate::command::cmd_meta::{self, CmdMeta};
use crate::command::keyspace_role::{record_role, RecordRole};
use crate::command::string;
use crate::command::Ctx;
use crate::ds::expire;
use crate::resp::codec::{
    append_array, append_bulk, append_bulk_string, append_error, append_int, append_null,
    append_string,
};
use crate::store::ops;
use crate::store::Store;

/// `(keys, expires)` over the whole local store, one physical scan:
/// live root records (raw strings + META kinds, skipping the TTL index,
/// family members and control-plane records); `expires` counts live
/// roots whose envelope carries a deadline. Deadlines already due are
/// skipped, the same lazy-expiry envelope check readers apply.
fn keyspace_counts(store: &Store, now: u64) -> Result<(u64, u64), String> {
    let (mut keys, mut expires) = (0u64, 0u64);
    ops::for_each_from(store, b"", false, &mut |k, v| {
        if let RecordRole::Root { deadline } = record_role(k, v) {
            if !expire::is_expired(deadline, now) {
                keys += 1;
                expires += u64::from(deadline > 0);
            }
        }
        true
    })?;
    Ok((keys, expires))
}

/// `DBSIZE`: count of live root keys in the local store.
pub async fn dbsize(ctx: &mut Ctx<'_>) {
    if !ctx.args.is_empty() {
        string::arity(ctx.out, "dbsize");
        return;
    }
    match keyspace_counts(&ctx.shared.store, expire::now_ms()) {
        Ok((keys, _)) => append_int(ctx.out, keys as i64),
        Err(e) => append_error(ctx.out, &format!("ERR: dbsize failed: {e}")),
    }
}

/// Per-process random hex run id (Redis advertises 40 hex chars).
fn run_id() -> &'static str {
    static RUN_ID: OnceLock<String> = OnceLock::new();
    RUN_ID.get_or_init(|| {
        let mut hex = String::with_capacity(40);
        while hex.len() < 40 {
            hex.push_str(&format!("{:016x}", crate::utils::rand_u64()));
        }
        hex.truncate(40);
        hex
    })
}

/// Seconds since the first call (effectively process start).
fn uptime_seconds() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs()
}

/// RESP bind port: the tail of `"<host>:<port>"` (0 when unparsable).
fn tcp_port(bind: &str) -> u64 {
    bind.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
}

/// `INFO [section...]`: one bulk string of `\r\n`-terminated lines.
/// Sections filter case-insensitively (no args = all; `all`/`default`/
/// `everything` = all); unknown names contribute nothing. Sections are
/// always emitted in Server, Cluster, Keyspace order.
pub async fn info(ctx: &mut Ctx<'_>) {
    let mut want = [true, true, true];
    if !ctx.args.is_empty() {
        want = [false, false, false];
        for section in &ctx.args {
            let lower = section.to_ascii_lowercase();
            match lower.as_slice() {
                b"server" => want[0] = true,
                b"cluster" => want[1] = true,
                b"keyspace" => want[2] = true,
                b"all" | b"default" | b"everything" => want = [true, true, true],
                _ => {} // unknown section: header omitted
            }
        }
    }
    let mut body = String::new();
    if want[0] {
        body.push_str("# Server\r\n");
        body.push_str("redis_version:7.4.0\r\n");
        body.push_str("rdb_implementation:RocksDB\r\n");
        body.push_str(&format!("process_id:{}\r\n", std::process::id()));
        body.push_str(&format!("tcp_port:{}\r\n", tcp_port(&ctx.shared.conf.bind)));
        body.push_str(&format!("run_id:{}\r\n", run_id()));
        body.push_str(&format!("uptime_in_seconds:{}\r\n", uptime_seconds()));
        body.push_str("\r\n");
    }
    if want[1] {
        body.push_str("# Cluster\r\ncluster_enabled:1\r\ncluster_mode:rdb\r\n\r\n");
    }
    if want[2] {
        body.push_str("# Keyspace\r\n");
        let counts = keyspace_counts(&ctx.shared.store, expire::now_ms());
        let (keys, expires) = match counts {
            Ok(c) => c,
            Err(e) => {
                append_error(ctx.out, &format!("ERR: info failed: {e}"));
                return;
            }
        };
        if keys > 0 {
            body.push_str(&format!("db0:keys={keys},expires={expires}\r\n"));
        }
        body.push_str("\r\n");
    }
    append_bulk(ctx.out, body.as_bytes());
}

/// `ECHO message`.
pub async fn echo(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        string::arity(ctx.out, "echo");
        return;
    }
    append_bulk(ctx.out, &ctx.args[0]);
}

/// `SELECT index`: cluster mode serves db 0 only; other numeric indexes
/// reply Redis' cluster-mode error, non-numerics the integer parse error.
pub async fn select(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        string::arity(ctx.out, "select");
        return;
    }
    match String::from_utf8_lossy(&ctx.args[0]).parse::<i64>() {
        Ok(0) => append_string(ctx.out, "OK"),
        Ok(_) => append_error(ctx.out, "ERR SELECT is not allowed in cluster mode"),
        Err(_) => append_error(ctx.out, "ERR value is not an integer or out of range"),
    }
}

/// `FLUSHDB [ASYNC|SYNC]`: delete every USER record in the local store
/// across all slot prefixes (roots, family members, expire index),
/// preserving control-plane records. The modifier is accepted and
/// ignored; the wipe is chunked (~1024 keys per committed batch) with
/// the scan resuming after each chunk.
/// One `COMMAND` reply entry (RESP2): [name, arity, flags, first, last,
/// step]; this server advertises no flags (empty array).
fn append_entry(out: &mut Vec<u8>, meta: &CmdMeta) {
    append_array(out, 6);
    append_bulk(out, meta.name.as_bytes());
    append_int(out, meta.arity);
    append_array(out, 0);
    append_int(out, meta.first_key);
    append_int(out, meta.last_key);
    append_int(out, meta.step);
}

fn unknown_subcommand(out: &mut Vec<u8>, sub: &[u8]) {
    append_error(
        out,
        &format!(
            "ERR Unknown subcommand or wrong number of arguments for '{}'. Try COMMAND HELP.",
            String::from_utf8_lossy(sub)
        ),
    );
}

/// `COMMAND` / `COMMAND COUNT|INFO|DOCS|GETKEYS`.
pub async fn command(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        append_array(ctx.out, cmd_meta::COMMANDS.len());
        for meta in cmd_meta::COMMANDS {
            append_entry(ctx.out, meta);
        }
        return;
    }
    let sub = ctx.args[0].to_ascii_lowercase();
    match sub.as_slice() {
        b"count" if ctx.args.len() == 1 => append_int(ctx.out, cmd_meta::COMMANDS.len() as i64),
        b"info" => command_info(ctx),
        b"docs" => command_docs(ctx),
        b"getkeys" => command_getkeys(ctx),
        _ => unknown_subcommand(ctx.out, &ctx.args[0]),
    }
}

/// `COMMAND INFO [name...]`: the same entries as bare `COMMAND`; unknown
/// requested names answer a null bulk; no names = every command.
fn command_info(ctx: &mut Ctx<'_>) {
    let names = &ctx.args[1..];
    if names.is_empty() {
        append_array(ctx.out, cmd_meta::COMMANDS.len());
        for meta in cmd_meta::COMMANDS {
            append_entry(ctx.out, meta);
        }
        return;
    }
    append_array(ctx.out, names.len());
    for name in names {
        match cmd_meta::lookup_meta(name) {
            Some(meta) => append_entry(ctx.out, meta),
            None => append_null(ctx.out),
        }
    }
}

/// `COMMAND DOCS [name...]`: flat array `[name, map, ...]` where each
/// map is a flat RESP2 array of key/value bulk pairs. Unknown requested
/// names are skipped; no names = every command.
fn command_docs(ctx: &mut Ctx<'_>) {
    let names = &ctx.args[1..];
    let metas: Vec<&CmdMeta> = if names.is_empty() {
        cmd_meta::COMMANDS.iter().collect()
    } else {
        names
            .iter()
            .filter_map(|n| cmd_meta::lookup_meta(n))
            .collect()
    };
    append_array(ctx.out, metas.len() * 2);
    for meta in metas {
        append_bulk(ctx.out, meta.name.as_bytes());
        append_array(ctx.out, 4);
        append_bulk_string(ctx.out, "summary");
        append_bulk_string(ctx.out, meta.name);
        append_bulk_string(ctx.out, "since");
        append_bulk_string(ctx.out, "1.0.0");
    }
}

/// Key positions of a full argv (command name INCLUDED) per the Redis
/// positional keyspec: `first, first+step, ...` up to `last`, where a
/// negative `last` counts back from argc. `None` when the command is
/// keyless or the positions do not fit argv.
fn key_positions(meta: &CmdMeta, argc: usize) -> Option<Vec<usize>> {
    if meta.first_key <= 0 || meta.step <= 0 {
        return None;
    }
    let last = if meta.last_key < 0 {
        argc as i64 + meta.last_key
    } else {
        meta.last_key
    };
    let mut positions = Vec::new();
    let mut pos = meta.first_key;
    while pos <= last {
        if pos < 1 || pos >= argc as i64 {
            return None; // position outside argv
        }
        positions.push(pos as usize);
        pos += meta.step;
    }
    (!positions.is_empty()).then_some(positions)
}

/// `COMMAND GETKEYS <full argv...>`: the keys the given command would
/// touch, resolved from the static table.
fn command_getkeys(ctx: &mut Ctx<'_>) {
    let argv = &ctx.args[1..];
    if argv.is_empty() {
        append_error(
            ctx.out,
            "ERR wrong number of arguments for 'command|getkeys' command",
        );
        return;
    }
    let invalid = "ERR Invalid command specified";
    let Some(meta) = cmd_meta::lookup_meta(&argv[0]) else {
        append_error(ctx.out, invalid);
        return;
    };
    match key_positions(meta, argv.len()) {
        Some(positions) => {
            append_array(ctx.out, positions.len());
            for p in positions {
                append_bulk(ctx.out, &argv[p]);
            }
        }
        None => append_error(ctx.out, invalid),
    }
}

#[cfg(test)]
#[path = "server_cmd_unit_tests.rs"]
mod tests;
