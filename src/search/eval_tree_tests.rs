//! Unit tests for the pure helpers of `eval_tree.rs` (position-chain
//! verification; the store-backed eval paths are covered by the
//! process-level e2e suites, except the stopword short-circuit and
//! the all-stopword KEYWORD phrase boundary, which get a small
//! store-backed harness here: put_meta + build_add_batch).

use super::verify_positions;
use super::{eval_node, new_tree_ctx};
use crate::search::ft_index::IndexMeta;
use crate::search::query_tree::QueryNode;

#[test]
fn single_term_chain_matches() {
    assert!(verify_positions(&[&[3]]));
    assert!(!verify_positions(&[&[]]));
}

#[test]
fn adjacency_chains() {
    let a: &[u64] = &[0, 4, 9];
    let b: &[u64] = &[5, 10, 20];
    // 4+1=5 in b -> match
    assert!(verify_positions(&[a, b]));
    let c: &[u64] = &[6, 21];
    // after a->b only cursor 4 survives (5 in b); 5+1=6 in c -> match
    assert!(verify_positions(&[a, b, c]));
    let far: &[u64] = &[100];
    assert!(!verify_positions(&[a, far]));
}

#[test]
fn chain_requires_contiguous_run_not_just_membership() {
    // x at 0 and 5; y at 1 only; z at 6 only: no contiguous run
    let x: &[u64] = &[0, 5];
    let y: &[u64] = &[1];
    let z: &[u64] = &[6];
    assert!(!verify_positions(&[x, y, z]));
}

#[test]
fn bigram_tokens_are_adjacent_ordinals() {
    // CJK bigrams overlap by one CHAR but are adjacent TOKENS, so
    // ordinals 0,1 chain fine.
    let t0: &[u64] = &[0];
    let t1: &[u64] = &[1, 7];
    let t2: &[u64] = &[2];
    assert!(verify_positions(&[t0, t1, t2]));
}

/// 停用词 term 与全停用词短语在查询侧直接短路为空命中集（短路先于
/// 任何 posting 读取，索引里本就没有这些词的 posting）。
#[test]
fn stopword_term_and_phrase_match_nothing() {
    let shared = crate::state::testutil::shared_with(crate::state::testutil::test_config());
    let meta = IndexMeta {
        stopwords: vec![b"the".to_vec(), b"of".to_vec()],
        ..Default::default()
    };
    let mut ctx = new_tree_ctx(&shared.store, b"pfx", b"idx", &meta);
    let hits = eval_node(
        &mut ctx,
        &QueryNode::Term {
            field: None,
            term: b"the".to_vec(),
            raw: b"the".to_vec(),
        },
    )
    .unwrap();
    assert!(hits.is_empty());
    let hits = eval_node(
        &mut ctx,
        &QueryNode::Phrase {
            field: None,
            terms: vec![b"the".to_vec(), b"of".to_vec()],
            raw: b"the of".to_vec(),
        },
    )
    .unwrap();
    assert!(hits.is_empty());
}

/// 全停用词短语在 KEYWORD scope 上仍走 raw 精确字节分支：停用词
/// 短路只属于 TEXT 路径（曾是函数级提前 return，keyword 分支根本
/// 到不了 -- 修复回归）。store-backed 装置：meta 记录 + 一条 KEYWORD
/// 文档，全程同步写（put_meta + build_add_batch + batch_write）。
#[test]
fn all_stopword_phrase_matches_keyword_raw_bytes() {
    let shared = crate::state::testutil::shared_with(crate::state::testutil::test_config());
    let meta = IndexMeta {
        fields: vec![crate::search::ft_index::IndexField {
            name: b"kw".to_vec(),
            ftype: crate::search::ft_index::FieldType::Keyword,
        }],
        stopwords: vec![b"the".to_vec(), b"of".to_vec()],
        ..Default::default()
    };
    let mut batch = rocksdb::WriteBatch::default();
    crate::search::ft_index::put_meta(&mut batch, b"pfx", b"idx", &meta);
    crate::store::ops::batch_write(&shared.store, batch).unwrap();
    let (rec, numvals, positions) =
        crate::search::ft_cmd::doc_record_of(&meta, br#"{"kw":"the of"}"#).unwrap();
    let add = crate::search::ft_index::build_add_batch(
        &shared.store,
        b"pfx",
        b"idx",
        &meta,
        b"k3",
        rec,
        &numvals,
        &positions,
    )
    .unwrap();
    crate::store::ops::batch_write(&shared.store, add).unwrap();

    let mut ctx = new_tree_ctx(&shared.store, b"pfx", b"idx", &meta);
    // tokenized terms are ALL stopwords, yet the KEYWORD raw path hits
    let hits = eval_node(
        &mut ctx,
        &QueryNode::Phrase {
            field: Some(b"kw".to_vec()),
            terms: vec![b"the".to_vec(), b"of".to_vec()],
            raw: b"the of".to_vec(),
        },
    )
    .unwrap();
    assert_eq!(hits.keys().collect::<Vec<_>>(), vec![&b"k3"[..].to_vec()]);
    // wrong raw bytes -> no hit (exact, not prefix/token match)
    let hits = eval_node(
        &mut ctx,
        &QueryNode::Phrase {
            field: Some(b"kw".to_vec()),
            terms: vec![b"the".to_vec(), b"of".to_vec()],
            raw: b"the off".to_vec(),
        },
    )
    .unwrap();
    assert!(hits.is_empty());
}
