//! Raft state machine (Rust port of Go `internal/rcache/fsm.go` +
//! `cache.go`): an in-memory replicated string map.
//!
//! Semantics ported from Go:
//! - `FSM.Apply` unmarshals `rtypes.RaftLogEntryData` JSON and sets
//!   Key -> Value;
//! - `FSM.Snapshot` serializes the FULL map as JSON;
//! - `FSM.Restore` MERGES the snapshot into the current map: keys present
//!   in the snapshot are overwritten, keys absent from it survive
//!   (Go `cacheManager.UnMarshal`).
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, RwLock};

use openraft::storage::{RaftSnapshotBuilder, RaftStateMachine};
use openraft::{
    EntryPayload, LogId, OptionalSend, Snapshot, SnapshotMeta, StorageError, StorageIOError,
    StoredMembership,
};

use crate::rcache::store::{LogStore, StoredSnapshot};
use crate::rcache::{typ, Node, NodeId, SnapshotData, TypeConfig};

/// The replicated KV map handle shared with the application (Go `CM`):
/// cloneable before the state machine moves into `Raft::new`, so HTTP
/// `/get`, `raft get` and the sync loops can read committed entries live.
pub type KvMap = Arc<RwLock<HashMap<String, String>>>;

/// Replicated state shared with the application (Go `cacheManager`).
#[derive(Debug, Clone, Default)]
pub struct StateMachineData {
    pub last_applied_log_id: Option<LogId<NodeId>>,
    pub last_membership: StoredMembership<NodeId, Node>,
    /// The replicated KV map.
    pub kv: KvMap,
}

/// In-memory state machine; snapshots are persisted through [`LogStore`]
/// (meta in RocksDB, data file under `snapshots/`).
#[derive(Debug, Clone)]
pub struct StateMachine {
    pub data: StateMachineData,
    /// Suffix of snapshot ids; incremented per builder so ids stay unique.
    snapshot_idx: u64,
    store: LogStore,
}

impl StateMachine {
    /// Build a state machine on `store`, replaying the persisted snapshot
    /// if any (Go restart restores from the snapshot file).
    pub fn new(store: LogStore) -> Result<StateMachine, StorageError<NodeId>> {
        let mut sm = StateMachine {
            data: StateMachineData::default(),
            snapshot_idx: 0,
            store,
        };
        if let Some(snap) = sm.store.load_snapshot()? {
            sm.restore_(&snap)?;
        }
        Ok(sm)
    }

    /// Go `cacheManager.UnMarshal`: overwrite keys present in the
    /// snapshot, keep every other key.
    fn restore_(&mut self, snap: &StoredSnapshot) -> Result<(), StorageError<NodeId>> {
        let incoming: HashMap<String, String> = serde_json::from_slice(&snap.data)
            .map_err(|e| StorageIOError::read_snapshot(Some(snap.meta.signature()), &e))?;
        self.data.last_applied_log_id = snap.meta.last_log_id;
        self.data.last_membership = snap.meta.last_membership.clone();
        let mut kv = self.data.kv.write().unwrap();
        for (k, v) in incoming {
            kv.insert(k, v);
        }
        Ok(())
    }
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachine {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let last_applied_log = self.data.last_applied_log_id;
        let last_membership = self.data.last_membership.clone();

        // Go FSM.Snapshot: JSON of the full map.
        let kv_json = {
            let kv = self.data.kv.read().unwrap();
            serde_json::to_vec(&*kv).map_err(|e| StorageIOError::read_state_machine(&e))?
        };

        let snapshot_id = if let Some(last) = last_applied_log {
            format!("{}-{}", last, self.snapshot_idx)
        } else {
            format!("--{}", self.snapshot_idx)
        };

        let meta = SnapshotMeta {
            last_log_id: last_applied_log,
            last_membership,
            snapshot_id,
        };

        self.store.save_snapshot(&meta, &kv_json)?;

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(kv_json)),
        })
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, Node>), StorageError<NodeId>> {
        Ok((
            self.data.last_applied_log_id,
            self.data.last_membership.clone(),
        ))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<String>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = typ::Entry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let entries = entries.into_iter();
        let mut replies = Vec::with_capacity(entries.size_hint().0);

        for ent in entries {
            self.data.last_applied_log_id = Some(ent.log_id);

            let mut reply = String::new();

            match ent.payload {
                EntryPayload::Blank => {}
                EntryPayload::Normal(req) => {
                    // Go FSM.Apply: set Key -> Value, reply the value
                    // (Go replies the error of Set, which is always nil).
                    reply = req.value.clone();
                    let mut kv = self.data.kv.write().unwrap();
                    kv.insert(req.key, req.value);
                }
                EntryPayload::Membership(mem) => {
                    self.data.last_membership = StoredMembership::new(Some(ent.log_id), mem);
                }
            }

            replies.push(reply);
        }
        Ok(replies)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.snapshot_idx += 1;
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<SnapshotData>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, Node>,
        snapshot: Box<SnapshotData>,
    ) -> Result<(), StorageError<NodeId>> {
        let stored = StoredSnapshot {
            meta: meta.clone(),
            data: snapshot.into_inner(),
        };
        // Go semantics: merge (in practice openraft only installs a
        // snapshot when the local log is empty, so the map is empty too).
        self.restore_(&stored)?;
        self.store.save_snapshot(&stored.meta, &stored.data)?;
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        Ok(self.store.load_snapshot()?.map(|s| Snapshot {
            meta: s.meta.clone(),
            snapshot: Box::new(Cursor::new(s.data)),
        }))
    }
}

