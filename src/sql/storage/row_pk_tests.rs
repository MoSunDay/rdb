//! Multi-column pk key-encoding tests: concatenation, exact-tail
//! decode, ordering across components, and the escaped-NUL varlen
//! component behavior shared with single-column keys.

use super::*;
use crate::sql::storage::row::{encode_row, pk_encode_row, row_slot, version_key};
use crate::sql::storage::schema::{ColumnDef, Engine, KeyModel};

fn schema(pks: &[&str]) -> TableSchema {
    let cols = [
        ("id", SqlType::Int),
        ("tag", SqlType::VarChar),
        ("day", SqlType::Date),
        ("at", SqlType::DateTime),
    ];
    TableSchema {
        id: 9,
        name: "t".into(),
        columns: cols
            .iter()
            .map(|(n, t)| ColumnDef {
                name: (*n).into(),
                sql_type: *t,
                nullable: false,
            })
            .collect(),
        pk: pks.iter().map(|p| p.to_string()).collect(),
        auto_increment: None,
        engine: Engine::Row,
        indexes: vec![],
        key_model: KeyModel::MySql,
        distribution: None,
    }
}

fn row(vals: &[Value]) -> Vec<Value> {
    vals.to_vec()
}

#[test]
fn single_column_pk_encoding_is_unchanged() {
    // The vec!["id"] schema must produce byte-identical keys to the
    // classic single-value pk_encode -- existing physical keys never move.
    let s = schema(&["id"]);
    let values = row(&[
        Value::Int(-5),
        Value::Str("x".into()),
        Value::Date(0),
        Value::DateTime(0),
    ]);
    assert_eq!(
        pk_encode_row(&s, &values).unwrap(),
        pk_encode(&Value::Int(-5)).unwrap()
    );
}

#[test]
fn int_int_composite_round_trip() {
    let s = schema(&["id", "day"]);
    let values = row(&[
        Value::Int(i64::MIN),
        Value::Str("k".into()),
        Value::Date(-123),
        Value::DateTime(0),
    ]);
    let key = pk_encode_row(&s, &values).unwrap();
    assert_eq!(key.len(), 9 + 9, "two fixed-width components");
    assert_eq!(
        pk_decode_row(&key, &s).unwrap(),
        vec![Value::Int(i64::MIN), Value::Date(-123)]
    );
    // Tail must be consumed exactly: an extra byte is a loud error.
    let mut long = key.clone();
    long.push(0xEE);
    assert!(pk_decode_row(&long, &s).is_err());
    // Truncation is a loud error too.
    assert!(pk_decode_row(&key[..key.len() - 1], &s).is_err());
}

#[test]
fn str_then_int_orders_lexicographically_by_components() {
    let s = schema(&["tag", "id"]);
    let key = |tag: &str, id: i64| {
        pk_encode_row(
            &s,
            &row(&[
                Value::Int(id),
                Value::Str(tag.into()),
                Value::Date(0),
                Value::DateTime(0),
            ]),
        )
        .unwrap()
    };
    // (a,2) < (a,10) < (ab,0) < (b,-9): first component dominates, and
    // the shorter "a" prefix (terminator) sorts below any continuation.
    assert!(key("a", 2) < key("a", 10));
    assert!(key("a", 10) < key("ab", 0));
    assert!(key("ab", 0) < key("b", -9));
    // Negative ints order correctly after the str component.
    assert!(key("b", -9) < key("b", -1));
    assert!(key("b", -1) < key("b", 0));
}

#[test]
fn str_component_with_nul_stays_self_delimiting() {
    let s = schema(&["tag", "id"]);
    let values = row(&[
        Value::Int(0),
        Value::Str("a\0b".into()),
        Value::Date(0),
        Value::DateTime(0),
    ]);
    let key = pk_encode_row(&s, &values).unwrap();
    assert_eq!(
        pk_decode_row(&key, &s).unwrap(),
        vec![Value::Str("a\0b".into()), Value::Int(0)]
    );
    // The Int component after an embedded NUL still decodes: the
    // escaped 0x00 0xFF pair cannot be mistaken for the terminator.
    let values2 = row(&[
        Value::Int(42),
        Value::Str("a\0".into()),
        Value::Date(0),
        Value::DateTime(0),
    ]);
    let k2 = pk_encode_row(&s, &values2).unwrap();
    assert_eq!(
        pk_decode_row(&k2, &s).unwrap(),
        vec![Value::Str("a\0".into()), Value::Int(42)]
    );
}

