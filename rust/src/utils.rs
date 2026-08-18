//! Small shared helpers (Rust mirror of Go `internal/utils/tools.go`).

use md5::{Digest, Md5};

/// Go `utils.MD5With40`: `result := hex(md5(s)); return result + result[24:]`.
///
/// 32 lowercase hex chars followed by the same digest's last 8 hex chars,
/// yielding a 40-char identifier.
pub fn md5_with40(s: &str) -> String {
    let digest = Md5::digest(s.as_bytes());
    let result = hex::encode(digest);
    let mut out = String::with_capacity(40);
    out.push_str(&result);
    out.push_str(&result[24..]);
    out
}

/// Go `utils.Exists`: true when the path exists (any fs entry).
pub fn exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok()
}

/// Go `bytes.Join(parts, nil)`-style concatenation of byte slices.
pub fn bytes_combine(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// Redis-style glob for `SCAN ... MATCH` / `KEYS`: `*`, `?`, `[...]`
/// classes (with `^` negation and `a-z` ranges) and `\` escaping the next
/// byte. An unterminated `[` is a literal `[` (Redis quirk). Backtracking
/// like Redis's own matcher; pathological `*` patterns can be slow.
pub fn glob_match(pattern: &[u8], s: &[u8]) -> bool {
    match pattern.first() {
        None => s.is_empty(),
        Some(b'*') => {
            glob_match(&pattern[1..], s) || (!s.is_empty() && glob_match(pattern, &s[1..]))
        }
        Some(b'?') => !s.is_empty() && glob_match(&pattern[1..], &s[1..]),
        Some(b'[') => match match_class(pattern, s) {
            // A class consumes one byte: never a hit for empty `s` (a
            // negated class "matches" empty in match_class, so slicing
            // `&s[1..]` below would panic; Redis also bails out early).
            Some((hit, rest)) => hit && !s.is_empty() && glob_match(rest, &s[1..]),
            None => s.first() == Some(&b'[') && glob_match(&pattern[1..], &s[1..]),
        },
        Some(b'\\') => match (pattern.get(1), s.first()) {
            (Some(&c), Some(&d)) => c == d && glob_match(&pattern[2..], &s[1..]),
            _ => false,
        },
        Some(&c) => s.first() == Some(&c) && glob_match(&pattern[1..], &s[1..]),
    }
}

/// One `[...]` class against `s[0]`; `Some((hit, rest-after-']'))`, or
/// `None` when the class never closes.
fn match_class<'a>(pattern: &'a [u8], s: &[u8]) -> Option<(bool, &'a [u8])> {
    let close = pattern.iter().skip(1).position(|&b| b == b']')? + 1;
    let mut body = &pattern[1..close];
    let negate = body.first() == Some(&b'^');
    if negate {
        body = &body[1..];
    }
    let hit = match s.first() {
        None => false,
        Some(&ch) => {
            let mut found = false;
            let mut i = 0;
            while i < body.len() {
                if body[i] == b'-' && i > 0 && i + 1 < body.len() {
                    if body[i - 1] <= ch && ch <= body[i + 1] {
                        found = true;
                    }
                    i += 2;
                } else {
                    if body[i] == ch {
                        found = true;
                    }
                    i += 1;
                }
            }
            found
        }
    };
    Some((hit != negate, &pattern[close + 1..]))
}

