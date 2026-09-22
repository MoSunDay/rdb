//! `_source` shaping and field-sort value extraction for the ES
//! executor: dotted-path lookup, include/exclude pattern filtering
//! over `serde_json::Value`, and the `SortVal` scalar + comparator
//! field sorts use. Pure functions, unit-tested here.

use std::cmp::Ordering;

use serde_json::{Map, Value};

use super::dsl::{SortDir, SourceFilter};

/// First value at a dotted path ("a.b.c"); plain segment names (ES
/// field paths, not JSON Pointer escapes).
pub fn path_get<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(v);
    }
    path.split('.').try_fold(v, |acc, seg| acc.get(seg))
}

/// Does `path` fall under `pattern`? Exact match, subtree match, or
/// trailing-`*` wildcard matching any remainder AT that segment
/// boundary ("a.b*" covers "a.b" and "a.b.c", not "a.bee" -- a
/// documented v1 simplification of ES glob semantics).
fn covers(pattern: &str, path: &str) -> bool {
    let base = pattern.strip_suffix('*').unwrap_or(pattern);
    let base = base.strip_suffix('.').unwrap_or(base);
    base.is_empty() || path == base || path.starts_with(&format!("{base}."))
}

/// Apply the `_source` filter: disabled -> Null, no patterns -> clone,
/// otherwise walk keeping the nodes that lie on an include path minus
/// excluded paths. Arrays are kept whole (v1: no per-element paths).
pub fn filter_source(v: &Value, f: &SourceFilter) -> Value {
    if f.disabled {
        return Value::Null;
    }
    if f.includes.is_empty() && f.excludes.is_empty() {
        return v.clone();
    }
    walk(v, "", f).unwrap_or(Value::Object(Map::new()))
}

/// `Some` = this subtree survives (possibly pruned); `None` = drop.
fn walk(v: &Value, path: &str, f: &SourceFilter) -> Option<Value> {
    let Value::Object(map) = v else {
        // scalar/array leaf: kept when nothing is included (only
        // excludes shape the output) or an include path covers it
        return if f.includes.is_empty() || f.includes.iter().any(|p| covers(p, path)) {
            Some(v.clone())
        } else {
            None
        };
    };
    let mut out = Map::new();
    for (k, child) in map {
        let child_path = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
        if f.excludes.iter().any(|p| covers(p, &child_path)) {
            continue;
        }
        if f.includes.iter().any(|p| covers(p, &child_path)) {
            out.insert(k.clone(), child.clone()); // whole subtree covered
            continue;
        }
        if let Some(filtered) = walk(child, &child_path, f) {
            // drop objects pruned down to nothing (their leaves all
            // fell outside the includes)
            if !matches!(&filtered, Value::Object(m) if m.is_empty()) {
                out.insert(k.clone(), filtered);
            }
        }
    }
    Some(Value::Object(out))
}

/// Sort key extracted from a source field value: numbers compare
/// numerically, any other present value by its JSON bytes (v1:
/// strings/bools/arrays together, no per-type collation), null or
/// absent = missing (sorts last regardless of direction).
#[derive(Debug, Clone, PartialEq)]
pub enum SortVal {
    Num(f64),
    Str(Vec<u8>),
    Missing,
}

pub fn sortval_of(v: &Value) -> SortVal {
    match v {
        Value::Null => SortVal::Missing,
        Value::Number(x) => SortVal::Num(x.as_f64().unwrap_or(f64::NAN)),
        other => SortVal::Str(other.to_string().into_bytes()),
    }
}

/// Compare two sort values under `dir`; MISSING sorts LAST in both
/// directions (ES convention). Numbers order before strings when a
/// field mixes both (cannot happen under one mapping in practice).
pub fn cmp_sortval(a: &SortVal, b: &SortVal, dir: SortDir) -> Ordering {
    match (a, b) {
        (SortVal::Missing, SortVal::Missing) => Ordering::Equal,
        (SortVal::Missing, _) => Ordering::Greater,
        (_, SortVal::Missing) => Ordering::Less,
        (SortVal::Num(x), SortVal::Num(y)) => flip(x.total_cmp(y), dir),
        (SortVal::Str(x), SortVal::Str(y)) => flip(x.cmp(y), dir),
        (SortVal::Num(_), SortVal::Str(_)) => flip(Ordering::Less, dir),
        (SortVal::Str(_), SortVal::Num(_)) => flip(Ordering::Greater, dir),
    }
}

fn flip(o: Ordering, dir: SortDir) -> Ordering {
    if dir == SortDir::Desc {
        o.reverse()
    } else {
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn path_get_walks_dotted_paths() {
        let v = json!({"a": {"b": {"c": 1}}, "ab": 2});
        assert_eq!(path_get(&v, "a.b.c"), Some(&json!(1)));
        assert_eq!(path_get(&v, "ab"), Some(&json!(2)));
        assert_eq!(path_get(&v, "a.x"), None);
        assert_eq!(path_get(&v, ""), Some(&v)); // empty path = root
    }

    #[test]
    fn filter_source_include_exclude_wildcards() {
        let v = json!({"a": {"b": 1, "c": 2}, "d": 3, "e": {"f": 4}});
        let f = SourceFilter { includes: vec!["a.b".into(), "d".into()], ..Default::default() };
        assert_eq!(filter_source(&v, &f), json!({"a": {"b": 1}, "d": 3}));
        let f = SourceFilter { excludes: vec!["a.*".into()], ..Default::default() };
        assert_eq!(filter_source(&v, &f), json!({"d": 3, "e": {"f": 4}}));
        let f = SourceFilter { includes: vec!["a.z".into()], ..Default::default() };
        assert_eq!(filter_source(&v, &f), json!({})); // nothing matched
        let f = SourceFilter { disabled: true, ..Default::default() };
        assert_eq!(filter_source(&v, &f), Value::Null);
        let f = SourceFilter::default();
        assert_eq!(filter_source(&v, &f), v); // no patterns = whole doc
    }

    #[test]
    fn sortval_ordering_and_missing_last() {
        let asc = SortDir::Asc;
        let desc = SortDir::Desc;
        let n1 = SortVal::Num(1.0);
        let n2 = SortVal::Num(2.0);
        let s = SortVal::Str(b"abc".to_vec());
        assert_eq!(cmp_sortval(&n1, &n2, asc), Ordering::Less);
        assert_eq!(cmp_sortval(&n1, &n2, desc), Ordering::Greater);
        assert_eq!(cmp_sortval(&n1, &s, asc), Ordering::Less); // nums first
        assert_eq!(cmp_sortval(&s, &n1, asc), Ordering::Greater);
        for dir in [asc, desc] {
            assert_eq!(cmp_sortval(&SortVal::Missing, &n1, dir), Ordering::Greater);
            assert_eq!(cmp_sortval(&n1, &SortVal::Missing, dir), Ordering::Less);
        }
        assert_eq!(sortval_of(&json!(3.5)), SortVal::Num(3.5));
        assert_eq!(sortval_of(&json!("x")), SortVal::Str(b"\"x\"".to_vec()));
        assert_eq!(sortval_of(&Value::Null), SortVal::Missing);
    }
}