#[cfg(test)]
pub mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::path::Path;

    use openraft::{CommittedLeaderId, Entry, EntryPayload, LogId, Membership, SnapshotMeta};

    use super::*;
    use crate::rcache::store::open;
    use crate::rtypes::RaftLogEntryData;

    /// Test entry builder, shared with the store tests.
    pub fn test_entry(index: u64, term: u64) -> typ::Entry {
        Entry {
            log_id: LogId::new(CommittedLeaderId::new(term, 1), index),
            payload: EntryPayload::Normal(RaftLogEntryData {
                key: format!("k{index}"),
                value: format!("v{index}"),
            }),
        }
    }

    fn new_sm(dir: &Path) -> StateMachine {
        StateMachine::new(open(dir).unwrap()).unwrap()
    }

    fn kv(sm: &StateMachine) -> HashMap<String, String> {
        sm.data.kv.read().unwrap().clone()
    }

    fn snap_data(snap: &Snapshot<TypeConfig>) -> Vec<u8> {
        snap.snapshot.get_ref().clone()
    }

    #[tokio::test]
    async fn apply_inserts_kv_and_tracks_last_applied() {
        let dir = tempfile::tempdir().unwrap();
        let mut sm = new_sm(dir.path());

        let replies = sm
            .apply([test_entry(1, 1), test_entry(2, 1)])
            .await
            .unwrap();
        assert_eq!(replies, vec!["v1".to_string(), "v2".to_string()]);

        let expected = HashMap::from([
            ("k1".to_string(), "v1".to_string()),
            ("k2".to_string(), "v2".to_string()),
        ]);
        assert_eq!(kv(&sm), expected);
        assert_eq!(sm.applied_state().await.unwrap().0.unwrap().index, 2);
    }

    #[tokio::test]
    async fn apply_blank_and_membership_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut sm = new_sm(dir.path());

        let blank = Entry {
            log_id: LogId::new(CommittedLeaderId::new(1, 1), 3),
            payload: EntryPayload::Blank,
        };
        let membership = Entry {
            log_id: LogId::new(CommittedLeaderId::new(1, 1), 4),
            payload: EntryPayload::Membership(Membership::new(vec![BTreeSet::from([1])], ())),
        };
        sm.apply([blank, membership]).await.unwrap();

        let (last, mem) = sm.applied_state().await.unwrap();
        assert_eq!(last.unwrap().index, 4);
        assert_eq!(mem.log_id().as_ref().unwrap().index, 4);
        assert!(kv(&sm).is_empty());
    }

    #[tokio::test]
    async fn snapshot_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut sm = new_sm(dir.path());
        sm.apply([test_entry(1, 1), test_entry(2, 1)])
            .await
            .unwrap();

        let snap = sm
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();
        // snapshot data is JSON of the FULL map
        let parsed: HashMap<String, String> = serde_json::from_slice(&snap_data(&snap)).unwrap();
        assert_eq!(parsed, kv(&sm));
        assert_eq!(snap.meta.last_log_id.unwrap().index, 2);

        // persisted and readable back as the current snapshot
        let cur = sm.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(cur.meta.snapshot_id, snap.meta.snapshot_id);
        assert_eq!(snap_data(&cur), snap_data(&snap));
    }

    #[tokio::test]
    async fn install_snapshot_merges_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut sm = new_sm(dir.path());
        sm.apply([test_entry(1, 1)]).await.unwrap(); // k1 -> v1

        let meta = SnapshotMeta {
            last_log_id: Some(test_entry(5, 2).log_id),
            last_membership: Default::default(),
            snapshot_id: "installed".to_string(),
        };
        let data = Box::new(Cursor::new(br#"{"k9":"v9"}"#.to_vec()));
        sm.install_snapshot(&meta, data).await.unwrap();

        let map = kv(&sm);
        assert_eq!(
            map.get("k1"),
            Some(&"v1".to_string()),
            "key absent from snapshot must survive"
        );
        assert_eq!(map.get("k9"), Some(&"v9".to_string()));
        assert_eq!(sm.applied_state().await.unwrap().0.unwrap().index, 5);

        let cur = sm.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(cur.meta.snapshot_id, "installed");
    }

    #[tokio::test]
    async fn restart_restores_from_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut sm = new_sm(dir.path());
            sm.apply([test_entry(1, 1)]).await.unwrap();
            sm.get_snapshot_builder()
                .await
                .build_snapshot()
                .await
                .unwrap();
        }
        // reopen: state is rebuilt from the persisted snapshot
        let mut sm = new_sm(dir.path());
        assert_eq!(
            kv(&sm),
            HashMap::from([("k1".to_string(), "v1".to_string())])
        );
        assert_eq!(sm.applied_state().await.unwrap().0.unwrap().index, 1);
    }
}
