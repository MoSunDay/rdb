//! rdb: Redis-cluster-compatible persistent KV store (Rust rewrite of the Go rdb).
//!
//! Layout mirrors the Go implementation: conf/hash/utils are foundations,
//! store wraps rocksdb, resp+router+command form the data plane,
//! rcache is the raft control plane, monitor exposes Prometheus metrics.

pub mod command;
pub mod conf;
pub mod ds;
pub mod hash;
pub mod lite;
pub mod monitor;
pub mod park;
pub mod rcache;
pub mod resp;
pub mod router;
pub mod rtypes;
pub mod state;
pub mod store;
pub mod topology;
pub mod utils;
