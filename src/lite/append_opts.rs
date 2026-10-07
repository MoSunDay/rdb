//! XADD/XTRIM option parsing and trim-victim computation, moved out of
//! [`super::append`] so the command bodies stay about the write itself:
//!
//! - XTRIM: `MAXLEN [<~|=>] <n>` | `MINID [<~|=>] <id> [LIMIT <n>]`
//! - XADD: `NOMKSTREAM`, the SAME trim options (applied to the
//!   post-append entry set) and the leading `DELAY <ms>` staging option.
//!
//! The `~` (approximate) and `=` (exact) flags are accepted for wire
//! compatibility but implemented IDENTICALLY: victims are computed
//! precisely, so the approximation never under-deletes. Likewise Redis
//! reserves `LIMIT` for the `~` form only; ours accepts it after both
//! forms with the same semantics -- one less error branch, no
//! behavioral difference.

use crate::store::{ops, Store};

use super::entries::id_from_key;
use super::model::{self, EntryId};

const BAD_TRIM: &str = "ERR wrong number of arguments for 'xtrim' command";
const NOT_INT: &str = "ERR value is not an integer or out of range";
const BAD_ID: &str = "ERR Invalid stream ID specified as stream command argument";

/// One trim request parsed off the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrimPlan {
    /// Keep at most `n` entries (drop the oldest beyond that).
    MaxLen(u64),
    /// Drop every entry with id strictly below `id`, at most `limit`
    /// per call (the remainder is a later call's work).
    MinId { id: EntryId, limit: Option<u64> },
}

/// XADD's option block.
#[derive(Default)]
pub(crate) struct XaddOpts {
    /// Stream missing -> do NOT create the key; reply a null bulk.
    pub nomkstream: bool,
    /// `DELAY <ms>`: stage the message for the due-time exchange
    /// (0 = plain append; see [`super::delay`]).
    pub delay_ms: u64,
    /// Trim applied after the append lands (None = no trim).
    pub trim: Option<TrimPlan>,
}

/// The parsed front matter of one XADD: options, id token (`b"*"` =
/// auto-generated) and the field-value pair tail.
pub(crate) struct XaddHead<'a> {
    pub opts: XaddOpts,
    pub id_arg: &'a [u8],
    pub pairs: &'a [Vec<u8>],
}

fn int_of(a: &[u8]) -> Option<u64> {
    std::str::from_utf8(a).ok()?.parse().ok()
}

fn is_flag(a: &[u8]) -> bool {
    a == b"~" || a == b"="
}

fn is_limit(a: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b"LIMIT")
}

/// `DELAY`'s value rule (moved from `delay::split_delay`): digits only,
/// u64 range -- signs, floats and garbage all refuse alike.
fn delay_ms_of(a: &[u8]) -> Option<u64> {
    if a.is_empty() || !a.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(a).ok()?.parse().ok()
}

/// Parse one trim option starting at `t[i]` (a MAXLEN/MINID keyword):
/// the plan plus the index of the first unconsumed token. Consumption
/// is incremental (an XADD tail legitimately continues after the
/// option); the XTRIM wrapper enforces full-tail consumption instead.
/// Error texts are the XTRIM family's historical strings.
fn trim_at(t: &[Vec<u8>], i: usize) -> Result<(TrimPlan, usize), &'static str> {
    if t[i].eq_ignore_ascii_case(b"MAXLEN") {
        let (val, next) = match t.get(i + 1) {
            None => return Err(BAD_TRIM),
            // A flag must be followed by its value; a bare flag falls
            // through as the (non-numeric) value -> NOT_INT, the
            // historical verdict for `MAXLEN ~`.
            Some(m) if is_flag(m) => match t.get(i + 2) {
                Some(n) => (n, i + 3),
                None => (m, i + 2),
            },
            Some(n) => (n, i + 2),
        };
        return match int_of(val) {
            Some(n) => Ok((TrimPlan::MaxLen(n), next)),
            None => Err(NOT_INT),
        };
    }
    if !t[i].eq_ignore_ascii_case(b"MINID") {
        return Err(BAD_TRIM); // callers only invoke on a trim keyword
    }
    let mut j = i + 1;
    if t.get(j).is_some_and(|a| is_flag(a)) {
        j += 1;
    }
    let Some(id_arg) = t.get(j) else {
        return Err(BAD_TRIM);
    };
    let id = model::parse_id(id_arg).ok_or(BAD_ID)?;
    j += 1;
    let mut limit = None;
    if t.get(j).is_some_and(|a| is_limit(a)) {
        match t.get(j + 1).and_then(|v| int_of(v)) {
            Some(n) => limit = Some(n),
            None => {
                return Err(match t.get(j + 1) {
                    None => BAD_TRIM,
                    Some(_) => NOT_INT,
                })
            }
        }
        j += 2;
    }
    Ok((TrimPlan::MinId { id, limit }, j))
}

