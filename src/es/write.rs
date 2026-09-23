//! ES write path: index lifecycle + document mutations layered on the
//! SAME kernel FT.* uses -- one user key per index, the index-key
//! latch held across every read-modify-write, ONE fsync batch per
//! mutation. Cross-node safety mirrors RESP `MOVED`: an index whose
//! slot this node does not own is refused with a 400 routing error
//! (ES clients fan out themselves, so the address travels in the
//! error text, not a 307).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use md5::{Digest, Md5};
use rocksdb::WriteBatch;

use crate::command::keys_core;
use crate::ds::latch;
use crate::hash;
use crate::router::{self, RouteDecision};
use crate::search::ft_cmd::{doc_record_of, index_state, IndexState};
use crate::search::ft_index;
use crate::search::index_codec::{DocRecord, IndexMeta};
use crate::state::Shared;
use crate::store::ops;

use super::mapping;
use super::reply::{self, Reply};

/// ES-shaped handler error: HTTP status + `error.type` + reason.
pub struct EsErr {
    pub status: u16,
    pub es_type: String,
    pub reason: String,
}

impl EsErr {
    pub fn new(status: u16, es_type: &str, reason: impl Into<String>) -> EsErr {
        EsErr {
            status,
            es_type: es_type.to_string(),
            reason: reason.into(),
        }
    }

    /// The kernel's 404 for every missing index.
    pub fn index_not_found(index: &str) -> EsErr {
        EsErr::new(
            404,
            "index_not_found_exception",
            format!("no such index [{index}]"),
        )
    }

    pub fn reply(&self) -> Reply {
        reply::error(self.status, &self.es_type, &self.reason)
    }
}

impl From<EsErr> for Reply {
    fn from(e: EsErr) -> Reply {
        e.reply()
    }
}

/// Store/io failures surface as a 500 envelope, never a panic.
impl From<String> for EsErr {
    fn from(e: String) -> EsErr {
        EsErr::new(500, "internal_server_error", format!("storage error: {e}"))
    }
}

/// Reject indexes whose slot this node does not own; returns the
/// physical "<slot>/" prefix for local ones. Reading the topology
/// lock recovers from poisoning (guarded data stays structurally
/// valid), like the RESP dispatch path.
pub fn slot_check(shared: &Shared, index: &str) -> Result<Vec<u8>, EsErr> {
    let (slot, prefix) = hash::slot_with_prefix(index.as_bytes());
    let decision = {
        let topo = shared
            .topology
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        router::route_with_owners(
            slot,
            &topo.stable_addrs,
            topo.per_node_slots,
            &shared.conf.bind,
            &topo.owner_map,
        )
    };
    match decision {
        RouteDecision::Local => Ok(prefix),
        RouteDecision::Moved { slot, addr } => Err(EsErr::new(
            400,
            "routing_exception",
            format!(
                "index '{}' maps to slot {} owned by '{}'; ES fan-out clients must target the owning node (RESP MOVED equivalent)",
                index, slot, addr
            ),
        )),
    }
}

/// `PUT /{index}`: parse mappings, create the index meta record.
/// Existing (or wrong-typed) keys refuse the write; `Ok(true)` is the
/// only outcome (kept bool-shaped for symmetry with the router).
pub async fn create_index(shared: &Shared, index: &str, body: &[u8]) -> Result<bool, EsErr> {
    let prefix = slot_check(shared, index)?;
    let key = index.as_bytes();
    let _guard = latch::lock(&shared.latch, &keys_core::latch_key(&prefix, key)).await;
    match index_state(&shared.store, &prefix, key) {
        IndexState::Present(_) => {
            return Err(EsErr::new(
                409,
                "resource_already_exists_exception",
                format!("index [{index}] already exists"),
            ));
        }
        IndexState::WrongType => {
            return Err(EsErr::new(
                400,
                "invalid_index_name_exception",
                format!("index [{index}] already exists as a non-index key"),
            ));
        }
        IndexState::Missing => {}
    }
    let fields = mapping::fields_of_mappings(body)
        .map_err(|e| EsErr::new(400, "mapper_parsing_exception", e))?;
    let meta = IndexMeta {
        expire_ms: 0,
        num_docs: 0,
        sum_doclen: 0,
        fields,
    };
    let mut batch = WriteBatch::default();
    ft_index::put_meta(&mut batch, &prefix, key, &meta);
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(EsErr::from)?;
    Ok(true)
}

/// Raw index meta read (no latch): `None` when the index is absent.
pub fn index_meta(shared: &Shared, index: &str) -> Result<Option<IndexMeta>, EsErr> {
    let (_slot, prefix) = hash::slot_with_prefix(index.as_bytes());
    match index_state(&shared.store, &prefix, index.as_bytes()) {
        IndexState::Missing => Ok(None),
        IndexState::WrongType => Err(EsErr::new(
            400,
            "invalid_index_name_exception",
            format!("key [{index}] exists as a non-index type"),
        )),
        IndexState::Present(meta) => Ok(Some(meta)),
    }
}

