//! Physical key layouts of SQL secondary/unique index entries (M2).
//!
//! Both kinds live in the SAME physical slot a point-lookup of the
//! indexed value would hash to, so one seek (unique) or one short range
//! scan (secondary) resolves the indexed value:
//!
//! ```text
//! secondary: "<slot/>" 0x21 table_id(u32 BE) col_pos(u32 BE) key_encode(col_value) pk_key -> b""
//! unique:    "<slot/>" 0x22 table_id(u32 BE) col_pos(u32 BE) key_encode(col_value)         -> pk_key
//! ```
//!
//! `slot = crc16(table_id BE ++ key_encode(col_value)) % 16384` -- the
//! row plane's slot hashing (`row::slot_of`) applied to the indexed
//! value instead of the pk, so all pks sharing one value land in one
//! slot region. `col_pos` is the column POSITION (the stable column id:
//! no ALTER exists, positions never move). Index entries carry NO
//! timestamp: they are latest-committed-state pointers, rewritten in
//! the same RocksDB batch as the row versions they mirror, and the
//! watermark GC (which folds only 0x20 row-kind keys) never touches
//! them. NULL column values are never indexed (multiple NULLs are legal
//! even for a unique index).

use crate::hash::crc16;
use crate::sql::storage::codec::{decode_key, encode_key, KIND_SQL_INDEX, KIND_SQL_UNIQUE_INDEX};
use crate::sql::storage::schema::{SqlType, Value};
use crate::store::rocksdb::slot_prefix;
use crate::topology;

/// crc16 slot of one index: hash of (table identity, column position).
/// The VALUE stays out of the hash on purpose: every entry of one index
/// then shares a single slot prefix, so eq lookups, range walks and DROP
/// sweeps each cover one contiguous keyspace (value order = key order
/// within it) while different indexes still spread across slots.
pub fn index_slot(table_id: u32, col_pos: u32) -> u16 {
    let mut hashed = table_id.to_be_bytes().to_vec();
    hashed.extend_from_slice(&col_pos.to_be_bytes());
    crc16(&hashed) % topology::SLOT_NUMBER as u16
}

/// Shared head of both layouts: slot prefix + kind + table + col pos.
fn head(slot: u16, kind: u8, table_id: u32, col_pos: u32) -> Vec<u8> {
    let mut k = slot_prefix(slot);
    k.push(kind);
    k.extend_from_slice(&table_id.to_be_bytes());
    k.extend_from_slice(&col_pos.to_be_bytes());
    k
}

/// Everything after the shared head of an index entry key.
pub fn tail(kind: u8, table_id: u32, col_pos: u32, col_key: &[u8], pk_key: &[u8]) -> Vec<u8> {
    let mut k = head(index_slot(table_id, col_pos), kind, table_id, col_pos);
    k.extend_from_slice(col_key);
    k.extend_from_slice(pk_key);
    k
}

/// Fixed head of one index's whole entry range: slot prefix + kind +
/// table + col pos (no value bytes). Every entry of the index starts
/// with these bytes, so walks `from` it are value-ordered.
pub fn index_prefix(table_id: u32, col_pos: u32, kind: u8) -> Vec<u8> {
    head(index_slot(table_id, col_pos), kind, table_id, col_pos)
}

/// Inclusive-lower start cursor of one index's whole entry range: the
/// first key of the (slot, kind, table, col) keyspace.
pub fn index_start(table_id: u32, col_pos: u32, kind: u8) -> Vec<u8> {
    index_prefix(table_id, col_pos, kind)
}

/// Start cursor of the DROP INDEX entry sweep: the beginning of the
/// index's slot (both kinds 0x21/0x22 live there, split by the kind
/// byte, so the sweep walks the whole slot and filters).
pub fn sweep_start(table_id: u32, col_pos: u32) -> Vec<u8> {
    slot_prefix(index_slot(table_id, col_pos))
}

/// Full secondary-index entry key: indexed value + owning pk.
pub fn secondary_key(table_id: u32, col_pos: u32, col_key: &[u8], pk_key: &[u8]) -> Vec<u8> {
    tail(KIND_SQL_INDEX, table_id, col_pos, col_key, pk_key)
}

/// Full unique-index entry key: indexed value only (pk is the payload).
pub fn unique_key(table_id: u32, col_pos: u32, col_key: &[u8]) -> Vec<u8> {
    tail(KIND_SQL_UNIQUE_INDEX, table_id, col_pos, col_key, b"")
}

/// Key prefix of every secondary entry for one (table, column, value):
/// a single contiguous range holds all matching pks.
pub fn secondary_value_prefix(table_id: u32, col_pos: u32, col_key: &[u8]) -> Vec<u8> {
    tail(KIND_SQL_INDEX, table_id, col_pos, col_key, b"")
}

