//! Lite-stream / MQ-lite consumption-semantics e2e tests, aggregated
//! into one crate (tests-reorg 2026-11-10).

#[path = "../common/mod.rs"]
mod common;

mod flushdb_lite_e2e;
mod lite_claim_opts_e2e;
mod lite_consumer_gc_e2e;
mod lite_consumer_gc_proc_e2e;
mod lite_delay_e2e;
mod lite_delay_migrate_e2e;
mod lite_delay_proc_e2e;
mod lite_dlq_e2e;
mod lite_dlq_triggers_e2e;
mod lite_e2e;
mod lite_group_dlq_e2e;
mod lite_group_e2e;
mod lite_multistream_e2e;
mod lite_ordered_e2e;
mod lite_pel_e2e;
mod lite_proc_e2e;
mod lite_reap_e2e;
mod lite_redeliver_e2e;
mod lite_redeliver_proc_e2e;
mod lite_streams_e2e;
mod lite_trim_minid_e2e;
mod lite_xinfo_full_e2e;
