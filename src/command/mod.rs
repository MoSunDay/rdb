//! Command registry and dispatch (Go: internal/command/command.go).
//!
//! Handlers are free functions over `Ctx`; responses are appended to `out`
//! as RESP frames via `crate::resp::codec` helpers. Handlers never touch the
//! socket directly; `quit` asks for connection close via `close_conn`.

pub mod bitops;
pub mod cluster;
pub mod cluster_slot;
pub mod cmd_meta;
pub mod flushdb;
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
pub mod keyspace_role;
pub mod list_block;
pub mod list_cmd;
pub mod list_move;
pub mod list_mpop;
pub mod list_ops;
pub mod list_rewrite;
pub mod migrate;
pub mod raft_cmd;
pub mod readonly;
pub mod server_cmd;
pub mod set_cmd;
pub mod set_scan;
pub mod setops_cmd;
pub mod string;
pub mod string_incr;
pub mod string_opts;
pub mod string_rw;
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
        "asking" => Some(|ctx| Box::pin(cluster::asking(ctx))),
        "restore" => Some(|ctx| Box::pin(migrate::restore(ctx))),
        "get" => Some(|ctx| Box::pin(string::get(ctx))),
        "set" => Some(|ctx| Box::pin(string::set(ctx))),
        "incr" => Some(|ctx| Box::pin(string_incr::incr(ctx))),
        "decr" => Some(|ctx| Box::pin(string_incr::decr(ctx))),
        "incrby" => Some(|ctx| Box::pin(string_incr::incrby(ctx))),
        "decrby" => Some(|ctx| Box::pin(string_incr::decrby(ctx))),
        "incrbyfloat" => Some(|ctx| Box::pin(string_incr::incrbyfloat(ctx))),
        "append" => Some(|ctx| Box::pin(string_rw::append(ctx))),
        "strlen" => Some(|ctx| Box::pin(string_rw::strlen(ctx))),
        "getset" => Some(|ctx| Box::pin(string_rw::getset(ctx))),
        "setnx" => Some(|ctx| Box::pin(string_rw::setnx(ctx))),
        "setex" => Some(|ctx| Box::pin(string_rw::setex(ctx))),
        "psetex" => Some(|ctx| Box::pin(string_rw::psetex(ctx))),
        "getdel" => Some(|ctx| Box::pin(string_rw::getdel(ctx))),
        "setrange" => Some(|ctx| Box::pin(string_rw::setrange(ctx))),
        "getrange" => Some(|ctx| Box::pin(string_rw::getrange(ctx))),
        "setbit" => Some(|ctx| Box::pin(bitops::setbit(ctx))),
        "getbit" => Some(|ctx| Box::pin(bitops::getbit(ctx))),
        "bitcount" => Some(|ctx| Box::pin(bitops::bitcount(ctx))),
        "bitpos" => Some(|ctx| Box::pin(bitops::bitpos(ctx))),
        "bitop" => Some(|ctx| Box::pin(bitops::bitop(ctx))),
        "command" => Some(|ctx| Box::pin(server_cmd::command(ctx))),
        "info" => Some(|ctx| Box::pin(server_cmd::info(ctx))),
        "dbsize" => Some(|ctx| Box::pin(server_cmd::dbsize(ctx))),
        "echo" => Some(|ctx| Box::pin(server_cmd::echo(ctx))),
        "select" => Some(|ctx| Box::pin(server_cmd::select(ctx))),
        "flushdb" => Some(|ctx| Box::pin(flushdb::flushdb(ctx))),
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
        "hmset" => Some(|ctx| Box::pin(hash_cmd::hmset(ctx))),
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
        "sintercard" => Some(|ctx| Box::pin(setops_cmd::sintercard(ctx))),
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
        "xrevrange" => Some(|ctx| Box::pin(crate::lite::range_rev::xrevrange(ctx))),
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
        "lmpop" => Some(|ctx| Box::pin(list_mpop::lmpop(ctx))),
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
        "zrevrange" => Some(|ctx| Box::pin(zset_range::zrevrange(ctx))),
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

