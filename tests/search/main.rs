//! FT.* full-text search e2e tests, aggregated into one crate
//! (tests-reorg 2026-11-10).

#[path = "../common/mod.rs"]
mod common;

mod ft_highlight_e2e;
mod search_e2e;
mod wire_ft_config_e2e;
