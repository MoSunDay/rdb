//! Columnar table engine: append-only, versioned segment files.
//!
//! Data lives in immutable files under `<store_path>/<bind>/columnar/`
//! (never inside RocksDB, whose LSM compaction would rewrite them);
//! RocksDB holds only the small segment meta records (kind 0x23).
//! M1 scope: the on-disk segment format (magic | column pages |
//! JSON footer | footer_len | crc32) plus PLAIN and string DICT page
//! encodings with per-page zonemaps.
//! M2 scope: append-only write path -- INSERT buffers rows in the
//! transaction and each COMMIT flushes one immutable segment per table,
//! publishing its 0x23 meta in the SAME atomic batch as the txn's row
//! writes. Visibility stays segment-level (`commit_ts <= read_ts`,
//! decided at read time, M3); the [`Registry`] caches live metas.
//! M3 scope: the read/scan path -- `reader` decodes the Live segments
//! with `commit_ts <= read_ts` (plus an open txn's staged appends) in
//! `(commit_ts, segment_id)` order, and dist fans columnar reads out
//! to every cluster member.
pub mod commit;
pub mod decode;
pub mod encode;
pub mod format;
pub mod meta;
pub mod reader;
pub mod writer;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use meta::SegmentMeta;

/// In-memory cache of segment metas, rebuilt from RocksDB at startup
/// and kept current by every commit/decide. Reads never scan RocksDB.
#[derive(Default)]
pub struct Registry {
    inner: RwLock<BTreeMap<u32, Vec<SegmentMeta>>>,
}

impl Registry {
    /// Cloned metas of one table, sorted by `segment_id`.
    pub fn segments(&self, table_id: u32) -> Vec<SegmentMeta> {
        self.inner
            .read()
            .unwrap()
            .get(&table_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Upsert by `(table_id, segment_id)`, keeping the sorted order.
    pub fn insert(&self, meta: &SegmentMeta) {
        let mut map = self.inner.write().unwrap();
        let list = map.entry(meta.table_id).or_default();
        match list.binary_search_by_key(&meta.segment_id, |m| m.segment_id) {
            Ok(i) => list[i] = meta.clone(),
            Err(i) => list.insert(i, meta.clone()),
        }
    }

    /// Drop the listed segments of one table (2PC abort, drop table).
    pub fn remove(&self, table_id: u32, segment_ids: &[u64]) {
        let mut map = self.inner.write().unwrap();
        if let Some(list) = map.get_mut(&table_id) {
            list.retain(|m| !segment_ids.contains(&m.segment_id));
            if list.is_empty() {
                map.remove(&table_id);
            }
        }
    }

    /// Forget every segment of one table.
    pub fn drop_table(&self, table_id: u32) {
        self.inner.write().unwrap().remove(&table_id);
    }

    /// Every meta, sorted by `(table_id, segment_id)`.
    pub fn all(&self) -> Vec<SegmentMeta> {
        self.inner
            .read()
            .unwrap()
            .values()
            .flatten()
            .cloned()
            .collect()
    }

    /// Largest table id present (0 when empty).
    pub fn max_table_id(&self) -> u32 {
        self.inner
            .read()
            .unwrap()
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
    }
}

/// Rebuild the registry by one contiguous scan of the 0x23 region
/// (meta keys are slot-less by design, see `meta`). Undecodable metas
/// are skipped loudly, never fatal.
pub fn rebuild(store: &crate::store::Store) -> Arc<Registry> {
    let registry = Arc::new(Registry::default());
    let start = [crate::sql::storage::codec::KIND_SQL_SEGMENT];
    let _ = crate::store::ops::for_each_from(store, &start, false, &mut |key, value| {
        if key.first() != Some(&crate::sql::storage::codec::KIND_SQL_SEGMENT) {
            return false; // past the 0x23 region
        }
        if meta::parse_meta_key(key).is_none() {
            eprintln!(
                "columnar: skipping bad segment meta key ({} bytes)",
                key.len()
            );
            return true;
        }
        match meta::decode_meta(value) {
            Ok(m) => registry.insert(&m),
            Err(e) => eprintln!("columnar: skipping bad segment meta: {e}"),
        }
        true
    });
    registry
}

/// Process-wide registry cache keyed by `(store_path, bind)`, so the
/// registry never has to be threaded through every `Shared` site.
/// Tests get distinct keys because `state::testutil::shared_with`
/// gives every instance its own temp `store_path`.
type RegistryMap = std::collections::HashMap<(String, String), Arc<Registry>>;
static REGISTRIES: std::sync::LazyLock<std::sync::Mutex<RegistryMap>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(RegistryMap::new()));

/// The registry of `shared`'s instance (rebuilt on first use).
pub fn registry_of(shared: &crate::state::Shared) -> Arc<Registry> {
    let key = (shared.conf.store_path.clone(), shared.conf.bind.clone());
    let mut map = REGISTRIES.lock().unwrap();
    map.entry(key)
        .or_insert_with(|| rebuild(&shared.store))
        .clone()
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests_meta.rs"]
mod tests_meta;

#[cfg(test)]
#[path = "tests_2pc.rs"]
mod tests_2pc;

#[cfg(test)]
#[path = "tests_scan.rs"]
mod tests_scan;
