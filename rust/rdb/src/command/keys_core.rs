//! Storage mechanics shared by the generic key-space commands
//! (`rdb/src/command/keys.rs`): key resolution, latched deletes, TTL
//! writes and RENAME moves. Enumeration (SCAN/KEYS/RANDOMKEY) lives in
//! `keys_scan.rs`.
//!
//! Free functions over `Shared` + plain enums; every mutation takes the
//! per-user-key latch (encoding: `<slot_prefix> ++ <user_key>`, i.e.
//! `codec::string_key`) and holds it across the awaited write so
//! read-modify-write sequences serialize. Two-key operations (RENAME)
//! lock both latch keys in byte order to avoid ABBA deadlock.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::ds::codec;
use crate::ds::expire;
use crate::ds::latch;
use crate::state::Shared;
use crate::store::{self, ops, Store};

/// What one user key currently is. Store errors resolve as `Missing`
/// (key-space reads are best-effort, matching Go).
#[derive(Debug, PartialEq, Eq)]
pub enum KeyState {
    Missing,
    /// Legacy bare `<prefix> ++ <key>` string record (no envelope).
    RawString {
        value: Vec<u8>,
    },
    /// Typed record: meta kind + decoded envelope.
    Enveloped {
        kind: u8,
        expire_ms: u64,
        payload: Vec<u8>,
    },
}

impl KeyState {
    pub fn is_present(&self) -> bool {
        !matches!(self, KeyState::Missing)
    }

    /// Absolute expiry; 0 = no TTL (raw strings never have one).
    pub fn expire_ms(&self) -> u64 {
        match self {
            KeyState::Enveloped { expire_ms, .. } => *expire_ms,
            _ => 0,
        }
    }
}

/// Answer for TTL/PTTL: missing key (-2), no expiry (-1), else remaining.
#[derive(Debug, PartialEq, Eq)]
pub enum TtlAnswer {
    Missing,
    NoExpiry,
    Millis(u64),
}

/// NX/XX/GT/LT modifiers of the EXPIRE family.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TtlFlag {
    None,
    Nx,
    Xx,
    Gt,
    Lt,
}

/// Case-insensitive flag parse (`None` = unsupported flag).
pub fn parse_ttl_flag(arg: &[u8]) -> Option<TtlFlag> {
    match arg.to_ascii_uppercase().as_slice() {
        b"NX" => Some(TtlFlag::Nx),
        b"XX" => Some(TtlFlag::Xx),
        b"GT" => Some(TtlFlag::Gt),
        b"LT" => Some(TtlFlag::Lt),
        _ => None,
    }
}

/// Canonical latch key for a user key; ALL key-space mutations latch here.
pub fn latch_key(prefix: &[u8], key: &[u8]) -> Vec<u8> {
    codec::string_key(prefix, key)
}

/// Look one key up in both storage shapes, lazily purging expired records.
pub fn resolve(store: &Store, prefix: &[u8], key: &[u8], now: u64) -> KeyState {
    let raw = codec::string_key(prefix, key);
    if let Ok(Some(value)) = ops::get_physical(store, &raw) {
        return KeyState::RawString { value };
    }
    for kind in codec::meta_kinds() {
        let root = codec::data_key(prefix, *kind, key);
        let Ok(Some(value)) = ops::get_physical(store, &root) else {
            continue;
        };
        let (expire_ms, payload) = codec::decode_envelope(&value);
        if expire::is_expired(expire_ms, now) {
            if let Some(family) = codec::family_of(*kind) {
                let _ = expire::purge_if_expired(store, prefix, family, key, now);
            }
            return KeyState::Missing;
        }
        return KeyState::Enveloped {
            kind: *kind,
            expire_ms,
            payload: payload.to_vec(),
        };
    }
    KeyState::Missing
}

/// Does the flag allow replacing `old` (0 = no TTL) with `new`? Absent
/// TTL counts as infinity for GT/LT, per Redis.
pub fn flag_allows(flag: TtlFlag, old: u64, new: u64) -> bool {
    match flag {
        TtlFlag::None => true,
        TtlFlag::Nx => old == 0,
        TtlFlag::Xx => old > 0,
        TtlFlag::Gt => old > 0 && new > old,
        TtlFlag::Lt => old > 0 && new < old,
    }
}

