//! Kafka-wire front e2e tests, aggregated into one crate
//! (tests-reorg 2026-11-10).

#[path = "../common/mod.rs"]
mod common;

#[path = "../kafka_front_common/mod.rs"]
mod kafka_front_common;

mod kafka_admin_e2e;
mod kafka_codec_e2e;
mod kafka_fetch_e2e;
mod kafka_group_e2e;
mod kafka_group_failover_e2e;
mod kafka_headers_fidelity_e2e;
mod kafka_headers_roundtrip_e2e;
mod kafka_offsets_e2e;
mod kafka_produce_e2e;
mod kafka_rename_ledger_e2e;
mod kafka_topics_e2e;
mod kafka_wire_e2e;
