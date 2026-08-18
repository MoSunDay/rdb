//! Whole-hash reads and iteration: HGETALL/HKEYS/HVALS (one unbounded
//! pass over the field range), HSCAN (cursor = hex of the last field
//! returned, MATCH/COUNT like SCAN) and HRANDFIELD (random sampling via
//! `utils::rand_u64`; positive counts sample WITHOUT replacement, negative
//! counts repeat, matching Redis).

use crate::command::hash_cmd::{arity, hash_state, parse_i64, HashState, WRONGTYPE};
use crate::command::Ctx;
use crate::ds::hash_ds;
use crate::resp::codec::{
    append_array, append_bulk, append_bulk_string, append_error, append_null,
};
use crate::utils::rand_u64;

/// HGETALL key -> flat [f1, v1, ...] (missing key -> empty array).
pub async fn hgetall(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "hgetall");
        return;
    }
    if let HashState::WrongType = hash_state(ctx, &ctx.args[0]) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let page = hash_ds::collect_fields(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        None,
        None,
        0,
    );
    append_array(ctx.out, page.fields.len() * 2);
    for (f, v) in &page.fields {
        append_bulk(ctx.out, f);
        append_bulk(ctx.out, v);
    }
}

/// HKEYS key -> [field, ...].
pub async fn hkeys(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "hkeys");
        return;
    }
    if let HashState::WrongType = hash_state(ctx, &ctx.args[0]) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let page = hash_ds::collect_fields(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        None,
        None,
        0,
    );
    append_array(ctx.out, page.fields.len());
    for (f, _) in &page.fields {
        append_bulk(ctx.out, f);
    }
}

/// HVALS key -> [value, ...].
pub async fn hvals(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "hvals");
        return;
    }
    if let HashState::WrongType = hash_state(ctx, &ctx.args[0]) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let page = hash_ds::collect_fields(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        None,
        None,
        0,
    );
    append_array(ctx.out, page.fields.len());
    for (_, v) in &page.fields {
        append_bulk(ctx.out, v);
    }
}

/// `HSCAN key cursor [MATCH pattern] [COUNT n] [WITHVALUES]` -> `[cursor,
/// [f1, (v1,) f2, ...]]`. The cursor is the hex of the last field returned
/// ("0" restarts, "" resumes after an empty field); COUNT is a hint (default 10), MATCH globs fields.
/// WITHVALUES flattens value bulks after every field (this deviates from
/// real Redis, whose HSCAN ALWAYS returns pairs; the Go rdb has no HSCAN,
/// so this explicit opt-in is the local contract).
pub async fn hscan(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "hscan");
        return;
    }
    // "0" restarts; every other cursor hex-decodes to a field to resume
    // STRICTLY after — including "" (hex of an EMPTY field); treating ""
    // as a restart would livelock paging when the empty field lands on a
    // page boundary, and resuming after "" equals a fresh start for every
    // hash that has no empty field.
    let cursor_start = ctx.args[1] == b"0";
    let mut pattern: Option<Vec<u8>> = None;
    let mut count: usize = 10;
    let mut with_values = false;
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
            b"WITHVALUES" if i + 1 == ctx.args.len() => {
                with_values = true;
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
    if let HashState::WrongType = hash_state(ctx, &ctx.args[0]) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let page = hash_ds::collect_fields(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        from.as_deref(),
        pattern.as_deref(),
        count,
    );
    let cursor = match &page.next {
        None => "0".to_string(),
        Some(field) => hex::encode(field),
    };
    append_array(ctx.out, 2);
    append_bulk_string(ctx.out, &cursor);
    let per = if with_values { 2 } else { 1 };
    append_array(ctx.out, page.fields.len() * per);
    for (f, v) in &page.fields {
        append_bulk(ctx.out, f);
        if with_values {
            append_bulk(ctx.out, v);
        }
    }
}

/// `HRANDFIELD key [count [WITHVALUES]]`. Without count: one random field
/// (null bulk when the key is missing). count >= 0: up to `count` DISTINCT
/// fields; count < 0: |count| draws WITH repetition (values may repeat).
pub async fn hrandfield(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 3 {
        arity(ctx.out, "hrandfield");
        return;
    }
    let mut with_values = false;
    if ctx.args.len() == 3 {
        if ctx.args[2].eq_ignore_ascii_case(b"WITHVALUES") {
            with_values = true;
        } else {
            append_error(ctx.out, "ERR syntax error");
            return;
        }
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
    if with_values && count.is_none() {
        append_error(ctx.out, "ERR syntax error");
        return;
    }
    if let HashState::WrongType = hash_state(ctx, &ctx.args[0]) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let page = hash_ds::collect_fields(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        None,
        None,
        0,
    );
    match count {
        None => match pick_one(&page.fields) {
            Some(idx) => append_bulk(ctx.out, &page.fields[idx].0),
            None => append_null(ctx.out),
        },
        Some(n) => {
            let picks = pick_many(&page.fields, n);
            let per = if with_values { 2 } else { 1 };
            append_array(ctx.out, picks.len() * per);
            for idx in picks {
                let (f, v) = &page.fields[idx];
                append_bulk(ctx.out, f);
                if with_values {
                    append_bulk(ctx.out, v);
                }
            }
        }
    }
}

/// Uniform single index draw; `None` when the hash is empty.
fn pick_one(fields: &[(Vec<u8>, Vec<u8>)]) -> Option<usize> {
    if fields.is_empty() {
        None
    } else {
        Some((rand_u64() % fields.len() as u64) as usize)
    }
}

/// count > 0: distinct indices (rejection sampling over a used bitmap);
/// count < 0: |count| independent draws (repeats allowed); 0: none.
fn pick_many(fields: &[(Vec<u8>, Vec<u8>)], count: i64) -> Vec<usize> {
    let n = fields.len();
    if n == 0 || count == 0 {
        return Vec::new();
    }
    if count < 0 {
        return (0..count.unsigned_abs())
            .map(|_| (rand_u64() % n as u64) as usize)
            .collect();
    }
    let want = (count as u64).min(n as u64) as usize;
    let mut used = vec![false; n];
    let mut picks = Vec::with_capacity(want);
    while picks.len() < want {
        let idx = (rand_u64() % n as u64) as usize;
        if !used[idx] {
            used[idx] = true;
            picks.push(idx);
        }
    }
    picks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_fields() -> Vec<(Vec<u8>, Vec<u8>)> {
        vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]
    }

    #[test]
    fn pick_many_negative_count_bound_survives_i64_min() {
        let fields = two_fields();
        // Bounded negative draw: |count| independent picks, in range.
        let picks = pick_many(&fields, -5);
        assert_eq!(picks.len(), 5);
        assert!(picks.iter().all(|&i| i < fields.len()));
        // The draw bound comes from unsigned_abs(): plain `-count`
        // overflows at i64::MIN (panic in debug, wrap in release) and
        // would ask for a ~2^63-element vector.
        assert_eq!(i64::MIN.unsigned_abs(), 1u64 << 63);
    }

    #[test]
    fn pick_many_positive_and_zero_bounds() {
        assert!(pick_many(&two_fields(), 0).is_empty());
        assert!(pick_many(&[], 7).is_empty());
        assert_eq!(pick_many(&two_fields(), 99).len(), 2);
    }
}
