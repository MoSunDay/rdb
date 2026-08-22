//! Command registry and dispatch (Go: internal/command/command.go).
//!
//! Handlers are free functions over `Ctx`; responses are appended to `out`
//! as RESP frames via `crate::resp::codec` helpers. Handlers never touch the
//! socket directly; `quit` asks for connection close via `close_conn`.

pub mod cluster;
pub mod hash_cmd;
pub mod hash_incr;
pub mod hash_scan;
pub mod json_arr;
pub mod json_cmd;
pub mod json_obj;
pub mod json_path;
pub mod json_str;
pub mod keys;
pub mod keys_core;
pub mod keys_scan;
pub mod list_block;
pub mod list_cmd;
pub mod list_move;
pub mod list_ops;
pub mod list_rewrite;
pub mod migrate;
pub mod raft_cmd;
pub mod set_cmd;
pub mod set_scan;
pub mod setops_cmd;
pub mod string;
pub mod string_opts;
pub mod tx_cmd;
pub mod vectorset_attr;
pub mod vectorset_cmd;
pub mod vectorset_sim;
pub mod zset_block;
pub mod zset_cmd;
pub mod zset_pop;
pub mod zset_range;
pub mod zset_read;
pub mod zset_remops;
pub mod zset_scan;
pub mod zset_util;
pub mod zsetops_cmd;

use std::panic::AssertUnwindSafe;

use futures::FutureExt;

use crate::hash;
use crate::resp::codec;
use crate::router;
use crate::state;
use crate::tx::session::ConnState;

/// Per-command execution context (Go rtypes.CommandContext).
pub struct Ctx<'a> {
    pub shared: &'a state::Shared,
    /// "<decimal-slot>/" prefix computed from the (hash-tagged) first key;
    /// empty for whitelist commands.
    pub prefix_key: Vec<u8>,
    /// argv minus the command name.
    pub args: Vec<Vec<u8>>,
    /// RESP response buffer.
    pub out: &'a mut Vec<u8>,
    /// Set by `quit`; the connection layer closes after flushing.
    pub close_conn: bool,
    /// Connection transaction state (MULTI queue / WATCHes); handlers of
    /// the transaction control commands mutate it, everything else only
    /// reads it.
    pub conn: &'a mut ConnState,
    /// Whether the command performed a write; the connection layer uses
    /// this to implicitly UNWATCH after writes outside MULTI.
    pub wrote: bool,
}

impl<'a> Ctx<'a> {
    /// Commit a write batch through the async fsync path. All command
    /// handlers write through this method (never `ops::batch_write_async`
    /// directly) so the write is visible to the implicit-UNWATCH rule and,
    /// during EXEC replays, is covered by the transaction's latches.
    pub async fn commit(&mut self, batch: rocksdb::WriteBatch) -> Result<(), String> {
        self.wrote = true;
        crate::store::ops::batch_write_async(std::sync::Arc::clone(&self.shared.store), batch).await
    }
}

/// Boxed handler future: handlers are async because write commands await
/// off-worker fsyncs (see `store::set_async` and friends).
pub type HandlerFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
// `Ctx<'_>` (not `Ctx<'a>`) keeps the ctx's internal lifetime decoupled from
// the future's borrow: `&'a mut Ctx<'a>` is invariant and would expire the
// ctx for the post-await panic reply in `resp::conn`.
pub type Handler = for<'a> fn(&'a mut Ctx<'_>) -> HandlerFuture<'a>;

