//! Physical (versioned) row layout inside the shared RocksDB.
//!
//! ```text
//! version key = "<slot/>" 0x20 table_id(u32 BE) pk_key ts_suffix(8B)
//! version val = header(1B) || null_bitmap((cols+7)/8) || payload*
//!               header 0x01 = live row, 0x00 = tombstone (delete)
//! ```
//!
//! `ts_suffix` is the commit timestamp inverted (`!ts`, big endian), so
//! byte order puts the NEWEST version of a primary key first: a snapshot
//! reader takes the first entry whose decoded `ts <= read_ts`. The slot
//! is `crc16(table_id BE ++ pk_key) % 16384`, so one table's rows spread
//! over the existing RESP slot space untouched.
//!
//! M3 note: prepared-but-uncommitted 2PC versions use header 0x02 and are
//! skipped by snapshot readers until the raft decision converts them.

use crate::hash::crc16;
use crate::sql::storage::codec::{decode_key, encode_key, KIND_SQL_ROW};
use crate::sql::storage::schema::{SqlType, TableSchema, Value};
use crate::store;

/// Version-value header bytes.
pub const HEADER_TOMBSTONE: u8 = 0x00;
pub const HEADER_LIVE: u8 = 0x01;
/// M3 2PC: a version written by a voted-but-undecided PREPARE. Same
/// key layout and payload as its final form; only the header differs,
/// and the commit decision flips the byte in place. Snapshot readers
/// skip prepared versions ([`is_prepared`]); a flipped (0x01/0x00)
/// version becomes visible atomically.
pub const HEADER_PREPARED: u8 = 0x02;
/// Length of the inverted-timestamp suffix in the version key.
pub const TS_SUFFIX_LEN: usize = 8;

/// True when a raw version value is a prepared 2PC version (invisible
/// to every snapshot reader until its coordinator decides).
pub fn is_prepared(val: &[u8]) -> bool {
    val.first() == Some(&HEADER_PREPARED)
}

/// The header a prepared version flips TO on commit: a prepared value
/// of length 1 is a tombstone (live rows always carry the null bitmap,
/// so they are strictly longer).
pub fn final_header(prepared_val: &[u8]) -> u8 {
    if prepared_val.len() <= 1 {
        HEADER_TOMBSTONE
    } else {
        HEADER_LIVE
    }
}

/// The prepared form of a final version value: header byte swapped for
/// [`HEADER_PREPARED`], payload untouched (the flip restores it).
pub fn prepared_value(final_val: &[u8]) -> Vec<u8> {
    let mut v = final_val.to_vec();
    v[0] = HEADER_PREPARED;
    v
}

/// crc16 slot of one row: table identity + pk keep different tables
/// independent in the hash.
pub fn row_slot(schema: &TableSchema, pk_key: &[u8]) -> u16 {
    slot_of(schema.id, pk_key)
}

/// Schema-free form of [`row_slot`]: crc16 slot from raw table id + pk
/// key bytes (used by txn conflict probing, which knows no schema).
pub fn slot_of(table_id: u32, pk_key: &[u8]) -> u16 {
    let mut hashed = table_id.to_be_bytes().to_vec();
    hashed.extend_from_slice(pk_key);
    // Same 16384 space as the RESP plane (`hash::slot`), so SQL rows ride
    // the existing slot sharding untouched.
    crc16(&hashed) % crate::topology::SLOT_NUMBER as u16
}

/// Order-preserving encoding of a PK value (schema-driven decode).
pub fn pk_encode(pk: &Value) -> Result<Vec<u8>, String> {
    encode_key(pk)
}

pub fn pk_decode(bytes: &[u8], ty: SqlType) -> Result<Value, String> {
    let (v, rest) = decode_key(bytes, ty)?;
    if !rest.is_empty() {
        return Err("trailing bytes after primary key".into());
    }
    Ok(v)
}

/// Inverted commit timestamp: ascending bytes = descending ts (newest
/// version of one pk sorts first).
pub fn ts_suffix(ts: u64) -> [u8; TS_SUFFIX_LEN] {
    (!ts).to_be_bytes()
}

