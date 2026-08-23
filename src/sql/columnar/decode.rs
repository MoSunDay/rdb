//! Segment file reader: footer validation (delegated to format) plus
//! page decoding with projection (only the requested column pages are
//! touched) and page-index access for zonemap pruning.

use crate::sql::columnar::encode::{ENC_DICT, ENC_PLAIN};
use crate::sql::columnar::format::{self, ColumnFooter, Footer, PageFooter};
use crate::sql::storage::schema::{SqlType, Value};

/// Validate + parse a segment file; returns (footer owned, page region
/// slice borrowed from `bytes`). Projection: callers decode only the
/// columns they need via [`decode_column`] / [`decode_page`].
pub fn open(bytes: &[u8]) -> Result<(Footer, &[u8]), String> {
    let (region, footer) = format::split_file(bytes)?;
    Ok((footer, region))
}

/// Decode one page of a column. `file` is the WHOLE segment file: the
/// footer records absolute file offsets, so the page bytes are sliced
/// out of `file` directly (the region [`open`] returned is a sub-slice
/// of it and cannot address absolute offsets on its own).
pub fn decode_page(file: &[u8], page: &PageFooter, ty: SqlType) -> Result<Vec<Value>, String> {
    let start = usize::try_from(page.offset).map_err(|_| "corrupt page: offset".to_string())?;
    let end = start
        .checked_add(page.len as usize)
        .ok_or("corrupt page: offset overflow")?;
    if end > file.len() {
        return Err("corrupt page: out of bounds".to_string());
    }
    let mut buf = &file[start..end];
    let n = read_u32(&mut buf)? as usize;
    if n != page.num_values as usize {
        return Err("corrupt page: value count mismatch".to_string());
    }
    let bitmap = take(&mut buf, n.div_ceil(8))?;
    let null_at = |i: usize| bitmap[i / 8] >> (i % 8) & 1 == 1;
    let mut out = Vec::with_capacity(n);
    match page.encoding.as_str() {
        ENC_PLAIN => {
            for i in 0..n {
                if null_at(i) {
                    out.push(Value::Null);
                } else {
                    out.push(read_payload(&mut buf, ty)?);
                }
            }
        }
        ENC_DICT => {
            let dict_count = read_u32(&mut buf)? as usize;
            let mut dict = Vec::with_capacity(dict_count);
            for _ in 0..dict_count {
                dict.push(read_bytes(&mut buf)?.to_vec());
            }
            for i in 0..n {
                if null_at(i) {
                    out.push(Value::Null);
                    continue;
                }
                let code = read_u32(&mut buf)? as usize;
                let entry = dict
                    .get(code)
                    .ok_or("corrupt page: dict code out of range")?;
                out.push(match ty {
                    SqlType::Blob => Value::Bytes(entry.clone()),
                    _ => Value::Str(string_from(entry)?),
                });
            }
        }
        other => return Err(format!("unknown page encoding '{other}'")),
    }
    if !buf.is_empty() {
        return Err("corrupt page: trailing bytes".to_string());
    }
    Ok(out)
}

/// Decode every page of one column (in page order) into one Vec --
/// used by tests and later compaction.
pub fn decode_column(file: &[u8], col: &ColumnFooter, ty: SqlType) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for page in &col.pages {
        out.extend(decode_page(file, page, ty)?);
    }
    Ok(out)
}

fn string_from(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "corrupt page: invalid utf8".to_string())
}

/// Payload of one non-null PLAIN value, typed by the schema column.
fn read_payload(buf: &mut &[u8], ty: SqlType) -> Result<Value, String> {
    Ok(match ty {
        SqlType::Bool => match take(buf, 1)?[0] {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            _ => return Err("corrupt page: bool payload".to_string()),
        },
        SqlType::Int => Value::Int(i64::from_be_bytes(take(buf, 8)?.try_into().unwrap())),
        SqlType::Double => Value::Double(f64::from_bits(u64::from_be_bytes(
            take(buf, 8)?.try_into().unwrap(),
        ))),
        SqlType::VarChar => Value::Str(string_from(read_bytes(buf)?)?),
        SqlType::Blob => Value::Bytes(read_bytes(buf)?.to_vec()),
    })
}

/// Bounds-checked slice consumption: any overrun is a corrupt page.
fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
    if buf.len() < n {
        return Err("corrupt page".to_string());
    }
    let (head, rest) = buf.split_at(n);
    *buf = rest;
    Ok(head)
}

fn read_u32(buf: &mut &[u8]) -> Result<u32, String> {
    Ok(u32::from_be_bytes(take(buf, 4)?.try_into().unwrap()))
}

fn read_bytes<'a>(buf: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let len = read_u32(buf)? as usize;
    take(buf, len)
}
