//! rdb: Redis-cluster-compatible persistent KV store (Rust rewrite of the Go rdb).
//!
//! Layout mirrors the Go implementation: conf/hash/utils are foundations,
//! store wraps rocksdb, resp+router+command form the data plane,
//! rcache is the raft control plane, monitor exposes Prometheus metrics.
//!
//! Feature split: `store` exposes only the RocksDB KV layer (`src/store`)
//! so an embedder can build without the raft/SQL/search stack; `full`
//! (default) adds every other module. See `Cargo.toml` `[features]`.

pub mod store;
// Compile-time tokio_unstable guard; see build_guard.rs.
pub mod build_guard;

#[cfg(feature = "full")]
pub mod command;
#[cfg(feature = "full")]
pub mod conf;
#[cfg(feature = "full")]
pub mod ds;
#[cfg(feature = "full")]
pub mod hash;
#[cfg(feature = "full")]
pub mod lite;
#[cfg(feature = "full")]
pub mod monitor;
#[cfg(feature = "full")]
pub mod park;
#[cfg(feature = "full")]
pub mod rcache;
#[cfg(feature = "full")]
pub mod resp;
#[cfg(feature = "full")]
pub mod router;
#[cfg(feature = "full")]
pub mod rtypes;
#[cfg(feature = "full")]
pub mod search;
#[cfg(feature = "full")]
pub mod sql;
#[cfg(feature = "full")]
pub mod state;
#[cfg(feature = "full")]
pub mod topology;
#[cfg(feature = "full")]
pub mod tx;
#[cfg(feature = "full")]
pub mod utils;
