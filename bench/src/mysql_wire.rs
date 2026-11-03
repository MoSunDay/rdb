//! MySQL request-side wire codec for the bench's hand-rolled client:
//! the HandshakeResponse41 builder with the mysql_native_password
//! scramble, and COM_QUERY over the text protocol. Exactly what the
//! load loops need -- no TLS, no compression, no prepared statements
//! (the query text protocol suffices), mirroring the kafka_wire.rs
//! precedent of keeping the bench crate independent of the rdb lib.
//! Reply decoding lives in `mysql_reply`.

use sha1::{Digest, Sha1};

// ---- capability flags (client announcement) ------------------------------

const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;

/// Capabilities this client announces. Deliberately NOT set:
/// `CLIENT_DEPRECATE_EOF` (result sets then end in classic 0xFE EOF
/// packets, the simpler terminator to parse), `CLIENT_CONNECT_WITH_DB`
/// (single default database), `CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA`
/// (the auth response keeps its 1-byte length prefix).
pub const CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_PLUGIN_AUTH;

/// Max packet payload before the 3-byte length field saturates.
pub const MAX_PAYLOAD: usize = 0x00ff_ffff;

/// Auth plugin the rdb frontend offers (and this client answers with).
pub const AUTH_PLUGIN: &str = "mysql_native_password";

/// SHA1 of `x` (mysql_native_password stages are all 20 bytes).
fn sha1_20(x: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out.copy_from_slice(&Sha1::digest(x));
    out
}

/// mysql_native_password client token:
/// `SHA1(pass) XOR SHA1(salt ++ SHA1(SHA1(pass)))`; an empty password
/// sends an empty token (mirrors the server-side `native_password_matches`).
pub fn native_password_token(salt: &[u8], password: &str) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let stage1 = sha1_20(password.as_bytes());
    let stage2 = sha1_20(&stage1);
    let mut seed = salt.to_vec();
    seed.extend_from_slice(&stage2);
    let mask = sha1_20(&seed);
    stage1.iter().zip(mask.iter()).map(|(a, b)| a ^ b).collect()
}

/// HandshakeResponse41 payload for `user`/`password` against the given
/// greeting `salt` (4.1 layout: caps, max packet, charset, 23 filler
/// bytes, NUL-terminated user, length-prefixed auth token, plugin name).
pub fn handshake_response(user: &str, salt: &[u8], password: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(&CAPABILITIES.to_le_bytes());
    out.extend_from_slice(&(MAX_PAYLOAD as u32).to_le_bytes());
    out.push(0x21); // charset: utf8_general_ci (echoed back, never interpreted)
    out.extend_from_slice(&[0u8; 23]);
    out.extend_from_slice(user.as_bytes());
    out.push(0);
    let token = native_password_token(salt, password);
    out.push(token.len() as u8);
    out.extend_from_slice(&token);
    out.extend_from_slice(AUTH_PLUGIN.as_bytes());
    out.push(0);
    out
}

/// COM_QUERY payload: command byte 0x03 then the statement text.
pub fn query_packet(sql: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(sql.len() + 1);
    out.push(0x03);
    out.extend_from_slice(sql.as_bytes());
    out
}

/// Frame header for `payload` at sequence `seq` (3-byte LE length + seq).
pub fn frame_header(payload_len: usize, seq: u8) -> [u8; 4] {
    let len = payload_len.min(MAX_PAYLOAD) as u32;
    [len as u8, (len >> 8) as u8, (len >> 16) as u8, seq]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known vector, computed independently (python hashlib) and shared
    /// with the server-side `auth::native_password_known_vector` test:
    /// password "pw123", salt 1..=20.
    #[test]
    fn scramble_matches_server_side_vector() {
        let salt: Vec<u8> = (1..=20u8).collect();
        let token = native_password_token(&salt, "pw123");
        assert_eq!(
            token,
            vec![
                0x80, 0xf4, 0x28, 0x0c, 0xa4, 0xd5, 0x7f, 0xdf, 0x04, 0x73, 0x11, 0xc3, 0x7c, 0x20,
                0x94, 0xef, 0x76, 0xd5, 0x88, 0x5f
            ]
        );
        assert!(native_password_token(&salt, "").is_empty());
    }

    #[test]
    fn handshake_response_layout() {
        let salt: Vec<u8> = (1..=20u8).collect();
        let packet = handshake_response("root", &salt, "pw123");
        assert_eq!(&packet[..4], &CAPABILITIES.to_le_bytes());
        assert_eq!(packet[8], 0x21); // charset
        assert_eq!(&packet[9..32], &[0u8; 23]); // filler
        let user_end = 32 + "root".len();
        assert_eq!(&packet[32..user_end], b"root");
        assert_eq!(packet[user_end], 0);
        assert_eq!(packet[user_end + 1], 20); // token length prefix
        let plugin_at = user_end + 2 + 20;
        assert_eq!(
            &packet[plugin_at..plugin_at + AUTH_PLUGIN.len()],
            AUTH_PLUGIN.as_bytes()
        );
        assert_eq!(*packet.last().unwrap(), 0);
    }

    #[test]
    fn query_and_header_bytes() {
        let packet = query_packet("SELECT 1");
        assert_eq!(
            packet,
            [0x03, b'S', b'E', b'L', b'E', b'C', b'T', b' ', b'1']
        );
        assert_eq!(frame_header(300, 2), [0x2c, 0x01, 0x00, 0x02]);
        assert_eq!(frame_header(MAX_PAYLOAD + 5, 0)[..3], [0xff, 0xff, 0xff]);
    }
}
