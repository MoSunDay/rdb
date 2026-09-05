//! XREVRANGE: the descending twin of XRANGE -- entries newest-first
//! within `[start, end]` (both bounds may carry an exclusive `(`).
//! Mirrors `append::xrange`'s framing and COUNT handling but iterates
//! DOWN from the end bound via `store::ops::for_each_down_from` and
//! stops at the start bound instead of seeking up from it.

use crate::command::Ctx;
use crate::resp::codec as resp;
use crate::store::ops;

use super::entries::{append_entry_frame, id_from_key, stream_of, Entry};
use super::model;

/// `XREVRANGE <parent/child> end start [COUNT count]`: walk down from
/// the END bound (inclusive unless `(`-prefixed), stop past the START
/// bound, cap at COUNT. Missing stream -> empty array.
pub async fn xrevrange(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 || ctx.args.len() > 5 {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xrevrange' command",
        );
    }
    let count = if ctx.args.len() >= 4 {
        // The only legal 4th token is COUNT (with its value as the 5th).
        if !ctx.args[3].eq_ignore_ascii_case(b"COUNT") {
            return resp::append_error(
                ctx.out,
                "ERR wrong number of arguments for 'xrevrange' command",
            );
        }
        let Some(val) = ctx.args.get(4) else {
            return resp::append_error(
                ctx.out,
                "ERR wrong number of arguments for 'xrevrange' command",
            );
        };
        match std::str::from_utf8(val)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        {
            Some(n) if n > 0 => n,
            _ => return resp::append_error(ctx.out, "ERR value is not an integer or out of range"),
        }
    } else {
        usize::MAX
    };
    // Arguments arrive highest-first: args[1] is the END, args[2] the START.
    let (end, start) = match (
        model::parse_bound(&ctx.args[1]),
        model::parse_bound(&ctx.args[2]),
    ) {
        (Some(e), Some(s)) => (e, s),
        _ => {
            return resp::append_error(
                ctx.out,
                "ERR Invalid stream ID specified as stream command argument",
            )
        }
    };
    let Some((stream, prefix)) = stream_of(ctx, 0) else {
        return;
    };
    let base = model::entry_base(&prefix, &stream);
    let from = model::entry_key(&prefix, &stream, end.id);
    let mut entries = Vec::new();
    let res = ops::for_each_down_from(&ctx.shared.store, &from, end.excl, &mut |k, v| {
        if !k.starts_with(&base) {
            return false; // left this stream's window from above
        }
        let Some(id) = id_from_key(&base, k) else {
            return false;
        };
        let past_start = if start.excl {
            id <= start.id
        } else {
            id < start.id
        };
        if past_start {
            return false;
        }
        if let Some(fields) = model::decode_entry(v) {
            entries.push(Entry { id, fields });
        }
        entries.len() < count
    });
    match res {
        Err(e) => resp::append_error(ctx.out, &format!("ERR: xrevrange failed: {e}")),
        Ok(()) => {
            resp::append_array(ctx.out, entries.len());
            for e in &entries {
                append_entry_frame(ctx.out, e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Handler-level tests: `xrevrange` is not yet in the command
    //! registry, so it (and the seeding XADD) are driven directly over a
    //! real store. Lite handlers derive the physical prefix from the
    //! PARENT topic name themselves, so `prefix_key` stays empty.

    use super::*;
    use crate::command::test_ctx;
    use crate::resp::codec::test_reader::{self, Frame};
    use crate::state::{testutil, Shared};

    fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
        let guard = crate::command::string::TEST_STORE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut conf = testutil::test_config();
        conf.bind = bind.to_string();
        (guard, testutil::shared_with(conf))
    }

    /// Drive one lite handler directly (it is not yet in the registry):
    /// lite handlers derive the physical prefix from the PARENT topic
    /// name themselves, so `prefix_key` stays empty.
    fn call_direct(
        shared: &Shared,
        handler: for<'a> fn(&'a mut Ctx<'_>) -> crate::command::HandlerFuture<'a>,
        args: &[&[u8]],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
        let mut ctx = test_ctx(shared, Vec::new(), argv, &mut out);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(handler(&mut ctx));
        out
    }

    fn call(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
        call_direct(shared, |ctx| Box::pin(xrevrange(ctx)), args)
    }

    fn xadd(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
        call_direct(
            shared,
            |ctx| Box::pin(super::super::append::xadd(ctx)),
            args,
        )
    }

    fn xrange(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
        call_direct(
            shared,
            |ctx| Box::pin(super::super::append::xrange(ctx)),
            args,
        )
    }

    /// Entry ids of an XREVRANGE reply, in reply order.
    fn ids_of(reply: &[u8]) -> Vec<String> {
        test_reader::parse(reply)
            .into_iter()
            .map(|row| match row {
                Frame::Array(mut parts) => match parts.remove(0) {
                    Frame::Bulk(id) => String::from_utf8(id).unwrap(),
                    _ => panic!("entry frame starts with the id bulk"),
                },
                _ => panic!("entry frames"),
            })
            .collect()
    }

    #[test]
    fn xrevrange_full_window_is_newest_first() {
        let (_g, s) = shared_for("127.0.0.1:43150");
        for i in 1..=4u8 {
            let id = format!("1-{i}");
            assert_eq!(
                xadd(&s, &[b"t/q1", id.as_bytes(), b"f", &[b'v', b'0' + i]]),
                format!("${}\r\n{id}\r\n", id.len()).into_bytes()
            );
        }
        // `+` .. `-` (passed as end, start): everything, descending.
        assert_eq!(
            ids_of(&call(&s, &[b"t/q1", b"+", b"-"])),
            vec!["1-4", "1-3", "1-2", "1-1"]
        );
        // The ascending twin agrees on membership, opposite order.
        let mut up = ids_of(&xrange(&s, &[b"t/q1", b"-", b"+"]));
        up.reverse();
        assert_eq!(ids_of(&call(&s, &[b"t/q1", b"+", b"-"])), up);
    }

    #[test]
    fn xrevrange_exclusive_bounds_and_count() {
        let (_g, s) = shared_for("127.0.0.1:43151");
        for i in 1..=4u8 {
            let id = format!("1-{i}");
            xadd(&s, &[b"t/q2", id.as_bytes(), b"sku", &[b'v', b'0' + i]]);
        }
        // Exclusive end drops 1-4; exclusive start drops 1-1.
        assert_eq!(
            ids_of(&call(&s, &[b"t/q2", b"(1-4", b"(1-1"])),
            vec!["1-3", "1-2"]
        );
        // COUNT caps the reply at the NEWEST end.
        assert_eq!(
            ids_of(&call(&s, &[b"t/q2", b"+", b"-", b"COUNT", b"2"])),
            vec!["1-4", "1-3"]
        );
        // COUNT 0 / garbage is rejected.
        assert_eq!(
            call(&s, &[b"t/q2", b"+", b"-", b"COUNT", b"0"]),
            b"-ERR value is not an integer or out of range\r\n".to_vec()
        );
        // A non-COUNT 4th token is an arity error.
        assert_eq!(
            call(&s, &[b"t/q2", b"+", b"-", b"BOGUS", b"2"]),
            b"-ERR wrong number of arguments for 'xrevrange' command\r\n".to_vec()
        );
    }

    #[test]
    fn xrevrange_empty_windows_and_errors() {
        let (_g, s) = shared_for("127.0.0.1:43152");
        // Missing stream: empty array, not an error.
        assert_eq!(call(&s, &[b"t/q3", b"+", b"-"]), b"*0\r\n".to_vec());
        xadd(&s, &[b"t/q3", b"1-1", b"f", b"v"]);
        xadd(&s, &[b"t/q3", b"1-2", b"f", b"v"]);
        // End below start: nothing can match.
        assert_eq!(call(&s, &[b"t/q3", b"1-1", b"1-2"]), b"*0\r\n".to_vec());
        // A single-entry window including both endpoints.
        assert_eq!(ids_of(&call(&s, &[b"t/q3", b"1-1", b"1-1"])), vec!["1-1"]);
        // Bad ids.
        assert_eq!(
            call(&s, &[b"t/q3", b"nonsense", b"-"]),
            b"-ERR Invalid stream ID specified as stream command argument\r\n".to_vec()
        );
        assert_eq!(
            call(&s, &[b"t/q3", b"+", b"1-x"]),
            b"-ERR Invalid stream ID specified as stream command argument\r\n".to_vec()
        );
        // Arity: 2 args is too few, 6 is too many.
        assert_eq!(
            call(&s, &[b"t/q3", b"+"]),
            b"-ERR wrong number of arguments for 'xrevrange' command\r\n".to_vec()
        );
        assert_eq!(
            call(&s, &[b"t/q3", b"+", b"-", b"COUNT", b"1", b"x"]),
            b"-ERR wrong number of arguments for 'xrevrange' command\r\n".to_vec()
        );
        // A bare parent name needs the full stream error.
        assert_eq!(
            call(&s, &[b"t", b"+", b"-"]),
            b"-ERR a full stream name 'parent/child' is required\r\n".to_vec()
        );
    }
}