/// Redirect line for one routed command, or `None` when this node serves it.
///
/// Redis cluster semantics, in order:
/// 1. Slot in this node's IMPORTING table: only an ASKING connection is
///    served; anyone else is redirected `MOVED` back to the source node.
/// 2. Normal ownership routing: the raft `slot_owner_map` first, then the
///    equal-split band. Foreign slots redirect `MOVED`.
/// 3. Slot in this node's MIGRATING table and the key absent locally: the
///    data already moved (or never existed) -> `ASK` to the destination.
pub(crate) fn redirect_line(
    shared: &state::Shared,
    slot: u16,
    key: &[u8],
    asking: bool,
) -> Option<String> {
    // 1. IMPORTING gate (before ownership routing: the importing node may
    // not own the slot in any routing table yet).
    // These routing locks are read on EVERY cross-node command, OUTSIDE the
    // handler panic net: a poisoned lock (a control-plane writer panicked
    // while holding it) must not cascade-panic the connection task, and the
    // guarded data is still structurally valid -- so recover, never unwrap.
    let importing = shared
        .importing
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(src) = importing.get(&slot) {
        if !asking {
            return Some(router::moved_error_line(slot, src));
        }
        return None;
    }
    drop(importing);
    // 2. ownership routing with per-slot overrides.
    let decision = {
        let topo = shared
            .topology
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        router::route_with_owners(
            slot,
            &topo.stable_addrs,
            topo.per_node_slots,
            &shared.conf.bind,
            &topo.owner_map,
        )
    };
    if let router::RouteDecision::Moved { slot, addr } = decision {
        return Some(router::moved_error_line(slot, &addr));
    }
    // 3. MIGRATING source: missing key -> ASK to the destination.
    let migrating = shared
        .migrating
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(dst) = migrating.get(&slot) {
        if !key_present(shared, slot, key) {
            return Some(format!("ASK {} {}", slot, dst));
        }
    }
    None
}

