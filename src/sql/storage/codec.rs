//! Value <-> bytes codecs for SQL rows and index entries.
//!
//! Two encodings exist:
//! - **key encoding**: order-preserving (signed ints flip the sign bit,
//!   doubles use the total-order bit trick) so byte order = value order.
//!   Used inside PK/index key bytes, never self-delimiting (schema drives
//!   the decode).
//! - **payload encoding**: fixed-width fields, lengths for var-length.
//!   Used for row values and index value payloads.
//!
//! Byte-level layouts follow the typed-codec style of `ds/codec.rs`
//! (tag byte + payload), but with SQL-specific kind bytes (0x20/0x21/0x22)
//! that do not collide with the RESP families recorded there.

use crate::sql::storage::schema::{SqlType, Value};

/// Physical record kinds of the SQL data plane (RESP kinds end at 0x12,
/// 0xFD is the expire index; SQL starts at 0x20).
pub const KIND_SQL_ROW: u8 = 0x20;
pub const KIND_SQL_INDEX: u8 = 0x21;
pub const KIND_SQL_UNIQUE_INDEX: u8 = 0x22;

/// Encode a value with its type tag (self-describing; used for index
/// payloads where the reader may only know the index column type later).
pub fn encode_typed(value: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    match value {
        Value::Null => out.push(0x00),
        Value::Bool(b) => {
            out.push(0x01);
            out.push(*b as u8);
        }
        Value::Int(i) => {
            out.push(0x02);
            out.extend_from_slice(&i.to_be_bytes());
        }
        Value::Double(d) => {
            out.push(0x03);
            out.extend_from_slice(&d.to_bits().to_be_bytes());
        }
        Value::Str(s) => {
            out.push(0x04);
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(0x05);
            out.extend_from_slice(&(b.len() as u32).to_be_bytes());
            out.extend_from_slice(b);
        }
    }
    out
}

/// Decode a [`encode_typed`] payload.
pub fn decode_typed(bytes: &[u8]) -> Result<(Value, &[u8]), String> {
    let (tag, rest) = bytes.split_first().ok_or("empty typed value")?;
    match *tag {
        0x00 => Ok((Value::Null, rest)),
        0x01 => {
            let (b, rest) = rest.split_first().ok_or("bool payload truncated")?;
            Ok((Value::Bool(*b != 0), rest))
        }
        0x02 => {
            if rest.len() < 8 {
                return Err("int payload truncated".into());
            }
            let (raw, rest) = rest.split_at(8);
            Ok((
                Value::Int(i64::from_be_bytes(raw.try_into().unwrap())),
                rest,
            ))
        }
        0x03 => {
            if rest.len() < 8 {
                return Err("double payload truncated".into());
            }
            let (raw, rest) = rest.split_at(8);
            Ok((
                Value::Double(f64::from_bits(u64::from_be_bytes(raw.try_into().unwrap()))),
                rest,
            ))
        }
        0x04 | 0x05 => {
            if rest.len() < 4 {
                return Err("varlen payload truncated".into());
            }
            let (len_raw, rest) = rest.split_at(4);
            let len = u32::from_be_bytes(len_raw.try_into().unwrap()) as usize;
            if rest.len() < len {
                return Err("varlen payload shorter than length".into());
            }
            let (raw, rest) = rest.split_at(len);
            Ok((
                if *tag == 0x04 {
                    Value::Str(String::from_utf8_lossy(raw).into_owned())
                } else {
                    Value::Bytes(raw.to_vec())
                },
                rest,
            ))
        }
        other => Err(format!("unknown typed value tag 0x{other:02x}")),
    }
}

/// Order-preserving key encoding: byte order equals value order (NULL
/// first, then bool/int/double by magnitude, strings bytewise).
pub fn encode_key(value: &Value) -> Result<Vec<u8>, String> {
    Ok(match value {
        Value::Null => vec![0x00],
        Value::Bool(b) => vec![0x01, *b as u8],
        Value::Int(i) => {
            let mut v = vec![0x02];
            v.extend_from_slice(&(*i ^ i64::MIN).to_be_bytes());
            v
        }
        Value::Double(d) => {
            let bits = d.to_bits();
            // Positive doubles keep the sign bit set (sorts after
            // negatives), negatives invert the rest.
            let ordered = if bits & 0x8000_0000_0000_0000 != 0 {
                !bits
            } else {
                bits | 0x8000_0000_0000_0000
            };
            let mut v = vec![0x03];
            v.extend_from_slice(&ordered.to_be_bytes());
            v
        }
        Value::Str(s) => key_bytes(0x04, s.as_bytes())?,
        Value::Bytes(b) => key_bytes(0x05, b)?,
    })
}

