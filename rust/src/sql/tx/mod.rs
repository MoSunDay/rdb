//! SQL transaction machinery: timestamp oracle (M1: node-local) and,
//! from M2 on, MVCC snapshot sessions, conflict detection and GC.

pub mod global;
pub mod nodes;
pub mod session;
pub mod ts;

pub use global::ClusterTs;
pub use session::{
    begin, commit, conflict_check, merge_rows, rollback, stage_delete, stage_upsert, Txn, TxnKey,
    TxnWrite,
};
pub use ts::Oracle;
