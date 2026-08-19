//! RESP2 codec: parse + write, byte-exact with the Go redcon fork
//! (`ReadNextCommand`/`readTelnetCommand`, Redis-kind only); pure functions.

/// Result of attempting to parse one command from a connection buffer.
pub enum ParseOutcome {
    /// Not enough bytes yet; read more data and retry.
    Incomplete,
    /// A full command; `consumed` bytes belong to it, rest = next command.
    Complete { args: Vec<Vec<u8>>, consumed: usize },
    /// Protocol error; `msg` mirrors redcon, `consumed` is 0 (Go server
    /// replies `-ERR Protocol error: <msg>` and closes the connection).
    ProtocolError { msg: String, consumed: usize },
}

fn protocol_error(msg: &str) -> ParseOutcome {
    ParseOutcome::ProtocolError {
        msg: msg.to_string(),
        consumed: 0,
    }
}

/// Upper bound on a `*N` multibulk header: the count drives the argument
/// Vec's growth and (pre-clamp) its eager preallocation, so a garbage or
/// huge header must error instead of accepting an absurd element count.
/// 1M elements is far beyond any real command.
const MAX_MULTIBULK_COUNT: i64 = 1_048_576;

/// Clamp on the eager initial argument allocation in `parse_multibulk`:
/// `Vec::with_capacity(count)` runs on the `*N` header BEFORE any payload
/// arrives, so an unclamped `*1048576\r\n` would allocate ~24MB per PARSE
/// ATTEMPT -- and parse_command re-runs on every Incomplete retry while
/// the client dribbles frames, repeatedly allocating/freeing it (amplified
/// DoS). The Vec grows naturally as complete bulk args arrive, so the
/// clamp only delays allocation; `MAX_MULTIBULK_COUNT` validation stays.
const ARGS_PREALLOC_CAP: usize = 16;

/// Upper bound on a single `$N` bulk payload, mirroring Redis
/// `proto-max-bulk-len` (512MiB). Checked on the header alone, before any
/// payload bytes are read. i64 because `parse_int` yields i64.
pub const MAX_BULK_LEN: i64 = 512 * 1024 * 1024;

/// Parse one command from `buf` (redcon `ReadNextCommand`, Redis-kind).
pub fn parse_command(buf: &[u8]) -> ParseOutcome {
    if buf.is_empty() {
        return ParseOutcome::Incomplete;
    }
    match buf[0] {
        b'*' => parse_multibulk(buf),
        // '$' routes to the Tile38 native protocol in the Go fork; unsupported.
        b'$' => protocol_error("invalid message"),
        _ => parse_telnet(buf),
    }
}

/// redcon `parseInt`: empty input -> 0/ok; ASCII digits with an optional
/// leading '-'; overflow wraps (Go int semantics).
fn parse_int(b: &[u8]) -> Option<i64> {
    if b.len() == 1 && b[0].is_ascii_digit() {
        return Some((b[0] - b'0') as i64);
    }
    let (mut n, mut sign, mut i) = (0i64, false, 0usize);
    if !b.is_empty() && b[0] == b'-' {
        sign = true;
        i = 1;
    }
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            return None;
        }
        n = n.wrapping_mul(10).wrapping_add((b[i] - b'0') as i64);
        i += 1;
    }
    if sign {
        n = n.wrapping_neg();
    }
    Some(n)
}

