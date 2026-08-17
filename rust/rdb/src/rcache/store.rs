//! Raft log store on RocksDB (Rust port of Go `internal/rcache` storage).
//!
//! Layout under the data dir (Go `StorePath + "/" + Bind + "/raft"`):
//! RocksDB column families `logs` (entries keyed by big-endian u64 index)
//! and `store` (vote, last-purged log id, snapshot meta), plus
//! `snapshots/` holding at most one file, like Go's
//! `raft.NewFileSnapshotStore(dir, 1, ...)`.
#![allow(clippy::result_large_err)]

use std::fmt::Debug;
use std::fs;
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openraft::storage::{LogFlushed, LogState, RaftLogStorage};
use openraft::{
    AnyError, Entry, ErrorSubject, ErrorVerb, LogId, OptionalSend, RaftLogReader, SnapshotMeta,
    StorageError, StorageIOError, Vote,
};
use rocksdb::{ColumnFamily, ColumnFamilyDescriptor, Direction, IteratorMode, Options, DB};

use crate::rcache::{Node, NodeId, TypeConfig};

#[path = "store_snapshot.rs"]
mod store_snapshot;

pub use store_snapshot::StoredSnapshot;

type StorageResult<T> = Result<T, StorageError<NodeId>>;
type Meta = SnapshotMeta<NodeId, Node>;

/// Column families: log entries (big-endian u64 index keys) / stable state.
const CF_LOGS: &str = "logs";
const CF_STORE: &str = "store";

const KEY_VOTE: &[u8] = b"vote";
const KEY_LAST_PURGED: &[u8] = b"last_purged_log_id";
const KEY_COMMITTED: &[u8] = b"committed_log_id";
const KEY_SNAPSHOT_META: &[u8] = b"snapshot_meta";

/// Raft log store. Cheap to clone: the RocksDB handle is shared via `Arc`,
/// and a clone is what `RaftLogStorage::get_log_reader` hands out.
#[derive(Debug, Clone)]
pub struct LogStore {
    db: Arc<DB>,
    /// `<data_dir>/snapshots`; retention is 1, like the Go original.
    snapshot_dir: PathBuf,
}

/// Big-endian index encoding so RocksDB key order matches log order.
fn id_to_bin(id: u64) -> [u8; 8] {
    id.to_be_bytes()
}

fn bin_to_id(buf: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[..8]);
    u64::from_be_bytes(b)
}

/// Wrap any error source into a storage IO error.
fn io_err(subject: ErrorSubject<NodeId>, verb: ErrorVerb, e: AnyError) -> StorageError<NodeId> {
    StorageError::IO {
        source: StorageIOError::new(subject, verb, e),
    }
}

/// Open (or create) the raft storage under `data_dir`. The caller computes
/// `data_dir`; nothing here knows about StorePath/Bind.
pub fn open<P: AsRef<Path>>(data_dir: P) -> StorageResult<LogStore> {
    let data_dir = data_dir.as_ref();
    let mut db_opts = Options::default();
    db_opts.create_if_missing(true);
    db_opts.create_missing_column_families(true);

    let logs = ColumnFamilyDescriptor::new(CF_LOGS, Options::default());
    let stable = ColumnFamilyDescriptor::new(CF_STORE, Options::default());
    let db = DB::open_cf_descriptors(&db_opts, data_dir, vec![logs, stable])
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, AnyError::new(&e)))?;

    let snapshot_dir = data_dir.join("snapshots");
    fs::create_dir_all(&snapshot_dir).map_err(|e| {
        io_err(
            ErrorSubject::Snapshot(None),
            ErrorVerb::Write,
            AnyError::new(&e),
        )
    })?;

    Ok(LogStore {
        db: Arc::new(db),
        snapshot_dir,
    })
}

impl LogStore {
    fn logs(&self) -> &ColumnFamily {
        self.db.cf_handle(CF_LOGS).unwrap()
    }

    fn store(&self) -> &ColumnFamily {
        self.db.cf_handle(CF_STORE).unwrap()
    }