/// Go CommandHander map; names matched lowercase by the caller.
pub fn lookup(name: &str) -> Option<Handler> {
    match name {
        "ping" => Some(|ctx| Box::pin(string::ping(ctx))),
        "quit" => Some(|ctx| Box::pin(string::quit(ctx))),
        "get" => Some(|ctx| Box::pin(string::get(ctx))),
        "set" => Some(|ctx| Box::pin(string::set(ctx))),
        "del" => Some(|ctx| Box::pin(keys::del(ctx))),
        "unlink" => Some(|ctx| Box::pin(keys::del(ctx))),
        "exists" => Some(|ctx| Box::pin(keys::exists(ctx))),
        "type" => Some(|ctx| Box::pin(keys::type_(ctx))),
        "expire" => Some(|ctx| Box::pin(keys::expire(ctx))),
        "pexpire" => Some(|ctx| Box::pin(keys::pexpire(ctx))),
        "expireat" => Some(|ctx| Box::pin(keys::expireat(ctx))),
        "pexpireat" => Some(|ctx| Box::pin(keys::pexpireat(ctx))),
        "ttl" => Some(|ctx| Box::pin(keys::ttl(ctx))),
        "pttl" => Some(|ctx| Box::pin(keys::pttl(ctx))),
        "persist" => Some(|ctx| Box::pin(keys::persist(ctx))),
        "scan" => Some(|ctx| Box::pin(keys::scan(ctx))),
        "keys" => Some(|ctx| Box::pin(keys::keys_cmd(ctx))),
        "randomkey" => Some(|ctx| Box::pin(keys::randomkey(ctx))),
        "rename" => Some(|ctx| Box::pin(keys::rename(ctx))),
        "renamenx" => Some(|ctx| Box::pin(keys::renamenx(ctx))),
        "hset" => Some(|ctx| Box::pin(hash_cmd::hset(ctx))),
        "hsetnx" => Some(|ctx| Box::pin(hash_cmd::hsetnx(ctx))),
        "hget" => Some(|ctx| Box::pin(hash_cmd::hget(ctx))),
        "hmget" => Some(|ctx| Box::pin(hash_cmd::hmget(ctx))),
        "hdel" => Some(|ctx| Box::pin(hash_cmd::hdel(ctx))),
        "hlen" => Some(|ctx| Box::pin(hash_cmd::hlen(ctx))),
        "hexists" => Some(|ctx| Box::pin(hash_cmd::hexists(ctx))),
        "hstrlen" => Some(|ctx| Box::pin(hash_cmd::hstrlen(ctx))),
        "hgetall" => Some(|ctx| Box::pin(hash_scan::hgetall(ctx))),
        "hkeys" => Some(|ctx| Box::pin(hash_scan::hkeys(ctx))),
        "hvals" => Some(|ctx| Box::pin(hash_scan::hvals(ctx))),
        "hincrby" => Some(|ctx| Box::pin(hash_incr::hincrby(ctx))),
        "hincrbyfloat" => Some(|ctx| Box::pin(hash_incr::hincrbyfloat(ctx))),
        "hrandfield" => Some(|ctx| Box::pin(hash_scan::hrandfield(ctx))),
        "hscan" => Some(|ctx| Box::pin(hash_scan::hscan(ctx))),
        "sadd" => Some(|ctx| Box::pin(set_cmd::sadd(ctx))),
        "srem" => Some(|ctx| Box::pin(set_cmd::srem(ctx))),
        "smembers" => Some(|ctx| Box::pin(set_cmd::smembers(ctx))),
        "sismember" => Some(|ctx| Box::pin(set_cmd::sismember(ctx))),
        "smismember" => Some(|ctx| Box::pin(set_cmd::smismember(ctx))),
        "scard" => Some(|ctx| Box::pin(set_cmd::scard(ctx))),
        "spop" => Some(|ctx| Box::pin(set_cmd::spop(ctx))),
        "srandmember" => Some(|ctx| Box::pin(set_scan::srandmember(ctx))),
        "smove" => Some(|ctx| Box::pin(set_cmd::smove(ctx))),
        "sscan" => Some(|ctx| Box::pin(set_scan::sscan(ctx))),
        "sdiff" => Some(|ctx| Box::pin(setops_cmd::sdiff(ctx))),
        "sdiffstore" => Some(|ctx| Box::pin(setops_cmd::sdiffstore(ctx))),
        "sinter" => Some(|ctx| Box::pin(setops_cmd::sinter(ctx))),
        "sinterstore" => Some(|ctx| Box::pin(setops_cmd::sinterstore(ctx))),
        "sunion" => Some(|ctx| Box::pin(setops_cmd::sunion(ctx))),
        "sunionstore" => Some(|ctx| Box::pin(setops_cmd::sunionstore(ctx))),
        "mget" => Some(|ctx| Box::pin(string::mget(ctx))),
        "mset" => Some(|ctx| Box::pin(string::mset(ctx))),
        "config" => Some(|ctx| Box::pin(string::config(ctx))),
        "cluster" => Some(|ctx| Box::pin(cluster::handle(ctx))),
        "raft" => Some(|ctx| Box::pin(raft_cmd::handle(ctx))),
        "migrate" => Some(|ctx| Box::pin(migrate::handle(ctx))),
        "xadd" => Some(|ctx| Box::pin(crate::lite::append::xadd(ctx))),
        "xlen" => Some(|ctx| Box::pin(crate::lite::read::xlen(ctx))),
        "xrange" => Some(|ctx| Box::pin(crate::lite::append::xrange(ctx))),
        "xtrim" => Some(|ctx| Box::pin(crate::lite::append::xtrim(ctx))),
        "xdel" => Some(|ctx| Box::pin(crate::lite::append::xdel(ctx))),
        "xidle" => Some(|ctx| Box::pin(crate::lite::append::xidle(ctx))),
        "xread" => Some(|ctx| Box::pin(crate::lite::read::xread(ctx))),
        "xreadgroup" => Some(|ctx| Box::pin(crate::lite::read::xreadgroup(ctx))),
        "xack" => Some(|ctx| Box::pin(crate::lite::ack::xack(ctx))),
        "xgroup" => Some(|ctx| Box::pin(crate::lite::group::xgroup(ctx))),
        "xinfo" => Some(|ctx| Box::pin(crate::lite::info::xinfo(ctx))),
        "xpending" => Some(|ctx| Box::pin(crate::lite::pending::xpending(ctx))),
        "xclaim" => Some(|ctx| Box::pin(crate::lite::claim::xclaim(ctx))),
        "xautoclaim" => Some(|ctx| Box::pin(crate::lite::autoclaim::xautoclaim(ctx))),
        "xpick" => Some(|ctx| Box::pin(crate::lite::info::xpick(ctx))),
        // List family: blocking pops (list_block), moves + LINSERT/LPOS
        // (list_move), reads/writes (list_cmd), pops (list_ops) and
        // LREM/LTRIM rewrites (list_rewrite).
        "blpop" => Some(|ctx| Box::pin(list_block::blpop(ctx))),
        "blmove" => Some(|ctx| Box::pin(list_block::blmove(ctx))),
        "brpop" => Some(|ctx| Box::pin(list_block::brpop(ctx))),
        "brpoplpush" => Some(|ctx| Box::pin(list_block::brpoplpush(ctx))),
        "lindex" => Some(|ctx| Box::pin(list_cmd::lindex(ctx))),
        "linsert" => Some(|ctx| Box::pin(list_move::linsert(ctx))),
        "llen" => Some(|ctx| Box::pin(list_cmd::llen(ctx))),
        "lmove" => Some(|ctx| Box::pin(list_move::lmove(ctx))),
        "lpop" => Some(|ctx| Box::pin(list_ops::lpop(ctx))),
        "lpos" => Some(|ctx| Box::pin(list_move::lpos(ctx))),
        "lpush" => Some(|ctx| Box::pin(list_cmd::lpush(ctx))),
        "lpushx" => Some(|ctx| Box::pin(list_cmd::lpushx(ctx))),
        "lrange" => Some(|ctx| Box::pin(list_cmd::lrange(ctx))),
        "lrem" => Some(|ctx| Box::pin(list_rewrite::lrem(ctx))),
        "lset" => Some(|ctx| Box::pin(list_cmd::lset(ctx))),
        "ltrim" => Some(|ctx| Box::pin(list_rewrite::ltrim(ctx))),
        "rpop" => Some(|ctx| Box::pin(list_ops::rpop(ctx))),
        "rpoplpush" => Some(|ctx| Box::pin(list_move::rpoplpush(ctx))),
        "rpush" => Some(|ctx| Box::pin(list_cmd::rpush(ctx))),
        "rpushx" => Some(|ctx| Box::pin(list_cmd::rpushx(ctx))),
        // Sorted-set family: writes + shared state helpers (zset_cmd),
        // point reads (zset_read), removals/pops (zset_pop) and the
        // ZRANGE family (zset_range).
        "zadd" => Some(|ctx| Box::pin(zset_cmd::zadd(ctx))),
        "zincrby" => Some(|ctx| Box::pin(zset_cmd::zincrby(ctx))),
        "zcard" => Some(|ctx| Box::pin(zset_read::zcard(ctx))),
        "zscore" => Some(|ctx| Box::pin(zset_read::zscore(ctx))),
        "zmscore" => Some(|ctx| Box::pin(zset_read::zmscore(ctx))),
        "zcount" => Some(|ctx| Box::pin(zset_read::zcount(ctx))),
        "zrank" => Some(|ctx| Box::pin(zset_read::zrank(ctx))),
        "zrevrank" => Some(|ctx| Box::pin(zset_read::zrevrank(ctx))),
        "zrandmember" => Some(|ctx| Box::pin(zset_read::zrandmember(ctx))),
        "zrem" => Some(|ctx| Box::pin(zset_pop::zrem(ctx))),
        "zpopmin" => Some(|ctx| Box::pin(zset_pop::zpopmin(ctx))),
        "zpopmax" => Some(|ctx| Box::pin(zset_pop::zpopmax(ctx))),
        "zrange" => Some(|ctx| Box::pin(zset_range::zrange(ctx))),
        "zrangebyscore" => Some(|ctx| Box::pin(zset_range::zrangebyscore(ctx))),
        "zrevrangebyscore" => Some(|ctx| Box::pin(zset_range::zrevrangebyscore(ctx))),
        "zrangebylex" => Some(|ctx| Box::pin(zset_range::zrangebylex(ctx))),
        "zrevrangebylex" => Some(|ctx| Box::pin(zset_range::zrevrangebylex(ctx))),
        "zlexcount" => Some(|ctx| Box::pin(zset_range::zlexcount(ctx))),
        // Range removals (zset_remops), cursor iteration (zset_scan),
        // multi-key algebra (zsetops_cmd) and blocking pops (zset_block).
        "zremrangebyrank" => Some(|ctx| Box::pin(zset_remops::zremrangebyrank(ctx))),
        "zremrangebyscore" => Some(|ctx| Box::pin(zset_remops::zremrangebyscore(ctx))),
        "zremrangebylex" => Some(|ctx| Box::pin(zset_remops::zremrangebylex(ctx))),
        "zscan" => Some(|ctx| Box::pin(zset_scan::zscan(ctx))),
        "zunionstore" => Some(|ctx| Box::pin(zsetops_cmd::zunionstore(ctx))),
        "zinterstore" => Some(|ctx| Box::pin(zsetops_cmd::zinterstore(ctx))),
        "zdiffstore" => Some(|ctx| Box::pin(zsetops_cmd::zdiffstore(ctx))),
        "bzpopmin" => Some(|ctx| Box::pin(zset_block::bzpopmin(ctx))),
        "bzpopmax" => Some(|ctx| Box::pin(zset_block::bzpopmax(ctx))),
        // JSON documents (json_cmd/json_str/json_arr/json_obj).
        "json.set" => Some(|ctx| Box::pin(json_cmd::json_set(ctx))),
        "json.get" => Some(|ctx| Box::pin(json_cmd::json_get(ctx))),
        "json.del" => Some(|ctx| Box::pin(json_cmd::json_del(ctx))),
        "json.forget" => Some(|ctx| Box::pin(json_cmd::json_forget(ctx))),
        "json.type" => Some(|ctx| Box::pin(json_cmd::json_type(ctx))),
        "json.mget" => Some(|ctx| Box::pin(json_cmd::json_mget(ctx))),
        "json.strappend" => Some(|ctx| Box::pin(json_str::json_strappend(ctx))),
        "json.strlen" => Some(|ctx| Box::pin(json_str::json_strlen(ctx))),
        "json.numincrby" => Some(|ctx| Box::pin(json_str::json_numincrby(ctx))),
        "json.arrappend" => Some(|ctx| Box::pin(json_arr::json_arrappend(ctx))),
        "json.arrpop" => Some(|ctx| Box::pin(json_arr::json_arrpop(ctx))),
        "json.arrindex" => Some(|ctx| Box::pin(json_arr::json_arrindex(ctx))),
        "json.arrinsert" => Some(|ctx| Box::pin(json_arr::json_arrinsert(ctx))),
        "json.arrlen" => Some(|ctx| Box::pin(json_arr::json_arrlen(ctx))),
        "json.arrtrim" => Some(|ctx| Box::pin(json_arr::json_arrtrim(ctx))),
        "json.objkeys" => Some(|ctx| Box::pin(json_obj::json_objkeys(ctx))),
        "json.objlen" => Some(|ctx| Box::pin(json_obj::json_objlen(ctx))),
        // Vector sets (vectorset_cmd/vectorset_attr/vectorset_sim).
        "vadd" => Some(|ctx| Box::pin(vectorset_cmd::vadd(ctx))),
        "vrem" => Some(|ctx| Box::pin(vectorset_cmd::vrem(ctx))),
        "vcard" => Some(|ctx| Box::pin(vectorset_cmd::vcard(ctx))),
        "vdim" => Some(|ctx| Box::pin(vectorset_cmd::vdim(ctx))),
        "vsetattr" => Some(|ctx| Box::pin(vectorset_attr::vsetattr(ctx))),
        "vgetattr" => Some(|ctx| Box::pin(vectorset_attr::vgetattr(ctx))),
        "vsim" => Some(|ctx| Box::pin(vectorset_sim::vsim(ctx))),
        // Search engine (search::ft_cmd / search::ft_search): the index
        // key is argv[1], so cluster routing/keyspec defaults apply.
        "ft.create" => Some(|ctx| Box::pin(crate::search::ft_cmd::ft_create(ctx))),
        "ft.add" => Some(|ctx| Box::pin(crate::search::ft_cmd::ft_add(ctx))),
        "ft.del" => Some(|ctx| Box::pin(crate::search::ft_cmd::ft_del(ctx))),
        "ft.drop" => Some(|ctx| Box::pin(crate::search::ft_cmd::ft_drop(ctx))),
        "ft.dropindex" => Some(|ctx| Box::pin(crate::search::ft_cmd::ft_drop(ctx))),
        "ft.build" => Some(|ctx| Box::pin(crate::search::ft_cmd::ft_build(ctx))),
        "ft.info" => Some(|ctx| Box::pin(crate::search::ft_cmd::ft_info(ctx))),
        "ft.search" => Some(|ctx| Box::pin(crate::search::ft_search::ft_search(ctx))),
        "multi" => Some(|ctx| Box::pin(tx_cmd::multi(ctx))),
        "exec" => Some(|ctx| Box::pin(tx_cmd::exec(ctx))),
        "discard" => Some(|ctx| Box::pin(tx_cmd::discard(ctx))),
        "watch" => Some(|ctx| Box::pin(tx_cmd::watch(ctx))),
        "unwatch" => Some(|ctx| Box::pin(tx_cmd::unwatch(ctx))),
        _ => None,
    }
}

