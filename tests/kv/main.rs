//! RESP/KV data-plane e2e tests, aggregated into one crate to cut
//! per-file link overhead (tests-reorg 2026-11-10).

#[path = "../common/mod.rs"]
mod common;

mod bits_e2e;
mod blocking_concurrency_e2e;
mod ds_e2e;
mod expire_e2e;
mod gaps_e2e;
mod hash_read_e2e;
mod hash_set_e2e;
mod json_e2e;
mod keys_more_e2e;
mod kv_newcmds_proc_e2e;
mod list_e2e;
mod resp_e2e;
mod resp_reply_semantics;
mod server_surface_e2e;
mod set_more_e2e;
mod string_e2e;
mod string_more_e2e;
mod tx_e2e;
mod vectorset_e2e;
mod wire_families_e2e;
mod zset_e2e;
mod zset_more_e2e;
