//! Page encodings: PLAIN for every type, DICT for strings when it
//! actually wins (StarRocks-style 0.7 ratio gate, fallback to PLAIN).
//! Every page holds at most MAX_PAGE_VALUES values and carries a
//! zonemap (min/max/null_count) built while encoding.

use std::cmp::Ordering;

use crate::sql::columnar::format::{self, ColumnFooter, Footer, PageFooter, FOOTER_VERSION};
use crate::sql::storage::schema::{SqlType, TableSchema, Value};

/// Hard cap of values stored in a single page.
pub const MAX_PAGE_VALUES: usize = 8192;
/// Page encoding names recorded in the footer.
pub const ENC_PLAIN: &str = "plain";
pub const ENC_DICT: &str = "dict";

/// One encoded page plus its zonemap.
#[derive(Debug)]
pub struct EncodedPage {
    pub encoding: String,
    pub bytes: Vec<u8>,
    pub num_values: u32,
    pub null_count: u32,
    pub min: Value,
    pub max: Value,
}

/// Per-column segment-level zonemap (aggregate of the column's pages).
#[derive(Debug)]
pub struct ColumnZone {
    pub name: String,
    pub sql_type: SqlType,
    pub nullable: bool,
    pub null_count: u64,
    pub min: Value,
    pub max: Value,
}

/// Total order over same-variant value pairs (NULL never appears: the
/// zonemap only folds non-null values). Str/Bytes compare by bytes and
/// also cross-compare against each other; mismatched variants -> None.
fn cmp_vals(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Double(x), Value::Double(y)) => Some(x.partial_cmp(y).unwrap_or(Ordering::Equal)),
        // Temporal values compare within their own type only (a DATE and
        // a DATETIME never share a column, so cross-type is None).
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        (Value::DateTime(x), Value::DateTime(y)) => Some(x.cmp(y)),
        (Value::Str(x), Value::Str(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (Value::Bytes(x), Value::Bytes(y)) => Some(x.cmp(y)),
        (Value::Str(x), Value::Bytes(y)) => Some(x.as_bytes().cmp(y.as_slice())),
        (Value::Bytes(x), Value::Str(y)) => Some(x.as_slice().cmp(y.as_bytes())),
        _ => None,
    }
}

fn fold_min(acc: &mut Option<Value>, v: &Value) {
    if matches!(v, Value::Null) {
        return;
    }
    let next = match acc.take() {
        None => v.clone(),
        Some(m) => match cmp_vals(&m, v) {
            Some(Ordering::Greater) => v.clone(),
            _ => m,
        },
    };
    *acc = Some(next);
}

fn fold_max(acc: &mut Option<Value>, v: &Value) {
    if matches!(v, Value::Null) {
        return;
    }
    let next = match acc.take() {
        None => v.clone(),
        Some(m) => match cmp_vals(&m, v) {
            Some(Ordering::Less) => v.clone(),
            _ => m,
        },
    };
    *acc = Some(next);
}

fn bitmap_len(n: usize) -> usize {
    n.div_ceil(8)
}

fn set_null_bit(bitmap: &mut [u8], i: usize) {
    bitmap[i / 8] |= 1 << (i % 8);
}

fn push_payload(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => unreachable!("nulls live in the bitmap"),
        Value::Bool(b) => out.push(*b as u8),
        Value::Int(i) => out.extend_from_slice(&i.to_be_bytes()),
        Value::Double(d) => out.extend_from_slice(&d.to_bits().to_be_bytes()),
        // Temporal PLAIN payloads are 8B BE integers, same as Int.
        Value::Date(i) => out.extend_from_slice(&i.to_be_bytes()),
        Value::DateTime(i) => out.extend_from_slice(&i.to_be_bytes()),
        Value::Str(s) => {
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            out.extend_from_slice(&(b.len() as u32).to_be_bytes());
            out.extend_from_slice(b);
        }
    }
}

/// `[u32 BE num_values][null bitmap][payloads of the NON-NULL values]`.
fn encode_plain(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(values.len() as u32).to_be_bytes());
    let bm_off = out.len();
    out.resize(out.len() + bitmap_len(values.len()), 0);
    for (i, v) in values.iter().enumerate() {
        if matches!(v, Value::Null) {
            set_null_bit(&mut out[bm_off..], i);
        } else {
            push_payload(&mut out, v);
        }
    }
    out
}

/// Distinct string/bytes entries in first-appearance order; None when a
/// non-null value is not a string/blob (dict only serves string types).
fn dict_entries(values: &[Value]) -> Option<Vec<Vec<u8>>> {
    let mut entries: Vec<Vec<u8>> = Vec::new();
    for v in values {
        let bs: &[u8] = match v {
            Value::Null => continue,
            Value::Str(s) => s.as_bytes(),
            Value::Bytes(b) => b,
            _ => return None,
        };
        if !entries.iter().any(|e| e.as_slice() == bs) {
            entries.push(bs.to_vec());
        }
    }
    Some(entries)
}

