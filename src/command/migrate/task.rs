//! Admin plane of `MIGRATE`: the `migrate task <slot> <src> <dst>`
//! orchestration of the redis-cli `--cluster reshard` protocol and
//! `migrate list` (the raft-persisted task record). Dispatch lives in
//! [`super::handle`]; the data-plane transport in [`super::data`].

use std::sync::atomic::Ordering;

use tokio::net::TcpStream;

use crate::command::Ctx;
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

/// `migrate list` — the single most recent task as one JSON bulk.
pub(super) fn migrate_list(ctx: &mut Ctx<'_>) {
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
pub(super) async fn migrate_task(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 4 {
        super::migrate_helper(ctx);
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    use crate::command::migrate::test_support::{call, call_async, fake_node, shared_for};

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
