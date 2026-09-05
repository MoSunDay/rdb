//! Unit tests for the bitmap commands; every expectation was first
//! observed on a live Redis 7.2.5 (see the contracts in `bitops.rs`).
//! Handlers are driven directly — the router wiring lives elsewhere.
//! Multi-key commands use `{g}`-tagged keys (one slot); `{a}`/`{b}` tags
//! form the cross-slot outgroup.

use super::*;
use crate::command::hash_cmd;
use crate::command::string;
use crate::command::test_ctx;
use crate::command::Handler;
use crate::state::{testutil, Shared};

const PREFIX: &[u8] = b"70/";

fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
    let guard = string::TEST_STORE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut conf = testutil::test_config();
    conf.bind = bind.to_string();
    (guard, testutil::shared_with(conf))
}

fn call(shared: &Shared, handler: Handler, args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let mut ctx = test_ctx(shared, PREFIX.to_vec(), argv, &mut out);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(handler(&mut ctx));
    out
}

fn sbit(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
    call(shared, |ctx| Box::pin(setbit(ctx)), args)
}
fn gbit(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
    call(shared, |ctx| Box::pin(getbit(ctx)), args)
}
fn bcount(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
    call(shared, |ctx| Box::pin(bitcount(ctx)), args)
}
fn bpos(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
    call(shared, |ctx| Box::pin(bitpos(ctx)), args)
}
fn bop(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
    call(shared, |ctx| Box::pin(bitop(ctx)), args)
}

/// Seed a string via the real SET (optionally with a TTL).
fn seed(shared: &Shared, key: &[u8], val: &[u8], ex_secs: Option<&[u8]>) {
    let mut args: Vec<&[u8]> = vec![key, val];
    if let Some(s) = ex_secs {
        args.extend_from_slice(&[b"EX", s]);
    }
    call(shared, |ctx| Box::pin(string::set(ctx)), &args);
}

/// The stored string (GET payload).
fn stored(shared: &Shared, key: &[u8]) -> Vec<u8> {
    let reply = call(shared, |ctx| Box::pin(string::get(ctx)), &[key]);
    let end = reply.iter().position(|&b| b == b'\n').expect("bulk header");
    let len: usize = std::str::from_utf8(&reply[1..end - 1])
        .expect("len")
        .parse()
        .expect("bulk length");
    reply[end + 1..end + 1 + len].to_vec()
}

/// Key existence from storage (independent of the commands under test).
fn present(shared: &Shared, key: &[u8]) -> bool {
    keys_core::resolve(&shared.store, PREFIX, key, crate::ds::expire::now_ms()).is_present()
}

/// Remaining TTL (0 = none), from storage.
fn expire_of(shared: &Shared, key: &[u8]) -> u64 {
    keys_core::resolve(&shared.store, PREFIX, key, crate::ds::expire::now_ms()).expire_ms()
}

mod pos_op;
mod set_get_count;