/// Var-length key component: tag + bytes + 0x00 terminator. Embedded NUL
/// bytes are REJECTED -- the terminator is the only self-delimiter, so an
/// embedded one would silently truncate on decode. (v1 restriction:
/// indexed/PK strings may not contain NUL.)
fn key_bytes(tag: u8, bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.contains(&0x00) {
        return Err("NUL byte not allowed in string/blob key values".into());
    }
    let mut v = Vec::with_capacity(bytes.len() + 2);
    v.push(tag);
    v.extend_from_slice(bytes);
    v.push(0x00);
    Ok(v)
}

/// Decode one order-preserving key value of a known type. Returns the value
/// and the remaining bytes (callers chaining fixed-type keys).
pub fn decode_key(bytes: &[u8], ty: SqlType) -> Result<(Value, &[u8]), String> {
    let (tag, rest) = bytes.split_first().ok_or("empty key value")?;
    let (val, rest) = match (*tag, ty) {
        (0x01, SqlType::Bool) => {
            let (b, r) = rest.split_first().ok_or("bool key truncated")?;
            (Value::Bool(*b != 0), r)
        }
        (0x02, SqlType::Int) => {
            let (raw, r) = rest.split_at_checked(8).ok_or("int key truncated")?;
            (
                Value::Int(i64::from_be_bytes(raw.try_into().unwrap()) ^ i64::MIN),
                r,
            )
        }
        (0x03, SqlType::Double) => {
            let (raw, r) = rest.split_at_checked(8).ok_or("double key truncated")?;
            let ordered = u64::from_be_bytes(raw.try_into().unwrap());
            let bits = if ordered & 0x8000_0000_0000_0000 != 0 {
                ordered & 0x7fff_ffff_ffff_ffff
            } else {
                !ordered
            };
            (Value::Double(f64::from_bits(bits)), r)
        }
        (0x04, SqlType::VarChar) | (0x05, SqlType::Blob) => {
            // Terminator is required: encoded keys of these types always
            // carry one, so position 0 would mean an empty string key
            // followed by nothing -- malformed for a chained key.
            let end = rest
                .iter()
                .position(|&b| b == 0x00)
                .ok_or("varlen key unterminated")?;
            let (raw, r_with_term) = rest.split_at(end);
            let (_, r) = r_with_term
                .split_first()
                .ok_or("varlen key missing terminator")?;
            (
                if *tag == 0x04 {
                    Value::Str(String::from_utf8(raw.to_vec()).map_err(|_| "invalid utf8 key")?)
                } else {
                    Value::Bytes(raw.to_vec())
                },
                r,
            )
        }
        (t, _) => return Err(format!("key tag 0x{t:02x} does not match type {ty:?}")),
    };
    Ok((val, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_round_trip_all_kinds() {
        let vals = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int(-5),
            Value::Double(1.5),
            Value::Str("h\u{e9}llo".into()),
            Value::Bytes(vec![0, 1, 255]),
        ];
        for v in vals {
            let enc = encode_typed(&v);
            let (back, rest) = decode_typed(&enc).expect("decode");
            assert_eq!(back, v);
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn key_order_preserving_ints() {
        let enc = |i: i64| encode_key(&Value::Int(i)).expect("enc");
        assert!(enc(-3) < enc(-1));
        assert!(enc(-1) < enc(0));
        assert!(enc(0) < enc(2));
        assert!(enc(i64::MIN) < enc(i64::MAX));
    }

    #[test]
    fn key_order_preserving_strings_and_doubles() {
        assert!(
            encode_key(&Value::Str("a".into())).unwrap()
                < encode_key(&Value::Str("ab".into())).unwrap()
        );
        assert!(
            encode_key(&Value::Double(-2.0)).unwrap() < encode_key(&Value::Double(1.0)).unwrap()
        );
        let enc = encode_key(&Value::Double(0.25)).unwrap();
        let (back, rest) = decode_key(&enc, SqlType::Double).expect("decode");
        assert_eq!(back, Value::Double(0.25));
        assert!(rest.is_empty());
    }

    #[test]
    fn chained_key_decode_consumes_prefix() {
        let mut enc = encode_key(&Value::Int(9)).unwrap();
        enc.extend_from_slice(&[0xAA, 0xBB]);
        let (v, rest) = decode_key(&enc, SqlType::Int).expect("decode");
        assert_eq!(v, Value::Int(9));
        assert_eq!(rest, &[0xAA, 0xBB]);
    }
}
