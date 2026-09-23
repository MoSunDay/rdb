//! `_search` request knobs beyond the query tree: knn, sort, the
//! from/size window and `_source` filtering. Pure JSON shaping, no
//! Store access; `dsl::parse_search` delegates here.

use serde_json::{Map, Value};

use super::dsl::{err, parse_query, DslError, KnnPlan, SortDir, SortKey, SourceFilter};

pub(super) fn parse_knn(v: &Value) -> Result<KnnPlan, DslError> {
    let Some(obj) = v.as_object() else {
        return Err(err(400, "parsing_exception", "[knn] must be an object"));
    };
    let field = match obj.get("field") {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(err(
                400,
                "parsing_exception",
                "[knn] requires a string field",
            ))
        }
    };
    let vector = match obj.get("query_vector") {
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                let Some(x) = it.as_f64().filter(|x| x.is_finite()) else {
                    return Err(err(
                        400,
                        "parsing_exception",
                        "[knn] query_vector must be finite numbers",
                    ));
                };
                out.push(x);
            }
            out
        }
        _ => {
            return Err(err(
                400,
                "parsing_exception",
                "[knn] requires a query_vector array",
            ))
        }
    };
    if vector.is_empty() {
        return Err(err(
            400,
            "parsing_exception",
            "[knn] query_vector must not be empty",
        ));
    }
    let k = knn_uint(obj, "k", 10)?;
    // ES defaults num_candidates to 10000; v1 uses max(k*10, 100)
    // capped at 10000 (a recall/cost point, not an ES mirror).
    let default_cands = (k * 10).clamp(100, 10_000);
    let num_candidates = knn_uint(obj, "num_candidates", default_cands)?;
    let filter = match obj.get("filter") {
        None | Some(Value::Null) => None,
        Some(q) => Some(Box::new(parse_query(q)?)),
    };
    Ok(KnnPlan {
        field,
        vector,
        k,
        num_candidates,
        filter,
    })
}

/// knn's integer knobs: 1..=10000, `illegal_argument` outside.
fn knn_uint(obj: &Map<String, Value>, key: &str, default: usize) -> Result<usize, DslError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => match v.as_u64() {
            Some(x) if (1..=10_000).contains(&x) => Ok(x as usize),
            _ => Err(err(
                400,
                "illegal_argument_exception",
                &format!("[knn] {key} must be an integer between 1 and 10000"),
            )),
        },
    }
}

pub(super) fn parse_sort(v: &Value) -> Result<Vec<SortKey>, DslError> {
    let items: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![v],
        Value::Null => return Ok(Vec::new()),
        _ => return Err(err(400, "parsing_exception", "[sort] must be an array")),
    };
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        match it {
            Value::String(s) => out.push(sort_key_named(s, default_order(s))?),
            Value::Object(obj) => {
                let Some((name, cfg)) = obj.iter().next() else {
                    return Err(err(
                        400,
                        "parsing_exception",
                        "[sort] entry cannot be empty",
                    ));
                };
                let dir = match cfg {
                    Value::Object(b) => match b.get("order") {
                        None => default_order(name),
                        Some(Value::String(s)) if s == "asc" => SortDir::Asc,
                        Some(Value::String(s)) if s == "desc" => SortDir::Desc,
                        _ => {
                            return Err(err(
                                400,
                                "parsing_exception",
                                "[sort] order must be 'asc' or 'desc'",
                            ))
                        }
                    },
                    _ => default_order(name),
                };
                out.push(sort_key_named(name, dir)?);
            }
            _ => {
                return Err(err(
                    400,
                    "parsing_exception",
                    "[sort] entries must be strings or objects",
                ))
            }
        }
    }
    Ok(out)
}

/// Plain "_score" sorts DESC, every other plain name ASC (ES rules).
fn default_order(name: &str) -> SortDir {
    if name == "_score" {
        SortDir::Desc
    } else {
        SortDir::Asc
    }
}

fn sort_key_named(name: &str, dir: SortDir) -> Result<SortKey, DslError> {
    match name {
        "_score" => Ok(SortKey::Score(dir)),
        "_doc" => Ok(SortKey::Doc),
        _ => Ok(SortKey::Field {
            field: name.to_string(),
            dir,
        }),
    }
}

pub(super) fn parse_source(v: Option<&Value>) -> Result<SourceFilter, DslError> {
    let Some(v) = v else {
        return Ok(SourceFilter::default());
    };
    match v {
        Value::Bool(false) => Ok(SourceFilter {
            disabled: true,
            ..SourceFilter::default()
        }),
        Value::Bool(true) | Value::Null => Ok(SourceFilter::default()),
        Value::String(s) => Ok(SourceFilter {
            includes: vec![s.clone()],
            ..SourceFilter::default()
        }),
        Value::Array(a) => {
            let includes = strings(a, "_source")?;
            Ok(SourceFilter {
                includes,
                ..SourceFilter::default()
            })
        }
        Value::Object(obj) => {
            // ES spells them includes/excludes; accept include/exclude
            // too, MERGING when both appear.
            let mut includes = Vec::new();
            let mut excludes = Vec::new();
            for (keys, out) in [
                (["includes", "include"], &mut includes),
                (["excludes", "exclude"], &mut excludes),
            ] {
                for key in keys {
                    if let Some(v) = obj.get(key) {
                        out.extend(patterns(Some(v))?);
                    }
                }
            }
            Ok(SourceFilter {
                includes,
                excludes,
                ..SourceFilter::default()
            })
        }
        _ => Err(err(
            400,
            "parsing_exception",
            "[_source] must be a boolean, array or object",
        )),
    }
}

fn patterns(v: Option<&Value>) -> Result<Vec<String>, DslError> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(a)) => strings(a, "_source"),
        Some(_) => Err(err(
            400,
            "parsing_exception",
            "[_source] patterns must be strings or arrays",
        )),
    }
}

fn strings(a: &[Value], key: &str) -> Result<Vec<String>, DslError> {
    a.iter()
        .map(|v| {
            v.as_str().map(str::to_string).ok_or_else(|| {
                err(
                    400,
                    "parsing_exception",
                    &format!("[{key}] entries must be strings"),
                )
            })
        })
        .collect()
}

/// from/size as non-negative integers (missing key -> default).
pub(super) fn uint_of(
    obj: &Map<String, Value>,
    key: &str,
    default: usize,
) -> Result<usize, DslError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().map(|x| x as usize).ok_or_else(|| {
            err(
                400,
                "parsing_exception",
                &format!("[{key}] must be a non-negative integer"),
            )
        }),
    }
}
