//! Process-level sweep of the backup listener's ENTIRE read-only surface
//! (driver; the ALLOWED table lives in `backup_surface_common/table.rs`,
//! seeding in `backup_surface_common/seed.rs`): `src/command/readonly.rs::ALLOWED` names 87
//! commands; this test drives EVERY one of them over the REAL backup
//! port (spawn_node_backup) in a table-driven loop, so the allowlist can
//! never silently shrink (a runtime set-equality assert against
//! `readonly::ALLOWED` backs that up).
//!
//! Coverage: 87 of the 87 ALLOWED commands exercised; skipped: NONE.
//! Every ALLOWED entry has a non-blocking form (XREAD is called without
//! BLOCK; the mutating stream relatives XIDLE/XREADGROUP/XACK are NOT in
//! ALLOWED).
//!
//! The backup listener is backed by its OWN store (`backup_store_path`),
//! separate from the normal listener's store -- writes seeded over the
//! NORMAL port (proving the gate is per-listener) are invisible on the
//! backup port, so every data read pins that command's MISSING-KEY reply
//! shape (`$-1`, `*0`, `:0` ...). A handful of ALLOWED commands
//! legitimately reply their OWN error on an empty store (VDIM/VSIM,
//! FT.INFO/FT.SEARCH, XINFO STREAM, XPENDING, bare EXEC/DISCARD) --
//! those errors prove the gate PASSED and dispatch reached the handler.
//! CLUSTER/RAFT/MIGRATE/RESTORE are NOT in ALLOWED; the negative-control
//! loop pins their exact gate error too.

mod backup_surface_common;
mod common;

use common::{cmd_one_shot, spawn_node_backup, wait_resp_ready, TOKEN};

use backup_surface_common::seed::{seed_all, wait_backup_ready};
use backup_surface_common::table::{check, needs_full, table};
use backup_surface_common::GATE;

/// One process, both listeners: seed every family on the NORMAL port,
/// sweep the whole ALLOWED table over the BACKUP port (with a
/// set-equality guard against `readonly::ALLOWED`), then prove the gate
/// still stops one write per family with the exact replica error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_listener_serves_every_allowed_read_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut node, backup) = spawn_node_backup(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    seed_all(&node.resp).await;
    wait_backup_ready(&backup, &node, 15).await;

    let rows = table();
    // The table may never drift from the allowlist: every ALLOWED name
    // present, nothing invented (the second XINFO variant row is exempt
    // -- it reuses the same ALLOWED name).
    let mut names: Vec<&str> = rows.iter().map(|(n, _, _)| *n).collect();
    names.sort_unstable();
    names.dedup();
    let mut allowed: Vec<&str> = rdb::command::readonly::ALLOWED.to_vec();
    allowed.sort_unstable();
    assert_eq!(names, allowed, "table() must cover ALLOWED exactly");

    let ctx = node.ctx();
    for (name, args, pred) in rows {
        let r = if needs_full(&pred) {
            common::lite::cmd_full_reply(&backup, TOKEN, args, 300).await
        } else {
            cmd_one_shot(&backup, TOKEN, args).await
        };
        check(name, &r, &pred, &ctx);
    }

    // Negative control: one mutating command per family (plus the admin
    // plane and the transactional write replay) -- each replies the exact
    // Redis-standard replica error line.
    let writes: Vec<&[&[u8]]> = vec![
        &[b"SET", b"sk", b"x"],
        &[b"SETBIT", b"sk", b"0", b"1"],
        &[b"APPEND", b"sk", b"x"],
        &[b"INCR", b"ctr"],
        &[b"GETSET", b"sk", b"x"],
        &[b"DEL", b"sk"],
        &[b"UNLINK", b"sk"],
        &[b"EXPIRE", b"sk", b"10"],
        &[b"PERSIST", b"sk"],
        &[b"RENAME", b"sk", b"sk2"],
        &[b"HSET", b"hk", b"f", b"v"],
        &[b"HDEL", b"hk", b"f1"],
        &[b"SADD", b"setk", b"m9"],
        &[b"SPOP", b"setk"],
        &[b"SMOVE", b"setk", b"setk2", b"m1"],
        &[b"ZADD", b"zsk", b"9", b"z"],
        &[b"ZREM", b"zsk", b"a"],
        &[b"LPUSH", b"lk", b"x"],
        &[b"LPOP", b"lk"],
        &[b"XADD", b"st/q1", b"*", b"f", b"v"],
        &[b"XGROUP", b"CREATE", b"st/q1", b"g2", b"0-0"],
        &[b"XACK", b"st/q1", b"g1", b"0-0"],
        &[b"JSON.SET", b"jk", b"$", b"1"],
        &[b"JSON.DEL", b"jk"],
        &[b"FT.CREATE", b"fidx2", b"SCHEMA", b"t", b"TEXT"],
        &[b"FT.ADD", b"fidx", b"doc2", b"body"],
        &[b"FT.DEL", b"fidx", b"doc1"],
        &[b"RESTORE", b"sk", b"0", b"\x00"],
        &[b"FLUSHDB"],
        &[b"BITOP", b"AND", b"dst", b"sk"],
        &[b"CLUSTER", b"INFO"],
        &[b"RAFT", b"GET", b"cluster_slots_stable_instances"],
        &[b"MIGRATE", b"127.0.0.1", b"1", b"sk", b"0", b"10"],
    ];
    for args in &writes {
        let r = cmd_one_shot(&backup, TOKEN, args).await;
        assert_eq!(
            r,
            GATE,
            "{} must reply the exact READONLY error",
            String::from_utf8_lossy(args[0])
        );
    }

    // The process survived the whole sweep.
    assert!(
        node.child.try_wait().unwrap().is_none(),
        "rdb must still be alive\n{ctx}"
    );
    node.kill_now();
}