/// Parse `XTRIM <stream> <trim option...>` (the full command argv):
/// exactly one trim option consuming the whole tail.
pub(crate) fn parse_trim(argv: &[Vec<u8>]) -> Result<TrimPlan, &'static str> {
    let starts_trim = argv
        .get(1)
        .is_some_and(|a| a.eq_ignore_ascii_case(b"MAXLEN") || a.eq_ignore_ascii_case(b"MINID"));
    if !starts_trim {
        return Err(BAD_TRIM);
    }
    match trim_at(argv, 1) {
        Ok((plan, end)) if end == argv.len() => Ok(plan),
        _ => Err(BAD_TRIM),
    }
}

/// Scan leading XADD options from `t[i]` (NOMKSTREAM / trim, plus DELAY
/// when `allow_delay` -- DELAY is historically a POST-id option and is
/// excluded from the pre-id pass so the id-elision parity of the plain
/// dialect keeps its meaning); options are repeatable -- a repeated
/// option overwrites the earlier one, mirroring Redis's "options
/// precede the id/pairs" ambiguity resolution. Returns the index of
/// the first unconsumed token.
fn scan_opts(
    t: &[Vec<u8>],
    i: usize,
    o: &mut XaddOpts,
    allow_delay: bool,
) -> Result<usize, &'static str> {
    let mut i = i;
    while let Some(a) = t.get(i) {
        if a.eq_ignore_ascii_case(b"NOMKSTREAM") {
            o.nomkstream = true;
            i += 1;
        } else if allow_delay && a.eq_ignore_ascii_case(b"DELAY") {
            let ms = t.get(i + 1).and_then(|v| delay_ms_of(v)).ok_or(NOT_INT)?;
            o.delay_ms = ms;
            i += 2;
        } else if a.eq_ignore_ascii_case(b"MAXLEN") || a.eq_ignore_ascii_case(b"MINID") {
            let (plan, next) = trim_at(t, i)?;
            o.trim = Some(plan);
            i = next;
        } else {
            break;
        }
    }
    Ok(i)
}

/// Parse the XADD argument tail after the stream name. The optional id
/// keeps its historical parity rule (`name f v` vs `name id f v`,
/// DELAY included in the count as before) for the plain dialect --
/// option tokens must not skew that parity, so once a LEADING option
/// block (NOMKSTREAM / trim) was consumed the id becomes REQUIRED next
/// (Redis's own grammar: clients spell the auto id `*` out loud). A
/// trailing option block follows the id (DELAY keeps its historical
/// after-the-id slot; NOMKSTREAM/trim are accepted on either side),
/// then the field-value pairs, which must be a non-empty even list.
pub(crate) fn parse_xadd<'a>(args: &'a [Vec<u8>]) -> Result<XaddHead<'a>, &'static str> {
    const BAD: &str = "ERR wrong number of arguments for 'xadd' command";
    let t = &args[1..];
    let mut opts = XaddOpts::default();
    let mut i = scan_opts(t, 0, &mut opts, false)?;
    let (id_arg, has_id): (&[u8], bool) = if i > 0 {
        match t.get(i) {
            Some(id) => (id.as_slice(), true),
            None => return Err(BAD),
        }
    } else if t.len() % 2 == 1 {
        (t[0].as_slice(), true)
    } else {
        (b"*", false)
    };
    if has_id {
        i += 1;
    }
    i = scan_opts(t, i, &mut opts, true)?;
    let pairs = &t[i..];
    if pairs.len() < 2 || !pairs.len().is_multiple_of(2) {
        return Err(BAD);
    }
    Ok(XaddHead {
        opts,
        id_arg,
        pairs,
    })
}

