//! Set iteration and sampling: SSCAN (cursor = hex of the last member
//! returned; MATCH/COUNT like SCAN) and SRANDMEMBER (random reads; positive
//! counts sample without replacement, negative counts repeat). SPOP -- the
//! removing twin of SRANDMEMBER -- stays in `set_cmd` beside the other
//! mutations.

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::set_cmd::{set_state, SetState};
use crate::command::Ctx;
use crate::ds::set_ds;
use crate::resp::codec::{
    append_array, append_bulk, append_bulk_string, append_error, append_null,
};
use crate::utils::rand_u64;

/// `SRANDMEMBER key [count]`: random members without removal; negative
/// counts draw WITH repetition. No count: one bulk member or null.
pub async fn srandmember(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 2 {
        arity(ctx.out, "srandmember");
        return;
    }
    let count: Option<i64> = match ctx.args.get(1) {
        None => None,
        Some(arg) => match parse_i64(arg) {
            Some(n) => Some(n),
            None => {
                append_error(ctx.out, "ERR value is not an integer or out of range");
                return;
            }
        },
    };
    if let SetState::WrongType = set_state(ctx, &ctx.args[0]) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let page = match set_ds::collect_members(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        None,
        None,
        0,
    ) {
        Ok(page) => page,
        Err(_) => {
            append_error(ctx.out, "ERR: srandmember failed");
            return;
        }
    };
    let n = page.members.len();
    match count {
        None => {
            if n == 0 {
                append_null(ctx.out);
            } else {
                append_bulk(ctx.out, &page.members[(rand_u64() % n as u64) as usize]);
            }
        }
        Some(c) => {
            if n == 0 || c == 0 {
                append_array(ctx.out, 0);
                return;
            }
            if c < 0 {
                // Repeating draws: |c| members, independent picks.
                append_array(ctx.out, c.unsigned_abs() as usize);
                for _ in 0..c.unsigned_abs() {
                    append_bulk(ctx.out, &page.members[(rand_u64() % n as u64) as usize]);
                }
            } else {
                // Distinct picks: partial Fisher-Yates over a prefix of
                // length min(c, n) -- the header must match that count.
                let mut idx: Vec<usize> = (0..n).collect();
                let want = (c as usize).min(n);
                for i in 0..want {
                    let j = i + (rand_u64() % (idx.len() - i) as u64) as usize;
                    idx.swap(i, j);
                }
                append_array(ctx.out, want);
                for &i in &idx[..want] {
                    append_bulk(ctx.out, &page.members[i]);
                }
            }
        }
    }
}

/// `SSCAN key cursor [MATCH pattern] [COUNT n]` -> `[cursor, [member ...]]`
/// (cursor = hex of the last member returned; "0" restarts, "" resumes after the empty member).
pub async fn sscan(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "sscan");
        return;
    }
    // "0" restarts; every other cursor hex-decodes to a member to resume
    // STRICTLY after — including "" (hex of an EMPTY member); treating ""
    // as a restart would livelock paging when the empty member lands on a
    // page boundary, and resuming after "" equals a fresh start for every
    // set that has no empty member.
    let cursor_start = ctx.args[1] == b"0";
    let mut pattern: Option<Vec<u8>> = None;
    let mut count: usize = 10;
    let mut i = 2;
    while i < ctx.args.len() {
        let opt = ctx.args[i].to_ascii_uppercase();
        match opt.as_slice() {
            b"MATCH" if i + 1 < ctx.args.len() => {
                pattern = Some(ctx.args[i + 1].clone());
                i += 2;
            }
            b"COUNT" if i + 1 < ctx.args.len() => {
                match parse_i64(&ctx.args[i + 1]) {
                    Some(n) if n > 0 => count = n as usize,
                    _ => {
                        append_error(ctx.out, "ERR value is not an integer or out of range");
                        return;
                    }
                }
                i += 2;
            }
            _ => {
                append_error(ctx.out, "ERR syntax error");
                return;
            }
        }
    }
    let from = if cursor_start {
        None
    } else {
        match hex::decode(&ctx.args[1]) {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                append_error(ctx.out, "ERR invalid cursor");
                return;
            }
        }
    };
    if let SetState::WrongType = set_state(ctx, &ctx.args[0]) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let page = match set_ds::collect_members(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        from.as_deref(),
        pattern.as_deref(),
        count,
    ) {
        Ok(page) => page,
        Err(_) => {
            append_error(ctx.out, "ERR: sscan failed");
            return;
        }
    };
    let cursor = match &page.next {
        None => "0".to_string(),
        Some(member) => hex::encode(member),
    };
    append_array(ctx.out, 2);
    append_bulk_string(ctx.out, &cursor);
    append_array(ctx.out, page.members.len());
    for m in &page.members {
        append_bulk(ctx.out, m);
    }
}