/// DICT wins iff `dict_size * 10 <= plain_size * 7` (0.7 ratio gate).
fn dict_wins(values: &[Value], entries: &[Vec<u8>]) -> bool {
    let bitmap = bitmap_len(values.len()) as u64;
    let mut payload = 0u64;
    let mut non_null = 0u64;
    for v in values {
        if let Some(len) = match v {
            Value::Str(s) => Some(s.len()),
            Value::Bytes(b) => Some(b.len()),
            _ => None,
        } {
            payload += 4 + len as u64;
            non_null += 1;
        }
    }
    if non_null == 0 {
        return false;
    }
    let plain_size = bitmap + payload;
    let distinct: u64 = entries.iter().map(|e| 4 + e.len() as u64).sum();
    let dict_size = bitmap + 4 + distinct + 4 * non_null;
    dict_size * 10 <= plain_size * 7
}

/// `[u32 BE num_values][null bitmap][u32 BE dict_count][entries][codes]`.
fn encode_dict(values: &[Value], entries: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(values.len() as u32).to_be_bytes());
    let bm_off = out.len();
    out.resize(out.len() + bitmap_len(values.len()), 0);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        out.extend_from_slice(&(e.len() as u32).to_be_bytes());
        out.extend_from_slice(e);
    }
    for (i, v) in values.iter().enumerate() {
        let bs: &[u8] = match v {
            Value::Null => {
                set_null_bit(&mut out[bm_off..], i);
                continue;
            }
            Value::Str(s) => s.as_bytes(),
            Value::Bytes(b) => b,
            _ => unreachable!("dict pages only hold strings/blobs"),
        };
        let code = entries
            .iter()
            .position(|e| e.as_slice() == bs)
            .expect("dict_entries collected every non-null value");
        out.extend_from_slice(&(code as u32).to_be_bytes());
    }
    out
}

fn encode_page(ty: SqlType, values: &[Value]) -> EncodedPage {
    let mut null_count = 0u32;
    let mut min = None;
    let mut max = None;
    for v in values {
        if matches!(v, Value::Null) {
            null_count += 1;
        }
        fold_min(&mut min, v);
        fold_max(&mut max, v);
    }
    let entries = match ty {
        SqlType::VarChar | SqlType::Blob => dict_entries(values),
        _ => None,
    };
    let use_dict = matches!(&entries, Some(e) if dict_wins(values, e));
    let (encoding, bytes) = if use_dict {
        (
            ENC_DICT.to_string(),
            encode_dict(values, entries.as_ref().unwrap()),
        )
    } else {
        (ENC_PLAIN.to_string(), encode_plain(values))
    };
    EncodedPage {
        encoding,
        bytes,
        num_values: values.len() as u32,
        null_count,
        min: min.unwrap_or(Value::Null),
        max: max.unwrap_or(Value::Null),
    }
}

/// Chunk one column's values into pages (<= MAX_PAGE_VALUES each) and
/// encode them. `values.len()` may be 0 (no pages). Type comes from the
/// schema; values are trusted to match it (the write path coerced them).
pub fn encode_column_pages(ty: SqlType, values: &[Value]) -> Vec<EncodedPage> {
    values
        .chunks(MAX_PAGE_VALUES)
        .map(|chunk| encode_page(ty, chunk))
        .collect()
}

/// Encode a full segment file for `rows` (all rows same width =
/// schema.columns.len(); validate the width, error otherwise).
/// Returns (file bytes, per-column zones in schema column order).
/// Zero rows is legal (empty pages; zones min/max = Null, null_count 0).
pub fn build_segment(
    schema: &TableSchema,
    rows: &[Vec<Value>],
) -> Result<(Vec<u8>, Vec<ColumnZone>), String> {
    let width = schema.columns.len();
    for (i, row) in rows.iter().enumerate() {
        if row.len() != width {
            return Err(format!("row {i} has width {}, expected {width}", row.len()));
        }
    }
    let mut body = Vec::new();
    let mut columns = Vec::with_capacity(width);
    let mut zones = Vec::with_capacity(width);
    for (ci, col) in schema.columns.iter().enumerate() {
        let values: Vec<Value> = rows.iter().map(|r| r[ci].clone()).collect();
        let pages = encode_column_pages(col.sql_type, &values);
        let mut footers = Vec::with_capacity(pages.len());
        let (mut ordinal, mut null_count) = (0u32, 0u64);
        let (mut min, mut max) = (None, None);
        for p in &pages {
            footers.push(PageFooter {
                offset: format::MAGIC.len() as u64 + body.len() as u64,
                len: p.bytes.len() as u32,
                num_values: p.num_values,
                first_ordinal: ordinal,
                encoding: p.encoding.clone(),
                null_count: p.null_count,
                min: p.min.clone(),
                max: p.max.clone(),
            });
            ordinal += p.num_values;
            null_count += u64::from(p.null_count);
            fold_min(&mut min, &p.min);
            fold_max(&mut max, &p.max);
            body.extend_from_slice(&p.bytes);
        }
        columns.push(ColumnFooter {
            name: col.name.clone(),
            pages: footers,
        });
        zones.push(ColumnZone {
            name: col.name.clone(),
            sql_type: col.sql_type,
            nullable: col.nullable,
            null_count,
            min: min.unwrap_or(Value::Null),
            max: max.unwrap_or(Value::Null),
        });
    }
    let footer = Footer {
        version: FOOTER_VERSION,
        num_rows: rows.len() as u64,
        columns,
    };
    let file = format::assemble_file(&footer, &body)?;
    Ok((file, zones))
}
