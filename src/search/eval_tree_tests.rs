//! Unit tests for the pure helpers of `eval_tree.rs` (position-chain
//! verification; the store-backed eval paths are covered by the
//! process-level e2e suites, except the stopword short-circuit which
//! must hold before any read).

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
        },
    )
    .unwrap();
    assert!(hits.is_empty());
    let hits = eval_node(
        &mut ctx,
        &QueryNode::Phrase {
            field: None,
            terms: vec![b"the".to_vec(), b"of".to_vec()],
        },
    )
    .unwrap();
    assert!(hits.is_empty());
}
