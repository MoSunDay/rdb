//! Distributed SQL data plane on top of the RESP/openraft/RocksDB stack.
//!
//! MySQL-protocol frontend (`front`), sqlparser-based IR (`parse`),
//! raft-replicated catalog + row codec (`storage`), executor (`exec`),
//! planner (`plan`), secondary indexes (`index`), transactions (`tx`),
//! civil DATE/DATETIME math (`temporal`) and the node-to-node
//! scatter-gather/2PC layer (`dist`).

pub mod columnar;
pub mod dist;
pub mod exec;
pub mod front;
pub mod index;
pub mod parse;
pub mod plan;
pub mod storage;
pub mod temporal;
/// Embedded turso engine (second SQL engine behind the MySQL front,
/// cfg `turso-engine`; runtime-gated by conf `turso_engine` +
/// `turso_db_dir`). See plans/2026-11-09-turso-engine.
#[cfg(feature = "turso-engine")]
pub mod turso;
pub mod tx;
