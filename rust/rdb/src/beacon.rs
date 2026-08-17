//! Diagnostic heartbeat for silent task death (spawned from `main.rs`):
//! a periodic `[beacon]` line proves the runtime keeps scheduling, and a
//! final log fires if the raft metrics watch channel closes.
//!
//! The beacon is opt-in via `RDB_BEACON=1` and silent by default.

use std::sync::Arc;

use rdb::rcache::RdbRaft;

/// Returns true iff the beacon is enabled via `RDB_BEACON=1` (exact match;
/// missing or any other value disables it).
pub(crate) fn enabled() -> bool {
    std::env::var("RDB_BEACON")
        .map(|v| v == "1")
        .unwrap_or(false)
}

pub(crate) fn spawn_beacon(raft: Arc<RdbRaft>) {
    tokio::spawn(async move {
        let mut rx = raft.metrics();
        let mut iv = tokio::time::interval(std::time::Duration::from_millis(250));
        iv.tick().await;
        loop {
            tokio::select! {
                _ = iv.tick() => {
                    eprintln!("[beacon] {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis());
                }
                r = rx.changed() => {
                    if r.is_err() {
                        eprintln!("[beacon] raft metrics channel CLOSED");
                        break;
                    }
                }
            }
        }
        eprintln!("[beacon] EXIT");
    });
}
