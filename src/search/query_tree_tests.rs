//! Unit tests for the shared query-tree parser (`query_tree.rs`).

use super::{parse_query_tree, QueryDialect, QueryNode};

fn ft(q: &str) -> Option<QueryNode> {
    parse_query_tree(q.as_bytes(), QueryDialect::Ft).unwrap()
}

fn qs(q: &str) -> Option<QueryNode> {
    parse_query_tree(q.as_bytes(), QueryDialect::QueryString).unwrap()
}

fn term(t: &str) -> QueryNode {
    QueryNode::Term {
        field: None,
        term: t.as_bytes().to_vec(),
    }
}

#[test]
fn match_all_and_empty() {
    assert_eq!(ft("*"), None);
    assert_eq!(ft(""), Some(QueryNode::And(Vec::new())));
    assert_eq!(ft("   "), Some(QueryNode::And(Vec::new())));
}

#[test]
fn v1_compat_terms_and_scopes() {
    assert_eq!(
        ft("hello world"),
        Some(QueryNode::And(vec![term("hello"), term("world")]))
    );
    assert_eq!(
        ft("@title:redis"),
        Some(QueryNode::Term {
            field: Some(b"title".to_vec()),
            term: b"redis".to_vec(),
        })
    );
    // v1 quirk kept: a bare `foo:bar` is NOT a scope in the Ft
    // dialect -- it tokenizes to two AND-ed terms.
    assert_eq!(
        ft("foo:bar"),
        Some(QueryNode::And(vec![term("foo"), term("bar")]))
    );
    // noise-only clause vanishes
    assert_eq!(ft("hello ,.!"), Some(term("hello")));
}

#[test]
fn keywords_or_and_not() {
    assert_eq!(
        ft("a OR b"),
        Some(QueryNode::Or(vec![term("a"), term("b")]))
    );
    assert_eq!(ft("a | b"), Some(QueryNode::Or(vec![term("a"), term("b")])));
    assert_eq!(
        ft("a b | c"),
        Some(QueryNode::And(vec![
            term("a"),
            QueryNode::Or(vec![term("b"), term("c")])
        ]))
    );
    assert_eq!(
        ft("hello -world"),
        Some(QueryNode::And(vec![
            term("hello"),
            QueryNode::Not(Box::new(term("world")))
        ]))
    );
    assert_eq!(
        ft("hello NOT world"),
        Some(QueryNode::And(vec![
            term("hello"),
            QueryNode::Not(Box::new(term("world")))
        ]))
    );
    // lowercase are ordinary terms; AND is implicit
    assert_eq!(
        ft("a and b"),
        Some(QueryNode::And(vec![term("a"), term("and"), term("b")]))
    );
    assert_eq!(
        ft("a AND b"),
        Some(QueryNode::And(vec![term("a"), term("b")]))
    );
}

#[test]
fn phrases_and_prefix() {
    assert_eq!(
        ft(r#""full text search""#),
        Some(QueryNode::Phrase {
            field: None,
            terms: vec![b"full".to_vec(), b"text".to_vec(), b"search".to_vec()],
        })
    );
    // single-token phrase folds to a Term; scope rides through
    assert_eq!(
        ft(r#"@title:"redis""#),
        Some(QueryNode::Term {
            field: Some(b"title".to_vec()),
            term: b"redis".to_vec(),
        })
    );
    assert_eq!(
        ft("redi*"),
        Some(QueryNode::Prefix {
            field: None,
            prefix: b"redi".to_vec(),
        })
    );
    assert_eq!(
        ft("@title:red*"),
        Some(QueryNode::Prefix {
            field: Some(b"title".to_vec()),
            prefix: b"red".to_vec(),
        })
    );
    // star alone tokenizes to nothing -> star dropped, not a prefix
    assert_eq!(ft("hello *"), Some(term("hello")));
}

#[test]
fn query_string_dialect() {
    assert_eq!(
        qs("title:redis AND 向量"),
        Some(QueryNode::And(vec![
            QueryNode::Term {
                field: Some(b"title".to_vec()),
                term: b"redis".to_vec(),
            },
            QueryNode::Term {
                field: None,
                // CJK single term survives segmentation as one token
                term: "向量".as_bytes().to_vec(),
            },
        ]))
    );
    // bare scope needs a letter: 12:30 stays plain terms
    assert_eq!(
        qs("12:30"),
        Some(QueryNode::And(vec![term("12"), term("30")]))
    );
    assert_eq!(
        qs("NOT tag:redis"),
        Some(QueryNode::Not(Box::new(QueryNode::Term {
            field: Some(b"tag".to_vec()),
            term: b"redis".to_vec(),
        })))
    );
    assert_eq!(
        qs(r#"title:"向量检索""#),
        Some(QueryNode::Phrase {
            field: Some(b"title".to_vec()),
            terms: vec!["向量".as_bytes().to_vec(), "检索".as_bytes().to_vec()],
        })
    );
}

#[test]
fn unterminated_phrase_takes_rest() {
    assert_eq!(
        ft(r#""full text"#),
        Some(QueryNode::Phrase {
            field: None,
            terms: vec![b"full".to_vec(), b"text".to_vec()],
        })
    );
}

#[test]
fn numeric_ranges() {
    let numrange = |f: &str, gte: Option<f64>, lte: Option<f64>| QueryNode::NumRange {
        field: f.as_bytes().to_vec(),
        gte,
        lte,
    };
    // 闭区间；`*`/`±inf`（大小写不敏感）= 开界
    assert_eq!(
        ft("@n:[10 20]"),
        Some(numrange("n", Some(10.0), Some(20.0)))
    );
    assert_eq!(ft("@n:[-inf +inf]"), Some(numrange("n", None, None)));
    assert_eq!(ft("@n:[10 *]"), Some(numrange("n", Some(10.0), None)));
    assert_eq!(
        ft("@n:[-INF 1.5e2]"),
        Some(numrange("n", None, Some(150.0)))
    );
    // qs 方言：裸 scope 同样产 NumRange（共享 parser）
    assert_eq!(
        qs("price:[1.5 2.5]"),
        Some(numrange("price", Some(1.5), Some(2.5)))
    );
    // 非法区间 = 硬错误，不静默丢子句
    assert_eq!(
        parse_query_tree(b"@n:[10]", QueryDialect::Ft),
        Err("ERR bad numeric range")
    );
    assert_eq!(
        parse_query_tree(b"@n:[a b]", QueryDialect::Ft),
        Err("ERR bad numeric range")
    );
    assert_eq!(
        parse_query_tree(b"@n:[1 2 3]", QueryDialect::Ft),
        Err("ERR bad numeric range")
    );
    // 取反自然包 Not；与其他子句按隐式 AND 组合
    assert_eq!(
        ft("-@n:[1 5]"),
        Some(QueryNode::Not(Box::new(numrange(
            "n",
            Some(1.0),
            Some(5.0)
        ))))
    );
    assert_eq!(
        ft("@n:[1 5] hello"),
        Some(QueryNode::And(vec![
            numrange("n", Some(1.0), Some(5.0)),
            term("hello")
        ]))
    );
    // 无 scope 的 `[..]` 不触发区间语法，走旧 word 路径（`[1` -> 词 "1"）
    assert_eq!(
        ft("[1 5]"),
        Some(QueryNode::And(vec![term("1"), term("5")]))
    );
    // 未闭合的 `[` 吞到输入结尾（短语约定）
    assert_eq!(ft("@n:[10 20"), Some(numrange("n", Some(10.0), Some(20.0))));
}
