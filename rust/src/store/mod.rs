//! Storage layer (Rust rewrite of Go `internal/store`).
//!
//! The Go original persists data with cockroachdb/pebble; this rewrite uses
//! RocksDB as the embedded engine instead. Only the engine changes: the
//! physical key format is identical -- every key is stored as
//! `"<decimal-slot>/" + key` (the caller supplies the prefix via
//! [`slot_prefix`], no padding). All writes are synchronous, matching Go's
//! `pebble.Sync` usage everywhere.
//!
//! Two intentional deviations fix known Go bugs (documented in `rocksdb.rs`):
//! - `del` returns whether the key existed (Go always replied success).
//! - `size` returns an estimated key count (Go always returned 0).

pub mod ops;
pub mod rocksdb;

pub use ops::*;
pub use rocksdb::*;
