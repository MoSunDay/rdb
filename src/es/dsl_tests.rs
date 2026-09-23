//! Unit tests for `es::dsl` + `es::opts`: request defaults,
//! every clause form, sort/_source shapes, knn defaults and the
//! error envelope (window limit, unknown query type).

use super::*;
use crate::es::opts::parse_sort;

#[test]
fn defaults_for_empty_bodies() {
    for body in [&b""[..], b"{}", b"   "] {
        let p = parse_search(body).unwrap();
        assert!(matches!(p.query, Plan::MatchAll));
        assert!(p.knn.is_none() && p.sort.is_empty());
        assert_eq!((p.from, p.size), (0, 10));
        assert!(!p.source.disabled && p.source.includes.is_empty());
    }
    let p = parse_search(br#"{"track_total_hits": false}"#).unwrap(); // unknown key ignored
    assert!(matches!(p.query, Plan::MatchAll));
}

#[test]
fn match_shorthand_and_operator() {
    let p = parse_search(br#"{"query":{"match":{"title":"Hello World"}}}"#).unwrap();
    match p.query {
        Plan::Match {
            field,
            terms,
            operator,
        } => {
            assert_eq!(field, "title");
            assert_eq!(terms, vec![b"hello".to_vec(), b"world".to_vec()]);
            assert_eq!(operator, BoolOp::Or);
        }
        other => panic!("{other:?}"),
    }
    let p = parse_search(br#"{"query":{"match":{"t":{"query":"a b","operator":"and"}}}}"#).unwrap();
    match p.query {
        Plan::Match { operator, .. } => assert_eq!(operator, BoolOp::And),
        other => panic!("{other:?}"),
    }
    assert!(parse_search(br#"{"query":{"match":{"t":{"operator":"xor"}}}}"#).is_err());
}

#[test]
fn term_terms_and_range() {
    let p = parse_search(br#"{"query":{"term":{"tag":"red"}}}"#).unwrap();
    match p.query {
        Plan::Term { field, value } => {
            assert_eq!(field, "tag");
            assert_eq!(value, TermValue::Bytes(b"red".to_vec()));
        }
        other => panic!("{other:?}"),
    }
    assert!(parse_search(br#"{"query":{"term":{"ok":true}}}"#).is_err());
    let p = parse_search(br#"{"query":{"terms":{"n":[1,"x"]}}}"#).unwrap();
    match p.query {
        Plan::Terms { values, .. } => {
            assert_eq!(values[0], TermValue::Number(1.0));
            assert_eq!(values[1], TermValue::Bytes(b"x".to_vec()));
        }
        other => panic!("{other:?}"),
    }
    let p = parse_search(br#"{"query":{"range":{"p":{"gte":1,"lt":5}}}}"#).unwrap();
    match p.query {
        Plan::Range {
            field,
            gte,
            gt,
            lte,
            lt,
        } => {
            assert_eq!(field, "p");
            assert_eq!((gte, gt, lte, lt), (Some(1.0), None, None, Some(5.0)));
        }
        other => panic!("{other:?}"),
    }
    assert!(parse_search(br#"{"query":{"range":{"p":{"gte":"now"}}}}"#).is_err());
}

#[test]
fn bool_nesting_and_arrays() {
    let p = parse_search(
        br#"{"query":{"bool":{"must":{"term":{"a":"x"}},"should":[{"term":{"b":1}},{"match_all":{}}],"must_not":{"range":{"c":{"gt":2}}}}}}"#,
    )
    .unwrap();
    match p.query {
        Plan::Bool {
            must,
            filter,
            should,
            must_not,
        } => {
            assert_eq!(must.len(), 1);
            assert!(filter.is_empty());
            assert_eq!(should.len(), 2);
            assert!(matches!(must_not[0], Plan::Range { .. }));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn sort_forms() {
    let p = parse_search(br#"{"sort":["_score","_doc",{"price":{"order":"desc"}},"name",{"_score":{"order":"asc"}}]}"#).unwrap();
    assert_eq!(
        p.sort,
        vec![
            SortKey::Score(SortDir::Desc),
            SortKey::Doc,
            SortKey::Field {
                field: "price".into(),
                dir: SortDir::Desc
            },
            SortKey::Field {
                field: "name".into(),
                dir: SortDir::Asc
            },
            SortKey::Score(SortDir::Asc),
        ]
    );
    assert!(parse_sort(&serde_json::json!([{"price": {"order": "up"}}])).is_err());
}

#[test]
fn source_forms() {
    assert!(
        parse_search(br#"{"_source":false}"#)
            .unwrap()
            .source
            .disabled
    );
    let p = parse_search(br#"{"_source":["a.b"]}"#).unwrap();
    assert_eq!(p.source.includes, vec!["a.b".to_string()]);
    let p =
        parse_search(br#"{"_source":{"includes":"x","excludes":["y"],"exclude":"z"}}"#).unwrap();
    assert_eq!(p.source.includes, vec!["x".to_string()]);
    assert_eq!(p.source.excludes, vec!["y".to_string(), "z".to_string()]);
}

#[test]
fn knn_defaults_and_bounds() {
    let p = parse_search(br#"{"knn":{"field":"v","query_vector":[1,2,3]}}"#).unwrap();
    let k = p.knn.unwrap();
    assert_eq!((k.k, k.num_candidates), (10, 100));
    assert!(k.filter.is_none());
    let p = parse_search(br#"{"knn":{"field":"v","query_vector":[1],"k":2000}}"#).unwrap();
    assert_eq!(p.knn.unwrap().num_candidates, 10_000); // max(2000*10,100) capped
    let e = parse_search(br#"{"knn":{"field":"v","query_vector":[1],"k":0}}"#).unwrap_err();
    assert_eq!(e.es_type, "illegal_argument_exception");
    assert!(parse_search(br#"{"knn":{"field":"v","query_vector":[]}}"#).is_err());
    let p =
        parse_search(br#"{"knn":{"field":"v","query_vector":[1],"filter":{"term":{"t":"x"}}}}"#)
            .unwrap();
    assert!(p.knn.unwrap().filter.is_some());
}

#[test]
fn window_limit_and_unknown_query() {
    let e = parse_search(br#"{"from":9999,"size":10}"#).unwrap_err();
    assert_eq!(
        (e.status, e.es_type.as_str()),
        (400, "illegal_argument_exception")
    );
    assert!(e.reason.contains("Result window is too large"));
    let e = parse_search(br#"{"query":{"foo":{}}}"#).unwrap_err();
    assert_eq!(e.es_type, "parsing_exception");
    assert!(e.reason.contains("unknown query type 'foo'"));
}
