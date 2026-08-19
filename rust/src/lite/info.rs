//! Introspection: `XINFO` (STREAM / GROUPS / TOPICS / LITE / HELP) and the
//! Rust-only `XPICK` queue-selection command.
//!
//! Replies are flat arrays of `field, value` pairs (the RESP codec has no
//! map type); `XINFO TOPICS` and `XINFO LITE` are Lite extensions.

use crate::command::Ctx;
use crate::hash;
use crate::resp::codec as resp;

use super::model::{self, MetaRead};
use super::offset;
use super::select;

fn stream_and_prefix(ctx: &mut Ctx<'_>, i: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    super::entries::stream_of(ctx, i)
}

fn append_id_field(out: &mut Vec<u8>, id: model::EntryId) {
    resp::append_bulk(out, model::format_id(id).as_bytes());
}

/// `XINFO STREAM <stream>`.
fn stream_info(ctx: &mut Ctx<'_>, stream: &[u8], prefix: &[u8]) {
    let meta = match model::read_meta(&ctx.shared.store, prefix, stream) {
        Ok(MetaRead::Live(m)) => m,
        Ok(_) => return resp::append_error(ctx.out, "ERR no such key"),
        Err(e) => return resp::append_error(ctx.out, &format!("ERR: xinfo failed: {e}")),
    };
    let groups = super::group::groups_of(&ctx.shared.store, prefix, stream)
        .map(|g| g.len())
        .unwrap_or(0);
    resp::append_array(ctx.out, 8);
    resp::append_bulk_string(ctx.out, "length");
    resp::append_int(ctx.out, meta.len as i64);
    resp::append_bulk_string(ctx.out, "last-generated-id");
    append_id_field(ctx.out, meta.last_id());
    resp::append_bulk_string(ctx.out, "groups");
    resp::append_int(ctx.out, groups as i64);
    resp::append_bulk_string(ctx.out, "idle-ms");
    resp::append_int(ctx.out, meta.idle_ms as i64);
}

/// `XINFO GROUPS <stream>`: one flat array per group.
fn groups_info(ctx: &mut Ctx<'_>, stream: &[u8], prefix: &[u8]) {
    let groups = match super::group::groups_of(&ctx.shared.store, prefix, stream) {
        Ok(g) => g,
        Err(e) => return resp::append_error(ctx.out, &format!("ERR: xinfo failed: {e}")),
    };
    resp::append_array(ctx.out, groups.len());
    for (name, p) in groups {
        resp::append_array(ctx.out, 6);
        resp::append_bulk(ctx.out, &name);
        resp::append_bulk_string(ctx.out, "last-delivered-id");
        append_id_field(
            ctx.out,
            model::EntryId {
                ms: p.delivered_ms,
                seq: p.delivered_seq,
            },
        );
        resp::append_bulk_string(ctx.out, "committed-id");
        append_id_field(
            ctx.out,
            model::EntryId {
                ms: p.committed_ms,
                seq: p.committed_seq,
            },
        );
    }
}

/// `XINFO TOPICS <parent>`: Lite extension listing `[child, length]` pairs.
fn topics_info(ctx: &mut Ctx<'_>, parent: &[u8]) {
    let prefix = hash::slot_with_prefix(parent).1;
    let children = match select::discover_children(
        &ctx.shared.store,
        &prefix,
        parent,
        select::DEFAULT_LIMIT,
    ) {
        Ok(c) => c,
        Err(e) => return resp::append_error(ctx.out, &format!("ERR: xinfo failed: {e}")),
    };
    resp::append_array(ctx.out, children.len() * 2);
    for child in children {
        let mut stream = parent.to_vec();
        stream.push(b'/');
        stream.extend_from_slice(&child);
        let len = match model::read_meta(&ctx.shared.store, &prefix, &stream) {
            Ok(MetaRead::Live(m)) => m.len as i64,
            _ => 0,
        };
        resp::append_bulk(ctx.out, &child);
        resp::append_int(ctx.out, len);
    }
}

