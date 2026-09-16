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

use crate::sql::storage::schema::{SqlType, Value, MAX_DECIMAL_SCALE};

/// Physical record kinds of the SQL data plane (RESP kinds end at 0x12,
/// 0xFD is the expire index; SQL starts at 0x20).
pub const KIND_SQL_ROW: u8 = 0x20;
pub const KIND_SQL_INDEX: u8 = 0x21;
pub const KIND_SQL_UNIQUE_INDEX: u8 = 0x22;
/// Columnar segment meta — JSON `SegmentMeta` value.
pub const KIND_SQL_SEGMENT: u8 = 0x23;

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
        // Fixed-point decimal: scale byte + 16B BE mantissa.
        Value::Decimal(m, s) => {
            out.push(0x08);
            out.push(*s);
            out.extend_from_slice(&m.to_be_bytes());
        }
        Value::Date(i) => {
            out.push(0x06);
            out.extend_from_slice(&i.to_be_bytes());
        }
        Value::DateTime(i) => {
            out.push(0x07);
            out.extend_from_slice(&i.to_be_bytes());
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
        // Temporal payloads are 8B BE integers (days / micros).
        0x06 => {
            let (raw, rest) = rest.split_at_checked(8).ok_or("date payload truncated")?;
            Ok((
                Value::Date(i64::from_be_bytes(raw.try_into().unwrap())),
                rest,
            ))
        }
        // Fixed-point decimal: scale byte + 16B BE mantissa.
        0x08 => {
            let (scale, rest) = rest.split_first().ok_or("decimal scale truncated")?;
            if *scale > MAX_DECIMAL_SCALE {
                return Err(format!("decimal scale {scale} exceeds maximum"));
            }
            let (raw, rest) = rest
                .split_at_checked(16)
                .ok_or("decimal payload truncated")?;
            Ok((
                Value::Decimal(i128::from_be_bytes(raw.try_into().unwrap()), *scale),
                rest,
            ))
        }
        0x07 => {
            let (raw, rest) = rest
                .split_at_checked(8)
                .ok_or("datetime payload truncated")?;
            Ok((
                Value::DateTime(i64::from_be_bytes(raw.try_into().unwrap())),
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
        Value::Int(i) => key_int(0x02, *i),
        Value::Date(i) => key_int(0x06, *i),
        Value::DateTime(i) => key_int(0x07, *i),
        // Fixed-width decimal: tag + 16B sign-flipped mantissa. The
        // scale is NOT in the bytes -- key decode is schema-driven
        // (`SqlType::Decimal.scale`), and fixed width means index tails
        // split deterministically without a terminator.
        Value::Decimal(m, _) => key_decimal(*m),
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

/// Var-length key component: tag + escaped bytes + 0x00 terminator.
/// Embedded NULs are ESCAPED order-preservingly (0x00 -> 0x00 0xFF, the
/// FoundationDB tuple trick): every 0x00 in the stream is then either an
/// escape-pair head or the terminator, so concatenated key components
/// (multi-column pks, index value+pk tails) split without ambiguity.
/// Terminators sort below every continuation, so byte order still
/// equals value order across the escape.
fn key_bytes(tag: u8, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut v = Vec::with_capacity(bytes.len() + 2);
    v.push(tag);
    for &b in bytes {
        if b == 0x00 {
            v.push(0x00);
            v.push(0xFF);
        } else {
            v.push(b);
        }
    }
    v.push(0x00);
    Ok(v)
}

/// Fixed-width integer key component (Int/Date/DateTime): tag + 8B BE
/// sign-flipped payload, so byte order equals value order.
fn key_int(tag: u8, i: i64) -> Vec<u8> {
    let mut v = Vec::with_capacity(9);
    v.push(tag);
    v.extend_from_slice(&(i ^ i64::MIN).to_be_bytes());
    v
}

/// Fixed-width decimal key component: tag + 16B BE mantissa with the
/// sign bit flipped, so byte order equals numeric order.
fn key_decimal(m: i128) -> Vec<u8> {
    let mut v = Vec::with_capacity(17);
    v.push(0x08);
    v.extend_from_slice(&(m ^ i128::MIN).to_be_bytes());
    v
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
        // Scale comes from the schema, never the bytes (see
        // `encode_key`): the mantissa alone decides the ordering.
        (0x08, SqlType::Decimal { scale, .. }) => {
            if scale > MAX_DECIMAL_SCALE {
                return Err(format!("decimal scale {scale} exceeds maximum"));
            }
            let (raw, r) = rest.split_at_checked(16).ok_or("decimal key truncated")?;
            (
                Value::Decimal(
                    i128::from_be_bytes(raw.try_into().unwrap()) ^ i128::MIN,
                    scale,
                ),
                r,
            )
        }
        (0x06, SqlType::Date) => {
            let (raw, r) = rest.split_at_checked(8).ok_or("date key truncated")?;
            (
                Value::Date(i64::from_be_bytes(raw.try_into().unwrap()) ^ i64::MIN),
                r,
            )
        }
        (0x07, SqlType::DateTime) => {
            let (raw, r) = rest.split_at_checked(8).ok_or("datetime key truncated")?;
            (
                Value::DateTime(i64::from_be_bytes(raw.try_into().unwrap()) ^ i64::MIN),
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
            // Scan for the terminator: a 0x00 followed by 0xFF opens an
            // escaped literal NUL, any other successor ends the value
            // (see `key_bytes`). A missing terminator is malformed for
            // a chained key.
            let mut raw: Vec<u8> = Vec::with_capacity(rest.len());
            let mut i = 0;
            while i < rest.len() {
                if rest[i] != 0x00 {
                    raw.push(rest[i]);
                    i += 1;
                    continue;
                }
                let next = rest.get(i + 1).copied();
                if next == Some(0xFF) {
                    raw.push(0x00); // escaped literal NUL
                    i += 2;
                } else {
                    break; // terminator (or end of input)
                }
            }
            let (_, r) = rest
                .split_at_checked(i)
                .ok_or("varlen key missing terminator")?;
            let (_, r) = r.split_first().ok_or("varlen key missing terminator")?;
            (
                if *tag == 0x04 {
                    Value::Str(String::from_utf8(raw).map_err(|_| "invalid utf8 key")?)
                } else {
                    Value::Bytes(raw)
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

    /// Deterministic xorshift64* PRNG: no external rng crate, and the
    /// seed makes every failure reproducible.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// Full-width i128 mantissa, sign included.
        fn next_i128(&mut self) -> i128 {
            let lo = u128::from(self.next_u64());
            let hi = u128::from(self.next_u64());
            ((hi << 64) | lo) as i128
        }

        fn next_scale(&mut self) -> u8 {
            [0u8, 2, 9, 38][(self.next_u64() % 4) as usize]
        }
    }

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

    #[test]
    fn temporal_typed_round_trip() {
        for v in [
            Value::Date(0),
            Value::Date(-1),
            Value::Date(19_782), // 2024-02-29
            Value::DateTime(0),
            Value::DateTime(-1),
            Value::DateTime(1_709_208_000_000_000),
        ] {
            let enc = encode_typed(&v);
            let (back, rest) = decode_typed(&enc).expect("decode");
            assert_eq!(back, v);
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn temporal_key_order_preserving_and_no_int_collision() {
        let day = |i: i64| encode_key(&Value::Date(i)).unwrap();
        assert!(day(-1) < day(0));
        assert!(day(0) < day(19_782));
        assert!(day(i64::MIN) < day(i64::MAX));
        // Same numeric value under different tags must not collide.
        assert_ne!(day(7), encode_key(&Value::Int(7)).unwrap());
        let us = |i: i64| encode_key(&Value::DateTime(i)).unwrap();
        assert_ne!(us(7), day(7));
        assert_ne!(us(7), encode_key(&Value::Int(7)).unwrap());
        // Type-mismatched decodes are rejected.
        assert!(decode_key(&day(7), SqlType::Int).is_err());
        assert!(decode_key(&us(7), SqlType::Date).is_err());
    }

    #[test]
    fn temporal_key_round_trip_negative_days() {
        for days in [-719_162i64, -1, 0, 19_782, 2_932_893] {
            let enc = encode_key(&Value::Date(days)).unwrap();
            let (back, rest) = decode_key(&enc, SqlType::Date).expect("decode");
            assert_eq!(back, Value::Date(days));
            assert!(rest.is_empty());
        }
        for us in [-86_400_000_000i64, -1, 0, 1_709_208_000_123_456] {
            let enc = encode_key(&Value::DateTime(us)).unwrap();
            let (back, rest) = decode_key(&enc, SqlType::DateTime).expect("decode");
            assert_eq!(back, Value::DateTime(us));
            assert!(rest.is_empty());
        }
    }
    #[test]
    fn decimal_typed_round_trip_and_scale_guard() {
        for (m, s) in [
            (0i128, 0u8),
            (-1, 2),
            (12345, 9),
            (i128::MIN, 38),
            (i128::MAX, 38),
        ] {
            let enc = encode_typed(&Value::Decimal(m, s));
            let (back, rest) = decode_typed(&enc).expect("decode");
            assert_eq!(back, Value::Decimal(m, s));
            assert!(rest.is_empty());
        }
        // Trailing bytes survive (chained payloads).
        let mut enc = encode_typed(&Value::Decimal(-7, 3));
        enc.extend_from_slice(&[0xCC]);
        let (back, rest) = decode_typed(&enc).expect("decode");
        assert_eq!(back, Value::Decimal(-7, 3));
        assert_eq!(rest, &[0xCC]);
        // Scale beyond the cap is corrupt, not clampable.
        assert!(decode_typed(&[0x08, 39, 0]).is_err());
        // Truncated mantissa.
        assert!(decode_typed(&[0x08, 2, 0, 0, 0]).is_err());
        assert!(decode_typed(&[0x08]).is_err());
    }

    #[test]
    fn decimal_key_order_scan_neighbors_strictly_increase() {
        // Exhaustive scan: every adjacent pair of mantissae in
        // -2000..2000 must encode strictly increasing bytes.
        let key = |m: i128| encode_key(&Value::Decimal(m, 2)).unwrap();
        for m in -2000i128..2000 {
            assert!(key(m) < key(m + 1), "order broken at {m}");
        }
    }

    #[test]
    fn decimal_key_order_boundaries_and_powers_of_ten() {
        let key = |m: i128| encode_key(&Value::Decimal(m, 0)).unwrap();
        assert!(key(i128::MIN) < key(i128::MIN + 1));
        assert!(key(i128::MAX - 1) < key(i128::MAX));
        assert!(key(i128::MIN) < key(0));
        assert!(key(0) < key(i128::MAX));
        // Sign change is the sharpest boundary.
        assert!(key(-1) < key(0));
        assert!(key(0) < key(1));
        // Power-of-ten mantissa neighbors (digit rollovers).
        for k in 0..=38 {
            let p = 10i128.pow(k);
            assert!(key(p - 1) < key(p), "10^{k}");
            assert!(key(p) < key(p + 1), "10^{k}");
            assert!(key(-p - 1) < key(-p), "-10^{k}");
            assert!(key(-p) < key(-p + 1), "-10^{k}");
        }
    }

    #[test]
    fn decimal_key_order_random_pairs_across_scales() {
        // The key encoding ignores scale: ordering is the mantissa's.
        // Random full-width pairs at scales {0,2,9,38} must hold.
        let mut rng = Rng(0x5EED_1234_ABCD_0001);
        for _ in 0..2000 {
            let a = rng.next_i128();
            let b = rng.next_i128();
            let sa = rng.next_scale();
            let sb = rng.next_scale();
            let ka = encode_key(&Value::Decimal(a, sa)).unwrap();
            let kb = encode_key(&Value::Decimal(b, sb)).unwrap();
            assert_eq!(a.cmp(&b), ka.cmp(&kb), "({a},{sa}) vs ({b},{sb})");
        }
    }

    #[test]
    fn decimal_key_round_trip_schema_driven_scale() {
        for (m, s) in [
            (0i128, 0u8),
            (-12345, 2),
            (1, 9),
            (i128::MIN, 38),
            (i128::MAX, 0),
        ] {
            let enc = encode_key(&Value::Decimal(m, s)).unwrap();
            assert_eq!(enc.len(), 17, "fixed-width decimal key");
            let ty = SqlType::Decimal {
                precision: 38,
                scale: s,
            };
            let (back, rest) = decode_key(&enc, ty).expect("decode");
            assert_eq!(back, Value::Decimal(m, s));
            assert!(rest.is_empty());
            // Chained tail after the fixed-width component.
            let mut chained = enc.clone();
            chained.extend_from_slice(&[0xEE]);
            let (back, rest) = decode_key(&chained, ty).expect("decode");
            assert_eq!(back, Value::Decimal(m, s));
            assert_eq!(rest, &[0xEE]);
        }
        // Truncated mantissa is a loud error.
        let ty = SqlType::Decimal {
            precision: 10,
            scale: 2,
        };
        assert!(decode_key(&[0x08, 0, 0], ty).is_err());
    }

    #[test]
    fn decimal_tag_collides_with_no_other_type() {
        let d = encode_key(&Value::Decimal(7, 2)).unwrap();
        assert_eq!(d[0], 0x08);
        for other in [
            encode_key(&Value::Int(7)).unwrap(),
            encode_key(&Value::Double(7.0)).unwrap(),
            encode_key(&Value::Date(7)).unwrap(),
            encode_key(&Value::DateTime(7)).unwrap(),
            encode_key(&Value::Str("7".into())).unwrap(),
        ] {
            assert_ne!(d, other);
        }
        // Typed form is distinct from every other tag too.
        assert_eq!(encode_typed(&Value::Decimal(7, 2))[0], 0x08);
        // Type-mismatched decodes are rejected both ways.
        let ty = SqlType::Decimal {
            precision: 10,
            scale: 2,
        };
        assert!(decode_key(&d, SqlType::Int).is_err());
        assert!(decode_key(&encode_key(&Value::Int(7)).unwrap(), ty).is_err());
        assert!(decode_key(&encode_key(&Value::Date(7)).unwrap(), ty).is_err());
    }
}