/// DEL/UNLINK: remove the key in whichever shape it exists. `Ok(true)`
/// only when something was deleted. The delete range covers the whole
/// family regardless of races; a stale index entry left behind by a
/// concurrent TTL write is swept later by the active-expire sampler.
pub async fn delete_records(
    shared: &Shared,
    prefix: &[u8],
    key: &[u8],
    now: u64,
) -> Result<bool, String> {
    match resolve(&shared.store, prefix, key, now) {
        KeyState::Missing => Ok(false),
        KeyState::RawString { .. } => {
            // Fast path: one bare record, atomic single delete.
            store::del_async(Arc::clone(&shared.store), prefix.to_vec(), key.to_vec()).await
        }
        KeyState::Enveloped {
            kind, expire_ms, ..
        } => {
            let family = codec::family_of(kind).unwrap_or(codec::STRING_FAMILY);
            let _guard = latch::lock(&shared.latch, &latch_key(prefix, key)).await;
            let mut batch = WriteBatch::default();
            expire::family_delete_entries(&mut batch, prefix, family, key, expire_ms);
            ops::batch_write_async(Arc::clone(&shared.store), batch)
                .await
                .map(|_| true)
        }
    }
}

/// EXPIRE & friends: set an absolute TTL. `new_ms <= now` deletes the key
/// outright (Redis semantics for a past deadline). Raw strings migrate to
/// enveloped STRING_TTL records; `Ok(false)` = key missing or flag refused.
pub async fn apply_ttl(
    shared: &Shared,
    prefix: &[u8],
    key: &[u8],
    new_ms: u64,
    flag: TtlFlag,
    now: u64,
) -> Result<bool, String> {
    let _guard = latch::lock(&shared.latch, &latch_key(prefix, key)).await;
    let state = resolve(&shared.store, prefix, key, now);
    if !state.is_present() {
        return Ok(false);
    }
    let old = state.expire_ms();
    if !flag_allows(flag, old, new_ms) {
        return Ok(false);
    }
    let mut batch = WriteBatch::default();
    if new_ms <= now {
        delete_batch(&mut batch, prefix, key, &state);
    } else {
        match state {
            KeyState::RawString { value } => {
                let root = codec::data_key(prefix, codec::KIND_STRING_TTL, key);
                batch.put(&root, codec::encode_envelope(new_ms, &value));
                batch.delete(codec::string_key(prefix, key));
                expire::set_ttl_entries(&mut batch, prefix, root, 0, new_ms);
            }
            KeyState::Enveloped { kind, payload, .. } => {
                let root = codec::data_key(prefix, kind, key);
                batch.put(&root, codec::encode_envelope(new_ms, &payload));
                expire::set_ttl_entries(&mut batch, prefix, root, old, new_ms);
            }
            KeyState::Missing => unreachable!("checked above"),
        }
    }
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map(|_| true)
}

/// PERSIST: drop the TTL. Enveloped strings migrate back to bare records;
/// `Ok(false)` = missing key or no TTL to clear.
pub async fn persist_key(
    shared: &Shared,
    prefix: &[u8],
    key: &[u8],
    now: u64,
) -> Result<bool, String> {
    let _guard = latch::lock(&shared.latch, &latch_key(prefix, key)).await;
    let state = resolve(&shared.store, prefix, key, now);
    match state {
        KeyState::Missing | KeyState::RawString { .. } => Ok(false),
        KeyState::Enveloped { expire_ms: 0, .. } => Ok(false),
        KeyState::Enveloped {
            kind,
            expire_ms,
            payload,
        } => {
            let mut batch = WriteBatch::default();
            let root = codec::data_key(prefix, kind, key);
            if kind == codec::KIND_STRING_TTL {
                batch.put(codec::string_key(prefix, key), payload);
                batch.delete(&root);
            } else {
                batch.put(&root, codec::encode_envelope(0, &payload));
            }
            expire::set_ttl_entries(&mut batch, prefix, root, expire_ms, 0);
            ops::batch_write_async(Arc::clone(&shared.store), batch)
                .await
                .map(|_| true)
        }
    }
}

/// Batch entries removing `state`'s records (DEL and past-deadline EXPIRE).
fn delete_batch(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], state: &KeyState) {
    match state {
        KeyState::RawString { .. } => {
            batch.delete(codec::string_key(prefix, key));
        }
        KeyState::Enveloped {
            kind, expire_ms, ..
        } => {
            let family = codec::family_of(*kind).unwrap_or(codec::STRING_FAMILY);
            expire::family_delete_entries(batch, prefix, family, key, *expire_ms);
        }
        KeyState::Missing => {}
    }
}

