//! Legacy RedisJSON v1 deterministic paths -- the only navigation model
//! the JSON commands accept. Grammar (no wildcards, no recursive
//! descent, no filters; anything else is a parse error):
//!
//! ```text
//! path    = root | [ "$" ] seg*
//! root    = "." | "$"          (the empty segment list)
//! seg     = "." name           name = 1+ bytes from {. [ ]}
//!         | "['" name "']"     name = 0+ bytes except '
//!         | "[" index "]"      index = decimal i64 (optional -)
//! ```
//!
//! Navigation is over `serde_json::Value` trees with plain free
//! functions: [`get_at`]/[`get_at_mut`] descend (missing/wrong-type
//! steps read as absent; negative indexes NEVER resolve in reads),
//! [`set_at`] is the JSON.SET writer (auto-creates missing object
//! fields, appends at `index == len`), [`remove_at`] is the JSON.DEL
//! writer (object field removal / array removal with shift).

use serde_json::{Map, Value};

/// One step of a parsed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathSeg {
    /// `.`name / `['`name`']`
    Field(Vec<u8>),
    /// `[`index`]`
    Index(i64),
}

/// Parse one path argument; `Some(vec![])` = root (`.` or `$`).
pub(crate) fn parse_path(arg: &[u8]) -> Option<Vec<PathSeg>> {
    if arg.is_empty() {
        return None; // empty argument: no path at all
    }
    if arg == b"." || arg == b"$" {
        return Some(Vec::new());
    }
    let mut rest = match arg.split_first() {
        Some((b'$', tail)) if !tail.is_empty() => tail,
        _ => arg,
    };
    let mut segs = Vec::new();
    while !rest.is_empty() {
        let (b, tail) = rest.split_first()?;
        match b {
            b'.' => {
                let end = tail
                    .iter()
                    .position(|&c| c == b'.' || c == b'[' || c == b']')
                    .unwrap_or(tail.len());
                if end == 0 {
                    return None; // `..` / trailing `.` / `.[]`
                }
                segs.push(PathSeg::Field(tail[..end].to_vec()));
                rest = &tail[end..];
            }
            b'[' => match tail.split_first()? {
                (b'\'', tail2) => {
                    let end = tail2.iter().position(|&c| c == b'\'')?;
                    let (close, _) = tail2[end + 1..].split_first()?;
                    if close != &b']' {
                        return None; // unterminated / nested brackets
                    }
                    segs.push(PathSeg::Field(tail2[..end].to_vec()));
                    rest = &tail2[end + 2..];
                }
                (_, _) => {
                    let end = tail.iter().position(|&c| c == b']')?;
                    let idx: i64 = std::str::from_utf8(&tail[..end]).ok()?.parse().ok()?;
                    segs.push(PathSeg::Index(idx));
                    rest = &tail[end + 1..];
                }
            },
            _ => return None, // stray bytes (incl. a bare `*`)
        }
    }
    Some(segs)
}

