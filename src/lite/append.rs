//! Producing side of Lite streams: XADD / XLEN / XRANGE / XTRIM / XDEL /
//! XIDLE. Every meta-mutating write runs under the per-stream latch and
//! lands in ONE batched fsync (meta + entry + TTL index together).

use std::sync::atomic::Ordering;

use rocksdb::WriteBatch;

use crate::command::Ctx;
use crate::ds::{expire, latch, wait};
use crate::hash;
use crate::monitor;
use crate::resp::codec as resp;
use crate::store::ops;

use super::entries::{append_entry_frame, count_reap, id_from_key, stream_of, Entry};
use super::model::{self, MetaPayload, MetaRead};
use super::{offset, select, stat_bump, TopicName};

/// `XADD <parent[/child]> [<id|*>] <f> <v> [<f> <v> ...]`: first write
/// auto-creates the stream. A bare parent name auto-picks a queue
/// (round-robin) and the reply becomes `[full-stream, id]`.
pub async fn xadd(ctx: &mut Ctx<'_>) {
    // Optional id: parity disambiguates `name f v` from `name id f v`.
    if ctx.args.len() < 3 {
        return resp::append_error(ctx.out, "ERR wrong number of arguments for 'xadd' command");
    }
    let args = ctx.args.clone();
    let (id_arg, pairs): (&[u8], &[Vec<u8>]) = if (args.len() - 1).is_multiple_of(2) {
        (b"*", &args[1..])
    } else {
        (&args[1], &args[2..])
    };
    if pairs.len() < 2 || pairs.len() % 2 != 0 {
        return resp::append_error(ctx.out, "ERR wrong number of arguments for 'xadd' command");
    }
    let name = match super::parse_topic_name(&args[0]) {
        Ok(n) => n,
        Err(e) => return resp::append_error(ctx.out, &e),
    };
    let (parent, child, auto) = match &name {
        TopicName::Stream(p, c) => (p.clone(), c.clone(), false),
        TopicName::Parent(p) => {
            let prefix0 = hash::slot_with_prefix(p).1;
            let kids = match select::discover_children(&ctx.shared.store, &prefix0, p, 64) {
                Ok(k) => k,
                Err(e) => return resp::append_error(ctx.out, &format!("ERR: xadd failed: {e}")),
            };
            let c = select::pick_round_robin(&ctx.shared.lite.picks, p, &kids);
            (p.clone(), c, true)
        }
    };
    let mut stream = parent.clone();
    stream.push(b'/');
    stream.extend_from_slice(&child);
    let prefix = hash::slot_with_prefix(&parent).1;
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;

    let now = expire::now_ms();
    let read = model::read_meta(
        &ctx.shared.store,
        &prefix,
        &stream,
        Some(ctx.shared.lite.as_ref()),
    );
    let (meta, fresh) = match read {
        Err(e) => return resp::append_error(ctx.out, &format!("ERR: xadd failed: {e}")),
        Ok(MetaRead::Purged) => {
            count_reap(ctx);
            (
                MetaPayload {
                    created_ms: now,
                    ..Default::default()
                },
                true,
            )
        }
        Ok(MetaRead::Missing) => (
            MetaPayload {
                created_ms: now,
                ..Default::default()
            },
            true,
        ),
        Ok(MetaRead::Live(m)) => (m, false),
    };
    let id =
        if id_arg == b"*" {
            match model::auto_id((!fresh).then_some(meta.last_id()), now) {
                // Saturated last id: a generated id would equal the last one,
                // silently overwriting its entry (Redis rejects the add).
                None => return resp::append_error(
                    ctx.out,
                    "ERR The stream has exhausted the last possible ID, unable to add more items",
                ),
                Some(id) => id,
            }
        } else {
            match model::parse_id(id_arg) {
            None => {
                return resp::append_error(
                    ctx.out,
                    "ERR Invalid stream ID specified as stream command argument",
                )
            }
            Some(id) if !fresh && id <= meta.last_id() => return resp::append_error(
                ctx.out,
                "ERR The ID specified in XADD is equal or smaller than the stream last item's id",
            ),
            Some(id) => id,
        }
        };

    let mut next = meta.clone();
    next.last_ms = id.ms;
    next.last_seq = id.seq;
    next.len += 1;
    let mkey = model::meta_key(&prefix, &stream);
    let old_expire = if fresh {
        0
    } else {
        model::current_expire(&ctx.shared.store, &prefix, &stream)
    };
    // Every append retouches the idle deadline (idle = no writes). An
    // explicit XADD id may carry a huge id.ms, so `now.max(id.ms) +
    // idle_ms` can overflow u64 and wrap into a small/corrupted deadline
    // that instantly reaps the family: reject the write instead.
    let new_expire = if next.idle_ms == 0 {
        0
    } else {
        match now.max(id.ms).checked_add(next.idle_ms) {
            Some(deadline) => deadline,
            None => return resp::append_error(ctx.out, "ERR ID or idle deadline overflow"),
        }
    };

    let mut batch = WriteBatch::default();
    batch.put(&mkey, model::encode_meta_at(&next, new_expire));
    let fpairs: Vec<(&[u8], &[u8])> = pairs
        .chunks(2)
        .map(|c| (c[0].as_slice(), c[1].as_slice()))
        .collect();
    batch.put(
        model::entry_key(&prefix, &stream, id),
        model::encode_entry(&fpairs),
    );
    expire::set_ttl_entries(&mut batch, &prefix, mkey.clone(), old_expire, new_expire);

    if let Err(e) = ctx.commit(batch).await {
        return resp::append_error(ctx.out, &format!("ERR: xadd failed: {e}"));
    }
    if fresh {
        ctx.shared
            .lite
            .stats
            .streams_live
            .fetch_add(1, Ordering::Relaxed);
        offset::remove_stream(&ctx.shared.lite.offsets, &stream);
    }
    stat_bump(&ctx.shared.lite.stats.messages, 1);
    monitor::observe_lite_message(&ctx.shared.monitor, "add", 1);
    // Wake EVERY hub key a blocked reader could be parked on for this
    // append (both use the parent-derived slot prefix, so they agree):
    // the appended child stream's meta key (where XREAD/XREADGROUP on
    // `parent/child` park, via park_wait::wait_targets) AND the bare parent
    // topic's key. Notifying only the child key left anything parked at
    // the parent level asleep until its BLOCK timeout even though data
    // had landed. notify on a key with no waiter is a no-op.
    wait::notify(&ctx.shared.wait_hub, &mkey);
    wait::notify(&ctx.shared.wait_hub, &model::meta_key(&prefix, &parent));

    let id_str = model::format_id(id);
    if auto {
        resp::append_array(ctx.out, 2);
        resp::append_bulk(ctx.out, &stream);
        resp::append_bulk(ctx.out, id_str.as_bytes());
    } else {
        resp::append_bulk(ctx.out, id_str.as_bytes());
    }
}