/// TTL/PTTL answer for an already-resolved key.
pub fn read_ttl(state: &KeyState, now: u64) -> TtlAnswer {
    match state {
        KeyState::Missing => TtlAnswer::Missing,
        KeyState::RawString { .. } | KeyState::Enveloped { expire_ms: 0, .. } => {
            TtlAnswer::NoExpiry
        }
        KeyState::Enveloped { expire_ms, .. } => TtlAnswer::Millis(expire_ms.saturating_sub(now)),
    }
}

/// RENAME/RENAMENX outcome.
#[derive(Debug, PartialEq, Eq)]
pub enum RenameOutcome {
    SrcMissing,
    /// RENAMENX and the destination exists.
    DstBlocked,
    Moved,
}

/// Move one key's whole record family (envelope + elements + expire index
/// entry) from `src` to `dst` under both latch keys (locked in byte
/// order), overwriting an existing destination atomically.
pub async fn rename_key(
    shared: &Shared,
    prefix: &[u8],
    src: &[u8],
    dst: &[u8],
    nx: bool,
    now: u64,
) -> Result<RenameOutcome, String> {
    if src == dst {
        // Same key: a plain existence check (the two latch locks would
        // otherwise self-deadlock on one latch cell).
        return Ok(if resolve(&shared.store, prefix, src, now).is_present() {
            RenameOutcome::Moved
        } else {
            RenameOutcome::SrcMissing
        });
    }
    let (ka, kb) = {
        let ka = latch_key(prefix, src);
        let kb = latch_key(prefix, dst);
        if ka <= kb {
            (ka, kb)
        } else {
            (kb, ka)
        }
    };
    let _ga = latch::lock(&shared.latch, &ka).await;
    let _gb = latch::lock(&shared.latch, &kb).await;
    let src_state = resolve(&shared.store, prefix, src, now);
    if !src_state.is_present() {
        return Ok(RenameOutcome::SrcMissing);
    }
    let dst_state = resolve(&shared.store, prefix, dst, now);
    if nx && dst_state.is_present() {
        return Ok(RenameOutcome::DstBlocked);
    }
    let mut batch = WriteBatch::default();
    delete_batch(&mut batch, prefix, dst, &dst_state);
    match &src_state {
        KeyState::RawString { value } => {
            batch.put(codec::string_key(prefix, dst), value);
            batch.delete(codec::string_key(prefix, src));
        }
        KeyState::Enveloped {
            kind, expire_ms, ..
        } => {
            move_family(
                &mut batch,
                &shared.store,
                prefix,
                *kind,
                src,
                dst,
                *expire_ms,
            );
        }
        KeyState::Missing => unreachable!("checked above"),
    }
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map(|_| RenameOutcome::Moved)
}

/// Copy every record of `src`'s family to `dst` (rewriting the user key
/// in each physical key), delete the src ranges, move the index entry.
fn move_family(
    batch: &mut WriteBatch,
    store: &Store,
    prefix: &[u8],
    kind: u8,
    src: &[u8],
    dst: &[u8],
    expire_ms: u64,
) {
    let family = codec::family_of(kind).unwrap_or(codec::STRING_FAMILY);
    // Per-kind ranges (see codec::family_delete_ranges): collect and
    // delete each, so other keys' records never enter the move.
    let ranges = codec::family_delete_ranges(prefix, family, src);
    let mut records: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (lower, upper) in &ranges {
        let _ = ops::for_each_from(store, lower, false, &mut |k, v| {
            if k >= upper.as_slice() {
                return false;
            }
            records.push((k.to_vec(), v.to_vec()));
            true
        });
    }
    let plen = expire::slot_prefix_len(&ranges[0].0).unwrap_or(prefix.len());
    for (k, v) in records {
        match codec::decode_data_key(&k, plen) {
            Some((kind, _, suffix)) => {
                batch.put(codec::elem_key(prefix, kind, dst, suffix), v);
            }
            None => batch.put(k, v), // unparseable: keep verbatim
        }
    }
    for (lower, upper) in ranges {
        batch.delete_range(lower, upper);
    }
    let src_root = codec::data_key(prefix, family.0, src);
    let dst_root = codec::data_key(prefix, family.0, dst);
    expire::set_ttl_entries(batch, prefix, dst_root, 0, expire_ms);
    if expire_ms > 0 {
        batch.delete(codec::expire_index_key(prefix, expire_ms, &src_root));
    }
}
