//! ES HTTP front e2e tests, aggregated into one crate
//! (tests-reorg 2026-11-10).

#[path = "../common/mod.rs"]
mod common;

#[path = "../es_common/mod.rs"]
mod es_common;

mod es_auth_e2e;
mod es_bulk_interop_e2e;
mod es_e2e;
mod es_search_e2e;