/// `XINFO LITE`: runtime counters (Lite extension).
fn lite_info(ctx: &mut Ctx<'_>) {
    let s = &ctx.shared.lite.stats;
    resp::append_array(ctx.out, 10);
    resp::append_bulk_string(ctx.out, "messages");
    resp::append_int(
        ctx.out,
        s.messages.load(std::sync::atomic::Ordering::Relaxed) as i64,
    );
    resp::append_bulk_string(ctx.out, "acks");
    resp::append_int(
        ctx.out,
        s.acks.load(std::sync::atomic::Ordering::Relaxed) as i64,
    );
    resp::append_bulk_string(ctx.out, "streams-live");
    resp::append_int(
        ctx.out,
        s.streams_live.load(std::sync::atomic::Ordering::Relaxed),
    );
    resp::append_bulk_string(ctx.out, "streams-reaped");
    resp::append_int(
        ctx.out,
        s.streams_reaped.load(std::sync::atomic::Ordering::Relaxed) as i64,
    );
    resp::append_bulk_string(ctx.out, "offset-dirty");
    resp::append_int(ctx.out, offset::dirty_len(&ctx.shared.lite.offsets) as i64);
}

const HELP: &[&str] = &[
    "XINFO STREAM <stream> -- summary of one stream",
    "XINFO GROUPS <stream> -- consumer groups of one stream",
    "XINFO TOPICS <parent> -- queues of a Lite parent topic",
    "XINFO LITE -- Lite runtime counters",
    "No help available for subcommand",
];

/// `XINFO <sub> ...`.
pub async fn xinfo(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        return resp::append_error(ctx.out, "ERR wrong number of arguments for 'xinfo' command");
    }
    let sub = ctx.args[0].to_ascii_lowercase();
    match sub.as_slice() {
        b"help" => {
            resp::append_array(ctx.out, HELP.len() - 1);
            for line in &HELP[..HELP.len() - 1] {
                resp::append_bulk_string(ctx.out, line);
            }
        }
        b"lite" if ctx.args.len() == 1 => lite_info(ctx),
        b"stream" | b"groups" | b"topics" if ctx.args.len() == 2 => {
            let arg = ctx.args[1].clone();
            match sub.as_slice() {
                b"topics" => {
                    if !super::valid_part(&arg) {
                        return resp::append_error(ctx.out, "ERR invalid topic name");
                    }
                    topics_info(ctx, &arg)
                }
                _ => {
                    let Some((stream, prefix)) = stream_and_prefix(ctx, 1) else {
                        return;
                    };
                    if sub.as_slice() == b"stream" {
                        stream_info(ctx, &stream, &prefix)
                    } else {
                        groups_info(ctx, &stream, &prefix)
                    }
                }
            }
        }
        _ => resp::append_error(
            ctx.out,
            &format!(
                "ERR Unknown subcommand or wrong number of arguments for '{}'",
                String::from_utf8_lossy(&ctx.args[0])
            ),
        ),
    }
}

/// `XPICK <parent> <round_robin|hash|least_backlog> [shard]` (Lite only):
/// pure selection -- replies the full `parent/child` stream name.
pub async fn xpick(ctx: &mut Ctx<'_>) {
    if !matches!(ctx.args.len(), 2 | 3) {
        return resp::append_error(ctx.out, "ERR wrong number of arguments for 'xpick' command");
    }
    let parent = ctx.args[0].clone();
    if !super::valid_part(&parent) {
        return resp::append_error(ctx.out, "ERR invalid topic name");
    }
    let Some(strategy) = select::parse_strategy(&ctx.args[1]) else {
        return resp::append_error(
            ctx.out,
            "ERR unknown pick strategy (round_robin|hash|least_backlog)",
        );
    };
    if strategy == select::Strategy::Hash && ctx.args.len() != 3 {
        return resp::append_error(ctx.out, "ERR hash strategy requires a shard key");
    }
    let prefix = hash::slot_with_prefix(&parent).1;
    let children =
        match select::discover_children(&ctx.shared.store, &prefix, &parent, select::DEFAULT_LIMIT)
        {
            Ok(c) => c,
            Err(e) => return resp::append_error(ctx.out, &format!("ERR: xpick failed: {e}")),
        };
    let child = match strategy {
        select::Strategy::RoundRobin => {
            select::pick_round_robin(&ctx.shared.lite.picks, &parent, &children)
        }
        select::Strategy::Hash => select::pick_hash(&children, &ctx.args[2]).unwrap_or_else(|| {
            select::pick_round_robin(&ctx.shared.lite.picks, &parent, &children)
        }),
        select::Strategy::LeastBacklog => {
            select::pick_least_backlog(&ctx.shared.store, &prefix, &parent, &children)
        }
    };
    let mut stream = parent;
    stream.push(b'/');
    stream.extend_from_slice(&child);
    resp::append_bulk(ctx.out, &stream);
}
