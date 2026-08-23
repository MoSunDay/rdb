//! `MIGRATE` (data transport + admin tasks) and `RESTORE`.
//!
//! Data plane: `MIGRATE host port key db timeout [COPY] [REPLACE]
//! [KEYS key ...]` dumps each key on THIS node and transports it to the
//! target with `ASKING` + `RESTORE` (the target's IMPORTING table serves
//! only ASKING connections). The dump wire format lives in
//! `crate::ds::dump`; the outbound RESP client in `crate::resp::client`.
//! Admin plane (rdb extension): `migrate task <slot> <src> <dst>` drives
//! the full redis-cli `--cluster reshard` protocol against the two nodes
//! over the outbound client (src MIGRATING -> dst IMPORTING -> drain via
//! GETKEYSINSLOT + MIGRATE -> NODE on both -> STABLE). The one current
//! task (JSON) replicates through the raft key `migrate_task`; `migrate
//! list` returns it.

use std::sync::atomic::Ordering;

use rocksdb::WriteBatch;
use tokio::net::TcpStream;

use crate::command::keys_core;
use crate::command::Ctx;
use crate::ds::{dump, expire, latch};
use crate::hash;
use crate::resp::client::{self, Reply};
use crate::resp::codec::{append_array, append_bulk_string, append_error, append_string};
use crate::rtypes;
use crate::state;

/// Raft key holding the most recent task (one JSON document).
const TASK_KEY: &str = "migrate_task";

/// Millisecond bound on each orchestration hop (connect + reply).
/// Per-batch budget for the source-side MIGRATE call and the
/// connect/AUTH handshakes. Matches `redis-cli --cluster reshard`'s
/// own default (60000 ms): one batch may hold up to 1024 keys and each
/// RESTORE on the destination is a synced write.
const STEP_TIMEOUT_MS: u64 = 60_000;

/// `MIGRATE ...` dispatch; lowercase-only entries, exactly as Go's map.
pub async fn handle(ctx: &mut Ctx<'_>) {
    let Some(first) = ctx.args.first() else {
        migrate_helper(ctx);
        return;
    };
    match first.as_slice() {
        b"help" => migrate_helper(ctx),
        b"task" => migrate_task(ctx).await,
        b"list" => migrate_list(ctx),
        // Anything else is the DATA command: `MIGRATE host port key db
        // timeout ...` (first arg = host).
        _ => migrate_data(ctx).await,
    }
}

/// Go quirk kept: the usage message is an ERROR reply.
fn migrate_helper(ctx: &mut Ctx<'_>) {
    append_error(ctx.out, "migrate [ list | task ]");
}

/// `migrate list` — the single most recent task as one JSON bulk.
fn migrate_list(ctx: &mut Ctx<'_>) {
    let raft = ctx.shared.raft.read().unwrap();
    let val = state::raft_get(&raft, TASK_KEY);
    if val.is_empty() {
        // Go quirk kept: `strings.Split("", ",")` yields ONE empty item,
        // not zero -- clients written against the Go server rely on it.
        append_array(ctx.out, 1);
        append_bulk_string(ctx.out, "");
    } else {
        append_bulk_string(ctx.out, &val);
    }
}

/// `migrate task <slot> <src> <dst>` — one-command slot migration.
///
/// Drives the reshard protocol against the two nodes: `SETSLOT <slot>
/// MIGRATING` on the source, `IMPORTING` on the target, then drains the
/// slot through `GETKEYSINSLOT` + `MIGRATE` (the source performs the
/// DUMP/RESTORE transport itself), then `NODE` + `STABLE` on both. The
/// task is persisted (JSON, final status) to the raft `migrate_task`
/// key. One run at a time: a concurrent `migrate task` is refused.
async fn migrate_task(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 4 {
        migrate_helper(ctx);
        return;
    }
    let slot: u16 = match std::str::from_utf8(&ctx.args[1])
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(s) => s,
        None => {
            append_error(ctx.out, "ERR Invalid slot");
            return;
        }
    };
    let src = String::from_utf8_lossy(&ctx.args[2]).into_owned();
    let dst = String::from_utf8_lossy(&ctx.args[3]).into_owned();
    if ctx.shared.migrate_busy.swap(true, Ordering::SeqCst) {
        append_error(ctx.out, "BUSY a slot migration is already running");
        return;
    }
    let result = run_migration(ctx, slot, &src, &dst).await;
    ctx.shared.migrate_busy.store(false, Ordering::SeqCst);
    let json = task_json(slot, &src, &dst, &result);
    persist_task(ctx, &json).await;
    match &result {
        Ok(_) => append_string(ctx.out, "OK"),
        Err(e) => append_error(ctx.out, &format!("ERR {e}")),
    }
}

