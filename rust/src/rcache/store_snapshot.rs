//! Snapshot persistence for the raft log store: the `snapshots/` directory
//! holds at most one file, like Go's `raft.NewFileSnapshotStore(dir, 1, ...)`.
#![allow(clippy::result_large_err)]

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

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

/// Write `data` to `path` DURABLY: fsync the file, then the parent
/// directory so the file's directory entry survives a crash — callers
/// persist a meta record pointing at this file and must never observe
/// the meta outliving the file it names.
fn write_durably(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
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

    /// Persist a snapshot in crash-safe order: (1) write + fsync the data
    /// file under `snapshots/`, (2) record the meta in the stable column
    /// family and flush, (3) only then delete older files (Go retention
    /// 1). Deleting BEFORE the meta flush used to leave the still-current
    /// old meta pointing at a removed file, crash-looping `load_snapshot`
    /// forever.
    pub fn save_snapshot(&self, meta: &Meta, data: &[u8]) -> StorageResult<()> {
        let err = |verb: ErrorVerb, e: &std::io::Error| Self::snap_err(meta, verb, e);

        let file_name = snapshot_file_name(meta);
        let path = self.snapshot_dir.join(&file_name);
        // (1) data file durable before anything references it.
        write_durably(&path, data).map_err(|e| err(ErrorVerb::Write, &e))?;

        // (2) meta record durable: stable CF + WAL flush.
        let sig = meta.signature();
        self.put_cf_json(KEY_SNAPSHOT_META, meta, |e| {
            StorageIOError::write_snapshot(Some(sig.clone()), e)
        })?;
        self.flush(ErrorSubject::Snapshot(Some(sig)), ErrorVerb::Write)?;

        // (3) retention 1, like Go NewFileSnapshotStore(dir, 1, ...):
        // with the new meta durable, older files are unreferenced.
        for entry in fs::read_dir(&self.snapshot_dir).map_err(|e| err(ErrorVerb::Read, &e))? {
            let entry = entry.map_err(|e| err(ErrorVerb::Read, &e))?;
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                && *entry.file_name() != *file_name
            {
                fs::remove_file(entry.path()).map_err(|e| err(ErrorVerb::Delete, &e))?;
            }
        }
        Ok(())
    }

    /// Load the current snapshot: meta from the stable column family, data
    /// from `snapshots/`. `Ok(None)` if none has been saved yet.
    pub fn load_snapshot(&self) -> StorageResult<Option<StoredSnapshot>> {
        let meta = match self.get_cf_json(KEY_SNAPSHOT_META, |e| {
            io_err(ErrorSubject::Snapshot(None), ErrorVerb::Read, e)
        })? {
            Some(meta) => meta,
            None => return Ok(None),
        };
        let path = self.snapshot_dir.join(snapshot_file_name(&meta));
        let data = fs::read(&path).map_err(|e| Self::snap_err(&meta, ErrorVerb::Read, &e))?;
        Ok(Some(StoredSnapshot { meta, data }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::open;
    use super::*;
    use crate::rcache::fsm::tests::test_entry;

    fn meta(idx: u64) -> Meta {
        Meta {
            last_log_id: Some(test_entry(idx, 1).log_id),
            last_membership: Default::default(),
            snapshot_id: format!("snap-{idx}"),
        }
    }

    /// The save order must keep the durable meta pointing at a LIVE file
    /// at every step: after each save the file exists and loads, and two
    /// saves leave exactly one (newest) file behind. (The crash window
    /// itself — old meta + deleted old file — can't be injected at unit
    /// level; the fixed order data-durable -> meta-durable -> delete is
    /// what closes it.)
    #[test]
    fn save_snapshot_keeps_meta_pointing_at_a_live_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(dir.path()).unwrap();

        store.save_snapshot(&meta(1), br#"{"a":"1"}"#).unwrap();
        // Meta 1 is durable and its file exists: a load right here (a
        // crash after step 2 but before retention) must succeed.
        let loaded = store.load_snapshot().unwrap().expect("snapshot 1");
        assert_eq!(loaded.meta.snapshot_id, "snap-1");
        assert_eq!(loaded.data, br#"{"a":"1"}"#.to_vec());
        assert!(dir
            .path()
            .join("snapshots")
            .join(snapshot_file_name(&meta(1)))
            .exists());

        store.save_snapshot(&meta(2), br#"{"a":"2"}"#).unwrap();
        let files: Vec<String> = fs::read_dir(dir.path().join("snapshots"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files, vec![snapshot_file_name(&meta(2))]);
        let loaded = store.load_snapshot().unwrap().expect("snapshot 2");
        assert_eq!(loaded.meta.snapshot_id, "snap-2");
        assert_eq!(loaded.data, br#"{"a":"2"}"#.to_vec());
    }

    /// `write_durably` lands the full contents on disk (and fsyncs the
    /// file plus its directory without error).
    #[test]
    fn write_durably_roundtrips_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("snap-x.json");
        write_durably(&path, br#"{"k":"v"}"#).unwrap();
        assert_eq!(fs::read(&path).unwrap(), br#"{"k":"v"}"#.to_vec());
    }
}