/// `XRANGE <stream> <start|-|(|..> <end|+|..> [COUNT n]`.
pub async fn xrange(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 || ctx.args.len() > 5 {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xrange' command",
        );
    }
    let count = if ctx.args.len() == 5 && ctx.args[3].eq_ignore_ascii_case(b"COUNT") {
        match std::str::from_utf8(&ctx.args[4])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        {
            Some(n) if n > 0 => n,
            _ => return resp::append_error(ctx.out, "ERR value is not an integer or out of range"),
        }
    } else {
        usize::MAX
    };
    let (start, end) = match (
        model::parse_bound(&ctx.args[1]),
        model::parse_bound(&ctx.args[2]),
    ) {
        (Some(s), Some(e)) => (s, e),
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
    let from = model::entry_key(&prefix, &stream, start.id);
    let mut entries = Vec::new();
    let res = ops::for_each_from(&ctx.shared.store, &from, start.excl, &mut |k, v| {
        if !k.starts_with(&base) {
            return false;
        }
        let Some(id) = id_from_key(&base, k) else {
            return false;
        };
        let past_end = if end.excl { id >= end.id } else { id > end.id };
        if past_end {
            return false;
        }
        if let Some(fields) = model::decode_entry(v) {
            entries.push(Entry { id, fields });
        }
        entries.len() < count
    });
    match res {
        Err(e) => resp::append_error(ctx.out, &format!("ERR: xrange failed: {e}")),
        Ok(()) => {
            resp::append_array(ctx.out, entries.len());
            for e in &entries {
                append_entry_frame(ctx.out, e);
            }
        }
    }
}

