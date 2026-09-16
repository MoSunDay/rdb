//! SQL transaction machinery: timestamp oracle (M1: node-local) and,
//! from M2 on, MVCC snapshot sessions, conflict detection and GC.

pub mod floor;
pub mod global;
pub mod latch;
pub mod nodes;
pub mod session;
pub mod ts;

pub use global::ClusterTs;
pub use session::{
    begin, commit, conflict_check, merge_rows, release_savepoint, rollback, rollback_to, savepoint,
    stage_append, stage_delete, stage_upsert, unknown_savepoint, Savepoint, Txn, TxnKey, TxnWrite,
};
pub use ts::Oracle;
