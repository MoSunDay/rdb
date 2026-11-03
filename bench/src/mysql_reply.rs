//! MySQL reply-side wire decoding for the bench's hand-rolled client:
//! the HandshakeV10 greeting (server version + the 20-byte auth salt),
//! OK/ERR/EOF packet classification and the length-encoded integers and
//! strings of the text result-set protocol. Pure byte-slicing functions
//! only; the async packet loop that drives them lives in `mysql`,
//! mirroring the kafka.rs / kafka_reply.rs split.

/// What the handshake needs from the server greeting.
pub struct Greeting {
    pub version: String,
    /// 20-byte mysql_native_password scramble.
    pub salt: Vec<u8>,
    /// Auth plugin name offered (answered verbatim by the client).
    pub plugin: String,
}

/// Classify a reply packet's first byte: 0x00 OK, 0xff ERR, 0xfe EOF
/// (only when the payload is shorter than 9 bytes -- otherwise 0xfe is
/// an ordinary length-encoded value header), anything else is data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplyKind {
    Ok,
    Err,
    Eof,
    Data,
}

pub fn reply_kind(payload: &[u8]) -> ReplyKind {
    match payload.first() {
        Some(&0x00) => ReplyKind::Ok,
        Some(&0xff) => ReplyKind::Err,
        Some(&0xfe) if payload.len() < 9 => ReplyKind::Eof,
        _ => ReplyKind::Data,
    }
}

/// Parse a HandshakeV10 payload: protocol 10, NUL-terminated version,
/// connection id, 8-byte salt part 1, capability/status/charset block,
/// 13-byte salt part 2 (last byte is the trailing NUL), plugin name.
pub fn parse_greeting(payload: &[u8]) -> Result<Greeting, String> {
    let bad = |what: &str| format!("bad handshake greeting: {what}");
    if payload.first() != Some(&10) {
        return Err(bad("protocol version != 10"));
    }
    let version_end = payload[1..]
        .iter()
        .position(|b| *b == 0)
        .map(|p| p + 1)
        .ok_or_else(|| bad("unterminated server version"))?;
    let version = String::from_utf8_lossy(&payload[1..version_end]).into_owned();
    let mut p = version_end + 1 + 4; // skip version NUL + connection id
    let salt1 = payload
        .get(p..p + 8)
        .ok_or_else(|| bad("salt part 1"))?
        .to_vec();
    p += 8 + 1; // salt part 1 + filler
    let caps_lo = u16::from_le_bytes(
        payload
            .get(p..p + 2)
            .ok_or_else(|| bad("capabilities"))?
            .try_into()
            .expect("2 bytes"),
    ) as u32;
    p += 2 + 1 + 2; // caps lo + charset + status flags
    let caps_hi = u16::from_le_bytes(
        payload
            .get(p..p + 2)
            .ok_or_else(|| bad("capabilities upper"))?
            .try_into()
            .expect("2 bytes"),
    ) as u32;
    p += 2 + 1 + 10; // caps hi + auth data len + reserved
    let caps = caps_lo | (caps_hi << 16);
    // The 13-byte part 2 only rides along when the server announced
    // CLIENT_SECURE_CONNECTION (0x8000); the salt ends at the first NUL.
    let mut salt = salt1;
    if caps & 0x8000 != 0 {
        let rest = payload.get(p..).ok_or_else(|| bad("salt part 2"))?;
        let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
        salt.extend_from_slice(&rest[..end.min(12)]);
    }
    let plugin = payload[p + 13..]
        .split(|b| *b == 0)
        .next()
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .unwrap_or_default();
    Ok(Greeting {
        version,
        salt,
        plugin,
    })
}

/// ERR packet: "#<errno as u16 LE> <sqlstate> <message>" as one string.
pub fn parse_err(payload: &[u8]) -> String {
    let code = payload
        .get(1..3)
        .map(|b| u16::from_le_bytes(b.try_into().expect("2 bytes")))
        .unwrap_or(0);
    // Skip '#' + 5-byte sqlstate when present, else the message is raw.
    let text_at = if payload.get(3) == Some(&b'#') { 9 } else { 3 };
    let text = payload.get(text_at..).unwrap_or(&[]);
    format!("mysql error {code}: {}", String::from_utf8_lossy(text))
}

/// Column count of a result set: the first reply packet, a length-encoded
/// integer, when it is not OK/ERR/EOF.
pub fn column_count(payload: &[u8]) -> Result<usize, String> {
    match lenenc_int(payload, 0) {
        Some((n, consumed)) if consumed <= payload.len() && n > 0 && n < 4096 => Ok(n as usize),
        _ => Err(format!("bad column count packet: {payload:?}")),
    }
}