/// Inverse of [`ts_suffix`].
pub fn ts_from_suffix(suffix: &[u8]) -> u64 {
    !u64::from_be_bytes(suffix.try_into().expect("ts suffix len"))
}

/// Full physical RocksDB key of one row VERSION.
pub fn version_key(schema: &TableSchema, slot: u16, pk_key: &[u8], ts: u64) -> Vec<u8> {
    let mut k = store::rocksdb::slot_prefix(slot);
    k.push(KIND_SQL_ROW);
    k.extend_from_slice(&schema.id.to_be_bytes());
    k.extend_from_slice(pk_key);
    k.extend_from_slice(&ts_suffix(ts));
    k
}

/// Slot-prefix part of a scan over one table+slot (version_key minus pk).
pub fn slot_table_prefix(slot: u16, table_id: u32) -> Vec<u8> {
    let mut k = store::rocksdb::slot_prefix(slot);
    k.push(KIND_SQL_ROW);
    k.extend_from_slice(&table_id.to_be_bytes());
    k
}

/// Physical key prefix shared by EVERY version of (table_id, pk_key):
/// versions of one pk sort newest-first, so the first store key at or
/// after this prefix is the newest version (ts probing without a schema).
pub fn version_prefix(table_id: u32, pk_key: &[u8]) -> Vec<u8> {
    let mut k = slot_table_prefix(slot_of(table_id, pk_key), table_id);
    k.extend_from_slice(pk_key);
    k
}

/// Decode the pk tail of a physical row key (input starts at the pk bytes).
pub fn pk_from_key_tail(tail: &[u8], schema: &TableSchema) -> Result<Value, String> {
    pk_decode(tail, schema.pk_type())
}

/// Decompose a store key into `(slot, table_id, pk_key, ts)` when it is a
/// SQL row version; the whole-table scan filter.
pub fn parse_version_key(key: &[u8]) -> Option<(u16, u32, Vec<u8>, u64)> {
    let slash = key.iter().position(|&b| b == b'/')?;
    let slot: u32 = std::str::from_utf8(&key[..slash]).ok()?.parse().ok()?;
    if key.len() < slash + 1 + 1 + 4 + TS_SUFFIX_LEN || key[slash + 1] != KIND_SQL_ROW {
        return None;
    }
    let slot = u16::try_from(slot).ok()?;
    let body = &key[slash + 2..];
    let table_id = u32::from_be_bytes(body[..4].try_into().ok()?);
    let (pk, ts_raw) = body[4..].split_at(body.len() - 4 - TS_SUFFIX_LEN);
    Some((
        slot,
        table_id,
        pk.to_vec(),
        ts_from_suffix(&body[body.len() - TS_SUFFIX_LEN..]),
    ))
    .filter(|_| ts_raw.len() == TS_SUFFIX_LEN)
}

/// Encode a live row value: header + null bitmap + typed payloads.
pub fn encode_row(schema: &TableSchema, values: &[Value]) -> Result<Vec<u8>, String> {
    if values.len() != schema.columns.len() {
        return Err(format!(
            "row has {} values, table {} has {} columns",
            values.len(),
            schema.name,
            schema.columns.len()
        ));
    }
    let mut out = Vec::with_capacity(17 + values.len() * 8);
    out.push(HEADER_LIVE);
    for chunk in values.chunks(8) {
        let mut bitmap: u8 = 0;
        for (i, v) in chunk.iter().enumerate() {
            if matches!(v, Value::Null) {
                bitmap |= 1 << i;
            }
        }
        out.push(bitmap);
    }
    for (v, col) in values.iter().zip(&schema.columns) {
        if matches!(v, Value::Null) {
            continue;
        }
        encode_payload(&mut out, col.sql_type, v)?;
    }
    Ok(out)
}

/// Encode a deletion tombstone value.
pub fn encode_tombstone() -> Vec<u8> {
    vec![HEADER_TOMBSTONE]
}

