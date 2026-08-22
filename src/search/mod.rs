//! Search-engine kernel on the RESP data plane (FT.* commands): a
//! search index is ONE user key living in ONE slot (hash-tag it for
//! multi-key colocated indexes), persisted through the standard
//! envelope + TTL + lazy-purge machinery (`ds::codec::SEARCH_FAMILY`).
//!
//! Layers (all pure functions + data carriers, no classes):
//! - `index_codec`: physical record codecs (meta/doc/posting/termstat/
//!   centroids/ANN partitions, kinds 0x13..=0x18)
//! - `tokenize`: jieba + Unicode hybrid CJK/Latin tokenizer (index and
//!   query side share it), bigram fallback for unknown Han runs
//! - `bm25`: Okapi BM25 scoring + bounded top-k collector
//! - `ft_index`: inverted-index read-modify-write paths (one fsync per
//!   FT.ADD/FT.DEL via the index-key latch)
//! - `ft_cmd` / `ft_search`: RESP command handlers
//! - `vecmath` / `quant` / `ann`: shared vector math, SQ8 scalar
//!   quantization, SPANN-style disk ANN (centroids in one record,
//!   SQ8 partition postings on disk, probe + exact rerank)
//!
//! Cross-node queries are CLIENT fan-out (Redis Cluster convention):
//! no new server RPCs; every node only promises node-local top-k.

pub mod ann;
pub mod bm25;
pub mod ft_build;
pub mod ft_cmd;
pub mod ft_index;
pub mod ft_query;
pub mod ft_search;
pub mod index_codec;
pub mod quant;
pub mod tokenize;
pub mod vecmath;