/// MOVED reply line for a foreign slot, or `None` when the slot is local.
pub(crate) fn moved_line(shared: &state::Shared, slot: u16) -> Option<String> {
    let decision = {
        let topo = shared.topology.read().unwrap();
        router::route(
            slot,
            &topo.stable_addrs,
            topo.per_node_slots,
            &shared.conf.bind,
        )
    };
    match decision {
        router::RouteDecision::Moved { slot, addr } => Some(router::moved_error_line(slot, &addr)),
        _ => None,
    }
}

fn arity_error(out: &mut Vec<u8>, cmd: &str) {
    codec::append_error(
        out,
        &format!("ERR wrong number of arguments for '{cmd}' command"),
    );
}

fn observe(shared: &state::Shared, cmd: &str, is_moved: bool, start: std::time::Instant) {
    crate::monitor::observe_latency(
        &shared.monitor,
        state::mode_label(shared.mode),
        cmd,
        is_moved,
        start.elapsed().as_millis() as f64,
    );
}

/// Execute one parsed command post-auth: lookup, slot routing, handler
/// under the panic net, latency metrics (Go `server.go` dispatch pipeline;
/// hoisted here so EXEC replays run the exact same path).
///
/// Returns whether the command performed a write (`Ctx::wrote`) -- the
/// connection layer unwatches on writes outside MULTI.
pub(crate) async fn dispatch(
    shared: &state::Shared,
    mut argv: Vec<Vec<u8>>,
    conn: &mut ConnState,
    out: &mut Vec<u8>,
    close: &mut bool,
) -> bool {
    let raw0 = String::from_utf8_lossy(&argv[0]);
    let first = raw0.to_lowercase();
    let start = std::time::Instant::now();

    let handler = match lookup(&first) {
        Some(h) => h,
        // Go: `ERR unknown command '<original case>'`; no latency observed.
        None => {
            codec::append_error(out, &format!("ERR unknown command '{raw0}'"));
            return false;
        }
    };

    // Slot routing for non-whitelisted commands.
    let mut prefix_key: Vec<u8> = Vec::new();
    if !router::is_whitelisted(&first) {
        // BREAKING (approved): Go indexed cmd.Args[1] unconditionally, so a
        // lone command name surfaced as a fabricated runtime-panic reply;
        // use the Redis-standard arity error instead. No latency sample on
        // this error path (Go observes only after the handler returns).
        if argv.len() < 2 {
            arity_error(out, &first);
            return false;
        }
        let tag = hash::hash_tag(&argv[1]);
        let (slot, prefix) = hash::slot_with_prefix(tag);
        prefix_key = prefix;
        if let Some(line) = moved_line(shared, slot) {
            codec::append_error(out, &line);
            observe(shared, &first, true, start);
            return false;
        }
    }

    // Run the handler behind Go's `defer recover()` safety net.
    let mut ctx = Ctx {
        shared,
        prefix_key,
        args: argv.split_off(1),
        out,
        close_conn: false,
        conn,
        wrote: false,
    };
    let panicked = {
        AssertUnwindSafe(handler(&mut ctx))
            .catch_unwind()
            .await
            .err()
    };
    match panicked {
        Some(payload) => {
            codec::append_error(
                ctx.out,
                &format!("fatal error: {}", panic_payload(&payload)),
            );
            // Reply text is unchanged, but the connection now closes after
            // the flush: a panicked handler may have desynced the framing,
            // so keep talking on it is unsafe.
            *close = true;
        }
        // Label order mirrors Go: (mode, lowercase command, was-MOVED). Go
        // observes after `fn(...)` returns; a panicking handler unwinds past
        // it, so no late sample is recorded on this error path.
        None => observe(shared, &first, false, start),
    }

    if ctx.close_conn {
        // Go `quit` writes its replies then closes the connection; the flush
        // happens in the caller before we return.
        *close = true;
    }
    ctx.wrote
}