/// Length-encoded integer at `pos`: 1-byte prefix under 251, else
/// 2/3/8 bytes little-endian; 0xfb marks NULL (strings only) and is
/// rejected here. Returns the value and the position just past it.
pub fn lenenc_int(payload: &[u8], pos: usize) -> Option<(u64, usize)> {
    let first = *payload.get(pos)?;
    match first {
        0xfb => None, // NULL marker (meaningful for strings only)
        0xfc => {
            let b = payload.get(pos + 1..pos + 3)?;
            let v = u16::from_le_bytes(b.try_into().expect("2 bytes")) as u64;
            Some((v, pos + 3))
        }
        0xfd => {
            let b = payload.get(pos + 1..pos + 4)?;
            Some((
                u64::from_le_bytes([b[0], b[1], b[2], 0, 0, 0, 0, 0]),
                pos + 4,
            ))
        }
        0xfe => payload
            .get(pos + 1..pos + 9)?
            .try_into()
            .ok()
            .map(|b: [u8; 8]| (u64::from_le_bytes(b), pos + 9)),
        n => Some((n as u64, pos + 1)),
    }
}

/// Length-encoded string at `pos`: a length-encoded length then that
/// many bytes. Returns the slice and the position just past it.
pub fn lenenc_str(payload: &[u8], pos: usize) -> Option<(&[u8], usize)> {
    let (len, next) = lenenc_int(payload, pos)?;
    let end = next.checked_add(len as usize)?;
    payload.get(next..end).map(|s| (s, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-for-byte the opensrv-mysql 0.7 HandshakeV10 layout the rdb
    /// frontend writes (salt ";X,po_k}>o6^Wz!/kM}N" adjusted, plugin
    /// mysql_native_password).
    fn greeting_bytes() -> Vec<u8> {
        let mut p = vec![10];
        p.extend_from_slice(b"8.0.32-rdb");
        p.push(0);
        p.extend_from_slice(&9u32.to_le_bytes());
        p.extend_from_slice(b";X,po_k}"); // salt part 1
        p.push(0); // filler
        p.extend_from_slice(&[0x08, 0x82]); // caps lo (PROTOCOL_41|SECURE_CONNECTION|...)
        p.push(0x21); // charset
        p.extend_from_slice(&[0x02, 0x00]); // status
        p.extend_from_slice(&[0x28, 0x01]); // caps hi (PLUGIN_AUTH|DEPRECATE_EOF|...)
        p.push(21); // auth data len
        p.extend_from_slice(&[0u8; 10]); // reserved
        p.extend_from_slice(b">o6^Wz!/kM}N"); // salt part 2 (12 bytes)
        p.push(0);
        p.extend_from_slice(b"mysql_native_password");
        p.push(0);
        p
    }

    #[test]
    fn greeting_parses_salt_and_plugin() {
        let g = parse_greeting(&greeting_bytes()).expect("greeting");
        assert_eq!(g.version, "8.0.32-rdb");
        assert_eq!(g.salt.len(), 20);
        assert_eq!(&g.salt[..8], b";X,po_k}");
        assert_eq!(&g.salt[8..], b">o6^Wz!/kM}N");
        assert_eq!(g.plugin, "mysql_native_password");
    }

    #[test]
    fn reply_kind_classification() {
        assert_eq!(reply_kind(&[0x00, 0x00]), ReplyKind::Ok);
        assert_eq!(reply_kind(&[0xff, 0x30, 0x04]), ReplyKind::Err);
        assert_eq!(reply_kind(&[0xfe, 0x00, 0x00, 0x02, 0x00]), ReplyKind::Eof);
        // A 0xfe first byte in a long payload is data (lenenc header).
        assert_eq!(reply_kind(&[0xfe; 12]), ReplyKind::Data);
        assert_eq!(reply_kind(&[0x03]), ReplyKind::Data);
    }

    #[test]
    fn err_packet_text() {
        let mut p = vec![0xff];
        p.extend_from_slice(&1071u16.to_le_bytes());
        p.extend_from_slice(b"#HY000Unknown table");
        assert_eq!(parse_err(&p), "mysql error 1071: Unknown table");
    }

    #[test]
    fn lenenc_roundtrip() {
        assert_eq!(lenenc_int(&[0x05], 0), Some((5, 1)));
        // 2-byte form: 0xfc then the raw u16 value little-endian.
        assert_eq!(lenenc_int(&[0xfc, 0x00, 0x01], 0), Some((256, 3)));
        assert_eq!(column_count(&[0x04]).ok(), Some(4));
        assert!(column_count(&[0xfb]).is_err());
        let mut row = vec![3];
        row.extend_from_slice(b"abc");
        row.push(0);
        let (s, next) = lenenc_str(&row, 0).expect("str");
        assert_eq!((s, next), (b"abc".as_slice(), 4));
        assert!(lenenc_str(&[0xfb], 0).is_none()); // NULL marker
    }
}
