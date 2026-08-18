//! ZSCAN: cursor iteration over one zset's member records. Mirrors
//! `set_scan` exactly -- the cursor is the hex of the last member
//! returned ("0" restarts; "" resumes after the empty member), MATCH filters with the glob matcher,
//! COUNT bounds one page (default 10) -- except the reply is FLAT:
//! `[cursor, member, score?, member, score?, ...]` with a score bulk
//! after each member under WITHSCORES (Redis's ZSCAN shape).
//!
//! The member records (`KIND_ZSET_MEMBER`) carry the member as the key
//! suffix AND the 8-byte sortable score as the value, so one window
//! scan yields both without point lookups; the per-kind bounds keep
//! other keys' records out (kind byte sorts before the key bytes).

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::zset_util::{append_score, zset_state, ZSetState};
use crate::command::Ctx;
use crate::ds::{expire, zset_ds};
use crate::resp::codec::{append_array, append_bulk, append_bulk_string, append_error};
use crate::store::{key_upper_bound, ops, Store};
use crate::utils::glob_match;

/// One ZSCAN page: matched `(member, score)` pairs plus the resume
/// cursor — Some(last member) continues strictly after it, None means
/// iteration finished (an empty member is a valid member).
struct ZScanPage {
    items: Vec<(Vec<u8>, f64)>,
    next: Option<Vec<u8>>,
}

/// Scan `key`'s member window from `from_member` (None = start),
/// keeping MATCH-passing members until `count` are in hand (0 =
/// unbounded). The member window's bounds mirror `set_ds`'s
/// `members_range` over the zset member kind.
fn collect_page(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    from_member: Option<&[u8]>,
    pattern: Option<&[u8]>,
    count: usize,
) -> ZScanPage {
    let lower = zset_ds::member_key(prefix, key, b"");
    let upper = key_upper_bound(&lower).unwrap_or_default();
    let (start, excl_start) = match from_member {
        Some(m) => (zset_ds::member_key(prefix, key, m), true),
        None => (lower.clone(), false),
    };
    let base = lower.len();
    let mut items: Vec<(Vec<u8>, f64)> = Vec::new();
    let mut next: Option<Vec<u8>> = None;
    let _ = ops::for_each_from(store, &start, excl_start, &mut |k, v| {
        if k >= upper.as_slice() {
            return false; // left this zset's member window
        }
        if let Some(member) = k.get(base..) {
            if pattern.is_none_or(|p| glob_match(p, member)) {
                // The member record's value IS the 8-byte sortable score.
                let score = v
                    .get(..8)
                    .and_then(|b| b.try_into().ok())
                    .map(u64::from_be_bytes)
                    .map(zset_ds::sortable_score)
                    .unwrap_or(0.0);
                items.push((member.to_vec(), score));
                if count != 0 && items.len() >= count {
                    next = Some(member.to_vec());
                    return false;
                }
            }
        }
        true
    });
    ZScanPage { items, next }
}

/// `ZSCAN key cursor [MATCH pattern] [COUNT n] [WITHSCORES]` ->
/// `[cursor-bulk, member, (score), ...]` flat array.
pub async fn zscan(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "zscan");
        return;
    }
    // "0" restarts; every other cursor hex-decodes to a member to resume
    // STRICTLY after. That includes "" (hex of an EMPTY member): treating
    // it as a restart would livelock paging when the empty member lands
    // exactly on a page boundary, and for sets without an empty member
    // resuming after "" is identical to a fresh start anyway.
    let cursor_start = ctx.args[1] == b"0";
    let mut pattern: Option<Vec<u8>> = None;
    let mut count: usize = 10;
    let mut withscores = false;
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
            b"WITHSCORES" => {
                withscores = true;
                i += 1;
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
    if let ZSetState::WrongType = zset_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        expire::now_ms(),
    ) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let page = collect_page(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        from.as_deref(),
        pattern.as_deref(),
        count,
    );
    let cursor = match &page.next {
        None => "0".to_string(),
        Some(member) => hex::encode(member),
    };
    let per = if withscores { 2 } else { 1 };
    append_array(ctx.out, 1 + page.items.len() * per);
    append_bulk_string(ctx.out, &cursor);
    for (member, score) in &page.items {
        append_bulk(ctx.out, member);
        if withscores {
            append_score(ctx.out, *score);
        }
    }
}