    /// Fsync the RocksDB WAL. Only vote and snapshot meta are fsynced; log
    /// appends and the committed pointer rely on the OS-page-cache-backed
    /// WAL (survive process crash, not power loss). Deviation from Go's
    /// bolt-store sync-every-write, required because openraft 0.9.25 bounds
    /// replication RPCs by heartbeat_interval; fsync here is 30-350ms.
    fn flush(&self, subject: ErrorSubject<NodeId>, verb: ErrorVerb) -> StorageResult<()> {
        self.db
            .flush_wal(true)
            .map_err(|e| io_err(subject, verb, AnyError::new(&e)))
    }

    fn get_cf_json<T: serde::de::DeserializeOwned>(
        &self,
        key: &[u8],
        err: impl FnOnce(&rocksdb::Error) -> StorageIOError<NodeId>,
    ) -> StorageResult<Option<T>> {
        Ok(self
            .db
            .get_cf(self.store(), key)
            .map_err(|e| err(&e))?
            .and_then(|v| serde_json::from_slice(&v).ok()))
    }

    fn put_cf_json<T: serde::Serialize>(
        &self,
        key: &[u8],
        val: &T,
        err: impl FnOnce(&rocksdb::Error) -> StorageIOError<NodeId>,
    ) -> StorageResult<()> {
        self.db
            .put_cf(self.store(), key, serde_json::to_vec(val).unwrap())
            .map_err(|e| err(&e))?;
        Ok(())
    }