/// Standard `*N\r\n$len\r\n...` multibulk parsing.
fn parse_multibulk(buf: &[u8]) -> ParseOutcome {
    let mut i = 1usize; // scan the "*N\r\n" header for its terminating '\n'
    while i < buf.len() {
        if buf[i] == b'\n' {
            if buf[i - 1] != b'\r' {
                return protocol_error("invalid multibulk length");
            }
            let count = match parse_int(&buf[1..i - 1]) {
                // Negative or over-cap counts error before allocation.
                Some(n) if (0..=MAX_MULTIBULK_COUNT).contains(&n) => n as usize,
                _ => return protocol_error("invalid multibulk length"),
            };
            i += 1; // first byte past the header
            if count == 0 {
                return ParseOutcome::Complete {
                    args: Vec::new(),
                    consumed: i,
                };
            }
            // Preallocation is clamped (ARGS_PREALLOC_CAP): the header
            // count is untrusted until its payload arrives, so only a
            // small initial capacity is bought eagerly and the Vec grows
            // as complete bulk args land.
            let mut args: Vec<Vec<u8>> = Vec::with_capacity(count.min(ARGS_PREALLOC_CAP));
            while args.len() < count {
                if i == buf.len() {
                    return ParseOutcome::Incomplete;
                }
                if buf[i] != b'$' {
                    return protocol_error(&format!("expected '$', got '{}'", buf[i] as char));
                }
                // Scan the "$len\r\n" prefix of this bulk argument.
                let s = i + 1;
                let mut done = false;
                while i < buf.len() {
                    if buf[i] == b'\n' {
                        if buf[i - 1] != b'\r' {
                            return protocol_error("invalid bulk length");
                        }
                        // Go fork typo-checks `count <= 0` (never fires); the
                        // intent is a non-negative bulk length, enforced here.
                        // N > MAX_BULK_LEN errors on the header alone
                        // (Redis `proto-max-bulk-len` parity), BEFORE the
                        // Incomplete check below, so no payload is needed.
                        let n = match parse_int(&buf[s..i - 1]) {
                            Some(v) if (0..=MAX_BULK_LEN).contains(&v) => v as usize,
                            _ => return protocol_error("invalid bulk length"),
                        };
                        let start = i + 1;
                        if buf.len() - start < n + 2 {
                            return ParseOutcome::Incomplete;
                        }
                        if buf[start + n] != b'\r' || buf[start + n + 1] != b'\n' {
                            return protocol_error("invalid bulk length");
                        }
                        args.push(buf[start..start + n].to_vec());
                        i = start + n + 2;
                        done = true;
                        break;
                    }
                    i += 1;
                }
                if !done {
                    return ParseOutcome::Incomplete;
                }
            }
            return ParseOutcome::Complete { args, consumed: i };
        }
        i += 1;
    }
    ParseOutcome::Incomplete
}

/// Plain-text / telnet command parsing (redcon `readTelnetCommand`).
/// A quote char is honored only at the start of a token; inside quotes a
/// backslash escapes the next byte (redcon translates \n \r \t to the
/// control byte and otherwise drops the backslash, keeping the byte).
fn parse_telnet(buf: &[u8]) -> ParseOutcome {
    let nl = match buf.iter().position(|&b| b == b'\n') {
        Some(p) => p,
        None => return ParseOutcome::Incomplete,
    };
    let mut line: &[u8] = if nl > 0 && buf[nl - 1] == b'\r' {
        &buf[..nl - 1]
    } else {
        &buf[..nl]
    };
    let (mut args, mut quote, mut quotech, mut escape): (Vec<Vec<u8>>, bool, u8, bool) =
        (Vec::new(), false, 0, false);
    // Mirrors the Go `outer:` loop: each pass scans the remaining line
    // until a space or quote boundary restarts it with a shorter line.
    loop {
        let mut nline: Vec<u8> = Vec::with_capacity(line.len());
        let (mut i, mut restart) = (0usize, false);
        while i < line.len() {
            let mut c = line[i];
            if !quote {
                if c == b' ' {
                    if !nline.is_empty() {
                        args.push(std::mem::take(&mut nline));
                    }
                    line = &line[i + 1..];
                    restart = true;
                    break;
                }
                if c == b'"' || c == b'\'' {
                    if i != 0 {
                        return protocol_error("unbalanced quotes in request");
                    }
                    quotech = c;
                    quote = true;
                    line = &line[i + 1..];
                    restart = true;
                    break;
                }
            } else if escape {
                escape = false;
                c = match c {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    _ => c,
                };
            } else if c == quotech {
                quote = false;
                quotech = 0;
                args.push(std::mem::take(&mut nline));
                line = &line[i + 1..];
                if !line.is_empty() && line[0] != b' ' {
                    return protocol_error("unbalanced quotes in request");
                }
                restart = true;
                break;
            } else if c == b'\\' {
                escape = true;
                i += 1;
                continue;
            }
            nline.push(c);
            i += 1;
        }
        if restart {
            continue;
        }
        if quote {
            return protocol_error("unbalanced quotes in request");
        }
        if !line.is_empty() {
            args.push(line.to_vec()); // Go appends the remaining slice as-is
        }
        break;
    }
    ParseOutcome::Complete {
        args,
        consumed: nl + 1,
    }
}

