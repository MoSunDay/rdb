//! Per-connection state machine: read RESP frames, dispatch handlers, flush
//! responses. Mirrors the MoSunDay/redcon fork connection loop plus the Go
//! server's dispatch pipeline (`internal/server/server.go`):
//!
//! 1. AUTH gate (fork behavior: only `AUTH`/`auth` with the raft token is
//!    accepted pre-auth; everything else gets `-ERR: NOAUTH`).
//! 2. Unknown command -> `-ERR unknown command '<original-case>'`.
//! 3. Non-whitelisted commands go through slot routing; foreign slots get a
//!    `-MOVED <slot> <addr>` redirect.
//! 4. Handlers run under `catch_unwind`, reproducing Go's `defer recover()`
//!    which replies `fatal error: <panic>` instead of dropping the client.
//!
//! DEVIATION (BREAKING, approved): Go indexed cmd.Args[0]/[1] unconditionally,
//! so an empty multibulk or a lone command name surfaced as a fabricated
//! runtime-panic reply; here both reply the Redis-standard arity error.
//!
//! DEVIATION (documented per task): Go measured latency with a fake clock
//! ticking every 5ms (`conf.Content.Sentinel.RTime`); here we observe real
//! elapsed milliseconds. Histogram buckets are unchanged. Like Go, no
//! latency sample is recorded on the arity-error or panic paths.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::command;
use crate::hash;
use crate::monitor;
use crate::resp::codec;
use crate::router;
use crate::state;

/// One connection: parse-loop with pipelining, write-after-drain.
pub async fn handle_conn(sock: TcpStream, shared: Arc<state::Shared>) {
    let (mut rd, mut wr) = sock.into_split();
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut out: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let mut authed = false;

    loop {
        let n = match rd.read(&mut chunk).await {
            Ok(0) | Err(_) => return, // EOF or socket error (redcon: close)
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);

        // Drain ALL complete commands currently buffered (pipelining).
        let mut close = false;
        loop {
            match codec::parse_command(&buf) {
                codec::ParseOutcome::Incomplete => break,
                codec::ParseOutcome::ProtocolError { msg, .. } => {
                    // Go redcon: `-ERR <protocol msg>` then close.
                    codec::append_error(&mut out, &format!("ERR {msg}"));
                    let _ = wr.write_all(&out).await;
                    return;
                }
                codec::ParseOutcome::Complete { args, consumed } => {
                    buf.drain(..consumed);
                    // A blocking command parks INSIDE dispatch; replies
                    // already buffered for earlier pipelined commands
                    // must reach the socket first or they are held
                    // hostage until the pop returns (flush-per-batch is
                    // too coarse once the batch itself blocks).
                    if args
                        .first()
                        .is_some_and(|a| may_block(&String::from_utf8_lossy(a).to_lowercase()))
                        && !out.is_empty()
                    {
                        if wr.write_all(&out).await.is_err() {
                            return;
                        }
                        out.clear();
                    }
                    process_command(&shared, args, &mut authed, &mut out, &mut close).await;
                    if close {
                        break;
                    }
                }
            }
        }

        if !out.is_empty() {
            if wr.write_all(&out).await.is_err() {
                return;
            }
            out.clear();
        }
        if close {
            return;
        }
    }
}

/// Commands whose dispatch may PARK the connection (blocking pops and
/// stream reads): any replies already buffered for earlier pipelined
/// commands must be flushed to the socket BEFORE dispatch, mirroring
/// Redis's behavior of answering everything ahead of a blocking call.
fn may_block(first: &str) -> bool {
    matches!(
        first,
        "blpop"
            | "brpop"
            | "blmove"
            | "brpoplpush"
            | "bzpopmin"
            | "bzpopmax"
            | "xread"
            | "xreadgroup"
    )
}

