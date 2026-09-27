//! Shared parts of the backup-surface sweep: the ALLOWED table +
//! predicates (`table`) and the seeding/readiness helpers (`seed`).
//! Mounted from `tests/backup_surface_e2e.rs` (same pattern as
//! `kafka_front_common`; only some helpers are used per mounting site).

#![allow(dead_code)]

pub mod seed;
pub mod table;

/// The exact gate error line (leading `-`, CRLF stripped by the reader).
pub const GATE: &[u8] = b"-READONLY You can't write against a read only replica.";
