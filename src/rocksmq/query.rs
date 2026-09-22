//! Query-string decoding for the RocksMQ HTTP front: ordered
//! key/value pairs with `application/x-www-form-urlencoded` decoding
//! (`+` = space, `%XX` = byte). Hand-rolled per the no-new-crates rule;
//! a malformed escape fails the whole query (the transport answers 400).

/// Decoded query string (duplicates kept; [`Query::get`] takes the
/// first).
pub struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    /// Parse the raw query text (between `?` and `#`).
    pub fn parse(raw: &str) -> Result<Query, String> {
        let mut pairs = Vec::new();
        for part in raw.split('&') {
            if part.is_empty() {
                continue;
            }
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (k, v),
                None => (part, ""),
            };
            pairs.push((decode_component(k)?, decode_component(v)?));
        }
        Ok(Query { pairs })
    }

    /// First value of `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// `+` -> space, `%XX` -> byte; a malformed escape is an error.
fn decode_component(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hi = hex_val(b.get(i + 1)).ok_or("malformed % escape")?;
                let lo = hex_val(b.get(i + 2)).ok_or("malformed % escape")?;
                out.push(hi << 4 | lo);
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "query is not valid UTF-8".to_string())
}

fn hex_val(b: Option<&u8>) -> Option<u8> {
    match b? {
        c @ b'0'..=b'9' => Some(c - b'0'),
        c @ b'a'..=b'f' => Some(c - b'a' + 10),
        c @ b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_pairs() {
        let q = Query::parse("channel=a%2Fb&group=g+1&empty=&n").unwrap();
        assert_eq!(q.get("channel"), Some("a/b"));
        assert_eq!(q.get("group"), Some("g 1"));
        assert_eq!(q.get("empty"), Some(""));
        assert_eq!(q.get("n"), Some(""));
        assert_eq!(q.get("missing"), None);
        assert_eq!(Query::parse("n=3&n=9").unwrap().get("n"), Some("3"));
    }

    #[test]
    fn rejects_malformed_escapes() {
        assert!(Query::parse("x=%zz").is_err());
        assert!(Query::parse("x=%2").is_err());
        // decoded bytes must still be UTF-8 (a lone 0xC3 is not)
        assert!(Query::parse("x=%C3").is_err());
        assert!(Query::parse("x=%C3%A9").unwrap().get("x") == Some("\u{e9}"));
    }
}