/// Dispatch one parsed command (Go `server.go` handler closure).
async fn process_command(
    shared: &state::Shared,
    mut argv: Vec<Vec<u8>>,
    authed: &mut bool,
    out: &mut Vec<u8>,
    close: &mut bool,
) {
    // BREAKING (approved): Go indexed cmd.Args[0] unconditionally, so an
    // empty multibulk (`*0`) surfaced as a fabricated runtime-panic reply;
    // use the Redis-standard arity error instead (no command name exists).
    if argv.is_empty() {
        arity_error(out, "");
        return;
    }

    // AUTH gate (MoSunDay/redcon fork, verbatim): pre-auth only a 2-arg
    // AUTH/auth carrying the raft token succeeds; anything else -> NOAUTH.
    // No latency is observed pre-auth (the handler is never invoked).
    if !*authed {
        if argv.len() == 2
            && (argv[0] == b"AUTH"[..] || argv[0] == b"auth"[..])
            && argv[1] == shared.conf.raft_token.as_bytes()
        {
            *authed = true;
            codec::append_string(out, "OK");
        } else {
            codec::append_error(out, "ERR: NOAUTH");
        }
        return;
    }

    let raw0 = String::from_utf8_lossy(&argv[0]);
    let first = raw0.to_lowercase();
    let start = std::time::Instant::now();

    let handler = match command::lookup(&first) {
        Some(h) => h,
        // Go: `ERR unknown command '<original case>'`; no latency observed.
        None => {
            codec::append_error(out, &format!("ERR unknown command '{raw0}'"));
            return;
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
            return;
        }
        let tag = hash::hash_tag(&argv[1]);
        let (slot, prefix) = hash::slot_with_prefix(tag);
        prefix_key = prefix;

        let decision = {
            let topo = shared.topology.read().unwrap();
            router::route(
                slot,
                &topo.stable_addrs,
                topo.per_node_slots,
                &shared.conf.bind,
            )
        };
        if let router::RouteDecision::Moved { slot, addr } = decision {
            codec::append_error(out, &router::moved_error_line(slot, &addr));
            observe(shared, &first, true, start);
            return;
        }
    }

    // Run the handler behind Go's `defer recover()` safety net.
    let mut ctx = command::Ctx {
        shared,
        prefix_key,
        args: argv.split_off(1),
        out,
        close_conn: false,
    };
    let panicked = {
        AssertUnwindSafe(handler(&mut ctx))
            .catch_unwind()
            .await
            .err()
    };
    match panicked {
        Some(payload) => {
            codec::append_error(ctx.out, &format!("fatal error: {}", payload_str(&payload)));
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
}

/// Redis-standard arity error; identical text to the command-module
/// `arity` helpers (e.g. `string::arity` / `hash_cmd::arity`).
fn arity_error(out: &mut Vec<u8>, cmd: &str) {
    codec::append_error(
        out,
        &format!("ERR wrong number of arguments for '{cmd}' command"),
    );
}

/// Latency helper keeping the Go label order via monitor::observe_latency.
fn observe(shared: &state::Shared, cmd: &str, is_moved: bool, start: std::time::Instant) {
    monitor::observe_latency(
        &shared.monitor,
        state::mode_label(shared.mode),
        cmd,
        is_moved,
        start.elapsed().as_millis() as f64,
    );
}

/// Panic payload -> message text (Go `err.(error).Error()`).
fn payload_str(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return s;
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.as_str();
    }
    "unknown panic payload"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_str_handles_common_types() {
        let s: String = "boom".to_string();
        assert_eq!(payload_str(&s), "boom");
        let lit: &'static str = "bang";
        assert_eq!(payload_str(&lit), "bang");
        let n: i32 = 7;
        assert_eq!(payload_str(&n), "unknown panic payload");
    }

    #[test]
    fn may_block_covers_exactly_the_parking_commands() {
        for cmd in [
            "blpop",
            "brpop",
            "blmove",
            "brpoplpush",
            "bzpopmin",
            "bzpopmax",
            "xread",
            "xreadgroup",
        ] {
            assert!(may_block(cmd), "{cmd} parks");
        }
        for cmd in ["lpop", "zadd", "get", "xadd", "auth", ""] {
            assert!(!may_block(cmd), "{cmd} never parks");
        }
    }
}