/// Decompose an index entry key: `(kind, table_id, col_pos, rest)` where
/// `rest = key_encode(value) ++ pk_key` (pk empty for unique). The whole
/// slot-space walk filter, mirroring `row::parse_version_key`.
pub fn parse_index_key(key: &[u8]) -> Option<(u8, u32, u32, Vec<u8>)> {
    let slash = key.iter().position(|&b| b == b'/')?;
    let slot: u32 = std::str::from_utf8(&key[..slash]).ok()?.parse().ok()?;
    let _ = u16::try_from(slot).ok()?;
    if key.len() < slash + 2 + 4 + 4 {
        return None;
    }
    let kind = key[slash + 1];
    if kind != KIND_SQL_INDEX && kind != KIND_SQL_UNIQUE_INDEX {
        return None;
    }
    let body = &key[slash + 2..];
    let table_id = u32::from_be_bytes(body[..4].try_into().ok()?);
    let col_pos = u32::from_be_bytes(body[4..8].try_into().ok()?);
    // The encoded value alone is at least a tag byte.
    if body.len() <= 8 {
        return None;
    }
    Some((kind, table_id, col_pos, body[8..].to_vec()))
}

/// Split an entry-key tail into its encoded column value and the pk
/// suffix. Fixed-width types split by length; var-length values end at
/// their 0x00 terminator (`codec::key_bytes`).
pub fn split_tail(tail: &[u8], ty: SqlType) -> Option<(&[u8], &[u8])> {
    let fixed = match ty {
        SqlType::Bool => 2,
        SqlType::Int | SqlType::Double => 9,
        SqlType::VarChar | SqlType::Blob => tail[1..].iter().position(|&b| b == 0x00)? + 2,
    };
    tail.split_at_checked(fixed)
}

/// Decode the column value of an entry tail (best effort; used by tests
/// and error messages, never on the read hot path).
pub fn value_of_tail(tail: &[u8], ty: SqlType) -> Option<Value> {
    let (col_key, _) = split_tail(tail, ty)?;
    decode_key(col_key, ty).ok().map(|(v, _)| v)
}

/// Order-preserving encoding of one indexed value (NULL is rejected by
/// callers before key building).
pub fn col_key_of(value: &Value) -> Result<Vec<u8>, String> {
    encode_key(value)
}

/// MySQL-style rendering for duplicate-key messages:
/// `Duplicate entry '<v>' for key '<index>'`.
pub fn value_display(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => format!("{}", *b as u8),
        Value::Int(i) => i.to_string(),
        Value::Double(d) => format!("{d}"),
        Value::Str(s) => format!("'{s}'"),
        Value::Bytes(b) => format!("x'{}'", hex::encode(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_hashes_table_and_column() {
        assert!(index_slot(7, 1) < 16384);
        // different columns (and usually different tables) land apart
        assert_ne!(index_slot(7, 1), index_slot(7, 2));
        assert_ne!(index_slot(7, 1), index_slot(8, 1));
        // the index keyspace is contiguous: start sorts before any value
        let ck = encode_key(&Value::Int(1)).unwrap();
        assert!(index_start(7, 1, KIND_SQL_INDEX) < secondary_key(7, 1, &ck, b"pk"));
    }

    #[test]
    fn secondary_key_layout_and_parse() {
        let col_key = encode_key(&Value::Str("red".into())).unwrap();
        let k = secondary_key(42, 1, &col_key, b"pk1");
        let (kind, table, pos, rest) = parse_index_key(&k).expect("parse");
        assert_eq!(kind, KIND_SQL_INDEX);
        assert_eq!(table, 42);
        assert_eq!(pos, 1);
        let (ck, pk) = split_tail(&rest, SqlType::VarChar).expect("split");
        assert_eq!(ck, col_key);
        assert_eq!(pk, b"pk1");
        assert!(k.starts_with(&secondary_value_prefix(42, 1, &col_key)));
    }

    #[test]
    fn unique_key_layout_and_parse() {
        let col_key = encode_key(&Value::Int(-9)).unwrap();
        let k = unique_key(42, 2, &col_key);
        let (kind, table, pos, rest) = parse_index_key(&k).expect("parse");
        assert_eq!(kind, KIND_SQL_UNIQUE_INDEX);
        assert_eq!((table, pos), (42, 2));
        let (ck, pk) = split_tail(&rest, SqlType::Int).expect("split");
        assert_eq!(ck, col_key);
        assert!(pk.is_empty());
        assert_eq!(value_of_tail(&rest, SqlType::Int), Some(Value::Int(-9)));
    }

    #[test]
    fn parse_rejects_non_index_keys() {
        assert!(parse_index_key(b"5/").is_none());
        assert!(parse_index_key(b"5/\x20\x00\x00\x00\x2a\x00\x00").is_none());
        assert!(parse_index_key(b"5/\x01somekey").is_none());
    }

    #[test]
    fn entries_of_one_value_share_a_contiguous_range() {
        let ck = encode_key(&Value::Str("red".into())).unwrap();
        let prefix = secondary_value_prefix(9, 1, &ck);
        for pk in ["a", "b", "zz"] {
            let k = secondary_key(9, 1, &ck, pk.as_bytes());
            assert!(k.starts_with(&prefix), "{pk}");
        }
        // a different value may live elsewhere but still parses
        let ck2 = encode_key(&Value::Str("blue".into())).unwrap();
        let k2 = secondary_key(9, 1, &ck2, b"a");
        assert!(!k2.starts_with(&prefix));
        assert!(parse_index_key(&k2).is_some());
    }
}
