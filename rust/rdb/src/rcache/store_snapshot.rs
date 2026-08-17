//! Snapshot persistence for the raft log store: the `snapshots/` directory
//! holds at most one file, like Go's `raft.NewFileSnapshotStore(dir, 1, ...)`.
#![allow(clippy::result_large_err)]

use std::fs;

use openraft::{AnyError, ErrorSubject, ErrorVerb, StorageError, StorageIOError};
use serde::{Deserialize, Serialize};

use super::{io_err, LogStore, Meta, StorageResult, KEY_SNAPSHOT_META};
use crate::rcache::NodeId;

/// A persisted snapshot: openraft metadata plus the serialized state
/// machine (JSON of the full map, Go `cacheManager.Marshal`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StoredSnapshot {
    pub meta: Meta,
    pub data: Vec<u8>,
}

/// Path-safe file name for a snapshot, derived from its id.
pub(super) fn snapshot_file_name(meta: &Meta) -> String {
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let sanitized: String = meta
        .snapshot_id
        .chars()
        .map(|c| if ok(c) { c } else { '-' })
        .collect();
    format!("snap-{sanitized}.json")
}

impl LogStore {
    /// Error constructor for snapshot IO failures on `meta`.
    fn snap_err(meta: &Meta, verb: ErrorVerb, e: &std::io::Error) -> StorageError<NodeId> {
        io_err(
            ErrorSubject::Snapshot(Some(meta.signature())),
            verb,
            AnyError::new(e),
        )
    }

    /// Persist a snapshot: write the data file under `snapshots/`, delete
    /// older files so at most one remains (Go retention 1), then record the
    /// meta in the stable column family.
    pub fn save_snapshot(&self, meta: &Meta, data: &[u8]) -> StorageResult<()> {
        let err = |verb: ErrorVerb, e: &std::io::Error| Self::snap_err(meta, verb, e);

        let file_name = snapshot_file_name(meta);
        let path = self.snapshot_dir.join(&file_name);
        fs::write(&path, data).map_err(|e| err(ErrorVerb::Write, &e))?;

        // retention 1, like Go NewFileSnapshotStore(dir, 1, ...)
        for entry in fs::read_dir(&self.snapshot_dir).map_err(|e| err(ErrorVerb::Read, &e))? {
            let entry = entry.map_err(|e| err(ErrorVerb::Read, &e))?;
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                && *entry.file_name() != *file_name
            {
                fs::remove_file(entry.path()).map_err(|e| err(ErrorVerb::Delete, &e))?;
            }
        }

        let sig = meta.signature();
        self.put_cf_json(KEY_SNAPSHOT_META, meta, |e| {
            StorageIOError::write_snapshot(Some(sig.clone()), e)
        })?;
        self.flush(ErrorSubject::Snapshot(Some(sig)), ErrorVerb::Write)
    }

    /// Load the current snapshot: meta from the stable column family, data
    /// from `snapshots/`. `Ok(None)` if none has been saved yet.
    pub fn load_snapshot(&self) -> StorageResult<Option<StoredSnapshot>> {
        let meta = match self.get_cf_json(KEY_SNAPSHOT_META, |e| StorageIOError::read(e))? {
            Some(meta) => meta,
            None => return Ok(None),
        };
        let path = self.snapshot_dir.join(snapshot_file_name(&meta));
        let data = fs::read(&path).map_err(|e| Self::snap_err(&meta, ErrorVerb::Read, &e))?;
        Ok(Some(StoredSnapshot { meta, data }))
    }
}