/// The protocol steps; `Ok(moved)` on success. Errors carry the failing
/// hop and the peer's reply verbatim (Redis error text preserved).
async fn run_migration(ctx: &Ctx<'_>, slot: u16, src: &str, dst: &str) -> Result<usize, String> {
    let src_id = crate::utils::md5_with40(src);
    let dst_id = crate::utils::md5_with40(dst);
    let (dst_host, dst_port) = split_addr(dst)?;
    let token = &ctx.shared.conf.raft_token;
    let slot_b = slot.to_string();
    let mut src_conn = client::connect_authed(src, token, STEP_TIMEOUT_MS)
        .await
        .map_err(|e| format!("src {src}: {e}"))?;
    let mut dst_conn = client::connect_authed(dst, token, STEP_TIMEOUT_MS)
        .await
        .map_err(|e| format!("dst {dst}: {e}"))?;

    cmd_ok(
        &mut src_conn,
        &[
            b"CLUSTER",
            b"SETSLOT",
            slot_b.as_bytes(),
            b"MIGRATING",
            dst_id.as_bytes(),
        ],
    )
    .await
    .map_err(|e| format!("src MIGRATING: {e}"))?;
    cmd_ok(
        &mut dst_conn,
        &[
            b"CLUSTER",
            b"SETSLOT",
            slot_b.as_bytes(),
            b"IMPORTING",
            src_id.as_bytes(),
        ],
    )
    .await
    .map_err(|e| format!("dst IMPORTING: {e}"))?;

    let mut moved = 0usize;
    for _ in 0..1024 {
        let keys = keys_in_slot(&mut src_conn, &slot_b)
            .await
            .map_err(|e| format!("src GETKEYSINSLOT: {e}"))?;
        if keys.is_empty() {
            break;
        }
        let mut args: Vec<Vec<u8>> = vec![
            b"MIGRATE".to_vec(),
            dst_host.as_bytes().to_vec(),
            dst_port.as_bytes().to_vec(),
            b"".to_vec(),
            b"0".to_vec(),
            STEP_TIMEOUT_MS.to_string().into_bytes(),
            b"KEYS".to_vec(),
        ];
        for k in &keys {
            args.push(k.clone());
        }
        let refs: Vec<&[u8]> = args.iter().map(|a| a.as_slice()).collect();
        cmd_ok(&mut src_conn, &refs)
            .await
            .map_err(|e| format!("src MIGRATE: {e}"))?;
        moved += keys.len();
    }

    cmd_ok(
        &mut src_conn,
        &[
            b"CLUSTER",
            b"SETSLOT",
            slot_b.as_bytes(),
            b"NODE",
            dst_id.as_bytes(),
        ],
    )
    .await
    .map_err(|e| format!("src NODE: {e}"))?;
    cmd_ok(
        &mut dst_conn,
        &[
            b"CLUSTER",
            b"SETSLOT",
            slot_b.as_bytes(),
            b"NODE",
            dst_id.as_bytes(),
        ],
    )
    .await
    .map_err(|e| format!("dst NODE: {e}"))?;
    cmd_ok(
        &mut src_conn,
        &[b"CLUSTER", b"SETSLOT", slot_b.as_bytes(), b"STABLE"],
    )
    .await
    .map_err(|e| format!("src STABLE: {e}"))?;
    cmd_ok(
        &mut dst_conn,
        &[b"CLUSTER", b"SETSLOT", slot_b.as_bytes(), b"STABLE"],
    )
    .await
    .map_err(|e| format!("dst STABLE: {e}"))?;
    Ok(moved)
}