/// Canonical rendering of a parsed path (legacy style, no `$`): root
/// prints `.`; simple fields print `.name`, fields containing `.[]`
/// print `['name']`; indexes print `[i]`.
pub(crate) fn path_display(segs: &[PathSeg]) -> String {
    if segs.is_empty() {
        return ".".to_string();
    }
    let mut out = String::new();
    for seg in segs {
        match seg {
            PathSeg::Field(f) => {
                let simple =
                    !f.is_empty() && !f.iter().any(|&c| c == b'.' || c == b'[' || c == b']');
                if simple {
                    out.push('.');
                    out.push_str(&String::from_utf8_lossy(f));
                } else {
                    out.push_str(&format!(
                        "['{}']",
                        String::from_utf8_lossy(f).replace('\'', "")
                    ));
                }
            }
            PathSeg::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

/// A field segment as a serde_json object key; non-UTF-8 bytes can never
/// match a JSON string key.
fn field_str(field: &[u8]) -> Option<&str> {
    std::str::from_utf8(field).ok()
}

/// First value at `segs`, or `None` when any step is absent or
/// type-mismatched (negative indexes never resolve).
pub(crate) fn get_at<'a>(doc: &'a Value, segs: &[PathSeg]) -> Option<&'a Value> {
    let mut cur = doc;
    for seg in segs {
        cur = match (cur, seg) {
            (Value::Object(m), PathSeg::Field(f)) => m.get(field_str(f)?)?,
            (Value::Array(a), PathSeg::Index(i)) => a.get(usize::try_from(*i).ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Mutable twin of [`get_at`] (mutation helpers keep the borrow chain
/// inside this module so callers never hand-walk the tree).
pub(crate) fn get_at_mut<'a>(doc: &'a mut Value, segs: &[PathSeg]) -> Option<&'a mut Value> {
    let mut cur = doc;
    for seg in segs {
        cur = match (cur, seg) {
            (Value::Object(m), PathSeg::Field(f)) => m.get_mut(field_str(f)?)?,
            (Value::Array(a), PathSeg::Index(i)) => a.get_mut(usize::try_from(*i).ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Why a JSON.SET navigation failed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SetErr {
    /// The path addresses nothing (array index beyond `len`, negative
    /// index, missing array ancestor).
    NotFound,
    /// A step found a container of the wrong kind (field under an
    /// array/scalar, index under an object/scalar).
    WrongType,
}

/// JSON.SET writer: place `new` at `segs`. Empty segs replace the root
/// (always OK). Intermediate missing object fields auto-create empty
/// objects (RedisJSON v1 legacy); the last segment inserts into an
/// object or assigns/appends in an array (`i == len` appends, beyond
/// that or negative is `NotFound`).
pub(crate) fn set_at(doc: &mut Value, segs: &[PathSeg], new: Value) -> Result<(), SetErr> {
    let Some((last, head)) = segs.split_last() else {
        *doc = new;
        return Ok(());
    };
    let mut cur = doc;
    for seg in head {
        cur = match (seg, cur) {
            (PathSeg::Field(f), Value::Object(m)) => m
                .entry(field_str(f).ok_or(SetErr::WrongType)?.to_string())
                .or_insert_with(|| Value::Object(Map::new())),
            (PathSeg::Index(i), Value::Array(a)) => a
                .get_mut(usize::try_from(*i).map_err(|_| SetErr::NotFound)?)
                .ok_or(SetErr::NotFound)?,
            _ => return Err(SetErr::WrongType),
        };
    }
    match (last, cur) {
        (PathSeg::Field(f), Value::Object(m)) => {
            m.insert(field_str(f).ok_or(SetErr::WrongType)?.to_string(), new);
            Ok(())
        }
        (PathSeg::Index(i), Value::Array(a)) => {
            if *i < 0 {
                return Err(SetErr::NotFound);
            }
            let idx = *i as usize;
            if idx > a.len() {
                return Err(SetErr::NotFound);
            }
            if idx == a.len() {
                a.push(new);
            } else {
                a[idx] = new;
            }
            Ok(())
        }
        _ => Err(SetErr::WrongType),
    }
}

/// JSON.DEL writer: remove the element at `segs` from its parent.
/// `false` = nothing was there (missing parent, missing field, index
/// out of range, wrong container kind). Root (empty segs) is the
/// caller's job (whole-key family delete).
pub(crate) fn remove_at(doc: &mut Value, segs: &[PathSeg]) -> bool {
    let Some((last, head)) = segs.split_last() else {
        return false;
    };
    let Some(parent) = get_at_mut(doc, head) else {
        return false;
    };
    match (last, parent) {
        (PathSeg::Field(f), Value::Object(m)) => m.remove(field_str(f).unwrap_or("")).is_some(),
        (PathSeg::Index(i), Value::Array(a)) if *i >= 0 && (*i as usize) < a.len() => {
            a.remove(*i as usize);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Value {
        serde_json::json!({"a": {"b": [1, 2, {"c": "x"}]}, "n": null, "s": "str"})
    }

    #[test]
    fn parse_accepts_legacy_forms() {
        assert_eq!(parse_path(b"."), Some(vec![]));
        assert_eq!(parse_path(b"$"), Some(vec![]));
        assert_eq!(parse_path(b"$.a.b"), Some(vec![f("a"), f("b")]));
        assert_eq!(parse_path(b".a[2].b"), Some(vec![f("a"), i(2), f("b")]));
        assert_eq!(
            parse_path(b"['weird.key'][1]"),
            Some(vec![f("weird.key"), i(1)])
        );
        assert_eq!(parse_path(b"['a']['']"), Some(vec![f("a"), f("")]));
        assert_eq!(parse_path(b".a[-1]"), Some(vec![f("a"), i(-1)]));
    }

    #[test]
    fn parse_rejects_everything_wilder() {
        for bad in [
            &b""[..],
            b"$.",
            b"..",
            b"a",
            b".a.",
            b".[",
            b"[]",
            b"[x]",
            b"[*]",
            b"*",
            b"..*",
            b"$..a",
            b".a[b]",
            b"['a",
            b"['a'x",
            b".a[1",
            b".a]",
            b"[9999999999999999999999]",
        ] {
            assert_eq!(
                parse_path(bad),
                None,
                "path {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    fn f(name: &str) -> PathSeg {
        PathSeg::Field(name.as_bytes().to_vec())
    }
    fn i(idx: i64) -> PathSeg {
        PathSeg::Index(idx)
    }

    #[test]
    fn get_at_walks_and_misses() {
        let d = doc();
        assert_eq!(
            get_at(&d, &[f("a"), f("b"), i(2), f("c")]),
            Some(&Value::String("x".into()))
        );
        assert_eq!(get_at(&d, &[f("n")]), Some(&Value::Null));
        // negative index never resolves; wrong kinds / absent fields miss
        assert_eq!(get_at(&d, &[f("a"), f("b"), i(-1)]), None);
        assert_eq!(get_at(&d, &[f("a"), i(0)]), None);
        assert_eq!(get_at(&d, &[f("s"), f("x")]), None);
        assert_eq!(get_at(&d, &[f("a"), f("b"), i(3)]), None);
    }

    #[test]
    fn set_at_replaces_root_autocreates_and_appends() {
        let mut d = doc();
        assert!(set_at(&mut d, &[], serde_json::json!(5)).is_ok());
        assert_eq!(d, serde_json::json!(5));
        // missing intermediate objects are auto-created (v1 legacy)
        let mut d = serde_json::json!({});
        assert!(set_at(&mut d, &[f("x"), f("y")], serde_json::json!(1)).is_ok());
        assert_eq!(d, serde_json::json!({"x": {"y": 1}}));
        // index == len appends; beyond that and negatives are NotFound
        let mut a = serde_json::json!([1, 2]);
        assert!(set_at(&mut a, &[i(2)], serde_json::json!(3)).is_ok());
        assert_eq!(a, serde_json::json!([1, 2, 3]));
        assert_eq!(
            set_at(&mut a, &[i(5)], serde_json::json!(9)),
            Err(SetErr::NotFound)
        );
        assert_eq!(
            set_at(&mut a, &[i(-1)], serde_json::json!(9)),
            Err(SetErr::NotFound)
        );
        // wrong container kinds are WrongType
        assert_eq!(
            set_at(&mut a, &[f("k")], serde_json::json!(9)),
            Err(SetErr::WrongType)
        );
        let mut o = serde_json::json!({"k": 1});
        assert_eq!(
            set_at(&mut o, &[i(0)], serde_json::json!(9)),
            Err(SetErr::WrongType)
        );
        // scalar ancestors refuse field descent (incl. explicit null)
        let mut n = serde_json::json!({"k": null});
        assert_eq!(
            set_at(&mut n, &[f("k"), f("z")], serde_json::json!(9)),
            Err(SetErr::WrongType)
        );
    }

    #[test]
    fn remove_at_field_index_and_misses() {
        let mut d = doc();
        assert!(remove_at(&mut d, &[f("a"), f("b"), i(1)]));
        assert_eq!(
            d,
            serde_json::json!({"a": {"b": [1, {"c": "x"}]}, "n": null, "s": "str"})
        );
        assert!(remove_at(&mut d, &[f("a")]));
        assert_eq!(d, serde_json::json!({"n": null, "s": "str"}));
        assert!(!remove_at(&mut d, &[f("n0pe")]));
        assert!(!remove_at(&mut d, &[f("s"), f("x")]));
        assert!(!remove_at(&mut d, &[i(0)]));
        assert!(!remove_at(&mut d, &[]), "root is the caller's job");
    }

    #[test]
    fn display_is_canonical_legacy_form() {
        assert_eq!(path_display(&[]), ".");
        assert_eq!(path_display(&[f("a"), i(2)]), ".a[2]");
        assert_eq!(path_display(&[f("we.ird")]), "['we.ird']");
    }
}
