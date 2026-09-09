//! `FLUSHDB`: chunked wipe of every user record in the local keyspace
//! (control-plane records preserved), serialized against the lite
//! offset flusher so no orphan stream-group records survive the wipe.

use rocksdb::WriteBatch;

use crate::command::keyspace_role::{record_role, RecordRole, FLUSH_PAGE};
use crate::command::string;
use crate::command::Ctx;
use crate::resp::codec::append_error;
use crate::resp::codec::append_string;
use crate::store::ops;

pub async fn flushdb(ctx: &mut Ctx<'_>) {
    if ctx.args.len() > 1 {
        string::arity(ctx.out, "flushdb");
        return;
    }
    if ctx.args.len() == 1 {
        let mode = ctx.args[0].to_ascii_lowercase();
        if mode.as_slice() != b"async" && mode.as_slice() != b"sync" {
            append_error(ctx.out, "ERR syntax error");
            return;
        }
    }
    // Serialize with any in-flight offset flush round FIRST (race
    // window a): rounds hold exactly the dirty streams' latches across
    // their awaited write, so taking those latches here waits out a
    // round that already passed `drop_superseded` and is mid-write --
    // its group records then land BEFORE the wipe and are deleted with
    // everything else, instead of landing after it and surviving as
    // orphans on the wiped keyspace.
    let latch_keys = crate::lite::dirty_latch_keys(ctx.shared);
    let mut guards = Vec::with_capacity(latch_keys.len());
    for key in &latch_keys {
        guards.push(crate::ds::latch::lock(&ctx.shared.latch, key).await);
    }
    // Every stream is being wiped, so every cached lite group state is
    // stale: drop the offset cache BEFORE scanning. A dirty entry left
    // behind would be re-written by the next background flush round and
    // resurrect an orphan group record on the wiped keyspace (the cache
    // is read-through; dropping it only costs a reload on next use).
    crate::lite::offset::clear_all(&ctx.shared.lite.offsets);
    crate::lite::ordered::clear(&ctx.shared.lite.owners);
    let mut cursor: Vec<u8> = Vec::new();
    loop {
        let mut chunk: Vec<Vec<u8>> = Vec::with_capacity(FLUSH_PAGE);
        let mut resume: Option<Vec<u8>> = None;
        let store = std::sync::Arc::clone(&ctx.shared.store);
        let scanned = ops::for_each_from(&store, &cursor, !cursor.is_empty(), &mut |k, v| {
            if matches!(record_role(k, v), RecordRole::Foreign) {
                return true; // control-plane: preserved
            }
            chunk.push(k.to_vec());
            if chunk.len() >= FLUSH_PAGE {
                resume = Some(k.to_vec());
                return false;
            }
            true
        });
        if let Err(e) = scanned {
            append_error(ctx.out, &format!("ERR: flushdb failed: {e}"));
            return;
        }
        if chunk.is_empty() {
            break; // scan exhausted
        }
        let mut batch = WriteBatch::default();
        for key in &chunk {
            batch.delete(key);
        }
        if let Err(e) = ctx.commit(batch).await {
            append_error(ctx.out, &format!("ERR: flushdb failed: {e}"));
            return;
        }
        match resume {
            Some(next) => cursor = next,
            None => break, // tail chunk committed: nothing left
        }
    }
    // Second clear (race window b): a client racing the chunked wipe
    // (XGROUP CREATE / XACK between chunks) re-marks the cache dirty;
    // those marks describe streams whose records the wipe just deleted,
    // so the next flush round must not write them back.
    crate::lite::offset::clear_all(&ctx.shared.lite.offsets);
    crate::lite::ordered::clear(&ctx.shared.lite.owners);
    drop(guards);
    append_string(ctx.out, "OK");
}