/// Send one command and require a simple `+OK` (or propagate the peer's
/// error verbatim).
async fn cmd_ok(stream: &mut TcpStream, args: &[&[u8]]) -> Result<(), String> {
    client::send_command(stream, args).await?;
    match client::read_reply_timed(stream, STEP_TIMEOUT_MS).await? {
        Reply::Simple(_) => Ok(()),
        Reply::Error(e) => Err(e),
        other => Err(format!("unexpected reply {other:?}")),
    }
}

/// `CLUSTER GETKEYSINSLOT <slot> 1000` -> the top-level keys found.
async fn keys_in_slot(stream: &mut TcpStream, slot_b: &str) -> Result<Vec<Vec<u8>>, String> {
    client::send_command(
        stream,
        &[b"CLUSTER", b"GETKEYSINSLOT", slot_b.as_bytes(), b"1000"],
    )
    .await?;
    match client::read_reply_timed(stream, STEP_TIMEOUT_MS).await? {
        Reply::Array(items) => {
            let mut keys = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Reply::Bulk(b) => keys.push(b),
                    other => return Err(format!("unexpected GETKEYSINSLOT item {other:?}")),
                }
            }
            Ok(keys)
        }
        Reply::Error(e) => Err(e),
        other => Err(format!("unexpected GETKEYSINSLOT reply {other:?}")),
    }
}

/// `host:port` split (last colon, so IPv6 literals survive).
fn split_addr(addr: &str) -> Result<(&str, &str), String> {
    addr.rsplit_once(':')
        .ok_or_else(|| format!("ERR bad address {addr}"))
}

/// Final task document; `moved` or the failing error (JSON-escaped).
fn task_json(slot: u16, src: &str, dst: &str, result: &Result<usize, String>) -> String {
    match result {
        Ok(moved) => format!(
            r#"{{"slot":{slot},"src":"{src}","dst":"{dst}","status":"done","moved":{moved}}}"#
        ),
        Err(e) => format!(
            r#"{{"slot":{slot},"src":"{src}","dst":"{dst}","status":"failed","error":"{}"}}"#,
            e.replace('"', "\\\"")
        ),
    }
}

/// Best-effort raft replication of the task document (same pattern as
/// `CLUSTER SETSLOT NODE`); non-leader callers accept the local miss.
async fn persist_task(ctx: &Ctx<'_>, json: &str) {
    let entry = rtypes::RaftLogEntryData {
        key: TASK_KEY.to_string(),
        value: json.to_string(),
    };
    let started = {
        let mut raft = ctx.shared.raft.write().unwrap();
        state::raft_apply_start(&mut raft, &entry)
    };
    if let Ok(ticket) = started {
        let _ = state::raft_apply_await(ticket).await;
    }
}

