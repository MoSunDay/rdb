//! `POST /range` for the RocksMQ HTTP front (P3 #12): read-only replay
//! of a channel's retained entries between two id bounds -- the
//! inspection/debug twin of `/consume`, answering "what is (or was) in
//! this channel" WITHOUT any delivery semantics.
//!
//! Read-only guarantee by construction: the handler dispatches plain
//! `XRANGE` (`src/lite/append.rs::xrange` -- registered read-only in
//! `src/command/readonly.rs`), the SAME Lite read path the RESP face
//! exposes. XRANGE scans entry keys (`model::entry_base` prefix via
//! `ops::for_each_from`) and never touches the PEL, group state or
//! redelivery bookkeeping; no group is created (contrast `/consume`,
//! whose group mode auto-creates one), so a `/range` over a channel
//! with no group leaves `/pending` 404 exactly as before. The reply
//! reuses [`super::consume_wait`]'s entry decoder + JSON shape, so the
//! field contract (`{"msgs":[{"id","body":base64}]}`) can never drift
//! from what `/consume` returns.
//!
//! Bound syntax is Lite's own XRANGE convention (`model::parse_bound`):
//! `-` = minimum, `+` = maximum, a leading `(` = exclusive, otherwise
//! an inclusive `<ms>-<seq>` id. Bounds are validated HERE (400 on a
//! malformed id, the front's parameter posture) so the engine only
//! ever sees well-formed argv.

use crate::state::Shared;

use super::api::{bad_request, channel_stream, run, server_error, HttpReply};
use super::consume_wait::{msg_of, msgs_json, Msg};
use super::query::Query;
use super::respv::{self, Value};
use super::MAX_BATCH;

/// `POST /range?channel=NAME[&begin=B][&end=E][&limit=K]` (`topic=` is
/// accepted as an alias, RocksMQ's own spelling): XRANGE replay in
/// ascending id order. `begin` defaults to `-`, `end` to `+`, `limit`
/// to [`MAX_BATCH`] (1..=MAX_BATCH, the `/consume` `n` family).
pub async fn range(shared: &Shared, query: &Query) -> HttpReply {
    let Some(channel) = query.get("channel").or_else(|| query.get("topic")) else {
        return bad_request("missing 'channel' query parameter");
    };
    if channel.is_empty() {
        return bad_request("empty 'channel' query parameter");
    }
    let begin = query.get("begin").unwrap_or("-");
    let end = query.get("end").unwrap_or("+");
    for (which, bound) in [("begin", begin), ("end", end)] {
        if let Err(e) = check_bound(bound) {
            return bad_request(&format!("invalid '{which}' bound: {e}"));
        }
    }
    let limit = match limit_of(query.get("limit")) {
        Ok(k) => k,
        Err(e) => return bad_request(&e),
    };
    let stream = match channel_stream(channel) {
        Ok(s) => s,
        Err(_) => return bad_request("invalid channel name"),
    };
    let limit_str = limit.to_string();
    let out = run(
        shared,
        &[
            b"XRANGE",
            stream.as_bytes(),
            begin.as_bytes(),
            end.as_bytes(),
            b"COUNT",
            limit_str.as_bytes(),
        ],
    )
    .await;
    match respv::parse(&out) {
        // XRANGE answers a flat array of `[id, fields]` entry frames
        // (not XREAD's `[stream, entries]` pairs); a missing stream is
        // simply an empty array.
        Ok(Value::Array(Some(frames))) => {
            let msgs: Vec<Msg> = frames.iter().filter_map(msg_of).collect();
            msgs_json(msgs)
        }
        Ok(Value::Array(None)) => msgs_json(Vec::new()),
        Ok(Value::Error(e)) => server_error(&String::from_utf8_lossy(&e)),
        _ => server_error("unexpected xrange reply"),
    }
}

/// `limit` query value -> page size (default [`MAX_BATCH`]; 0 or >
/// MAX_BATCH invalid -- the same ceiling class as `/consume`'s `n`).
fn limit_of(raw: Option<&str>) -> Result<usize, String> {
    match raw {
        None => Ok(MAX_BATCH),
        Some(raw) => {
            let n: usize = raw
                .parse()
                .map_err(|_| format!("invalid 'limit' parameter '{raw}'"))?;
            if n == 0 || n > MAX_BATCH {
                return Err(format!("'limit' must be 1..={MAX_BATCH}, got {n}"));
            }
            Ok(n)
        }
    }
}

/// Validate one XRANGE bound against Lite's grammar (`-`, `+`, `(<id>`
/// exclusive, `<id>` inclusive; ids are `<u64>-<u64>`). Pure.
fn check_bound(bound: &str) -> Result<(), String> {
    if bound == "-" || bound == "+" {
        return Ok(());
    }
    let id = bound.strip_prefix('(').unwrap_or(bound);
    if bound.starts_with('(') && (id == "-" || id == "+") {
        return Err("sentinel bounds cannot be exclusive".to_string());
    }
    match id.split_once('-') {
        Some((ms, seq))
            if !ms.is_empty()
                && !seq.is_empty()
                && ms.bytes().all(|b| b.is_ascii_digit())
                && seq.bytes().all(|b| b.is_ascii_digit())
                && ms.parse::<u64>().is_ok()
                && seq.parse::<u64>().is_ok() =>
        {
            Ok(())
        }
        _ => Err(format!(
            "expected -, + or [<ms>-<seq>] with optional '(' prefix, got '{bound}'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_grammar() {
        for ok in [
            "-",
            "+",
            "0-0",
            "1760000000000-1",
            "(0-0",
            "(1760000000000-1",
        ] {
            assert!(check_bound(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "(",
            "(-",
            "(+",
            "x",
            "5",
            "5-",
            "-1",
            "a-1",
            "1-a",
            "1-2-3",
            "18446744073709551616-0",
            "(x-1",
        ] {
            assert!(check_bound(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn limit_bounds() {
        assert_eq!(limit_of(None).unwrap(), MAX_BATCH);
        assert_eq!(limit_of(Some("1")).unwrap(), 1);
        assert_eq!(limit_of(Some(&MAX_BATCH.to_string())).unwrap(), MAX_BATCH);
        assert!(limit_of(Some("0")).is_err());
        assert!(limit_of(Some(&(MAX_BATCH + 1).to_string())).is_err());
        assert!(limit_of(Some("x")).is_err());
        assert!(limit_of(Some("")).is_err());
    }
}
