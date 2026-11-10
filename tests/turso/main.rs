//! Embedded turso-engine e2e tests, aggregated into one crate
//! (tests-reorg 2026-11-10).

#[path = "../common/mod.rs"]
mod common;

mod turso_catalog_e2e;
mod turso_crud_e2e;
mod turso_parity_e2e;
mod turso_setops_e2e;
mod turso_smoke_e2e;
mod turso_sqlite_interop_e2e;
mod turso_txn_e2e;
mod turso_vector_e2e;