/// Panic payload -> human text (Go prints `%v` of the recovered value).
fn panic_payload(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(n) = payload.downcast_ref::<i32>() {
        n.to_string()
    } else {
        "unknown panic payload".to_string()
    }
}

#[cfg(test)]
pub(crate) fn test_ctx<'a>(
    shared: &'a state::Shared,
    prefix_key: Vec<u8>,
    args: Vec<Vec<u8>>,
    out: &'a mut Vec<u8>,
) -> Ctx<'a> {
    // Tests never drive MULTI state; a leaked default is fine (one
    // small struct per call, test-only).
    let leaked: &'a mut ConnState = Box::leak(Box::new(ConnState::default()));
    test_ctx_with_conn(shared, prefix_key, args, out, leaked)
}

/// [`test_ctx`] with explicit per-connection transaction state (the
/// transaction-command tests drive a real session through it).
#[cfg(test)]
pub(crate) fn test_ctx_with_conn<'a>(
    shared: &'a state::Shared,
    prefix_key: Vec<u8>,
    args: Vec<Vec<u8>>,
    out: &'a mut Vec<u8>,
    conn: &'a mut ConnState,
) -> Ctx<'a> {
    Ctx {
        shared,
        prefix_key,
        args,
        out,
        close_conn: false,
        conn,
        wrote: false,
    }
}

#[cfg(test)]
#[path = "hash_tests.rs"]
mod hash_tests;

#[cfg(test)]
#[path = "set_tests.rs"]
mod set_tests;

#[cfg(test)]
#[path = "json_arr_tests.rs"]
mod json_arr_tests;
#[cfg(test)]
#[path = "json_tests.rs"]
mod json_tests;

#[cfg(test)]
#[path = "vectorset_tests.rs"]
mod vectorset_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_payload_strings_and_ints() {
        fn as_any(b: impl 'static + Send) -> Box<dyn std::any::Any + Send> {
            Box::new(b)
        }
        assert_eq!(panic_payload(&as_any("boom".to_string())), "boom");
        assert_eq!(panic_payload(&as_any("bang")), "bang");
        assert_eq!(panic_payload(&as_any(7_i32)), "7");
        assert_eq!(panic_payload(&as_any(true)), "unknown panic payload");
    }
}
