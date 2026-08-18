//! `SET key value [options...]` argument parsing (Redis 7 semantics),
//! split out of `string.rs` to keep that handler file small.
//!
//! Pure functions only: [`parse`] folds the option argv into a
//! [`SetOptions`] value; [`resolve_ttl`] later turns the TTL spec into an
//! absolute millisecond deadline so parsing itself stays free of clock
//! reads and side effects.

/// TTL option as written on the wire, resolved against "now" later.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TtlSpec {
    /// `EX seconds` / `PX milliseconds`: relative to the current time.
    RelativeMs(u64),
    /// `EXAT seconds` / `PXAT milliseconds`: absolute unix deadline (ms).
    /// A deadline in the past still writes the record -- it is simply
    /// due immediately, matching Redis.
    AbsoluteMs(u64),
}

/// Parsed `SET` options; the default value is plain `SET key value`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SetOptions {
    pub ttl: Option<TtlSpec>,
    pub nx: bool,
    pub xx: bool,
    pub keepttl: bool,
    pub get: bool,
}

/// Parse failure kinds; [`error_text`] maps them to Redis error strings.
#[derive(Debug, PartialEq, Eq)]
pub enum SetSyntaxError {
    /// NX with XX, KEEPTTL with any TTL option, a repeated GET or TTL
    /// option, an unknown option, or a TTL option missing its value.
    Syntax,
    /// A TTL option argument that is not an integer.
    NotInteger,
    /// A TTL option argument that is <= 0, or seconds that overflow.
    InvalidExpire,
}

/// The Redis error text for one parse failure.
pub fn error_text(err: &SetSyntaxError) -> &'static str {
    match err {
        SetSyntaxError::Syntax => "ERR syntax error",
        SetSyntaxError::NotInteger => "ERR value is not an integer or out of range",
        SetSyntaxError::InvalidExpire => "ERR invalid expire time in 'set' command",
    }
}

/// Absolute expiry in ms for a spec; `None` = no TTL option was given.
pub fn resolve_ttl(spec: Option<TtlSpec>, now: u64) -> Option<u64> {
    match spec {
        None => None,
        Some(TtlSpec::RelativeMs(ms)) => Some(now.saturating_add(ms)),
        Some(TtlSpec::AbsoluteMs(at)) => Some(at),
    }
}

fn parse_int(raw: &[u8]) -> Option<i64> {
    std::str::from_utf8(raw).ok()?.parse().ok()
}