// Writers: pure append helpers, byte-exact redcon format.

/// `+s\r\n` (simple string).
pub fn append_string(buf: &mut Vec<u8>, s: &str) {
    buf.push(b'+');
    buf.extend_from_slice(s.as_bytes());
    buf.extend_from_slice(b"\r\n");
}

/// `-msg\r\n` (error).
pub fn append_error(buf: &mut Vec<u8>, msg: &str) {
    buf.push(b'-');
    buf.extend_from_slice(msg.as_bytes());
    buf.extend_from_slice(b"\r\n");
}

/// `$-1\r\n` (null bulk string).
pub fn append_null(buf: &mut Vec<u8>) {
    buf.extend_from_slice(b"$-1\r\n");
}

/// Bulk string from a `&str`.
pub fn append_bulk_string(buf: &mut Vec<u8>, s: &str) {
    append_bulk(buf, s.as_bytes());
}

/// Raw bytes, unmodified.
pub fn append_raw(buf: &mut Vec<u8>, data: &[u8]) {
    buf.extend_from_slice(data);
}

/// `$len\r\ndata\r\n` (bulk string).
pub fn append_bulk(buf: &mut Vec<u8>, data: &[u8]) {
    buf.push(b'$');
    buf.extend_from_slice(data.len().to_string().as_bytes());
    buf.extend_from_slice(b"\r\n");
    buf.extend_from_slice(data);
    buf.extend_from_slice(b"\r\n");
}

/// `:n\r\n` (integer).
pub fn append_int(buf: &mut Vec<u8>, n: i64) {
    buf.push(b':');
    buf.extend_from_slice(n.to_string().as_bytes());
    buf.extend_from_slice(b"\r\n");
}

/// `*count\r\n` (array header).
pub fn append_array(buf: &mut Vec<u8>, count: usize) {
    buf.push(b'*');
    buf.extend_from_slice(count.to_string().as_bytes());
    buf.extend_from_slice(b"\r\n");
}

/// Minimal reply reader shared by command-layer unit tests: parses one
/// top-level frame (arrays and bulk strings; ints/errors read as raw
/// lines by the callers that need them) for exact assertions.
#[cfg(test)]
pub(crate) mod test_reader {
    /// One parsed frame.
    #[derive(Debug)]
    pub(crate) enum Frame {
        /// `$len\r\n<bytes>\r\n`.
        Bulk(Vec<u8>),
        /// `*n\r\n` plus `n` nested frames.
        Array(Vec<Frame>),
    }

    fn read_line<'a>(buf: &'a [u8], pos: &mut usize) -> &'a [u8] {
        let end = buf[*pos..]
            .iter()
            .position(|&b| b == b'\n')
            .expect("line ends")
            + *pos;
        let line = &buf[*pos..end - 1];
        *pos = end + 1;
        line
    }

    fn parse_frame(buf: &[u8], pos: &mut usize) -> Frame {
        let line = read_line(buf, pos);
        match line.first() {
            Some(b'*') => {
                let n: usize = std::str::from_utf8(&line[1..]).unwrap().parse().unwrap();
                Frame::Array((0..n).map(|_| parse_frame(buf, pos)).collect())
            }
            Some(b'$') => {
                let len: usize = std::str::from_utf8(&line[1..]).unwrap().parse().unwrap();
                let data = buf[*pos..*pos + len].to_vec();
                *pos += len + 2;
                Frame::Bulk(data)
            }
            _ => panic!("unexpected line {line:?}"),
        }
    }

    /// Parse a top-level array reply.
    pub(crate) fn parse(buf: &[u8]) -> Vec<Frame> {
        let mut pos = 0;
        match parse_frame(buf, &mut pos) {
            Frame::Array(items) => items,
            _ => panic!("top level must be an array"),
        }
    }

    /// Unwrap a bulk frame.
    pub(crate) fn bulk(f: &Frame) -> Vec<u8> {
        match f {
            Frame::Bulk(v) => v.clone(),
            _ => panic!("expected bulk"),
        }
    }
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod codec_tests;