#[test]
fn composite_key_ordering_spans_component_boundaries() {
    // "a\0x" vs "ab": embedded NUL sorts below 'b' even across the
    // escape (order-preserving escaping).
    let s = schema(&["tag"]);
    let enc = |t: &str| {
        pk_encode_row(
            &s,
            &row(&[
                Value::Int(0),
                Value::Str(t.into()),
                Value::Date(0),
                Value::DateTime(0),
            ]),
        )
        .unwrap()
    };
    assert!(enc("a\0x") < enc("ab"));
    assert!(enc("a") < enc("a\0"));
    assert!(enc("a") < enc("aé"));
    assert!(enc("aé") < enc("b"));
}

#[test]
fn version_key_tail_decodes_all_pk_columns() {
    let s = schema(&["day", "tag"]);
    let values = row(&[
        Value::Int(7),
        Value::Str("k".into()),
        Value::Date(19_782),
        Value::DateTime(1_709_208_000_123_456),
    ]);
    let pk_key = pk_encode_row(&s, &values).unwrap();
    let slot = row_slot(&s, &pk_key);
    let key = version_key(&s, slot, &pk_key, 33);
    let prefix = slot_table_prefix(slot, s.id);
    let tail = &key[prefix.len()..key.len() - TS_SUFFIX_LEN];
    assert_eq!(
        pk_from_key_tail(tail, &s).unwrap(),
        vec![Value::Date(19_782), Value::Str("k".into())]
    );
}

/// Composite-pk rows whose varlen component contains an embedded NUL
/// must coexist with a sibling row sharing the string's prefix: the
/// escaped key and the plain key are distinct groups for both the
/// visible-version scan and the GC sweep (restart regression).
#[test]
fn escaped_nul_key_and_plain_sibling_survive_scan_and_gc() {
    use crate::sql::exec::scan::visible_versions_between;
    use crate::sql::storage::gc;
    use crate::state::testutil;
    let shared = testutil::shared_with(testutil::test_config());
    let s = schema(&["id", "tag"]);

    let put = |a: i64, b: &str, v: i64, ts: u64| {
        let values = row(&[
            Value::Int(a),
            Value::Str(b.into()),
            Value::Date(0),
            Value::DateTime(v),
        ]);
        let pk_key = pk_encode_row(&s, &values).unwrap();
        let key = version_key(&s, row_slot(&s, &pk_key), &pk_key, ts);
        let val = encode_row(&s, &values).unwrap();
        let mut batch = rocksdb::WriteBatch::default();
        batch.put(key, val);
        crate::store::ops::batch_write(&shared.store, batch).unwrap();
    };
    put(1, "a\0b-probe", 7, 1);
    put(1, "ab", 8, 1);

    let n = crate::topology::SLOT_NUMBER as u16 - 1;
    let rows = visible_versions_between(&shared.store, &s, 1, 0, n).unwrap();
    assert_eq!(rows.len(), 2, "both keys visible: {rows:?}");

    let deleted = gc::sweep(&shared.store, 5, &std::collections::HashSet::new());
    assert_eq!(deleted, 0, "live anchors must never be swept");
    let rows = visible_versions_between(&shared.store, &s, 5, 0, n).unwrap();
    assert_eq!(rows.len(), 2, "both keys survive the sweep");

    // Reopen the SAME data dir (process-restart shape) and re-read.
    let path = shared.conf.store_path.clone();
    drop(shared);
    let reopened = crate::store::open(
        crate::store::data_path(&path, "127.0.0.1:32681")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let rows = visible_versions_between(&reopened, &s, 5, 0, n).unwrap();
    assert_eq!(rows.len(), 2, "both keys survive a store reopen");
}
