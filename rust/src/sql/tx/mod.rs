//! SQL transaction machinery: timestamp oracle (M1: node-local) and,
//! from M2 on, MVCC snapshot sessions, conflict detection and GC.

pub mod ts;

pub use ts::Oracle;
