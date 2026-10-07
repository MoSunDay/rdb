//! Connection admission guard for the RocksMQ HTTP front (P3 #13): a
//! verbatim mirror of the kafka front's `ConnGuard` (`src/kafka/conn.rs`,
//! `LIVE_CONNS` + `DEFAULT_MAX_CONNS` + RAII `Drop`). One process-wide
//! live-connection counter, capped by `rocksmq_max_connections` (0 =
//! the built-in [`DEFAULT_MAX_CONNS`]); the guard's `Drop` releases
//! its slot no matter which early return ends the connection (peer
//! close, malformed head, oversized body, idle timeout, cap eviction).
//!
//! At the cap the new TCP connection is refused EXACTLY the way kafka
//! refuses one: the handler task returns without writing a byte (the
//! socket drops = closed silently, no HTTP error page) after one
//! stderr line -- the kafka precedent in `src/kafka/conn.rs`:
//! `eprintln!("[kafka] connection refused: at cap {cap}")`.

use std::sync::atomic::{AtomicI64, Ordering};

/// Built-in connection cap when `rocksmq_max_connections` is 0
/// (kafka's `DEFAULT_MAX_CONNS` parity).
pub const DEFAULT_MAX_CONNS: i64 = 4096;

/// Live rocksmq HTTP connections (process-wide; the front has one
/// listener).
static LIVE_CONNS: AtomicI64 = AtomicI64::new(0);

/// Configured `rocksmq_max_connections` -> effective cap: a positive
/// value wins, anything else (0 = "unset", the default) falls back to
/// [`DEFAULT_MAX_CONNS`]. Pure, so the unit tests cover it without a
/// config file.
pub fn cap_of(configured: i64) -> i64 {
    if configured > 0 {
        configured
    } else {
        DEFAULT_MAX_CONNS
    }
}

/// Admission guard: counts one connection from accept to close, so
/// every early return releases its slot (kafka's `ConnGuard`).
pub struct ConnGuard;

impl ConnGuard {
    /// `None` = at/over the cap (the socket is dropped = closed).
    pub fn enter(cap: i64) -> Option<ConnGuard> {
        if LIVE_CONNS.fetch_add(1, Ordering::AcqRel) >= cap {
            LIVE_CONNS.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(ConnGuard)
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        LIVE_CONNS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> i64 {
        LIVE_CONNS.load(Ordering::Acquire)
    }

    #[test]
    fn cap_of_config_semantics() {
        assert_eq!(cap_of(0), DEFAULT_MAX_CONNS); // unset -> built-in
        assert_eq!(cap_of(-7), DEFAULT_MAX_CONNS); // junk never widens the cap
        assert_eq!(cap_of(1), 1);
        assert_eq!(cap_of(128), 128);
        assert_eq!(DEFAULT_MAX_CONNS, 4096); // kafka parity constant
    }

    #[test]
    fn enter_release_and_refusal() {
        // cap <= 0 refuses deterministically (the counter never goes
        // below zero), with the failed attempt leaving no residue.
        let base = live();
        assert!(ConnGuard::enter(0).is_none());
        assert!(ConnGuard::enter(-1).is_none());
        assert_eq!(live(), base);

        // Admission bumps the live count; Drop hands the slot back.
        let g = ConnGuard::enter(i64::MAX).unwrap();
        assert_eq!(live(), base + 1);
        drop(g);
        assert_eq!(live(), base);

        // At a cap of (live + 1) the next enter admits, the one after
        // is refused WITHOUT leaking its optimistic increment.
        let cap = live() + 1;
        let g = ConnGuard::enter(cap).unwrap();
        assert_eq!(live(), cap);
        assert!(ConnGuard::enter(cap).is_none());
        assert_eq!(live(), cap); // refused attempt rolled back
        drop(g);
        assert_eq!(live(), base);
    }
}