/// One XTRIM strategy, parsed ahead of any store access (pure).
enum TrimPlan {
    /// Keep the newest `n` entries.
    MaxLen(u64),
    /// Drop every entry with id strictly below `id`; `limit` caps the
    /// number removed in this call.
    MinId {
        id: model::EntryId,
        limit: Option<u64>,
    },
}

/// Parse `MAXLEN [<~|=>] <n>` | `MINID [<~|=>] <id> [LIMIT <n>]`.
/// The `~` (approximate) and `=` (exact) flags are accepted for wire
/// compatibility but implemented IDENTICALLY: victims are computed
/// precisely, so the approximation never under-deletes. Likewise Redis
/// reserves `LIMIT` for the `~` form only; ours accepts it after both
/// forms with the same semantics -- one less error branch, no
/// behavioral difference.
fn parse_trim(args: &[Vec<u8>]) -> Result<TrimPlan, &'static str> {
    const BAD: &str = "ERR wrong number of arguments for 'xtrim' command";
    let is_flag = |a: &[u8]| a == b"~" || a == b"=";
    if args.len() < 3 || args.len() > 6 {
        return Err(BAD);
    }
    let int_of = |a: &[u8]| -> Option<u64> {
        std::str::from_utf8(a)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
    };
    if args[1].eq_ignore_ascii_case(b"MAXLEN") {
        let n = match &args[2..] {
            [n] => n,
            [m, n] if is_flag(m) => n,
            _ => return Err(BAD),
        };
        return match int_of(n) {
            Some(n) => Ok(TrimPlan::MaxLen(n)),
            None => Err("ERR value is not an integer or out of range"),
        };
    }
    if !args[1].eq_ignore_ascii_case(b"MINID") {
        return Err(BAD);
    }
    let (id_arg, limit) = match &args[2..] {
        [id] => (id, None),
        [m, id] if is_flag(m) => (id, None),
        [id, l, n] if l.eq_ignore_ascii_case(b"LIMIT") => (id, Some(n)),
        [m, id, l, n] if is_flag(m) && l.eq_ignore_ascii_case(b"LIMIT") => (id, Some(n)),
        _ => return Err(BAD),
    };
    let Some(id) = model::parse_id(id_arg) else {
        return Err("ERR Invalid stream ID specified as stream command argument");
    };
    let limit = match limit {
        None => None,
        Some(n) => match int_of(n) {
            Some(n) => Some(n),
            None => return Err("ERR value is not an integer or out of range"),
        },
    };
    Ok(TrimPlan::MinId { id, limit })
}

/// XTRIM/XDEL guard against the kafka committed-offset ledger: a
/// stream with ANY kind-0x20 row has group commits pinning ordinals to
/// the active entry set, and trimming/deleting entries would shift the
/// ordinal<->id map under those readers. A STORE error is not a ledger
/// verdict -- it also rejects (conservative: never trim through a
/// blind spot) but with its own message plus a log line, so operators
/// can tell a guard trip from a store fault. Appends the dedicated
/// error text so operators can tell the guard apart from plain
/// argument failures.
fn ledger_guarded(ctx: &mut Ctx<'_>, prefix: &[u8], stream: &[u8]) -> bool {
    let guarded = match crate::kafka::ledger::has_rows(&ctx.shared.store, prefix, stream) {
        Ok(g) => g,
        Err(e) => {
            eprintln!(
                "[lite] ledger guard read failed on {}: {e}",
                String::from_utf8_lossy(stream)
            );
            resp::append_error(
                ctx.out,
                &format!(
                    "ERR ledger guard read failed for stream {}: {e}",
                    String::from_utf8_lossy(stream)
                ),
            );
            return true;
        }
    };
    if guarded {
        resp::append_error(
            ctx.out,
            &format!(
                "ERR stream {} has committed consumer-group offsets; \
                 destroy the owning consumer groups (XGROUP DESTROY) first",
                String::from_utf8_lossy(stream)
            ),
        );
    }
    guarded
}