/// `MIGRATE host port key db timeout [COPY] [REPLACE] [KEYS key ...]`.
///
/// Redis semantics: `db` is ignored (single logical DB), `timeout` is
/// milliseconds (0 = unbounded) and bounds each transport step. Keys are
/// migrated one at a time under the per-key latch; a transport or target
/// error aborts the remaining keys (already-moved keys stay moved, like
/// Redis). `COPY` keeps the source copy; `REPLACE` lets RESTORE overwrite
/// an existing target key.
async fn migrate_data(ctx: &mut Ctx<'_>) {
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::command::test_ctx;
    use crate::state::{testutil, Shared};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Every store-opening test holds the crate-wide lock for its whole
    /// lifetime (see `string::tests`): `shared_with` wipes the shared
    /// `/tmp/rdb-test-{pid}` root. Guard returned FIRST so it outlives the
    /// Shared (locals drop in reverse declaration order).
    fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
        let guard = crate::command::string::TEST_STORE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut conf = testutil::test_config();
        conf.bind = bind.to_string();
        (guard, testutil::shared_with(conf))
    }

    fn call(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
        let mut ctx = test_ctx(shared, vec![], argv, &mut out);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(handle(&mut ctx));
        out
    }

    /// `call` for `#[tokio::test]` bodies (no nested runtime).
    async fn call_async(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
        let mut ctx = test_ctx(shared, vec![], argv, &mut out);
        handle(&mut ctx).await;
        out
    }

    #[test]
    fn helper_is_an_error_reply() {
        let (_guard, shared) = shared_for("127.0.0.1:40401");
        let helper = b"-migrate [ list | task ]\r\n";
        assert_eq!(call(&shared, &[]), helper);
        assert_eq!(call(&shared, &[b"help"]), helper);
        // Unknown subcommands fall through to the DATA migrate: a lone
        // arg is an arity error (Redis text).
        assert_eq!(
            call(&shared, &[b"bogus"]),
            b"-ERR wrong number of arguments for 'migrate' command\r\n"
        );
        // Uppercase does not match the lowercase-only registry, so "TASK"
        // also falls through to the DATA command (arity error).
        assert_eq!(
            call(&shared, &[b"TASK"]),
            b"-ERR wrong number of arguments for 'migrate' command\r\n"
        );
    }

    #[test]
    fn task_arity_falls_back_to_helper() {
        let (_guard, shared) = shared_for("127.0.0.1:40404");
        assert_eq!(
            call(&shared, &[b"task", b"a", b"b"]),
            b"-migrate [ list | task ]\r\n"
        );
    }

    #[test]
    fn task_rejects_non_numeric_slot() {
        let (_guard, shared) = shared_for("127.0.0.1:40407");
        assert_eq!(
            call(&shared, &[b"task", b"abc", b"x", b"y"]),
            b"-ERR Invalid slot\r\n"
        );
    }

    #[test]
    fn task_busy_guard_refuses_concurrent_run() {
        let (_guard, shared) = shared_for("127.0.0.1:40408");
        shared.migrate_busy.store(true, Ordering::SeqCst);
        assert_eq!(
            call(&shared, &[b"task", b"1", b"x", b"y"]),
            b"-BUSY a slot migration is already running\r\n"
        );
        shared.migrate_busy.store(false, Ordering::SeqCst);
    }

    #[test]
    fn failed_task_records_json_and_releases_busy() {
        let (_guard, shared) = shared_for("127.0.0.1:40409");
        // 127.0.0.1:1 refuses instantly; the hop error names the source.
        let out = call(&shared, &[b"task", b"100", b"127.0.0.1:1", b"127.0.0.1:2"]);
        assert!(out.starts_with(b"-ERR src 127.0.0.1:1: "), "got {out:?}");
        // The guard is released even on failure.
        assert!(!shared.migrate_busy.load(Ordering::SeqCst));
        let list = call(&shared, &[b"list"]);
        assert!(list.starts_with(b"$"), "got {list:?}");
        let body =
            String::from_utf8_lossy(&list[list.iter().position(|&b| b == b'\r').unwrap() + 2..]);
        let body = body.trim_end_matches("\r\n").to_string();
        assert!(body.contains("\"slot\":100"), "got {body}");
        assert!(body.contains("\"src\":\"127.0.0.1:1\""), "got {body}");
        assert!(body.contains("\"status\":\"failed\""), "got {body}");
    }

    #[test]
    fn list_without_task_is_an_empty_bulk() {
        let (_guard, shared) = shared_for("127.0.0.1:40405");
        // Go quirk: an empty task list is an array holding one empty bulk.
        assert_eq!(call(&shared, &[b"list"]), b"*1\r\n$0\r\n\r\n");
    }

    /// Read one RESP command (array of bulk strings) from `stream`;
    /// `None` on a clean EOF (the peer closed).
    async fn read_cmd(stream: &mut TcpStream, line: &mut Vec<u8>) -> Option<Vec<Vec<u8>>> {
        if !read_cmd_line(stream, line).await {
            return None;
        }
        assert_eq!(line[0], b'*');
        let n: usize = std::str::from_utf8(&line[1..])
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            if !read_cmd_line(stream, line).await {
                return None;
            }
            assert_eq!(line[0], b'$');
            let len: usize = std::str::from_utf8(&line[1..])
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let mut data = vec![0u8; len];
            stream.read_exact(&mut data).await.unwrap();
            let mut crlf = [0u8; 2];
            stream.read_exact(&mut crlf).await.unwrap();
            assert_eq!(&crlf, b"\r\n");
            args.push(data);
        }
        Some(args)
    }

    /// `false` on EOF.
    async fn read_cmd_line(stream: &mut TcpStream, buf: &mut Vec<u8>) -> bool {
        buf.clear();
        let mut byte = [0u8; 1];
        loop {
            if stream.read_exact(&mut byte).await.is_err() {
                return false;
            }
            if byte[0] == b'\r' {
                let mut next = [0u8; 1];
                if stream.read_exact(&mut next).await.is_err() {
                    return false;
                }
                assert_eq!(next[0], b'\n');
                return true;
            }
            buf.push(byte[0]);
        }
    }

    /// Fake cluster node: AUTH +OK, then `SETSLOT`/`MIGRATE` -> +OK and
    /// `GETKEYSINSLOT` -> `keys` once, `*0` afterwards. SETSLOT
    /// subcommands are appended to `log`.
    async fn fake_node(
        keys: Vec<Vec<u8>>,
        log: Arc<Mutex<Vec<String>>>,
    ) -> (tokio::task::JoinHandle<()>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut line = Vec::new();
            let auth = read_cmd(&mut sock, &mut line).await.expect("auth frame");
            assert_eq!(auth[0], b"AUTH");
            sock.write_all(b"+OK\r\n").await.unwrap();
            let mut served = false;
            while let Some(cmd) = read_cmd(&mut sock, &mut line).await {
                // MIGRATE host port "" 0 timeout KEYS k1 ...: the empty
                // key + KEYS marker identify it (and the name is checked
                // so a missing command name cannot slip through).
                if cmd.len() >= 7 && cmd[0] == b"MIGRATE" && cmd[3].is_empty() && cmd[6] == b"KEYS"
                {
                    sock.write_all(b"+OK\r\n").await.unwrap();
                    continue;
                }
                assert_eq!(cmd[0], b"CLUSTER", "expected CLUSTER, got {:?}", cmd);
                match cmd[1].as_slice() {
                    // cmd = [CLUSTER, SETSLOT, slot, SUB, id?]
                    b"SETSLOT" => {
                        log.lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&cmd[3]).into_owned());
                        sock.write_all(b"+OK\r\n").await.unwrap();
                    }
                    b"GETKEYSINSLOT" => {
                        let reply = if served {
                            b"*0\r\n".to_vec()
                        } else {
                            served = true;
                            let mut out = format!("*{}\r\n", keys.len()).into_bytes();
                            for k in &keys {
                                out.extend_from_slice(format!("${}\r\n", k.len()).as_bytes());
                                out.extend_from_slice(k);
                                out.extend_from_slice(b"\r\n");
                            }
                            out
                        };
                        sock.write_all(&reply).await.unwrap();
                    }
                    other => {
                        panic!(
                            "unexpected cluster subcommand {:?}",
                            String::from_utf8_lossy(other)
                        );
                    }
                }
            }
        });
        (handle, addr)
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // TEST_STORE_LOCK guard must outlive Shared
    async fn task_orchestrates_full_reshard_protocol() {
        let (_guard, shared) = shared_for("127.0.0.1:40406");
        let log = Arc::new(Mutex::new(Vec::new()));
        let (src_srv, src) =
            fake_node(vec![b"k1".to_vec(), b"k2".to_vec()], Arc::clone(&log)).await;
        let (dst_srv, dst) = fake_node(vec![], Arc::clone(&log)).await;
        let out = call_async(&shared, &[b"task", b"100", src.as_bytes(), dst.as_bytes()]).await;
        assert_eq!(out, b"+OK\r\n");
        src_srv.await.unwrap();
        dst_srv.await.unwrap();
        // Strict protocol order across both peers (drain interleaves
        // GETKEYSINSLOT/MIGRATE between the MIGRATING and NODE phases).
        let order = {
            let log = log.lock().unwrap();
            log.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        };
        assert_eq!(
            order,
            vec![
                "MIGRATING".to_string(),
                "IMPORTING".to_string(),
                "NODE".to_string(),
                "NODE".to_string(),
                "STABLE".to_string(),
                "STABLE".to_string()
            ]
        );
        // The persisted task carries the moved count.
        let list = call_async(&shared, &[b"list"]).await;
        let body =
            String::from_utf8_lossy(&list[list.iter().position(|&b| b == b'\r').unwrap() + 2..]);
        let body = body.trim_end_matches("\r\n").to_string();
        assert!(body.contains("\"slot\":100"), "got {body}");
        assert!(body.contains("\"status\":\"done\""), "got {body}");
        assert!(body.contains("\"moved\":2"), "got {body}");
    }
}
