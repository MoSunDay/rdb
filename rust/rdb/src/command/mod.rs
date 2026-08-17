//! Command registry and dispatch (Go: internal/command/command.go).
//!
//! Handlers are free functions over `Ctx`; responses are appended to `out`
//! as RESP frames via `crate::resp::codec` helpers. Handlers never touch the
//! socket directly; `quit` asks for connection close via `close_conn`.

pub mod cluster;
pub mod hash_cmd;
pub mod hash_incr;
pub mod hash_scan;
pub mod keys;
pub mod keys_core;
pub mod keys_scan;
pub mod migrate;
pub mod raft_cmd;
pub mod set_cmd;
pub mod set_scan;
pub mod setops_cmd;
pub mod string;

use crate::state;

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
        "xpick" => Some(|ctx| Box::pin(crate::lite::info::xpick(ctx))),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn test_ctx<'a>(
    shared: &'a state::Shared,
    prefix_key: Vec<u8>,
    args: Vec<Vec<u8>>,
    out: &'a mut Vec<u8>,
) -> Ctx<'a> {
    Ctx {
        shared,
        prefix_key,
        args,
        out,
        close_conn: false,
    }
}

#[cfg(test)]
#[path = "hash_tests.rs"]
mod hash_tests;

#[cfg(test)]
#[path = "set_tests.rs"]
mod set_tests;