/// `XTRIM <stream> MAXLEN [<~|=>] <count>`: drop oldest entries beyond
/// `count`. `XTRIM <stream> MINID [<~|=>] <id> [LIMIT <count>]`: drop
/// every entry older than `id` (a `<ms>-0` id doubles as time-window
/// retention). Returns the number trimmed.
pub async fn xtrim(ctx: &mut Ctx<'_>) {
    let plan = match parse_trim(&ctx.args) {
        Ok(p) => p,
        Err(e) => return resp::append_error(ctx.out, e),
    };
    let Some((stream, prefix)) = stream_of(ctx, 0) else {
        return;
    };
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
    if ledger_guarded(ctx, &prefix, &stream) {
        return;
    }
    let Some(meta) = model::read_meta(
        &ctx.shared.store,
        &prefix,
        &stream,
        Some(ctx.shared.lite.as_ref()),
    )
    .ok()
    .and_then(|r| r.live()) else {
        return resp::append_int(ctx.out, 0);
    };
    let base = model::entry_base(&prefix, &stream);
    let victims = match plan {
        TrimPlan::MaxLen(maxlen) => {
            let trim = meta.len.saturating_sub(maxlen) as usize;
            if trim == 0 {
                return resp::append_int(ctx.out, 0);
            }
            // Cap the preallocation (trim is user-controlled u64) and never
            // walk past this stream's key range: a corrupted/over-counted
            // meta.len must not delete neighbouring keys.
            let mut victims = Vec::with_capacity(trim.min(4096));
            let _ = ops::for_each_from(&ctx.shared.store, &base, false, &mut |k, _| {
                if !k.starts_with(&base) {
                    return false;
                }
                victims.push(k.to_vec());
                victims.len() < trim
            });
            victims
        }
        TrimPlan::MinId { id: minid, limit } => {
            // Entry keys are laid out in id order, so the walk ends at
            // the FIRST id >= minid -- everything before it is a victim
            // by construction (and LIMIT simply stops the batch early,
            // leaving the rest for a later call; the budget is checked
            // BEFORE a victim is taken, so LIMIT 0 trims NOTHING).
            let mut victims = Vec::new();
            let _ = ops::for_each_from(&ctx.shared.store, &base, false, &mut |k, _| {
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
            victims
        }
    };
    let mut batch = WriteBatch::default();
    for k in &victims {
        batch.delete(k);
    }
    let mut next = meta.clone();
    next.len = next.len.saturating_sub(victims.len() as u64);
    let kept_expire = model::current_expire(&ctx.shared.store, &prefix, &stream);
    batch.put(
        model::meta_key(&prefix, &stream),
        model::encode_meta_at(&next, kept_expire),
    );
    match ctx.commit(batch).await {
        Err(e) => resp::append_error(ctx.out, &format!("ERR: xtrim failed: {e}")),
        Ok(()) => resp::append_int(ctx.out, victims.len() as i64),
    }
}

/// `XDEL <stream> <id> [id ...]`: remove specific entries.
pub async fn xdel(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        return resp::append_error(ctx.out, "ERR wrong number of arguments for 'xdel' command");
    }
    let mut ids = Vec::with_capacity(ctx.args.len() - 1);
    for a in &ctx.args[1..] {
        match model::parse_id(a) {
            Some(id) => ids.push(id),
            None => {
                return resp::append_error(
                    ctx.out,
                    "ERR Invalid stream ID specified as stream command argument",
                )
            }
        }
    }
    let Some((stream, prefix)) = stream_of(ctx, 0) else {
        return;
    };
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
    if ledger_guarded(ctx, &prefix, &stream) {
        return;
    }
    let Some(meta) = model::read_meta(
        &ctx.shared.store,
        &prefix,
        &stream,
        Some(ctx.shared.lite.as_ref()),
    )
    .ok()
    .and_then(|r| r.live()) else {
        return resp::append_int(ctx.out, 0);
    };
    // Duplicate ids must count once each (the batch delete is not visible
    // to the physical reads below), else XLEN drifts negative-ish.
    ids.sort_unstable();
    ids.dedup();
    let mut batch = WriteBatch::default();
    let mut found = 0usize;
    for id in &ids {
        let k = model::entry_key(&prefix, &stream, *id);
        if ops::get_physical(&ctx.shared.store, &k)
            .ok()
            .flatten()
            .is_some()
        {
            batch.delete(k);
            found += 1;
        }
    }
    if found > 0 {
        let mut next = meta.clone();
        next.len = next.len.saturating_sub(found as u64);
        let kept_expire = model::current_expire(&ctx.shared.store, &prefix, &stream);
        batch.put(
            model::meta_key(&prefix, &stream),
            model::encode_meta_at(&next, kept_expire),
        );
    }
    match ctx.commit(batch).await {
        Err(e) => resp::append_error(ctx.out, &format!("ERR: xdel failed: {e}")),
        Ok(()) => resp::append_int(ctx.out, found as i64),
    }
}

/// `XIDLE <stream> [<seconds>]`: set/query the idle TTL (0 clears). Uses
/// the uniform envelope + expire index, so the active-expiration loop
/// reclaims the whole stream family when the TTL fires.
pub async fn xidle(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 2 {
        return resp::append_error(ctx.out, "ERR wrong number of arguments for 'xidle' command");
    }
    let Some((stream, prefix)) = stream_of(ctx, 0) else {
        return;
    };
    let mkey = model::meta_key(&prefix, &stream);
    if ctx.args.len() == 1 {
        // Report the CONFIGURED idle seconds from the meta payload: the meta
        // envelope carries no expire (TTLs live in the expire index).
        let secs = match model::read_meta(
            &ctx.shared.store,
            &prefix,
            &stream,
            Some(ctx.shared.lite.as_ref()),
        ) {
            Ok(MetaRead::Live(m)) if m.idle_ms > 0 => m.idle_ms.div_ceil(1000) as i64,
            Ok(MetaRead::Live(_)) => -1,
            _ => -2,
        };
        return resp::append_int(ctx.out, secs);
    }
    let Some(secs) = std::str::from_utf8(&ctx.args[1])
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return resp::append_error(ctx.out, "ERR invalid idle seconds");
    };
    // `secs * 1000` can overflow u64 and wrap toward 0: the stream would
    // get an instant/wrong expiry and the active-expiration loop would
    // reap the whole family. Reject instead of writing a wrapped TTL.
    let Some(idle_ms) = secs.checked_mul(1000) else {
        return resp::append_error(ctx.out, "ERR invalid idle seconds");
    };
    let _guard = latch::lock(&ctx.shared.latch, &mkey).await;
    let read = model::read_meta(
        &ctx.shared.store,
        &prefix,
        &stream,
        Some(ctx.shared.lite.as_ref()),
    );
    let Some(meta) = read.ok().and_then(|r| r.live()) else {
        return resp::append_error(ctx.out, "ERR no such key");
    };
    let old_expire = model::current_expire(&ctx.shared.store, &prefix, &stream);
    let mut next = meta.clone();
    next.idle_ms = idle_ms;
    let new_expire = if idle_ms == 0 {
        0
    } else {
        // now + idle_ms can still overflow near the u64 ceiling even when
        // idle_ms itself is in range: reject rather than arm a wrapped
        // (past) deadline.
        match expire::now_ms().checked_add(idle_ms) {
            Some(deadline) => deadline,
            None => return resp::append_error(ctx.out, "ERR invalid idle seconds"),
        }
    };
    let mut batch = WriteBatch::default();
    batch.put(&mkey, model::encode_meta_at(&next, new_expire));
    expire::set_ttl_entries(&mut batch, &prefix, mkey.clone(), old_expire, new_expire);
    match ctx.commit(batch).await {
        Err(e) => resp::append_error(ctx.out, &format!("ERR: xidle failed: {e}")),
        Ok(()) => resp::append_string(ctx.out, "OK"),
    }
}
