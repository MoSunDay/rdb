//! CRC-16-CCITT/XMODEM hashing and Redis-cluster slot math.
//!
//! Rust mirror of Go `internal/utils/hash.go` (256-entry static table,
//! poly 0x1021, init 0, no reflection, no final xor) plus the hash-tag
//! parsing rules of Go `internal/server/server.go`.

/// CRC-16/XMODEM polynomial.
const POLY: u16 = 0x1021;

/// Build the same 256-entry table Go ships as `crc16tab` in hash.go.
const fn build_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0u16;
    while (i as usize) < 256 {
        let mut crc = i << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ POLY
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
}

static CRC16TAB: [u16; 256] = build_table();

/// Go `crc16sum`: table-driven CRC-16/XMODEM.
pub fn crc16(key: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in key {
        crc = (crc << 8) ^ CRC16TAB[((crc >> 8) as u8 ^ b) as usize];
    }
    crc
}

/// Go `GetSlotNumber`: CRC-16 mod 16384 (Redis cluster slot space).
pub fn slot_number(key: &[u8]) -> u16 {
    crc16(key) % 16384
}

/// Go `GetHash256`: CRC-16 mod 256.
pub fn hash256(key: &[u8]) -> u16 {
    crc16(key) % 256
}

/// Go `GetSlotNumberWithPrefixKey`: returns the slot and its decimal ASCII
/// encoding followed by `/` (no zero padding, e.g. slot 12 -> `b"12/"`).
pub fn slot_with_prefix(key: &[u8]) -> (u16, Vec<u8>) {
    let slot = slot_number(key);
    let mut prefix = slot.to_string().into_bytes();
    prefix.push(b'/');
    (slot, prefix)
}

/// Hash-tag extraction, replicated EXACTLY from Go `internal/server/server.go`:
///
/// ```text
/// start = bytes.Index(key, "{")
/// if start != -1 { start += 1; end = bytes.Index(key[start:], "}") }
/// if start != -1 && end != -1 { use key[start : start+end] } else { whole key }
/// ```
///
/// Only the FIRST `{` is considered; `}` is searched strictly after it.
/// Empty braces `{}` yield an empty slice (=> slot 0), a `{` without a
/// closing `}` falls back to the whole key. NOTE: the empty-tag behavior
/// intentionally differs from real Redis; the Go behavior is the contract.
pub fn hash_tag(key: &[u8]) -> &[u8] {
    if let Some(start) = key.iter().position(|&b| b == b'{') {
        let rest = &key[start + 1..];
        if let Some(end) = rest.iter().position(|&b| b == b'}') {
            return &rest[..end];
        }
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standard CRC-16/XMODEM check value.
    #[test]
    fn crc16_check_value() {
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    /// Spot-check generated table entries against the Go `crc16tab` literal.
    #[test]
    fn table_matches_go_literal() {
        assert_eq!(CRC16TAB[0], 0x0000);
        assert_eq!(CRC16TAB[1], 0x1021);
        assert_eq!(CRC16TAB[2], 0x2042);
        assert_eq!(CRC16TAB[16], 0x1231);
        assert_eq!(CRC16TAB[128], 0x9188);
        assert_eq!(CRC16TAB[255], 0x1ef0);
    }

    /// Golden vectors produced by the real Go implementation
    /// (crc16sum + table copied verbatim from internal/utils/hash.go,
    /// tag logic copied from internal/server/server.go). `srv_slot` is the
    /// routing slot Go's server computes, i.e. the slot of the hash tag
    /// (`GetSlotNumberWithPrefixKey(key[start:start+end])`).
    const GOLDEN: &[(&str, u16, u16, u16, &str, &str, u16)] = &[
        // (key, crc, slot, hash256, prefix, tag, srv_slot)
        ("foo", 0xAF96, 12182, 150, "12182/", "foo", 12182),
        ("bar", 0x93C5, 5061, 197, "5061/", "bar", 5061),
        ("user1000", 0x4D73, 3443, 115, "3443/", "user1000", 3443),
        (
            "{user1000}.following",
            0x6FBA,
            12218,
            186,
            "12218/",
            "user1000",
            3443,
        ),
        ("{}", 0x7B99, 15257, 153, "15257/", "", 0),
        (
            "hello:world",
            0xA762,
            10082,
            98,
            "10082/",
            "hello:world",
            10082,
        ),
        ("", 0x0000, 0, 0, "0/", "", 0),
        ("{abc", 0xC1BC, 444, 188, "444/", "{abc", 444),
        ("a{b}c{d}e", 0x034A, 842, 74, "842/", "b", 3300),
    ];

    #[test]
    fn golden_vectors() {
        for &(key, crc, slot, h256, prefix, tag, srv_slot) in GOLDEN {
            let k = key.as_bytes();
            assert_eq!(crc16(k), crc, "crc16({key:?})");
            assert_eq!(slot_number(k), slot, "slot_number({key:?})");
            assert_eq!(hash256(k), h256, "hash256({key:?})");
            assert_eq!(hash_tag(k), tag.as_bytes(), "hash_tag({key:?})");
            // Raw key encoding (no tag resolution).
            assert_eq!(slot_with_prefix(k), (slot, prefix.as_bytes().to_vec()));
            // Go server pipeline: prefix key + slot are built from the tag.
            let (got_srv_slot, got_srv_prefix) = slot_with_prefix(hash_tag(k));
            assert_eq!(got_srv_slot, srv_slot, "server slot of {key:?}");
            assert_eq!(
                got_srv_prefix,
                format!("{srv_slot}/").into_bytes(),
                "server prefix of {key:?}"
            );
        }
    }

    #[test]
    fn hash_tag_edge_cases() {
        assert_eq!(hash_tag(b"{user1000}.following"), b"user1000");
        // Empty braces -> empty tag (Go quirk: slot 0, unlike real Redis).
        assert_eq!(hash_tag(b"{}"), b"");
        // '{' without a closing '}' -> whole key.
        assert_eq!(hash_tag(b"{abc"), b"{abc");
        // Only the FIRST '{' is considered.
        assert_eq!(hash_tag(b"a{b}c{d}e"), b"b");
        assert_eq!(hash_tag(b"{a}{b}"), b"a");
        // '}' before any '{' is irrelevant; '{' without '}' -> whole key.
        assert_eq!(hash_tag(b"}abc{"), b"}abc{");
        // No braces -> whole key.
        assert_eq!(hash_tag(b"user1000"), b"user1000");
        assert_eq!(hash_tag(b""), b"");
    }

    #[test]
    fn empty_key_and_empty_tag_slot_zero() {
        assert_eq!(slot_number(b""), 0);
        assert_eq!(slot_with_prefix(b""), (0, b"0/".to_vec()));
        // Go server flow for key "{}": tag is empty -> slot 0, prefix "0/".
        assert_eq!(slot_with_prefix(hash_tag(b"{}")), (0, b"0/".to_vec()));
    }

    #[test]
    fn slot_with_prefix_no_zero_padding() {
        // Slot 12 -> b"12/", never b"0012/".
        assert_eq!(slot_with_prefix(b""), (0, b"0/".to_vec()));
        let (slot, prefix) = slot_with_prefix(b"foo");
        assert_eq!(prefix, format!("{slot}/").into_bytes());
        assert_eq!(prefix.last(), Some(&b'/'));
        assert!(prefix.len() <= 6); // up to 5 digits + '/'
    }
}
