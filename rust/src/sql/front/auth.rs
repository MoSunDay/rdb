//! mysql_native_password handshake verification.
//!
//! The client proves knowledge of the password without sending it:
//! `token = SHA1(password) XOR SHA1(salt ++ SHA1(SHA1(password)))`. The
//! server holds the plaintext password (config `mysql_password`) and simply
//! recomputes the same token, so verification is a pure function of
//! `(salt, password, auth_data)`.
//!
//! The connection scramble comes from [`salt_from_seed`]: opensrv's default
//! salt is one hard-coded constant shared by every server in the wild,
//! which would make a captured `(salt, token)` pair replayable forever.
//! A fresh per-connection salt pins each handshake to one connection. It
//! is splitmix64 output (no crypto RNG in the dependency tree), which is
//! fine for a scramble -- it only needs to be unguessable-in-advance to a
//! passive observer, not a secret.

use sha1::{Digest, Sha1};

/// Salt length of the mysql_native_password scramble (protocol constant).
pub const SCRAMBLE_LEN: usize = 20;

/// SHA1 of `x` as a fixed 20-byte array.
pub fn sha1_20(x: &[u8]) -> [u8; SCRAMBLE_LEN] {
    let mut out = [0u8; SCRAMBLE_LEN];
    out.copy_from_slice(&Sha1::digest(x));
    out
}

/// Verify a mysql_native_password `auth_data` token.
///
/// Empty password: the client sends an empty token, so the token matches
/// iff the configured password is also empty. The comparison folds over
/// all bytes before deciding (no early return), keeping the check's
/// timing independent of where the first difference sits.
pub fn native_password_matches(salt: &[u8], password: &str, auth_data: &[u8]) -> bool {
    if password.is_empty() {
        return auth_data.is_empty();
    }
    if salt.len() != SCRAMBLE_LEN || auth_data.len() != SCRAMBLE_LEN {
        return false;
    }
    let stage1 = sha1_20(password.as_bytes());
    let stage2 = sha1_20(&stage1);
    let mut seed = salt.to_vec();
    seed.extend_from_slice(&stage2);
    let expected = xor_20(stage1, sha1_20(&seed));
    ct_eq(&expected, auth_data)
}

/// XOR two 20-byte arrays.
fn xor_20(a: [u8; SCRAMBLE_LEN], b: [u8; SCRAMBLE_LEN]) -> [u8; SCRAMBLE_LEN] {
    let mut out = [0u8; SCRAMBLE_LEN];
    for i in 0..SCRAMBLE_LEN {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// Length-first, then bytewise fold: no branch exits on the first
/// mismatching byte.
fn ct_eq(a: &[u8; SCRAMBLE_LEN], b: &[u8]) -> bool {
    if b.len() != SCRAMBLE_LEN {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..SCRAMBLE_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Per-connection scramble from a caller-chosen seed (time + connection
/// counter at the call site). splitmix64 keeps successive connections'
/// salts uncorrelated; bytes are mapped into printable ASCII excluding
/// `\0` (C-string terminator) and `$`, matching the constraints of
/// opensrv's default salt.
pub fn salt_from_seed(seed: u64) -> [u8; SCRAMBLE_LEN] {
    let mut salt = [0u8; SCRAMBLE_LEN];
    let mut x = seed;
    for b in salt.iter_mut() {
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        // 93 printable codes in 0x21..=0x7f; fold out 0x00 and '$' (0x24).
        *b = 0x21 + (z % 93) as u8;
        if *b == b'$' {
            *b += 1;
        }
    }
    salt
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vector computed independently (python hashlib): password "pw123",
    /// salt = bytes 1..=20, token = SHA1(pw123) XOR SHA1(salt ++ SHA1(SHA1(pw123)))
    /// = 80f4280ca4d57fdf047311c37c2094ef76d5885f.
    #[test]
    fn native_password_known_vector() {
        let salt: Vec<u8> = (1..=20u8).collect();
        let token = hex::decode("80f4280ca4d57fdf047311c37c2094ef76d5885f").unwrap();
        assert!(native_password_matches(&salt, "pw123", &token));
    }

    /// Any single flipped token byte must fail (all 20 positions).
    #[test]
    fn native_password_rejects_flipped_bytes() {
        let salt: Vec<u8> = (1..=20u8).collect();
        let mut token = hex::decode("80f4280ca4d57fdf047311c37c2094ef76d5885f").unwrap();
        for i in 0..token.len() {
            token[i] ^= 1;
            assert!(!native_password_matches(&salt, "pw123", &token), "pos {i}");
            token[i] ^= 1;
        }
    }

    /// Wrong password, wrong salt and wrong lengths all fail.
    #[test]
    fn native_password_rejects_mismatches() {
        let salt: Vec<u8> = (1..=20u8).collect();
        let token = hex::decode("80f4280ca4d57fdf047311c37c2094ef76d5885f").unwrap();
        assert!(!native_password_matches(&salt, "pw124", &token));
        let other_salt = [0xaau8; 20];
        assert!(!native_password_matches(&other_salt, "pw123", &token));
        assert!(!native_password_matches(&salt[..19], "pw123", &token));
        assert!(!native_password_matches(&salt, "pw123", &token[..19]));
    }

    /// Empty password <-> empty token is the only accepted combination on
    /// that path; a real token against an empty configured password (or an
    /// empty token against a non-empty password) must fail.
    #[test]
    fn empty_password_matches_only_empty_token() {
        let salt: Vec<u8> = (1..=20u8).collect();
        assert!(native_password_matches(&salt, "", &[]));
        assert!(!native_password_matches(&salt, "pw123", &[]));
        let token = hex::decode("80f4280ca4d57fdf047311c37c2094ef76d5885f").unwrap();
        assert!(!native_password_matches(&salt, "", &token));
    }

    /// Salts stay in the safe printable range, never repeat a full 20-byte
    /// pattern for nearby seeds, and the generator is deterministic.
    #[test]
    fn salt_from_seed_range_and_uniqueness() {
        let a = salt_from_seed(1);
        let b = salt_from_seed(2);
        assert_eq!(a, salt_from_seed(1));
        assert_ne!(a, b);
        for s in [a, b, salt_from_seed(u64::MAX)] {
            assert_eq!(s.len(), SCRAMBLE_LEN);
            assert!(s.iter().all(|&c| (0x21..=0x7f).contains(&c) && c != b'$'));
        }
    }

    #[test]
    fn sha1_digest_matches_known_input() {
        assert_eq!(
            hex::encode(sha1_20(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }
}