/// Decode a version value into `(header, live row values)`; tombstones
/// return an empty value list.
pub fn decode_version(schema: &TableSchema, bytes: &[u8]) -> Result<(u8, Vec<Value>), String> {
    let Some((&header, rest)) = bytes.split_first() else {
        return Err("empty row version".into());
    };
    if header == HEADER_TOMBSTONE {
        return Ok((header, Vec::new()));
    }
    if header != HEADER_LIVE {
        return Err(format!("unknown row version header {header:#x}"));
    }
    let values = decode_row(schema, rest)?;
    Ok((header, values))
}

/// Decode a live-row payload (null bitmap + payloads; no header byte).
fn decode_row(schema: &TableSchema, bytes: &[u8]) -> Result<Vec<Value>, String> {
    let n = schema.columns.len();
    let bitmap_len = n.div_ceil(8);
    if bytes.len() < bitmap_len {
        return Err("row value shorter than null bitmap".into());
    }
    let (bitmap, mut rest) = bytes.split_at(bitmap_len);
    let mut values = Vec::with_capacity(n);
    for (i, col) in schema.columns.iter().enumerate() {
        let null = bitmap[i / 8] & (1 << (i % 8)) != 0;
        if null {
            values.push(Value::Null);
            continue;
        }
        let (v, r) = decode_payload(rest, col.sql_type)?;
        values.push(v);
        rest = r;
    }
    Ok(values)
}

