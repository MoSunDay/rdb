//! Test-side payload codecs (mirrors of the fronts' encoders).

/// Standard base64 decode (test-side twin of the fronts' `body`
/// encoder; no external crate -- scenario payloads are UTF-8).
pub fn b64d(s: &str) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let idx = |c: u8| A.iter().position(|&a| a == c).unwrap_or(0) as u32;
    let vals: Vec<u32> = s
        .bytes()
        .filter(|b| !matches!(b, b'\n' | b'\r' | b'='))
        .map(idx)
        .collect();
    let mut out = Vec::with_capacity(vals.len() / 4 * 3);
    for c in vals.chunks(4) {
        let n = (c[0] << 18)
            | (c.get(1).copied().unwrap_or(0) << 12)
            | (c.get(2).copied().unwrap_or(0) << 6)
            | c.get(3).copied().unwrap_or(0);
        let meaningful = c.len().saturating_sub(1); // 4 vals -> 3 bytes, 3 -> 2, 2 -> 1
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8][..meaningful]);
    }
    String::from_utf8_lossy(&out).into_owned()
}