/// `DELETE /{index}`: wipe the whole search family. `Ok(false)` when
/// the index was already gone (caller answers 404).
pub async fn drop_index(shared: &Shared, index: &str) -> Result<bool, EsErr> {
    let prefix = slot_check(shared, index)?;
    let key = index.as_bytes();
    let _guard = latch::lock(&shared.latch, &keys_core::latch_key(&prefix, key)).await;
    let meta = match index_state(&shared.store, &prefix, key) {
        IndexState::Missing => return Ok(false),
        IndexState::WrongType => {
            return Err(EsErr::new(
                400,
                "invalid_index_name_exception",
                format!("key [{index}] exists as a non-index type"),
            ));
        }
        IndexState::Present(meta) => meta,
    };
    let mut batch = WriteBatch::default();
    ft_index::delete_family(&mut batch, &prefix, key, meta.expire_ms);
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(EsErr::from)?;
    Ok(true)
}

/// `PUT|POST /{index}/_doc[/{id}]`: add-or-replace one document in a
/// single fsync. `create_only` is `?op_type=create` semantics (409 on
/// an existing id); returns whether the id was new.
pub async fn put_doc(
    shared: &Shared,
    index: &str,
    docid: &str,
    body: &[u8],
    create_only: bool,
) -> Result<bool, EsErr> {
    let prefix = slot_check(shared, index)?;
    let key = index.as_bytes();
    let _guard = latch::lock(&shared.latch, &keys_core::latch_key(&prefix, key)).await;
    let meta = match index_state(&shared.store, &prefix, key) {
        IndexState::Missing => return Err(EsErr::index_not_found(index)),
        IndexState::WrongType => {
            return Err(EsErr::new(
                400,
                "invalid_index_name_exception",
                format!("key [{index}] exists as a non-index type"),
            ));
        }
        IndexState::Present(meta) => meta,
    };
    let old =
        ft_index::read_doc(&shared.store, &prefix, key, docid.as_bytes()).map_err(EsErr::from)?;
    if create_only && old.is_some() {
        return Err(EsErr::new(
            409,
            "version_conflict_engine_exception",
            format!("[{docid}]: version conflict, document already exists (1)"),
        ));
    }
    let (rec, numvals) = doc_record_of(&meta, body).map_err(|e| {
        EsErr::new(
            400,
            "document_parsing_exception",
            e.trim_start_matches("ERR "),
        )
    })?;
    let batch = ft_index::build_add_batch(
        &shared.store,
        &prefix,
        key,
        &meta,
        docid.as_bytes(),
        rec,
        &numvals,
    )
    .map_err(EsErr::from)?;
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(EsErr::from)?;
    Ok(old.is_none())
}

/// `DELETE /{index}/_doc/{id}`: `None` when the doc was absent
/// (caller answers "not_found"); a missing INDEX is an error.
pub async fn del_doc(shared: &Shared, index: &str, docid: &str) -> Result<Option<()>, EsErr> {
    let prefix = slot_check(shared, index)?;
    let key = index.as_bytes();
    let _guard = latch::lock(&shared.latch, &keys_core::latch_key(&prefix, key)).await;
    let meta = match index_state(&shared.store, &prefix, key) {
        IndexState::Missing => return Err(EsErr::index_not_found(index)),
        IndexState::WrongType => {
            return Err(EsErr::new(
                400,
                "invalid_index_name_exception",
                format!("key [{index}] exists as a non-index type"),
            ));
        }
        IndexState::Present(meta) => meta,
    };
    match ft_index::build_del_batch(&shared.store, &prefix, key, &meta, docid.as_bytes())
        .map_err(EsErr::from)?
    {
        None => Ok(None),
        Some(batch) => {
            ops::batch_write_async(Arc::clone(&shared.store), batch)
                .await
                .map_err(EsErr::from)?;
            Ok(Some(()))
        }
    }
}

/// Server-generated document id: md5 of (wall-ms, process id,
/// process-global counter), 20 hex chars -- unique per process per
/// millisecond without any coordination.
pub fn auto_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let digest = Md5::digest(format!("{ms}-{}-{n}", std::process::id()).as_bytes());
    hex::encode(digest)[..20].to_string()
}

/// Raw document read (no latch): meta + record, or `None` when the
/// index or the doc is absent.
pub fn get_doc(
    shared: &Shared,
    index: &str,
    docid: &str,
) -> Result<Option<(IndexMeta, DocRecord)>, EsErr> {
    let Some(meta) = index_meta(shared, index)? else {
        return Ok(None);
    };
    let (_slot, prefix) = hash::slot_with_prefix(index.as_bytes());
    let rec = ft_index::read_doc(&shared.store, &prefix, index.as_bytes(), docid.as_bytes())
        .map_err(EsErr::from)?;
    Ok(rec.map(|r| (meta, r)))
}