/// Newest version visible at `read_ts` from an iterator ordered by
/// version key (descending ts). Returns the raw value bytes. Prepared
/// (0x02) versions are SKIPPED: an in-flight 2PC write must stay
/// invisible to every reader until its coordinator decides, so the
/// walk continues to the older committed sibling.
pub fn visible_value<'a, I>(versions: I, read_ts: u64) -> Option<(&'a [u8], u64)>
where
    I: IntoIterator<Item = (u64, &'a [u8])>,
{
    versions
        .into_iter()
        .find(|(ts, v)| *ts <= read_ts && !is_prepared(v))
        .map(|(ts, v)| (v, ts))
}

fn encode_payload(out: &mut Vec<u8>, ty: SqlType, v: &Value) -> Result<(), String> {
    match (ty, v) {
        (SqlType::Bool, Value::Bool(b)) => out.push(*b as u8),
        (SqlType::Int, Value::Int(i)) => out.extend_from_slice(&i.to_be_bytes()),
        (SqlType::Double, Value::Double(d)) => out.extend_from_slice(&d.to_bits().to_be_bytes()),
        (SqlType::VarChar, Value::Str(s)) => {
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        (SqlType::Blob, Value::Bytes(b)) => {
            out.extend_from_slice(&(b.len() as u32).to_be_bytes());
            out.extend_from_slice(b);
        }
        (ty, v) => {
            return Err(format!("value {v:?} does not fit column type {ty:?}"));
        }
    }
    Ok(())
}

fn decode_payload(mut rest: &[u8], ty: SqlType) -> Result<(Value, &[u8]), String> {
    let val = match ty {
        SqlType::Bool => {
            let (b, r) = rest.split_first().ok_or("bool payload truncated")?;
            rest = r;
            Value::Bool(*b != 0)
        }
        SqlType::Int => {
            let (raw, r) = split8(rest, "int")?;
            rest = r;
            Value::Int(i64::from_be_bytes(raw.try_into().unwrap()))
        }
        SqlType::Double => {
            let (raw, r) = split8(rest, "double")?;
            rest = r;
            Value::Double(f64::from_bits(u64::from_be_bytes(raw.try_into().unwrap())))
        }
        SqlType::VarChar | SqlType::Blob => {
            if rest.len() < 4 {
                return Err("varlen payload truncated".into());
            }
            let len = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
            if rest.len() < 4 + len {
                return Err("varlen payload shorter than length".into());
            }
            let raw = &rest[4..4 + len];
            rest = &rest[4 + len..];
            if ty == SqlType::VarChar {
                Value::Str(String::from_utf8(raw.to_vec()).map_err(|_| "invalid utf8 in row")?)
            } else {
                Value::Bytes(raw.to_vec())
            }
        }
    };
    Ok((val, rest))
}

fn split8<'a>(rest: &'a [u8], what: &str) -> Result<(&'a [u8], &'a [u8]), String> {
    if rest.len() < 8 {
        return Err(format!("{what} payload truncated"));
    }
    Ok(rest.split_at(8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::storage::schema::ColumnDef;

    fn schema() -> TableSchema {
        TableSchema {
            id: 42,
            name: "users".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    sql_type: SqlType::Int,
                    nullable: false,
                },
                ColumnDef {
                    name: "name".into(),
                    sql_type: SqlType::VarChar,
                    nullable: true,
                },
                ColumnDef {
                    name: "score".into(),
                    sql_type: SqlType::Double,
                    nullable: true,
                },
                ColumnDef {
                    name: "active".into(),
                    sql_type: SqlType::Bool,
                    nullable: false,
                },
                ColumnDef {
                    name: "avatar".into(),
                    sql_type: SqlType::Blob,
                    nullable: true,
                },
            ],
            pk: "id".into(),
            indexes: vec![],
        }
    }

    #[test]
    fn row_round_trip_with_nulls() {
        let s = schema();
        let row = vec![
            Value::Int(7),
            Value::Str("hiro".into()),
            Value::Null,
            Value::Bool(true),
            Value::Bytes(vec![1, 2, 3, 0]),
        ];
        let enc = encode_row(&s, &row).expect("encode");
        let (header, dec) = decode_version(&s, &enc).expect("decode");
        assert_eq!(header, HEADER_LIVE);
        assert_eq!(dec, row);
    }

    #[test]
    fn tombstone_round_trip() {
        let (h, vals) = decode_version(&schema(), &encode_tombstone()).expect("decode");
        assert_eq!(h, HEADER_TOMBSTONE);
        assert!(vals.is_empty());
    }

    #[test]
    fn version_keys_newest_first_and_parseable() {
        let s = schema();
        let pk = pk_encode(&Value::Int(-9)).expect("enc");
        let slot = row_slot(&s, &pk);
        let k10 = version_key(&s, slot, &pk, 10);
        let k9 = version_key(&s, slot, &pk, 9);
        assert!(k10 < k9, "higher ts must sort first");
        let (sl, tid, pkp, ts) = parse_version_key(&k10).expect("parse");
        assert_eq!((sl, tid, ts), (slot, s.id, 10));
        assert_eq!(pkp, pk);
    }

    #[test]
    fn parse_rejects_non_sql_keys() {
        assert!(parse_version_key(b"5/").is_none());
        assert!(parse_version_key(b"5/\x01somekey").is_none());
        assert!(parse_version_key(b"99999/x").is_none());
        // too short for table id + ts suffix
        assert!(parse_version_key(b"5/\x20\x00\x00").is_none());
    }

    #[test]
    fn pk_key_round_trip_and_slot_layout() {
        let s = schema();
        let pk = Value::Int(-9);
        let pk_key = pk_encode(&pk).expect("enc");
        let slot = row_slot(&s, &pk_key);
        let key = version_key(&s, slot, &pk_key, 1);
        let prefix = slot_table_prefix(slot, s.id);
        assert!(key.starts_with(&prefix));
        // the tail is pk bytes followed by the 8-byte ts suffix
        let tail = &key[prefix.len()..key.len() - TS_SUFFIX_LEN];
        assert_eq!(pk_from_key_tail(tail, &s).expect("dec"), pk);
    }

    #[test]
    fn same_pk_different_tables_spread() {
        let mut b = schema();
        b.id = 43;
        let pk = Value::Str("k".into());
        let pk_key = pk_encode(&pk).unwrap();
        assert!(row_slot(&schema(), &pk_key) < 16384);
        assert!(row_slot(&b, &pk_key) < 16384);
    }

    #[test]
    fn visible_value_picks_newest_at_or_below_read_ts() {
        let v9 = encode_tombstone();
        let v5 = encode_row(
            &schema(),
            &[
                Value::Int(1),
                Value::Null,
                Value::Null,
                Value::Bool(false),
                Value::Null,
            ],
        )
        .unwrap();
        let versions: Vec<(u64, &[u8])> = vec![(9, &v9), (5, &v5)];
        assert_eq!(visible_value(versions.iter().copied(), 4), None);
        let (val, ts) = visible_value(versions.iter().copied(), 5).expect("v");
        assert_eq!(ts, 5);
        assert_eq!(val, &v5[..]);
        let (val, ts) = visible_value(versions.iter().copied(), 100).expect("v");
        assert_eq!(ts, 9);
        assert_eq!(val, &v9[..]);
    }
}

#[cfg(test)]
mod prepared_tests {
    use super::*;

    const T: u32 = 7;

    fn ver(ts: u64, header: u8) -> (u64, Vec<u8>) {
        (ts, vec![header, 0xaa, 0xbb])
    }

    #[test]
    fn prepared_version_is_invisible() {
        // ts=20 committed, ts=30 prepared -> read at 25 sees ts=20.
        let versions = [ver(30, HEADER_PREPARED), ver(20, HEADER_LIVE)];
        let (v, ts) = visible_value(versions.iter().map(|(t, v)| (*t, v.as_slice())), 25)
            .expect("older committed version must be visible");
        assert_eq!(ts, 20);
        assert_eq!(v[0], HEADER_LIVE);
    }

    #[test]
    fn prepared_tombstone_does_not_shadow() {
        // A prepared tombstone at ts=30 must not hide the committed row
        // at ts=20, nor be read as a deletion.
        let versions = [ver(30, HEADER_PREPARED), ver(20, HEADER_LIVE)];
        let (v, ts) = visible_value(versions.iter().map(|(t, v)| (*t, v.as_slice())), 25)
            .expect("committed row survives a prepared shadow");
        assert_eq!(ts, 20);
        assert_eq!(v[0], HEADER_LIVE);
    }

    #[test]
    fn only_prepared_versions_yield_none() {
        let versions = [ver(30, HEADER_PREPARED)];
        assert!(visible_value(versions.iter().map(|(t, v)| (*t, v.as_slice())), 25).is_none());
        assert!(visible_value(versions.iter().map(|(t, v)| (*t, v.as_slice())), 99).is_none());
    }

    #[test]
    fn flip_to_final_makes_version_visible() {
        let v = vec![HEADER_PREPARED, 0x01, 0x02];
        assert!(is_prepared(&v));
        assert_eq!(final_header(&v), HEADER_LIVE);
        let mut flipped = v.clone();
        flipped[0] = final_header(&v);
        let versions = [(30u64, flipped)];
        let (got, ts) = visible_value(versions.iter().map(|(t, v)| (*t, v.as_slice())), 40)
            .expect("flipped version becomes visible");
        assert_eq!(ts, 30);
        assert_eq!(got[0], HEADER_LIVE);
    }

    #[test]
    fn prepared_tombstone_flip_roundtrip() {
        // Prepared tombstone = single 0x02 byte; commit keeps it a tombstone.
        let final_del = vec![HEADER_TOMBSTONE];
        let prep = prepared_value(&final_del);
        assert_eq!(prep, vec![HEADER_PREPARED]);
        assert_eq!(final_header(&prep), HEADER_TOMBSTONE);
        // Prepared live row roundtrip.
        let final_live = vec![HEADER_LIVE, 0x00, 0x00];
        let prep = prepared_value(&final_live);
        assert!(is_prepared(&prep));
        assert_eq!(final_header(&prep), HEADER_LIVE);
        assert_eq!(prep[1..], final_live[1..]);
    }

    #[test]
    fn slot_of_is_stable_crc16_mod_slots() {
        // Spot-check the documented collision property: same pk under
        // different tables may collide; distinct pks mostly differ.
        assert_eq!(slot_of(T, b"pk1"), slot_of(T, b"pk1"));
        let distinct: Vec<u16> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|k| slot_of(T, k.as_bytes()))
            .collect();
        assert!(
            distinct
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                >= 3
        );
    }
}
