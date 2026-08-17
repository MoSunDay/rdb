//! Producing side of Lite streams: XADD / XLEN / XRANGE / XTRIM / XDEL /
//! XIDLE. Every meta-mutating write runs under the per-stream latch and
//! lands in ONE batched fsync (meta + entry + TTL index together).

use std::sync::atomic::Ordering;
use std::sync::Arc;

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
            let kids = select::discover_children(&ctx.shared.store, &prefix0, p, 64);
            let c = select::pick_round_robin(&ctx.shared.lite.picks, p, &kids);
            (p.clone(), c, true)
        }
    };
    let mut stream = parent.clone();
    stream.push(b'/');
    stream.extend_from_slice(&child);
    let prefix = hash::slot_with_prefix(&parent).1;
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream));

    let now = expire::now_ms();
    let read = model::read_meta(&ctx.shared.store, &prefix, &stream);
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
    let id = if id_arg == b"*" {
        model::auto_id((!fresh).then_some(meta.last_id()), now)
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
    // Every append retouches the idle deadline (idle = no writes).
    let new_expire = if next.idle_ms == 0 {
        0
    } else {
        now.max(id.ms) + next.idle_ms
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

    if let Err(e) = ops::batch_write_async(Arc::clone(&ctx.shared.store), batch).await {
        return resp::append_error(ctx.out, &format!("ERR: xadd failed: {e}"));
    }
    if fresh {
        ctx.shared
            .lite
            .stats
            .streams_live
            .fetch_add(1, Ordering::Relaxed);
        offset::remove_stream(&ctx.shared.lite.offsets, &String::from_utf8_lossy(&stream));
    }
    stat_bump(&ctx.shared.lite.stats.messages, 1);
    monitor::observe_lite_message(&ctx.shared.monitor, "add", 1);
    wait::notify(&ctx.shared.wait_hub, &mkey);

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

/// `XTRIM <stream> MAXLEN [<~|=>] <count>`: drop oldest entries beyond
/// `count`. Returns the number trimmed.
pub async fn xtrim(ctx: &mut Ctx<'_>) {
    let bad = "ERR wrong number of arguments for 'xtrim' command";
    if ctx.args.len() < 3 || ctx.args.len() > 5 || !ctx.args[1].eq_ignore_ascii_case(b"MAXLEN") {
        return resp::append_error(ctx.out, bad);
    }
    let tail = &ctx.args[2..];
    let maxlen = match tail {
        [n] => n,
        [m, n] if m == b"~" || m == b"=" => n,
        _ => return resp::append_error(ctx.out, bad),
    };
    let Some(maxlen) = std::str::from_utf8(maxlen)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return resp::append_error(ctx.out, "ERR value is not an integer or out of range");
    };
    let Some((stream, prefix)) = stream_of(ctx, 0) else {
        return;
    };
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream));
    let Some(meta) = model::read_meta(&ctx.shared.store, &prefix, &stream)
        .ok()
        .and_then(|r| r.live())
    else {
        return resp::append_int(ctx.out, 0);
    };
    let trim = meta.len.saturating_sub(maxlen) as usize;
    if trim == 0 {
        return resp::append_int(ctx.out, 0);
    }
    let base = model::entry_base(&prefix, &stream);
    let mut victims = Vec::with_capacity(trim);
    let _ = ops::for_each_from(&ctx.shared.store, &base, false, &mut |k, _| {
        victims.push(k.to_vec());
        victims.len() < trim
    });
    let mut batch = WriteBatch::default();
    for k in &victims {
        batch.delete(k);
    }
    let mut next = meta.clone();
    next.len -= victims.len() as u64;
    let kept_expire = model::current_expire(&ctx.shared.store, &prefix, &stream);
    batch.put(
        model::meta_key(&prefix, &stream),
        model::encode_meta_at(&next, kept_expire),
    );
    match ops::batch_write_async(Arc::clone(&ctx.shared.store), batch).await {
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
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream));
    let Some(meta) = model::read_meta(&ctx.shared.store, &prefix, &stream)
        .ok()
        .and_then(|r| r.live())
    else {
        return resp::append_int(ctx.out, 0);
    };
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
        next.len -= found as u64;
        let kept_expire = model::current_expire(&ctx.shared.store, &prefix, &stream);
        batch.put(
            model::meta_key(&prefix, &stream),
            model::encode_meta_at(&next, kept_expire),
        );
    }
    match ops::batch_write_async(Arc::clone(&ctx.shared.store), batch).await {
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
        let secs = match model::read_meta(&ctx.shared.store, &prefix, &stream) {
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
    let _guard = latch::lock(&ctx.shared.latch, &mkey);
    let read = model::read_meta(&ctx.shared.store, &prefix, &stream);
    let Some(meta) = read.ok().and_then(|r| r.live()) else {
        return resp::append_error(ctx.out, "ERR no such key");
    };
    let old_expire = model::current_expire(&ctx.shared.store, &prefix, &stream);
    let mut next = meta.clone();
    next.idle_ms = secs * 1000;
    let new_expire = if next.idle_ms == 0 {
        0
    } else {
        expire::now_ms() + next.idle_ms
    };
    let mut batch = WriteBatch::default();
    batch.put(&mkey, model::encode_meta_at(&next, new_expire));
    expire::set_ttl_entries(&mut batch, &prefix, mkey.clone(), old_expire, new_expire);
    match ops::batch_write_async(Arc::clone(&ctx.shared.store), batch).await {
        Err(e) => resp::append_error(ctx.out, &format!("ERR: xidle failed: {e}")),
        Ok(()) => resp::append_string(ctx.out, "OK"),
    }
}
