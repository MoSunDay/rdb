//! rcache: raft control plane (Rust port of Go `internal/rcache`).
//!
//! The Go original runs hashicorp/raft with the RaftTCPAddress as node id
//! (`RaftConfig.LocalID = raft.ServerID(opts.RaftTCPAddress)`). openraft
//! 0.9.25 requires `NodeId: Copy`, so a plain `String` cannot be a node
//! id: numeric ids are used instead (deviation forced by the pinned
//! openraft version), and the RaftTCPAddress string travels as the `Node`
//! payload of every membership config, exactly like the Go transport
//! addressing.

pub mod fsm;
pub mod ha;
pub mod http;
pub mod join;
pub mod service;
pub mod store;
pub mod transport;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;

use openraft::Config;
use openraft::SnapshotPolicy;

/// Raft node id. Go uses the RaftTCPAddress string here, but openraft
/// 0.9.25 requires `NodeId: Copy`, so numeric ids are used and the
/// address string is carried by [`Node`] instead.
pub type NodeId = u64;

/// Node metadata: the RaftTCPAddress string (Go nodes are identified by
/// their address alone).
pub type Node = String;

/// Snapshot stream handle used by the openraft storage-v2 API.
pub type SnapshotData = Cursor<Vec<u8>>;

openraft::declare_raft_types!(
    /// Raft types of rdb: the replicated log entry is the Go
    /// `rtypes.RaftLogEntryData` (JSON `{"Key":...,"Value":...}`), the
    /// client-write response is a plain string.
    pub TypeConfig:
        D = crate::rtypes::RaftLogEntryData,
        R = String,
        Node = Node,
        NodeId = NodeId,
);

/// openraft type aliases (layout follows the raft-kv-rocksdb example).
pub mod typ {
    use openraft::error::Infallible;

    use crate::rcache::Node;
    use crate::rcache::NodeId;
    use crate::rcache::TypeConfig;

    pub type Entry = openraft::Entry<TypeConfig>;

    pub type RaftError<E = Infallible> = openraft::error::RaftError<NodeId, E>;
    pub type RPCError<E = Infallible> = openraft::error::RPCError<NodeId, Node, RaftError<E>>;

    pub type ClientWriteError = openraft::error::ClientWriteError<NodeId, Node>;
    pub type CheckIsLeaderError = openraft::error::CheckIsLeaderError<NodeId, Node>;
    pub type ForwardToLeader = openraft::error::ForwardToLeader<NodeId, Node>;
    pub type InitializeError = openraft::error::InitializeError<NodeId, Node>;

    pub type ClientWriteResponse = openraft::raft::ClientWriteResponse<TypeConfig>;
}

/// The raft instance type used across rcache.
pub type RdbRaft = openraft::Raft<TypeConfig>;

/// Raft tuning, ported from Go rcache (`raft.DefaultConfig()` plus the
/// overrides in `NewRaftNode`):
/// - `SnapshotThreshold = 1` (Go override) is not carried over: per-entry
///   snapshots caused fsync + purge storms, so the hashicorp default 8192
///   is used;
/// - `SnapshotInterval = 30s` has no openraft counterpart, the policy is
///   evaluated on each apply instead;
/// - hashicorp `ElectionTimeout = 1000ms` is randomized by hashicorp raft
///   to `[1000, 2000)`, which openraft's `[min, max)` range reproduces;
/// - hashicorp `HeartbeatTimeout = 1000ms` is the follower deadline; the
///   leader must beat it, and openraft requires
///   `heartbeat_interval < election_timeout_min`, so heartbeats go out at
///   half the timeout.
pub fn raft_config() -> Arc<Config> {
    let config = Config {
        heartbeat_interval: 500,
        election_timeout_min: 1000,
        election_timeout_max: 2000,
        // hashicorp/raft default SnapshotThreshold is 8192 (Go fidelity);
        // per-entry snapshots caused fsync + purge storms.
        snapshot_policy: SnapshotPolicy::LogsSinceLast(8192),
        ..Default::default()
    };
    Arc::new(config.validate().expect("static raft config must be valid"))
}

/// Assembled raft node (Go `rcache.RaftNodeInfo`, minus the hashicorp
/// bits that have no openraft counterpart).
pub struct RaftNode {
    pub raft: Arc<RdbRaft>,
    /// Live FSM map handle (Go `CM`); reads see committed entries at once.
    pub kv: fsm::KvMap,
}

/// Go `rcache.NewRaftNode`: create the data dir, open the log store,
/// build FSM + transport + raft instance, then optionally bootstrap.
/// Bootstrap errors are logged and ignored, exactly like the Go original
/// ignores the `BootstrapCluster` future's error.
pub async fn new_raft_node(data_dir: &str, raft_tcp_addr: &str) -> Result<RaftNode, String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("mkdir {data_dir}: {e}"))?;
    let log_store = store::open(data_dir).map_err(|e| format!("open raft log store: {e}"))?;
    let state_machine =
        fsm::StateMachine::new(log_store.clone()).map_err(|e| format!("new state machine: {e}"))?;
    let kv = state_machine.data.kv.clone();

    let id = transport::node_id_of(raft_tcp_addr);
    let raft = RdbRaft::new(
        id,
        raft_config(),
        transport::new(id),
        log_store,
        state_machine,
    )
    .await
    .map_err(|e| format!("new raft: {e}"))?;
    let raft = Arc::new(raft);

    if crate::conf::raft_bootstrap() {
        match raft.is_initialized().await {
            Ok(true) => {}
            Ok(false) => {
                let members = BTreeMap::from([(id, raft_tcp_addr.to_string())]);
                if let Err(e) = raft.initialize(members).await {
                    eprintln!("bootstrap raft cluster failed: {e}");
                }
            }
            Err(e) => eprintln!("check raft initialization failed: {e}"),
        }
    }

    Ok(RaftNode { raft, kv })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raft_config_matches_go_values() {
        let cfg = raft_config();
        // hashicorp HeartbeatTimeout=1000ms is a follower deadline; the
        // leader beats it at half the interval (openraft requires
        // heartbeat < election_timeout_min).
        assert_eq!(cfg.heartbeat_interval, 500);
        // hashicorp ElectionTimeout=1000ms randomized to [1000, 2000).
        assert_eq!(cfg.election_timeout_min, 1000);
        assert_eq!(cfg.election_timeout_max, 2000);
        assert!(
            matches!(cfg.snapshot_policy, SnapshotPolicy::LogsSinceLast(8192)),
            "hashicorp default SnapshotThreshold=8192"
        );
        // election/heartbeat knobs must be enabled, as in the Go original
        assert!(cfg.enable_tick && cfg.enable_elect && cfg.enable_heartbeat);
    }
}
