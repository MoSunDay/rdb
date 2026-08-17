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
//! DEVIATION (documented per task): Go measured latency with a fake clock
//! ticking every 5ms (`conf.Content.Sentinel.RTime`); here we observe real
//! elapsed milliseconds. Histogram buckets are unchanged.

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

/// Dispatch one parsed command (Go `server.go` handler closure).
async fn process_command(
    shared: &state::Shared,
    mut argv: Vec<Vec<u8>>,
    authed: &mut bool,
    out: &mut Vec<u8>,
    close: &mut bool,
) {
    // Go indexes cmd.Args[0] unconditionally; an empty multibulk (`*0`)
    // panics and the deferred recover replies with the runtime text.
    if argv.is_empty() {
        codec::append_error(
            out,
            "fatal error: runtime error: index out of range [0] with length 0",
        );
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
        // Go indexes cmd.Args[1] unconditionally; a lone command name panics
        // and the deferred recover replies with the runtime text.
        if argv.len() < 2 {
            codec::append_error(
                out,
                &format!(
                    "fatal error: runtime error: index out of range [1] with length {}",
                    argv.len()
                ),
            );
            observe(shared, &first, false, start);
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
    if let Some(payload) = panicked {
        codec::append_error(ctx.out, &format!("fatal error: {}", payload_str(&payload)));
    }

    // Label order mirrors Go: (mode, lowercase command, was-MOVED).
    observe(shared, &first, false, start);

    if ctx.close_conn {
        // Go `quit` writes its replies then closes the connection; the flush
        // happens in the caller before we return.
        *close = true;
    }
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
}