/// Physical entry keys `plan` removes, given the post-append live
/// length `live_len` (the caller's next.len, the new row included when
/// it is a real entry). `new_entry` names the in-batch appended row
/// (None for a DELAYed staging): the store scan cannot see it, so a
/// plan that reaches the newest entry (MAXLEN 0, or a MINID above the
/// new id) deletes it too. LIMIT stops the batch BEFORE a victim is
/// taken, so LIMIT 0 trims NOTHING.
pub(crate) fn trim_victims(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    plan: TrimPlan,
    live_len: u64,
    new_entry: Option<EntryId>,
) -> Vec<Vec<u8>> {
    let base = model::entry_base(prefix, stream);
    match plan {
        TrimPlan::MaxLen(maxlen) => {
            let trim = live_len.saturating_sub(maxlen) as usize;
            if trim == 0 {
                return Vec::new();
            }
            // Cap the preallocation (trim is user-controlled u64) and
            // never walk past this stream's key range: a corrupted or
            // over-counted meta.len must not delete neighbouring keys.
            let mut victims = Vec::with_capacity(trim.min(4096));
            let _ = ops::for_each_from(store, &base, false, &mut |k, _| {
                if !k.starts_with(&base) {
                    return false;
                }
                victims.push(k.to_vec());
                victims.len() < trim
            });
            if let Some(id) = new_entry {
                if victims.len() < trim {
                    victims.push(model::entry_key(prefix, stream, id));
                }
            }
            victims
        }
        TrimPlan::MinId { id: minid, limit } => {
            // Entry keys are laid out in id order, so the walk ends at
            // the FIRST id >= minid -- everything before it is a victim
            // by construction (and LIMIT simply stops the batch early,
            // leaving the rest for a later call).
            let mut victims = Vec::new();
            let _ = ops::for_each_from(store, &base, false, &mut |k, _| {
                if !k.starts_with(&base) {
                    return false;
                }
                if limit.is_some_and(|n| victims.len() as u64 >= n) {
                    return false;
                }
                match id_from_key(&base, k) {
                    Some(id) if id < minid => victims.push(k.to_vec()),
                    Some(_) => return false, // reached the keep boundary
                    None => {}               // foreign suffix inside the window: skip
                }
                true
            });
            if let Some(id) = new_entry {
                let under_limit = limit.is_none_or(|n| (victims.len() as u64) < n);
                if id < minid && under_limit {
                    victims.push(model::entry_key(prefix, stream, id));
                }
            }
            victims
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(list: &[&str]) -> Vec<Vec<u8>> {
        list.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn xadd_parses_options_on_either_side_of_the_id() {
        // Options before an explicit auto id (`*`): with a leading
        // option block the id is mandatory, like Redis's own grammar.
        let argv = toks(&["s", "NOMKSTREAM", "*", "f", "v"]);
        let head = parse_xadd(&argv).unwrap();
        assert!(head.opts.nomkstream);
        assert_eq!(head.id_arg, b"*");
        assert_eq!(head.pairs, [b"f".to_vec(), b"v".to_vec()]);
        // Trim + explicit id + DELAY after the id (the historical slot).
        let argv = toks(&[
            "s", "MINID", "=", "3-1", "LIMIT", "2", "1-9", "DELAY", "5", "f", "v",
        ]);
        let head = parse_xadd(&argv).unwrap();
        assert_eq!(
            head.opts.trim,
            Some(TrimPlan::MinId {
                id: EntryId { ms: 3, seq: 1 },
                limit: Some(2)
            })
        );
        assert_eq!(head.id_arg, b"1-9");
        assert_eq!(head.opts.delay_ms, 5);
        assert_eq!(head.pairs.len(), 2);
        // DELAY after the id keeps the plain-pairs path (historical
        // slot: the auto id is still elided by parity).
        let argv = toks(&["s", "1-1", "DELAY", "250", "f", "v"]);
        let head = parse_xadd(&argv).unwrap();
        assert_eq!(head.opts.delay_ms, 250);
        assert_eq!(head.id_arg, b"1-1");
        assert_eq!(head.pairs.len(), 2);
        // ... and alone in front of an elided auto id likewise.
        let argv = toks(&["s", "DELAY", "250", "f", "v"]);
        let head = parse_xadd(&argv).unwrap();
        assert_eq!(head.opts.delay_ms, 250);
        assert_eq!(head.id_arg, b"*");
        // The historical id-elision parity still works without options.
        let argv = toks(&["s", "1-1", "f", "v"]);
        let head = parse_xadd(&argv).unwrap();
        assert_eq!(head.id_arg, b"1-1");
        assert_eq!(head.pairs.len(), 2);
        // A leading option block makes the id mandatory (Redis grammar).
        let argv = toks(&["s", "NOMKSTREAM", "*", "f", "v"]);
        let head = parse_xadd(&argv).unwrap();
        assert!(head.opts.nomkstream);
        assert_eq!(head.id_arg, b"*");
        for bad in [
            vec!["s", "NOMKSTREAM", "f", "v"],
            vec!["s", "NOMKSTREAM"],
            vec!["s", "MAXLEN", "5", "f", "v"],
        ] {
            let refs: Vec<&str> = bad.clone();
            assert!(parse_xadd(&toks(&refs)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn xadd_rejects_malformed_options() {
        for bad in [
            vec!["s", "DELAY", "x", "f", "v"],
            vec!["s", "DELAY", "-1", "f", "v"],
            vec!["s", "f"],
            vec!["s", "NOMKSTREAM", "*", "f"],
        ] {
            let refs: Vec<&str> = bad.clone();
            assert!(parse_xadd(&toks(&refs)).is_err(), "{bad:?}");
        }
        let argv = toks(&["s", "DELAY", "x", "f", "v"]);
        match parse_xadd(&argv) {
            Err(e) => assert_eq!(e, NOT_INT),
            Ok(_) => panic!("malformed DELAY must refuse"),
        }
    }

    #[test]
    fn trim_parse_preserves_xtrim_shapes() {
        assert_eq!(
            parse_trim(&toks(&["s", "MAXLEN", "5"])),
            Ok(TrimPlan::MaxLen(5))
        );
        assert_eq!(
            parse_trim(&toks(&["s", "MAXLEN", "~", "5"])),
            Ok(TrimPlan::MaxLen(5))
        );
        assert_eq!(
            parse_trim(&toks(&["s", "MINID", "3-1", "LIMIT", "2"])),
            Ok(TrimPlan::MinId {
                id: EntryId { ms: 3, seq: 1 },
                limit: Some(2)
            })
        );
        for bad in [
            vec!["s"],
            vec!["s", "FROB"],
            vec!["s", "MAXLEN"],
            vec!["s", "MAXLEN", "x"],
            vec!["s", "MAXLEN", "~", "5", "LIMIT", "2"],
            vec!["s", "MINID"],
            vec!["s", "MINID", "x"],
            vec!["s", "MINID", "3-1", "LIMIT", "x"],
            vec!["s", "MINID", "~", "3-1", "LIMIT", "2", "extra"],
        ] {
            let refs: Vec<&str> = bad.clone();
            assert!(parse_trim(&toks(&refs)).is_err(), "{bad:?}");
        }
    }
}
