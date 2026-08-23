//! M5 background orphan/garbage sweep for columnar segments.
//!
//! Source of truth is the store's 0x23 metas, never the in-memory
//! registry. One sweep walks every meta key and classifies it:
//!
//! - undecodable meta or malformed key -> garbage;
//! - meta whose table is no longer in the raft catalog -> garbage
//!   (crash between the catalog tombstone and the meta-delete batch
//!   of `drop_table_segments`, or a decide(commit) that raced a DROP);
//! - `Prepared` meta referenced by NO in-doubt 2PC marker -> garbage
//!   once its file is missing or older than [`ORPHAN_MIN_AGE`] (a
//!   crash after Prepare, before Decide). A marker-referenced meta
//!   belongs to an undecided txn and is kept regardless of age;
//! - everything else is kept and its file joins the referenced set.
//!
//! Garbage metas go out in one WriteBatch, the registry is resynced
//! (remove garbage, idempotently re-insert every kept Live meta),
//! then the directory is walked: unreferenced `.col`/`.tmp` files
//! older than [`ORPHAN_MIN_AGE`] are unlinked. The age floor keeps
//! files of in-flight commits safe (the rename lands before the meta
//! batch does). Mirrors `storage::gc`: sync sweep on the blocking
//! pool, quiet rounds log nothing.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rocksdb::WriteBatch;

use super::meta::{self, SegmentMeta, SegmentState};
use super::{registry_of, writer, Registry};
use crate::sql::dist::participant;
use crate::sql::storage::catalog;
use crate::sql::storage::codec::KIND_SQL_SEGMENT;
use crate::state::Shared;
use crate::store::{ops, Store};

/// Same cadence as the MVCC watermark GC.
pub const SWEEP_PERIOD: Duration = Duration::from_secs(30);
/// Files younger than this are possibly mid-commit (file rename
/// precedes the meta batch); only older unreferenced files are
/// orphans.
pub const ORPHAN_MIN_AGE: Duration = Duration::from_secs(3600);

/// Outcome of one sweep round.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepStats {
    pub metas_deleted: u64,
    pub files_deleted: u64,
}

/// One synchronous sweep round (runs on the blocking pool).
pub fn sweep(shared: &Shared, orphan_age: Duration) -> Result<SweepStats, String> {
    sweep_core(
        &shared.store,
        &shared.raft,
        &shared.conf,
        &registry_of(shared),
        orphan_age,
    )
}

/// The sweep itself; [`sweep`] is the `Shared`-shaped wrapper that
/// resolves the process-wide registry.
fn sweep_core(
    store: &Store,
    raft: &std::sync::RwLock<crate::state::RaftState>,
    conf: &crate::conf::Config,
    registry: &Registry,
    orphan_age: Duration,
) -> Result<SweepStats, String> {
    let dir = writer::columnar_dir(conf);
    if !dir.exists() {
        return Ok(SweepStats::default());
    }
    let in_doubt: BTreeSet<Vec<u8>> = participant::in_doubt_keys(store).into_iter().collect();
    let mut garbage: Vec<Vec<u8>> = Vec::new();
    let mut garbage_ids: Vec<(u32, u64)> = Vec::new();
    let mut kept_live: Vec<SegmentMeta> = Vec::new();
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    ops::for_each_from(store, &[KIND_SQL_SEGMENT], false, &mut |key, value| {
        if key.first() != Some(&KIND_SQL_SEGMENT) {
            return false; // past the 0x23 region
        }
        let (id, meta) = match (meta::parse_meta_key(key), meta::decode_meta(value)) {
            (Some(id), Ok(m)) => (id, m),
            _ => {
                garbage.push(key.to_vec()); // undecodable -> garbage
                return true;
            }
        };
        // Tables gone from the catalog: drop crashed before the
        // meta-delete batch, or a decide(commit) raced a DROP.
        if !matches!(catalog::lookup_raft(raft, &meta.table_name), Ok(Some(_))) {
            garbage.push(key.to_vec());
            garbage_ids.push(id);
            return true;
        }
        if meta.state == SegmentState::Prepared
            && !in_doubt.contains(key)
            && file_stale(&dir, &meta.file, orphan_age)
        {
            garbage.push(key.to_vec());
            garbage_ids.push(id);
            return true;
        }
        referenced.insert(meta.file.clone());
        if meta.state == SegmentState::Live {
            kept_live.push(meta);
        }
        true
    })?;
    if !garbage.is_empty() {
        let mut batch = WriteBatch::default();
        for key in &garbage {
            batch.delete(key);
        }
        ops::batch_write(store, batch)?;
    }
    // Registry hygiene (store already matches): forget garbage,
    // idempotently re-insert every kept Live meta (self-heal).
    for (table_id, segment_id) in garbage_ids {
        registry.remove(table_id, &[segment_id]);
    }
    for meta in &kept_live {
        registry.insert(meta);
    }
    let files_deleted = sweep_files(&dir, &referenced, orphan_age);
    Ok(SweepStats {
        metas_deleted: garbage.len() as u64,
        files_deleted,
    })
}

/// True when `file` under `dir` is missing or its mtime is at least
/// `orphan_age` old; an unreadable mtime counts as young (conservative).
fn file_stale(dir: &Path, file: &str, orphan_age: Duration) -> bool {
    let md = match std::fs::metadata(dir.join(file)) {
        Ok(md) => md,
        Err(e) => return e.kind() == std::io::ErrorKind::NotFound,
    };
    md.modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age >= orphan_age)
}

/// Unlink unreferenced `.col`/`.tmp` files older than `orphan_age`;
/// unreadable mtimes are kept. Returns the number of files removed.
fn sweep_files(dir: &Path, referenced: &BTreeSet<String>, orphan_age: Duration) -> u64 {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("[columnar-gc] read_dir {}: {e}", dir.display());
            return 0;
        }
    };
    let mut deleted = 0u64;
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(name) = fname.to_str() else {
            continue;
        };
        if !(name.ends_with(".col") || name.ends_with(".tmp")) || referenced.contains(name) {
            continue;
        }
        let aged = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= orphan_age);
        if aged {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => deleted += 1,
                Err(e) => eprintln!("[columnar-gc] remove {}: {e}", entry.path().display()),
            }
        }
    }
    deleted
}

/// Periodic columnar sweep (spawned from main.rs next to the MVCC GC
/// loop). Each round parks the sync sweep on tokio's blocking pool;
/// quiet rounds log nothing.
pub async fn run_sweep_loop(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(SWEEP_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // consume the immediate first tick
    loop {
        ticker.tick().await;
        let shared = Arc::clone(&shared);
        match tokio::task::spawn_blocking(move || sweep(&shared, ORPHAN_MIN_AGE)).await {
            Err(e) => eprintln!("[columnar-gc] sweep task failed: {e}"),
            Ok(Err(e)) => eprintln!("[columnar-gc] sweep failed: {e}"),
            Ok(Ok(s)) if s.metas_deleted + s.files_deleted > 0 => eprintln!(
                "[columnar-gc] swept {} metas, {} orphan files",
                s.metas_deleted, s.files_deleted
            ),
            Ok(Ok(_)) => {}
        }
    }
}

/// main.rs hook: run [`run_sweep_loop`] on the normal listener's
/// engine (the backup listener is read-only by design and spawns no
/// data-plane tasks).
pub fn spawn_columnar_gc(shared: Arc<Shared>) {
    tokio::spawn(run_sweep_loop(shared));
}
