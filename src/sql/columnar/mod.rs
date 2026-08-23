//! Columnar table engine: append-only, versioned segment files.
//!
//! Data lives in immutable files under `<store_path>/<bind>/columnar/`
//! (never inside RocksDB, whose LSM compaction would rewrite them);
//! RocksDB holds only the small segment meta records (kind 0x23).
//! M1 scope: the on-disk segment format (magic | column pages |
//! JSON footer | footer_len | crc32) plus PLAIN and string DICT page
//! encodings with per-page zonemaps.
pub mod decode;
pub mod encode;
pub mod format;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