/// Does `key` (of `slot`) exist on this node? Best-effort read through the
/// standard resolver (lazy-expire purges detached, as on all read paths).
fn key_present(shared: &state::Shared, slot: u16, key: &[u8]) -> bool {
    let prefix = crate::store::rocksdb::slot_prefix(slot);
    crate::command::keys_core::resolve_arc(&shared.store, &prefix, key, crate::ds::expire::now_ms())
        .is_present()
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

    // Backup listener: read-only gate (Redis replica semantics). Runs
    // after lookup so unknown commands keep their specific error, and
    // covers EXEC replays too (they re-enter dispatch). Go's BackupServer
    // had no such gate.
    if shared.mode == state::Mode::Backup && !readonly::allowed(&first) {
        codec::append_error(out, readonly::ERROR);
        observe(shared, &first, false, start);
        return false;
    }

    // Slot routing for non-whitelisted commands.
    let mut prefix_key: Vec<u8> = Vec::new();
    if !router::is_whitelisted(&first) {
        // BREAKING (approved): Go indexed cmd.Args[1] unconditionally, so a
        // lone command name surfaced as a fabricated runtime-panic reply;
        // use the Redis-standard arity error instead. No latency sample on
        // this error path (Go observes only after the handler returns).
        // The routing key sits at argv[1] except for token-led commands
        // (LMPOP/SINTERCARD count, BITOP operation): router owns the index.
        let key_idx = router::routing_key_index(&first);
        if argv.len() < key_idx + 1 {
            arity_error(out, &first);
            return false;
        }
        let tag = hash::hash_tag(&argv[key_idx]);
        let (slot, prefix) = hash::slot_with_prefix(tag);
        prefix_key = prefix;
        let asking = conn.asking;
        conn.asking = false; // single-shot: consumed by this routed command
                             // The routing read runs BEFORE the handler panic net below but can
                             // panic in principle too (the MIGRATING check does a best-effort
                             // store read): catch it and reply exactly like a panicked handler,
                             // closing the connection after the flush.
        let line = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            redirect_line(shared, slot, &argv[key_idx], asking)
        })) {
            Ok(line) => line,
            Err(payload) => {
                codec::append_error(out, &format!("fatal error: {}", panic_payload(&payload)));
                *close = true;
                return false;
            }
        };
        if let Some(line) = line {
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
/// `pub(crate)`: the connection layer reuses it for its queue-path net.
pub(crate) fn panic_payload(payload: &Box<dyn std::any::Any + Send>) -> String {
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

/// Seed a string through the real SET handler (argv WITHOUT the command
/// name, like `test_ctx` callers). Test-only cross-module helper.
#[cfg(test)]
pub(crate) fn seed_for(shared: &state::Shared, key: &[u8], val: &[u8]) {
    let (_, prefix) = crate::hash::slot_with_prefix(crate::hash::hash_tag(key));
    let mut out = Vec::new();
    let argv = vec![key.to_vec(), val.to_vec()];
    let mut ctx = test_ctx(shared, prefix, argv, &mut out);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(crate::command::string::set(&mut ctx));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testutil;
    use crate::state::Shared;
    use crate::topology;

    const INSTANCES: &str = "127.0.0.1:32681,127.0.0.1:32683,127.0.0.1:32685";

    fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
        let guard = crate::command::string::TEST_STORE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut conf = testutil::test_config();
        conf.bind = bind.to_string();
        (guard, testutil::shared_with(conf))
    }

    /// Seed a string via the real SET handler (correct slot prefix).
    /// `test_ctx` receives argv WITHOUT the command name.
    fn seed(shared: &Shared, key: &[u8], val: &[u8]) {
        seed_for(shared, key, val);
    }

    #[test]
    fn redirect_importing_gate_serves_only_asking() {
        let (_guard, shared) = shared_for("127.0.0.1:32681");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        shared
            .importing
            .write()
            .unwrap()
            .insert(100, "127.0.0.1:32681".to_string());
        // Non-ASKING: redirected MOVED back to the source.
        assert_eq!(
            redirect_line(&shared, 100, b"k", false),
            Some("MOVED 100 127.0.0.1:32681".to_string())
        );
        // ASKING: served locally even though the band owner is this node
        // and the importing table names it as source.
        assert_eq!(redirect_line(&shared, 100, b"k", true), None);
    }

    #[test]
    fn redirect_owner_map_override_wins_over_band() {
        let (_guard, shared) = shared_for("127.0.0.1:32681");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        // Band says slot 100 lives here; the raft owner map overrides.
        assert_eq!(redirect_line(&shared, 100, b"k", false), None);
        shared
            .topology
            .write()
            .unwrap()
            .owner_map
            .insert(100, "127.0.0.1:32683".to_string());
        assert_eq!(
            redirect_line(&shared, 100, b"k", false),
            Some("MOVED 100 127.0.0.1:32683".to_string())
        );
    }

    #[test]
    fn redirect_migrating_missing_key_asks_to_destination() {
        let (_guard, shared) = shared_for("127.0.0.1:32681");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        // `{b}` tags to slot 3300, inside this node's (0-5461) band.
        let (slot, _) = crate::hash::slot_with_prefix(crate::hash::hash_tag(b"{b}here"));
        shared
            .migrating
            .write()
            .unwrap()
            .insert(slot, "127.0.0.1:32683".to_string());
        // Key absent locally -> ASK to the destination.
        assert_eq!(
            redirect_line(&shared, slot, b"gone", false),
            Some(format!("ASK {slot} 127.0.0.1:32683"))
        );
        // Key present locally -> served (still the source owner).
        seed(&shared, b"{b}here", b"1");
        assert_eq!(redirect_line(&shared, slot, b"{b}here", false), None);
    }

    /// Poison a routing lock the way real poisoning happens: a writer
    /// panics while holding the guard. (The catch_unwind prints a noisy
    /// panic backtrace to stderr; that is expected in tests.)
    fn poison<T>(lock: &std::sync::RwLock<T>) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = lock.write().unwrap();
            panic!("poison");
        }));
        assert!(lock.is_poisoned());
    }

    #[test]
    fn redirect_survives_poisoned_routing_locks() {
        let (_guard, shared) = shared_for("127.0.0.1:32681");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        let (slot, _) = crate::hash::slot_with_prefix(crate::hash::hash_tag(b"{b}here"));
        shared
            .migrating
            .write()
            .unwrap()
            .insert(slot, "127.0.0.1:32683".to_string());
        // Poison all three AFTER the setup writes: redirect_line must read
        // through the poison (the guarded data is still valid), not panic.
        poison(&shared.topology);
        poison(&shared.importing);
        poison(&shared.migrating);
        // Local band (slot 100) still served.
        assert_eq!(redirect_line(&shared, 100, b"k", false), None);
        // Foreign band still MOVED (7000 lives on the second node).
        assert_eq!(
            redirect_line(&shared, 7000, b"k", false),
            Some("MOVED 7000 127.0.0.1:32683".to_string())
        );
        // MIGRATING + absent key still ASK to the destination.
        assert_eq!(
            redirect_line(&shared, slot, b"gone", false),
            Some(format!("ASK {slot} 127.0.0.1:32683"))
        );
    }

    /// Same poison, one level up: dispatch routes (redirect_line) and runs
    /// the GET handler without the poisoned locks cascade-panicking the
    /// task. `bar` tags to slot 5061, inside the local (0-5461) band, so
    /// the absent key resolves to a null bulk, not a MOVED.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // TEST_STORE_LOCK guard must outlive Shared
    async fn dispatch_survives_poisoned_routing_locks() {
        let (_guard, shared) = shared_for("127.0.0.1:32681");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        poison(&shared.topology);
        poison(&shared.importing);
        poison(&shared.migrating);
        let mut out = Vec::new();
        let mut close = false;
        let wrote = dispatch(
            &shared,
            vec![b"get".to_vec(), b"bar".to_vec()],
            &mut ConnState::default(),
            &mut out,
            &mut close,
        )
        .await;
        assert_eq!(out, b"$-1\r\n");
        assert!(!close);
        assert!(!wrote);
    }

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
