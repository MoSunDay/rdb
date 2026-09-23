//! Minimal RESP reply parser (front-internal). The RocksMQ HTTP front
//! drives Lite commands through `command::dispatch`, which appends
//! exactly ONE reply value to a local buffer; this module turns those
//! bytes back into a [`Value`] tree the route handlers can walk. It is
//! a consumer of the codec (`resp::codec` is append-only), never a
//! general RESP socket reader: "short reply" errors mean an internal
//! bug, not a network condition.

/// One parsed RESP value (`+`/`-`/`:`/`$`/`*`).
#[derive(Debug, PartialEq)]
pub enum Value {
    Simple(Vec<u8>),
    Error(Vec<u8>),
    Int(i64),
    /// `$-1` decodes to `None`.
    Bulk(Option<Vec<u8>>),
    /// `*-1` (nil array) decodes to `None`.
    Array(Option<Vec<Value>>),
}

impl Value {
    /// Error-line text (`-` prefix stripped), if this is an error.
    pub fn error_text(&self) -> Option<String> {
        match self {
            Value::Error(e) => Some(String::from_utf8_lossy(e).to_string()),
            _ => None,
        }
    }

    pub fn as_bulk(&self) -> Option<&[u8]> {
        match self {
            Value::Bulk(Some(b)) => Some(b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(Some(v)) => Some(v),
            _ => None,
        }
    }
}

/// Parse one complete reply (trailing bytes are an error: dispatch
/// writes exactly one value).
pub fn parse(buf: &[u8]) -> Result<Value, String> {
    let (v, used) = parse_at(buf, 0)?;
    if used != buf.len() {
        return Err(format!("{} trailing bytes after reply", buf.len() - used));
    }
    Ok(v)
}

/// Parse the value starting at `at`; returns it plus the offset just
/// past it.
fn parse_at(b: &[u8], at: usize) -> Result<(Value, usize), String> {
    let ty = *b.get(at).ok_or("empty reply")?;
    let line_end = b[at + 1..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|i| at + 1 + i)
        .ok_or("unterminated reply line")?;
    let line = &b[at + 1..line_end];
    let after = line_end + 2;
    match ty {
        b'+' => Ok((Value::Simple(line.to_vec()), after)),
        b'-' => Ok((Value::Error(line.to_vec()), after)),
        b':' => Ok((Value::Int(parse_int(line)?), after)),
        b'$' => {
            let n = parse_int(line)?;
            if n < 0 {
                return Ok((Value::Bulk(None), after));
            }
            let end = after
                .checked_add(n as usize)
                .ok_or("bulk length overflow")?;
            if b.len() < end + 2 || &b[end..end + 2] != b"\r\n" {
                return Err("short bulk reply".to_string());
            }
            Ok((Value::Bulk(Some(b[after..end].to_vec())), end + 2))
        }
        b'*' => {
            let n = parse_int(line)?;
            if n < 0 {
                return Ok((Value::Array(None), after));
            }
            let mut items = Vec::new();
            let mut i = after;
            for _ in 0..n {
                let (v, next) = parse_at(b, i)?;
                items.push(v);
                i = next;
            }
            Ok((Value::Array(Some(items)), i))
        }
        other => Err(format!("unknown reply type byte '{}'", other as char)),
    }
}

fn parse_int(line: &[u8]) -> Result<i64, String> {
    std::str::from_utf8(line)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| format!("malformed integer line '{}'", String::from_utf8_lossy(line)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars() {
        assert_eq!(parse(b"+OK\r\n").unwrap(), Value::Simple(b"OK".to_vec()));
        assert_eq!(
            parse(b"-NOGROUP x\r\n").unwrap(),
            Value::Error(b"NOGROUP x".to_vec())
        );
        assert_eq!(parse(b":42\r\n").unwrap(), Value::Int(42));
        assert_eq!(
            parse(b"$5\r\nhello\r\n").unwrap(),
            Value::Bulk(Some(b"hello".to_vec()))
        );
        assert_eq!(parse(b"$-1\r\n").unwrap(), Value::Bulk(None));
    }

    #[test]
    fn parses_nested_arrays() {
        // XREADGROUP shape: [[stream, [[id, [f,v]], ...]]]
        let raw = b"*1\r\n*2\r\n$6\r\nch1/q0\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nv\r\n$5\r\nhello\r\n";
        let v = parse(raw).unwrap();
        let outer = v.as_array().unwrap();
        let pair = outer[0].as_array().unwrap();
        assert_eq!(pair[0].as_bulk().unwrap(), b"ch1/q0");
        let entry = pair[1].as_array().unwrap()[0].as_array().unwrap();
        assert_eq!(entry[0].as_bulk().unwrap(), b"1-1");
        let fields = entry[1].as_array().unwrap();
        assert_eq!(fields[0].as_bulk().unwrap(), b"v");
        assert_eq!(fields[1].as_bulk().unwrap(), b"hello");
        assert_eq!(parse(b"*-1\r\n").unwrap(), Value::Array(None));
        assert_eq!(parse(b"*0\r\n").unwrap(), Value::Array(Some(vec![])));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"").is_err());
        assert!(parse(b"$5\r\nabc").is_err());
        assert!(parse(b":x\r\n").is_err());
        assert!(parse(b"+OK\r\n+OK\r\n").is_err()); // trailing bytes
    }
}