/// Parse everything after `key value`. Options are case-insensitive and
/// order-free; conflicting or duplicated combinations are errors.
pub fn parse(args: &[Vec<u8>]) -> Result<SetOptions, SetSyntaxError> {
    let mut opts = SetOptions::default();
    let mut get_seen = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_ascii_uppercase();
        match arg.as_slice() {
            b"NX" => opts.nx = true,
            b"XX" => opts.xx = true,
            b"KEEPTTL" => opts.keepttl = true,
            b"GET" => {
                if get_seen {
                    return Err(SetSyntaxError::Syntax);
                }
                get_seen = true;
                opts.get = true;
            }
            b"EX" | b"PX" | b"EXAT" | b"PXAT" => {
                if opts.ttl.is_some() {
                    return Err(SetSyntaxError::Syntax);
                }
                // The option consumes a value argument too.
                let Some(raw) = args.get(i + 1) else {
                    return Err(SetSyntaxError::Syntax);
                };
                i += 1;
                let seconds = matches!(arg.as_slice(), b"EX" | b"EXAT");
                let n = parse_int(raw).ok_or(SetSyntaxError::NotInteger)?;
                if n <= 0 || (seconds && n > i64::MAX / 1000) {
                    return Err(SetSyntaxError::InvalidExpire);
                }
                let ms =
                    u64::try_from(n * if seconds { 1_000 } else { 1 }).expect("checked positive");
                opts.ttl = Some(if matches!(arg.as_slice(), b"EX" | b"PX") {
                    TtlSpec::RelativeMs(ms)
                } else {
                    TtlSpec::AbsoluteMs(ms)
                });
            }
            _ => return Err(SetSyntaxError::Syntax),
        }
        i += 1;
    }
    if (opts.nx && opts.xx) || (opts.keepttl && opts.ttl.is_some()) {
        return Err(SetSyntaxError::Syntax);
    }
    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&[u8]]) -> Result<SetOptions, SetSyntaxError> {
        parse(&args.iter().map(|a| a.to_vec()).collect::<Vec<_>>())
    }

    #[test]
    fn plain_and_single_options_parse() {
        assert_eq!(parse_args(&[]), Ok(SetOptions::default()));
        assert_eq!(
            parse_args(&[b"nx"]),
            Ok(SetOptions {
                nx: true,
                ..SetOptions::default()
            })
        );
        assert_eq!(
            parse_args(&[b"xX", b"GeT"]),
            Ok(SetOptions {
                xx: true,
                get: true,
                ..SetOptions::default()
            })
        );
        assert_eq!(
            parse_args(&[b"keepttl"]),
            Ok(SetOptions {
                keepttl: true,
                ..SetOptions::default()
            })
        );
        // EX seconds -> relative ms; PX stays in ms.
        assert_eq!(
            parse_args(&[b"EX", b"10"]),
            Ok(SetOptions {
                ttl: Some(TtlSpec::RelativeMs(10_000)),
                ..SetOptions::default()
            })
        );
        assert_eq!(
            parse_args(&[b"PX", b"7"]),
            Ok(SetOptions {
                ttl: Some(TtlSpec::RelativeMs(7)),
                ..SetOptions::default()
            })
        );
        // EXAT seconds / PXAT ms -> absolute ms.
        assert_eq!(
            parse_args(&[b"EXAT", b"5"]),
            Ok(SetOptions {
                ttl: Some(TtlSpec::AbsoluteMs(5_000)),
                ..SetOptions::default()
            })
        );
        assert_eq!(
            parse_args(&[b"PXAT", b"9"]),
            Ok(SetOptions {
                ttl: Some(TtlSpec::AbsoluteMs(9)),
                ..SetOptions::default()
            })
        );
    }

    #[test]
    fn conflicts_and_duplicates_are_syntax_errors() {
        for args in [
            &[b"NX" as &[u8], b"XX"][..],
            &[b"XX" as &[u8], b"NX"][..],
            &[b"KEEPTTL" as &[u8], b"EX", b"10"][..],
            &[b"EX" as &[u8], b"10", b"KEEPTTL"][..],
            &[b"PX" as &[u8], b"5", b"KEEPTTL"][..],
            &[b"KEEPTTL" as &[u8], b"PXAT", b"99"][..],
            &[b"EX" as &[u8], b"10", b"PX", b"20"][..],
            &[b"GET" as &[u8], b"GET"][..],
            &[b"BOGUS" as &[u8]][..],
            &[b"EX" as &[u8]][..],
        ] {
            assert_eq!(parse_args(args), Err(SetSyntaxError::Syntax), "{args:?}");
        }
    }

    #[test]
    fn bad_expire_values_error() {
        for args in [
            &[b"EX" as &[u8], b"0"][..],
            &[b"EX" as &[u8], b"-5"][..],
            &[b"PX" as &[u8], b"0"][..],
            &[b"PXAT" as &[u8], b"-1"][..],
            &[b"EXAT" as &[u8], b"0"][..],
        ] {
            assert_eq!(
                parse_args(args),
                Err(SetSyntaxError::InvalidExpire),
                "{args:?}"
            );
        }
        assert_eq!(
            parse_args(&[b"EX", b"abc"]),
            Err(SetSyntaxError::NotInteger)
        );
        assert_eq!(
            parse_args(&[b"PXAT", b"1.5"]),
            Err(SetSyntaxError::NotInteger)
        );
    }

    #[test]
    fn resolve_ttl_maths() {
        assert_eq!(resolve_ttl(None, 1_000), None);
        assert_eq!(
            resolve_ttl(Some(TtlSpec::RelativeMs(500)), 1_000),
            Some(1_500)
        );
        // Past absolute deadlines pass through untouched (write + due).
        assert_eq!(resolve_ttl(Some(TtlSpec::AbsoluteMs(1)), 1_000), Some(1));
        assert_eq!(
            resolve_ttl(Some(TtlSpec::RelativeMs(u64::MAX)), 10),
            Some(u64::MAX) // saturating, never wraps
        );
    }
}
