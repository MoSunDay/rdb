//! LMPOP: pop from the first non-empty list among several candidate
//! keys. All keys must hash to one slot (the RESP layer derives the
//! physical prefix from the first key); every distinct key's latch is
//! taken up front in byte order (the multi-key ABBA rule) so the
//! try-in-order pass runs under a consistent snapshot. The pop itself
//! reuses `list_ops::pop_core`, so element order and the drained-key
//! deletion match LPOP/RPOP counted replies exactly.

use crate::command::hash_cmd::{arity, parse_i64};
use crate::command::list_cmd::lock_sorted;
use crate::command::list_ops::pop_core;
use crate::command::Ctx;
use crate::ds::setops::require_same_slot;
use crate::resp::codec::{append_array, append_bulk, append_error};

/// LMPOP numkeys key [key ...] <LEFT|RIGHT> [COUNT count] -> a 2-element
/// array `[key, [elements...]]`, or a null array when every candidate is
/// missing/empty. COUNT defaults to 1 and must be positive.
pub async fn lmpop(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        arity(ctx.out, "lmpop");
        return;
    }
    let numkeys = match parse_i64(&ctx.args[0]) {
        Some(n) if n > 0 => n,
        _ => {
            append_error(ctx.out, "ERR numkeys should be greater than 0");
            return;
        }
    };
    if numkeys as usize > ctx.args.len() - 1 {
        append_error(
            ctx.out,
            "ERR Number of keys can't be greater than number of args",
        );
        return;
    }
    let keys = ctx.args[1..1 + numkeys as usize].to_vec();
    if !require_same_slot(ctx.out, &keys) {
        return;
    }
    let mut i = 1 + numkeys as usize;
    let left = match ctx.args.get(i) {
        Some(t) if t.eq_ignore_ascii_case(b"LEFT") => {
            i += 1;
            true
        }
        Some(t) if t.eq_ignore_ascii_case(b"RIGHT") => {
            i += 1;
            false
        }
        _ => {
            append_error(ctx.out, "ERR syntax error");
            return;
        }
    };
    let mut count: i64 = 1;
    while i < ctx.args.len() {
        if ctx.args[i].eq_ignore_ascii_case(b"COUNT") && i + 1 < ctx.args.len() {
            match parse_i64(&ctx.args[i + 1]) {
                Some(n) if n > 0 => count = n,
                _ => {
                    append_error(ctx.out, "ERR value is out of range, must be positive");
                    return;
                }
            }
            i += 2;
        } else {
            append_error(ctx.out, "ERR syntax error");
            return;
        }
    }

    let _guards = lock_sorted(ctx, &keys).await;
    for key in &keys {
        match pop_core(ctx, key, left, Some(count), "lmpop").await {
            None => return,                              // error reply already written
            Some(elems) if elems.is_empty() => continue, // try the next key
            Some(elems) => {
                append_array(ctx.out, 2);
                append_bulk(ctx.out, key);
                append_array(ctx.out, elems.len());
                for e in &elems {
                    append_bulk(ctx.out, e);
                }
                return;
            }
        }
    }
    // Every candidate was missing or drained: nil array.
    crate::resp::codec::append_raw(ctx.out, b"*-1\r\n");
}

#[cfg(test)]
mod tests {
    //! Handler-level tests (same harness shape as `list_tests`): the
    //! command is not yet in the registry, so the handler is driven
    //! directly. Ports 40861+ leave room for the other families.

    use super::*;
    use crate::command::test_ctx;
    use crate::resp::codec::test_reader::{self, Frame};
    use crate::state::{testutil, Shared};

    const PREFIX: &[u8] = b"70/";

    fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
        let guard = crate::command::string::TEST_STORE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut conf = testutil::test_config();
        conf.bind = bind.to_string();
        (guard, testutil::shared_with(conf))
    }

    /// Drive `lmpop` directly (bypassing the registry) over a fixed slot
    /// prefix; `{g}`-tagged keys keep every candidate in one slot.
    fn call(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
        let mut ctx = test_ctx(shared, PREFIX.to_vec(), argv, &mut out);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(lmpop(&mut ctx));
        out
    }

    fn reg(shared: &Shared, name: &str, args: &[&[u8]]) -> Vec<u8> {
        let handler = crate::command::lookup(name).unwrap();
        let mut out = Vec::new();
        let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
        // Same fixed physical prefix as `call`: `{g}` tags only steer the
        // same_slot check, not where the data lands.
        let mut ctx = test_ctx(shared, PREFIX.to_vec(), argv, &mut out);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(handler(&mut ctx));
        out
    }

    #[test]
    fn lmpop_left_first_non_empty_in_order() {
        let (_g, s) = shared_for("127.0.0.1:40861");
        reg(&s, "rpush", &[b"{g}b", b"1", b"2", b"3"]);
        reg(&s, "rpush", &[b"{g}a", b"x", b"y"]);
        // First non-empty key wins: LEFT pops head-first.
        assert_eq!(
            call(&s, &[b"2", b"{g}missing", b"{g}b", b"LEFT"]),
            b"*2\r\n$4\r\n{g}b\r\n*1\r\n$1\r\n1\r\n".to_vec()
        );
        // COUNT 2 keeps LPOP order (head first), then the next call moves
        // on to the SECOND key once the first is drained.
        assert_eq!(
            call(&s, &[b"2", b"{g}missing", b"{g}b", b"LEFT", b"COUNT", b"2"]),
            b"*2\r\n$4\r\n{g}b\r\n*2\r\n$1\r\n2\r\n$1\r\n3\r\n".to_vec()
        );
        // {g}b is gone now: the pass falls through to {g}a (COUNT still 1).
        assert_eq!(
            call(&s, &[b"3", b"{g}missing", b"{g}b", b"{g}a", b"LEFT"]),
            b"*2\r\n$4\r\n{g}a\r\n*1\r\n$1\r\nx\r\n".to_vec()
        );
        assert_eq!(
            call(&s, &[b"2", b"{g}missing", b"{g}a", b"LEFT", b"COUNT", b"5"]),
            b"*2\r\n$4\r\n{g}a\r\n*1\r\n$1\r\ny\r\n".to_vec()
        );
    }

    #[test]
    fn lmpop_right_order_matches_rpop() {
        let (_g, s) = shared_for("127.0.0.1:40862");
        reg(&s, "rpush", &[b"{g}k", b"a", b"b", b"c"]);
        // RIGHT pops tail-first: c, b.
        let reply = call(&s, &[b"1", b"{g}k", b"RIGHT", b"COUNT", b"2"]);
        let rows = test_reader::parse(&reply);
        let key = test_reader::bulk(rows.first().expect("key frame"));
        let mut elements = Vec::new();
        if let Some(Frame::Array(items)) = rows.get(1) {
            for it in items {
                elements.push(test_reader::bulk(it));
            }
        }
        assert_eq!(key, b"{g}k".to_vec());
        assert_eq!(elements, vec![b"c".to_vec(), b"b".to_vec()]);
        assert_eq!(
            reg(&s, "lrange", &[b"{g}k", b"0", b"-1"]),
            b"*1\r\n$1\r\na\r\n".to_vec()
        );
        // Count larger than the list: min(count, len), key deleted at 0.
        assert_eq!(
            call(&s, &[b"1", b"{g}k", b"RIGHT", b"COUNT", b"9"]),
            b"*2\r\n$4\r\n{g}k\r\n*1\r\n$1\r\na\r\n".to_vec()
        );
        assert_eq!(reg(&s, "exists", &[b"{g}k"]), b":0\r\n".to_vec());
    }

    #[test]
    fn lmpop_all_empty_or_missing_is_null_array() {
        let (_g, s) = shared_for("127.0.0.1:40863");
        assert_eq!(call(&s, &[b"1", b"{g}none", b"LEFT"]), b"*-1\r\n".to_vec());
        reg(&s, "rpush", &[b"{g}e", b"v"]);
        assert_eq!(reg(&s, "lpop", &[b"{g}e"]), b"$1\r\nv\r\n".to_vec());
        // A drained (deleted) list reads like a missing one.
        assert_eq!(
            call(&s, &[b"2", b"{g}none", b"{g}e", b"RIGHT"]),
            b"*-1\r\n".to_vec()
        );
        // Default COUNT is 1: a single element off the chosen end.
        reg(&s, "rpush", &[b"{g}f", b"m1", b"m2"]);
        assert_eq!(
            call(&s, &[b"1", b"{g}f", b"RIGHT"]),
            b"*2\r\n$4\r\n{g}f\r\n*1\r\n$2\r\nm2\r\n".to_vec()
        );
    }

    #[test]
    fn lmpop_crossslot_and_argument_errors() {
        let (_g, s) = shared_for("127.0.0.1:40864");
        // Keys in different slots.
        assert_eq!(
            call(&s, &[b"2", b"{g}a", b"{u}b", b"LEFT"]),
            b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
        );
        // numkeys: zero / negative / not an integer.
        for bad in [&b"0"[..], &b"-1"[..], &b"x"[..]] {
            assert_eq!(
                call(&s, &[bad, b"{g}a", b"LEFT"]),
                b"-ERR numkeys should be greater than 0\r\n".to_vec()
            );
        }
        // numkeys exceeding every remaining argument after it.
        assert_eq!(
            call(&s, &[b"4", b"{g}a", b"{g}b", b"LEFT"]),
            b"-ERR Number of keys can't be greater than number of args\r\n".to_vec()
        );
        // Redis quirk kept: a numkeys that swallows the direction token
        // treats it as a KEY, so the slot rule fires first.
        assert_eq!(
            call(&s, &[b"3", b"{g}a", b"{g}b", b"LEFT"]),
            b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
        );
        // Missing / unknown direction token (arity floor needs 3 args,
        // so the missing-direction case passes a second key).
        assert_eq!(
            call(&s, &[b"1", b"{g}a"]),
            b"-ERR wrong number of arguments for 'lmpop' command\r\n".to_vec()
        );
        assert_eq!(
            call(&s, &[b"1", b"{g}a", b"{g}b"]),
            b"-ERR syntax error\r\n".to_vec()
        );
        assert_eq!(
            call(&s, &[b"1", b"{g}a", b"up"]),
            b"-ERR syntax error\r\n".to_vec()
        );
        // COUNT: non-positive or unparsable; dangling COUNT.
        assert_eq!(
            call(&s, &[b"1", b"{g}a", b"LEFT", b"COUNT", b"0"]),
            b"-ERR value is out of range, must be positive\r\n".to_vec()
        );
        assert_eq!(
            call(&s, &[b"1", b"{g}a", b"LEFT", b"COUNT", b"x"]),
            b"-ERR value is out of range, must be positive\r\n".to_vec()
        );
        assert_eq!(
            call(&s, &[b"1", b"{g}a", b"LEFT", b"COUNT"]),
            b"-ERR syntax error\r\n".to_vec()
        );
        // Too few arguments at all.
        assert_eq!(
            call(&s, &[b"1"]),
            b"-ERR wrong number of arguments for 'lmpop' command\r\n".to_vec()
        );
    }
}
