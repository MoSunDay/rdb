//! Application-layer transactions: MULTI/EXEC/DISCARD/WATCH/UNWATCH.
//!
//! Isolation is a byte-sorted key-latch set held across the whole EXEC
//! replay (see `ds::latch`), NOT an engine-level OCC transaction -- the
//! OptimisticTransactionDB route was evaluated and rejected (see
//! `store::rocksdb::tests::occ_engine_evaluation_record`): base-store
//! writes would surface `Resource busy` failures on the hot path, family
//! delete_range is unavailable in transactional batches, and staged
//! writes are invisible to command read paths (no read-your-writes).
//!
//! Modules:
//! * `keyspec` -- queue-time key extraction (single-slot enforcement).
//! * `session` -- per-connection MULTI/WATCH state machine (pure data).
//! * `watch`   -- full-family value hashing for optimistic WATCH checks.
//! * `exec`    -- the EXEC engine: latch, validate, replay, reply.

pub mod exec;
pub mod keyspec;
pub mod session;
pub mod watch;
