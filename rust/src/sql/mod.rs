//! Distributed SQL data plane on top of the RESP/openraft/RocksDB stack.
//!
//! MySQL-protocol frontend (`front`), sqlparser-based IR (`parse`),
//! raft-replicated catalog + row codec (`storage`), executor (`exec`),
//! planner (`plan`), secondary indexes (`index`), transactions (`tx`) and
//! the node-to-node scatter-gather/2PC layer (`dist`).

pub mod exec;
pub mod front;
pub mod parse;
pub mod storage;
pub mod tx;
