//! Elasticsearch-compatible frontend (HTTP + JSON DSL) over the SAME
//! search kernel as FT.*: one index = one user key = one slot.
//! Layers:
//! - `dsl` parses a `_search` body into a pure `SearchPlan` (`opts`
//!   carries the knn/sort/_source knobs); `eval` turns each query
//!   clause into a (docid -> score) candidate set; `exec` sorts,
//!   windows and shapes the reply (`source` holds `_source` helpers).
//! - `mapping`: ES mappings JSON <-> kernel schema.
//! - `write`: index lifecycle + doc mutations (index-key latch, one
//!   fsync per mutation, MOVED-equivalent slot refusal).
//! - `reply` / `bulk` / `misc` / `router` / `http`: the ES wire --
//!   JSON envelopes, NDJSON `_bulk`, meta endpoints, dispatch and the
//!   hand-rolled HTTP transport.

pub mod bulk;
pub mod dsl;
pub mod eval;
pub mod exec;
pub mod http;
pub mod mapping;
pub mod misc;
pub mod opts;
pub mod reply;
pub mod router;
pub mod source;
pub mod write;
