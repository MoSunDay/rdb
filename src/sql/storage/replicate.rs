//! Post-DDL replication barrier: hold a catalog mutation's ack until
//! every reachable peer's FSM actually serves it.
//!
//! ## Why
//! A DDL returns when the raft entry is committed AND applied on the
//! leader. Followers apply asynchronously, so a read issued on another
//! node right after the ack (SHOW INDEX / SHOW TABLES from a CLI, or
//! the schema a follower-coordinated write plans against) could still
//! serve the pre-DDL catalog: a fresh unique index would not be
//! enforced on follower-coordinated writes and freshly created indexes
//! would be invisible in follower metadata reads for an unbounded
//! (topology/apply dependent) window.
//!
//! ## How
//! After the mutation applies locally, the leader polls each peer's
//! control API (`/get?key=...`, the exact same FSM view catalog reads
//! use) until it returns the value just written. `/get` and
//! `catalog::lookup` share `RaftState.live_kv`, so a matched poll
//! guarantees the peer's subsequent catalog reads see the mutation.
//!
//! The wait is best-effort by design: a peer that is down or slow only
//! bounds the DDL ack latency (poll deadline per peer); it never fails
//! the DDL, and a rejoining peer replays the same raft log anyway.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::rcache::join::{http_get_status, percent_encode};
use crate::sql::tx::nodes::{parse_registry, NodeBinds, SQL_NODES_KEY};
use crate::state::{self, Shared};

/// One poll round per peer FSM entry; small enough that the barrier
/// tracks apply lag tightly, coarse enough to stay off the wire.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Upper bound one peer may add to a DDL's ack latency. Followers
/// apply committed entries within a few ms on a healthy cluster; the
/// bound only matters for a peer that is dying mid-poll.
const PEER_DEADLINE: Duration = Duration::from_millis(1500);

/// Registry view of the OTHER nodes (self skipped by its http bind),
/// read from this node's FSM. Empty for single-node worlds, which
/// makes the whole barrier a no-op there.
fn peer_binds(shared: &Shared) -> BTreeMap<String, NodeBinds> {
    let raw = {
        let raft = shared.raft.read().unwrap();
        state::raft_get(&raft, SQL_NODES_KEY)
    };
    let self_http = &shared.conf.http_address;
    parse_registry(&raw)
        .into_iter()
        .filter(|(_, b)| !b.http.is_empty() && &b.http != self_http)
        .collect()
}

/// Whether the peer's FSM already serves `expected` for `key`.
async fn peer_has(peer_http: &str, token: &str, key: &str, expected: &str) -> bool {
    let url = format!(
        "http://{peer_http}/get?key={key}&raft-token={}",
        percent_encode(token)
    );
    // Unreachable/timeout peers surface as Err -> simply not fresh yet;
    // the outer deadline stops the retry loop.
    matches!(
        http_get_status(&url).await,
        Ok((200, body)) if body.strip_suffix('\n').unwrap_or(&body) == expected
    )
}

/// Wait until every reachable peer's FSM serves each `entries` value
/// (`key` -> exact value written through raft). Never errors: bounded
/// by [`PEER_DEADLINE`] per peer, silent for single-node worlds.
pub async fn wait_peers_serve(shared: &Shared, entries: &[(String, String)]) {
    if entries.is_empty() || std::env::var("RDB_SKIP_CATALOG_BARRIER").is_ok() {
        return;
    }
    let peers = peer_binds(shared);
    if peers.is_empty() {
        return;
    }
    let token = shared.conf.raft_token.clone();
    for (addr, binds) in peers {
        let deadline = Instant::now() + PEER_DEADLINE;
        let mut confirmed = true;
        for (key, expected) in entries {
            while !peer_has(&binds.http, &token, key, expected).await {
                if Instant::now() >= deadline {
                    confirmed = false;
                    break;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            if !confirmed {
                break;
            }
        }
        if !confirmed {
            eprintln!(
                "[catalog-barrier] peer {addr} did not confirm the catalog \
                 write within {PEER_DEADLINE:?}; continuing (best-effort)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unreachable peer is simply "not fresh yet" (Err side), never
    /// a panic: the barrier's contract is best-effort polling.
    #[tokio::test]
    async fn peer_has_treats_unreachable_as_stale() {
        assert!(!peer_has("127.0.0.1:1", "t", "sql_catalog/x", "json").await);
    }
}
