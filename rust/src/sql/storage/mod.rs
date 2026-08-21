//! SQL storage: schemas (raft-replicated catalog) and physical row codecs
//! (RocksDB, slot-prefixed keys shared with the RESP data plane).

pub mod catalog;
pub mod codec;
pub mod gc;
pub mod row;
pub mod schema;
