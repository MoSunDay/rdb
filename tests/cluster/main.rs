//! Control-plane / HA / process-lifecycle / migration e2e tests,
//! aggregated into one crate (tests-reorg 2026-11-10). ha_failover
//! and raft_cluster_e2e stay separate targets: they set
//! RAFT_BOOTSTRAP process-wide.

#[path = "../common/mod.rs"]
mod common;

#[path = "../backup_surface_common/mod.rs"]
mod backup_surface_common;

mod asking_restore_e2e;
mod backup_failover_e2e;
mod backup_readonly_e2e;
mod backup_surface_e2e;
mod migrate_e2e;
mod process_cluster_e2e;
mod process_failover_e2e;
mod process_metrics_e2e;
mod process_sigterm_e2e;
mod raft_transport;
mod routing_newcmds_e2e;
mod standalone_e2e;