    /// Write log entries; shared by [`RaftLogStorage::append`] and tests.
    fn append_(&self, entries: impl IntoIterator<Item = Entry<TypeConfig>>) -> StorageResult<()> {
        for entry in entries {
            self.db
                .put_cf(
                    self.logs(),
                    id_to_bin(entry.log_id.index),
                    serde_json::to_vec(&entry).map_err(|e| StorageIOError::write_logs(&e))?,
                )
                .map_err(|e| StorageIOError::write_logs(&e))?;
        }
        Ok(())
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> StorageResult<Vec<Entry<TypeConfig>>> {
        let start = match range.start_bound() {
            Bound::Included(x) => id_to_bin(*x),
            Bound::Excluded(x) => id_to_bin(*x + 1),
            Bound::Unbounded => id_to_bin(0),
        };
        self.db
            .iterator_cf(self.logs(), IteratorMode::From(&start, Direction::Forward))
            .map(|res| {
                let (id, val) = res.map_err(|e| StorageIOError::read_logs(&e))?;
                let entry: Entry<TypeConfig> =
                    serde_json::from_slice(&val).map_err(|e| StorageIOError::read_logs(&e))?;
                Ok((bin_to_id(&id), entry))
            })
            .take_while(|res| matches!(res, Ok((id, _)) if range.contains(id)))
            .map(|res| res.map(|(_, entry)| entry))
            .collect()
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> StorageResult<LogState<TypeConfig>> {
        let mut iter = self.db.iterator_cf(self.logs(), IteratorMode::End);
        let last = match iter.next() {
            Some(res) => {
                let (_, ent) = res.map_err(|e| StorageIOError::read_logs(&e))?;
                Some(
                    serde_json::from_slice::<Entry<TypeConfig>>(&ent)
                        .map_err(|e| StorageIOError::read_logs(&e))?
                        .log_id,
                )
            }
            None => None,
        };
        drop(iter);

        let last_purged_log_id = self.get_cf_json(KEY_LAST_PURGED, |e| StorageIOError::read(e))?;
        Ok(LogState {
            last_purged_log_id,
            last_log_id: last.or(last_purged_log_id),
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> StorageResult<()> {
        self.put_cf_json(KEY_VOTE, vote, |e| StorageIOError::write_vote(e))?;
        self.flush(ErrorSubject::Vote, ErrorVerb::Write)
    }

    async fn read_vote(&mut self) -> StorageResult<Option<Vote<NodeId>>> {
        self.get_cf_json(KEY_VOTE, |e| StorageIOError::read_vote(e))
    }

    /// Persist the committed log id; openraft rebuilds `committed` from it
    /// on restart/leadership rebuild (the trait default persists nothing,
    /// which breaks follower catch-up state — the reference rocksdb example
    /// persists it too).
    async fn save_committed(&mut self, committed: Option<LogId<NodeId>>) -> StorageResult<()> {
        self.put_cf_json(KEY_COMMITTED, &committed, |e| StorageIOError::write(e))
    }

    async fn read_committed(&mut self) -> StorageResult<Option<LogId<NodeId>>> {
        self.get_cf_json(KEY_COMMITTED, |e| StorageIOError::read(e))
    }

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>) -> StorageResult<()>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        self.append_(entries)?;
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    /// Delete logs from `log_id.index`, inclusive, to the end.
    async fn truncate(&mut self, log_id: LogId<NodeId>) -> StorageResult<()> {
        self.db
            .delete_range_cf(self.logs(), id_to_bin(log_id.index), id_to_bin(u64::MAX))
            .map_err(|e| StorageIOError::write_logs(&e))?;
        Ok(())
    }

    /// Delete logs from the start up to `log_id.index`, inclusive.
    async fn purge(&mut self, log_id: LogId<NodeId>) -> StorageResult<()> {
        self.put_cf_json(KEY_LAST_PURGED, &log_id, |e| StorageIOError::write(e))?;
        self.db
            .delete_range_cf(self.logs(), id_to_bin(0), id_to_bin(log_id.index + 1))
            .map_err(|e| StorageIOError::write_logs(&e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use openraft::Vote;

    use super::store_snapshot::snapshot_file_name;
    use super::*;
    use crate::rcache::fsm::tests::test_entry;

    fn open_tmp() -> (tempfile::TempDir, LogStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).unwrap();
        (dir, store)
    }

    async fn indexes(store: &mut LogStore) -> Vec<u64> {
        store
            .try_get_log_entries(..)
            .await
            .unwrap()
            .iter()
            .map(|e| e.log_id.index)
            .collect()
    }

    #[tokio::test]
    async fn stable_state_roundtrip() {
        let (_dir, mut store) = open_tmp();
        assert!(store.read_vote().await.unwrap().is_none());
        assert!(store.load_snapshot().unwrap().is_none());
        let vote = Vote::new_committed(3, 1);
        store.save_vote(&vote).await.unwrap();
        assert_eq!(store.read_vote().await.unwrap(), Some(vote));
    }

    #[tokio::test]
    async fn committed_roundtrip() {
        let (_dir, mut store) = open_tmp();
        assert!(store.read_committed().await.unwrap().is_none());
        let committed = Some(test_entry(3, 2).log_id);
        store.save_committed(committed).await.unwrap();
        assert_eq!(store.read_committed().await.unwrap(), committed);
        store.save_committed(None).await.unwrap();
        assert!(store.read_committed().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn append_read_purge_truncate() {
        let (_dir, mut store) = open_tmp();
        store.append_((0..6).map(|i| test_entry(i, 1))).unwrap();
        assert_eq!(indexes(&mut store).await, vec![0, 1, 2, 3, 4, 5]);

        let got = store.try_get_log_entries(2..4).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].log_id.index, 2);
        assert_eq!(got[1].log_id.index, 3);

        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id.unwrap().index, 5);
        assert_eq!(state.last_purged_log_id, None);

        store.truncate(test_entry(4, 1).log_id).await.unwrap(); // drop [4, +oo)
        assert_eq!(indexes(&mut store).await, vec![0, 1, 2, 3]);

        store.purge(test_entry(1, 1).log_id).await.unwrap(); // drop [0, 1]
        assert_eq!(indexes(&mut store).await, vec![2, 3]);
        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id.unwrap().index, 1);
        assert_eq!(state.last_log_id.unwrap().index, 3);
    }

    #[tokio::test]
    async fn snapshot_retention_and_roundtrip() {
        let (dir, store) = open_tmp();
        let meta = |idx: u64| Meta {
            last_log_id: Some(test_entry(idx, 1).log_id),
            last_membership: Default::default(),
            snapshot_id: format!("snap-{idx}"),
        };
        store.save_snapshot(&meta(1), br#"{"a":"1"}"#).unwrap();
        store.save_snapshot(&meta(2), br#"{"a":"2"}"#).unwrap();

        // retention 1: only the latest file survives
        let files: Vec<_> = fs::read_dir(dir.path().join("snapshots"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files, vec![snapshot_file_name(&meta(2))]);

        let loaded = store.load_snapshot().unwrap().unwrap();
        assert_eq!(loaded.meta.snapshot_id, "snap-2");
        assert_eq!(loaded.data, br#"{"a":"2"}"#);
    }
}