/// Cheap randomness without a rand dependency: splitmix64 over a global
/// counter, the clock and the pid. The counter alone guarantees distinct
/// consecutive draws; the clock/pid spread draws across runs and
/// processes (RANDOMKEY only needs approximate uniformity).
pub fn rand_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let draw = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = u64::from(std::process::id());
    // splitmix64 finalizer.
    let mut z = nanos ^ draw.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ pid.rotate_left(32);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures computed with:
    //   python3 -c 'import hashlib; h=hashlib.md5(b"...").hexdigest(); print(h+h[24:])'
    #[test]
    fn md5_with40_addr_fixture() {
        let got = md5_with40("127.0.0.1:32681");
        assert_eq!(got, "844806f0817b51006e8b41d51e1e67621e1e6762");
        assert_eq!(got.len(), 40);
        // Tail must equal the digest's own last 8 hex chars (positions 24..32).
        assert_eq!(&got[32..], &got[24..32]);
    }

    #[test]
    fn md5_with40_empty_fixture() {
        let got = md5_with40("");
        assert_eq!(got, "d41d8cd98f00b204e9800998ecf8427eecf8427e");
        assert_eq!(got.len(), 40);
        assert_eq!(&got[32..], &got[24..32]);
    }

    #[test]
    fn md5_with40_is_lowercase_hex() {
        let got = md5_with40("hello:world");
        assert_eq!(got.len(), 40);
        assert!(got
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(&got[32..], &got[24..32]);
    }

    #[test]
    fn exists_on_dir_file_and_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(exists(dir.path().to_str().unwrap()));

        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"x").expect("write");
        assert!(exists(file.to_str().unwrap()));

        assert!(!exists("/definitely/not/here/xyz-12345"));
        assert!(!exists(dir.path().join("gone").to_str().unwrap()));
    }

    #[test]
    fn bytes_combine_concatenates() {
        assert_eq!(bytes_combine(&[b"a", b"bc", b"def"]), b"abcdef".to_vec());
        assert_eq!(bytes_combine(&[]), Vec::<u8>::new());
        assert_eq!(bytes_combine(&[b"", b"", b""]), Vec::<u8>::new());
        assert_eq!(bytes_combine(&[b"only"]), b"only".to_vec());
        let head = b"12/".as_slice();
        let body = b"val".as_slice();
        assert_eq!(bytes_combine(&[head, body]), b"12/val".to_vec());
    }

    #[test]
    fn glob_match_star_question_and_literals() {
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"*", b""));
        assert!(glob_match(b"h?llo", b"hello"));
        assert!(!glob_match(b"h?llo", b"hllo"));
        assert!(glob_match(b"h*llo", b"heeeello"));
        assert!(glob_match(b"h*llo", b"hllo"));
        assert!(!glob_match(b"h*llo", b"heeeellx"));
        assert!(glob_match(b"abc", b"abc"));
        assert!(!glob_match(b"abc", b"abd"));
        assert!(!glob_match(b"abc", b"abcd"));
        assert!(!glob_match(b"", b"x"));
        assert!(glob_match(b"", b""));
    }

    #[test]
    fn glob_match_classes_ranges_and_negation() {
        assert!(glob_match(b"h[ae]llo", b"hello"));
        assert!(glob_match(b"h[ae]llo", b"hallo"));
        assert!(!glob_match(b"h[ae]llo", b"hillo"));
        assert!(glob_match(b"h[a-c]llo", b"hbllo"));
        assert!(!glob_match(b"h[a-c]llo", b"hdllo"));
        assert!(glob_match(b"h[^e]llo", b"hallo"));
        assert!(!glob_match(b"h[^e]llo", b"hello"));
        // Unterminated class degrades to a literal '['.
        assert!(glob_match(b"h[aello", b"h[aello"));
        assert!(!glob_match(b"h[aello", b"hallo"));
    }

    #[test]
    fn glob_match_class_consumes_one_byte() {
        // A class always consumes a byte, so empty s never matches — even
        // negated, where match_class reports a "hit" and the old
        // `&s[1..]` sliced out of range (SSCAN over an empty member with
        // MATCH [^x]* used to panic).
        assert!(!glob_match(b"[^x]*", b""));
        assert!(!glob_match(b"[a]*", b""));
        assert!(!glob_match(b"[^x]", b""));
        assert!(!glob_match(b"*[^x]*", b""));
        // One byte present: class semantics unchanged.
        assert!(glob_match(b"[^x]*", b"y"));
        assert!(!glob_match(b"[^x]*", b"x"));
        assert!(glob_match(b"[a]*", b"a"));
    }

    #[test]
    fn glob_match_escapes_and_multi_star() {
        assert!(glob_match(b"a\\*b", b"a*b"));
        assert!(!glob_match(b"a\\*b", b"axb"));
        assert!(glob_match(b"**z", b"abcz"));
        assert!(glob_match(b"*a*b*c*", b"xxaxxbxxc"));
        assert!(glob_match(b"a??", b"abc"));
        assert!(!glob_match(b"a??", b"ab"));
        // Binary bytes are just bytes.
        assert!(glob_match(&[0x01, b'*', 0xFF], &[0x01, 0x00, 0xFF]));
    }

    #[test]
    fn rand_u64_varies_and_is_non_deterministic() {
        let a = rand_u64();
        let b = rand_u64();
        let c = rand_u64();
        assert_ne!(a, b, "consecutive draws should differ");
        assert_ne!(b, c);
    }
}
