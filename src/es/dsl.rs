//! Elasticsearch Query-DSL subset parser: `_search` body bytes to a
//! pure `SearchPlan` (no Store access, so every default and error
//! shape is unit-testable). v1 subset: match_all/match/term/terms/
//! range/bool + knn + sort + from/size + _source. Deviations from
//! real ES: unknown TOP-LEVEL keys are ignored (ES tolerance), but an
//! unknown query type is a 400 `parsing_exception`; `term` takes a
//! bare scalar (the `{"value": ..}` object form is not parsed); an
//! empty `match` token list matches NOTHING (ES also matches nothing
//! on empty analyzed strings); `sort` ignores unmapped extra keys.

use serde_json::{Map, Value};

use crate::search::tokenize::tokenize;

use super::opts::{parse_knn, parse_sort, parse_source, uint_of};

/// ES-shaped error: HTTP status + `error.type` + `error.reason`.
#[derive(Debug, Clone, PartialEq)]
pub struct DslError {
    pub status: u16,
    pub es_type: String,
    pub reason: String,
}

pub fn err(status: u16, es_type: &str, reason: &str) -> DslError {
    DslError {
        status,
        es_type: es_type.to_string(),
        reason: reason.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoolOp {
    Or,
    And,
}

/// One parsed query clause tree (leaf = posting-set lookup).
#[derive(Debug, Clone)]
pub enum Plan {
    MatchAll,
    /// terms = tokenized (Text field) or exact bytes (Keyword); an
    /// EMPTY vec matches nothing by decision (ES alignment).
    Match {
        field: String,
        terms: Vec<Vec<u8>>,
        operator: BoolOp,
    },
    Term {
        field: String,
        value: TermValue,
    },
    Terms {
        field: String,
        values: Vec<TermValue>,
    },
    Range {
        field: String,
        gte: Option<f64>,
        gt: Option<f64>,
        lte: Option<f64>,
        lt: Option<f64>,
    },
    Bool {
        must: Vec<Plan>,
        filter: Vec<Plan>,
        should: Vec<Plan>,
        must_not: Vec<Plan>,
    },
}

/// Term value keeps both the raw JSON scalar and its f64 view
/// (numeric fields store f64 doc-values).
#[derive(Debug, Clone, PartialEq)]
pub enum TermValue {
    Bytes(Vec<u8>),
    Number(f64),
}

#[derive(Debug, Clone)]
pub struct KnnPlan {
    pub field: String,
    pub vector: Vec<f64>,
    pub k: usize,
    pub num_candidates: usize,
    pub filter: Option<Box<Plan>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SortKey {
    Score(SortDir),
    Doc,
    Field { field: String, dir: SortDir },
}

/// `_source` shaping: disabled, dotted-path include patterns, dotted
/// exclude patterns (trailing `*` wildcard per segment).
#[derive(Debug, Clone, Default)]
pub struct SourceFilter {
    pub disabled: bool,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SearchPlan {
    /// MatchAll when the body carries no "query".
    pub query: Plan,
    pub knn: Option<KnnPlan>,
    /// Empty = default `_score` desc.
    pub sort: Vec<SortKey>,
    pub from: usize,
    /// Default 10; from+size capped at 10000 (error beyond).
    pub size: usize,
    pub source: SourceFilter,
}

const WINDOW_LIMIT: u64 = 10_000;

/// Parse a `_search` body. Empty/`{}` bodies yield the defaults
/// (match_all, size 10). Unknown top-level keys are ignored.
pub fn parse_search(body: &[u8]) -> Result<SearchPlan, DslError> {
    let root: Value = if body.iter().all(|&b| b.is_ascii_whitespace()) {
        Value::Object(Map::new())
    } else {
        serde_json::from_slice(body)
            .map_err(|_| err(400, "x_content_parse_exception", "malformed JSON body"))?
    };
    let obj = match root {
        Value::Object(m) => m,
        Value::Null => Map::new(),
        _ => {
            return Err(err(
                400,
                "x_content_parse_exception",
                "request body must be a JSON object",
            ))
        }
    };
    let query = match obj.get("query") {
        None | Some(Value::Null) => Plan::MatchAll,
        Some(v) => parse_query(v)?,
    };
    let knn = match obj.get("knn") {
        None | Some(Value::Null) => None,
        Some(v) => Some(parse_knn(v)?),
    };
    let sort = match obj.get("sort") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => parse_sort(v)?,
    };
    let from = uint_of(&obj, "from", 0)? as u64;
    let size = uint_of(&obj, "size", 10)? as u64;
    if from + size > WINDOW_LIMIT {
        return Err(err(
            400,
            "illegal_argument_exception",
            &format!(
                "Result window is too large, from + size must be less than or equal to: [10000] but was [{}].",
                from + size
            ),
        ));
    }
    Ok(SearchPlan {
        query,
        knn,
        sort,
        from: from as usize,
        size: size as usize,
        source: parse_source(obj.get("_source"))?,
    })
}

/// Dispatch on the query object's single (first) key.
pub(super) fn parse_query(v: &Value) -> Result<Plan, DslError> {
    let Some(obj) = v.as_object() else {
        return Err(err(
            400,
            "parsing_exception",
            "[_search] query clause must be an object",
        ));
    };
    let Some((kind, spec)) = obj.iter().next() else {
        return Err(err(400, "parsing_exception", "[query] cannot be empty"));
    };
    match kind.as_str() {
        "match_all" => Ok(Plan::MatchAll),
        "match" => parse_match(spec),
        "term" => parse_term(spec),
        "terms" => parse_terms(spec),
        "range" => parse_range(spec),
        "bool" => parse_bool(spec),
        other => Err(err(
            400,
            "parsing_exception",
            &format!("[unknown query type '{other}']"),
        )),
    }
}

/// `{"field": "text"}` or `{"field": {"query": "..", "operator": ..}}`.
fn parse_match(spec: &Value) -> Result<Plan, DslError> {
    let Some(obj) = spec.as_object() else {
        return Err(err(400, "parsing_exception", "[match] must be an object"));
    };
    let Some((field, cfg)) = obj.iter().next() else {
        return Err(err(
            400,
            "parsing_exception",
            "[match] requires a field name",
        ));
    };
    let (text, operator) = match cfg {
        Value::String(s) => (s.clone(), BoolOp::Or),
        Value::Object(body) => {
            let text = match body.get("query") {
                Some(Value::String(s)) => s.clone(),
                _ => {
                    return Err(err(
                        400,
                        "parsing_exception",
                        "[match] query must be a string",
                    ))
                }
            };
            let operator = match body.get("operator") {
                None => BoolOp::Or,
                Some(Value::String(s)) if s == "or" => BoolOp::Or,
                Some(Value::String(s)) if s == "and" => BoolOp::And,
                Some(_) => {
                    return Err(err(
                        400,
                        "parsing_exception",
                        "[match] operator must be 'or' or 'and'",
                    ))
                }
            };
            (text, operator)
        }
        _ => {
            return Err(err(
                400,
                "parsing_exception",
                "[match] query must be a string",
            ))
        }
    };
    let terms = tokenize(&text)
        .into_iter()
        .map(|t| t.into_bytes())
        .collect();
    Ok(Plan::Match {
        field: field.clone(),
        terms,
        operator,
    })
}

/// String scalars stay raw bytes, numbers become their f64 view.
fn term_value(v: &Value) -> Result<TermValue, DslError> {
    match v {
        Value::String(s) => Ok(TermValue::Bytes(s.clone().into_bytes())),
        Value::Number(x) => x.as_f64().map(TermValue::Number).ok_or_else(|| {
            err(
                400,
                "parsing_exception",
                "term query value must be string or number",
            )
        }),
        _ => Err(err(
            400,
            "parsing_exception",
            "term query value must be string or number",
        )),
    }
}

fn parse_term(spec: &Value) -> Result<Plan, DslError> {
    let Some(obj) = spec.as_object() else {
        return Err(err(400, "parsing_exception", "[term] must be an object"));
    };
    let Some((field, raw)) = obj.iter().next() else {
        return Err(err(
            400,
            "parsing_exception",
            "[term] requires a field name",
        ));
    };
    Ok(Plan::Term {
        field: field.clone(),
        value: term_value(raw)?,
    })
}

fn parse_terms(spec: &Value) -> Result<Plan, DslError> {
    let Some(obj) = spec.as_object() else {
        return Err(err(400, "parsing_exception", "[terms] must be an object"));
    };
    let Some((field, raw)) = obj.iter().next() else {
        return Err(err(
            400,
            "parsing_exception",
            "[terms] requires a field name",
        ));
    };
    let Some(items) = raw.as_array() else {
        return Err(err(
            400,
            "parsing_exception",
            "[terms] value must be an array",
        ));
    };
    let values = items
        .iter()
        .map(term_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Plan::Terms {
        field: field.clone(),
        values,
    })
}

fn parse_range(spec: &Value) -> Result<Plan, DslError> {
    let Some(obj) = spec.as_object() else {
        return Err(err(400, "parsing_exception", "[range] must be an object"));
    };
    let Some((field, body)) = obj.iter().next() else {
        return Err(err(
            400,
            "parsing_exception",
            "[range] requires a field name",
        ));
    };
    let Some(body) = body.as_object() else {
        return Err(err(
            400,
            "parsing_exception",
            "[range] bounds must be an object",
        ));
    };
    let mut gte = None;
    let mut gt = None;
    let mut lte = None;
    let mut lt = None;
    for (k, v) in body {
        let Some(x) = v.as_f64() else {
            return Err(err(
                400,
                "parsing_exception",
                "range bounds must be numbers",
            ));
        };
        match k.as_str() {
            "gte" => gte = Some(x),
            "gt" => gt = Some(x),
            "lte" => lte = Some(x),
            "lt" => lt = Some(x),
            _ => {} // relation/format/boost tolerated and ignored
        }
    }
    Ok(Plan::Range {
        field: field.clone(),
        gte,
        gt,
        lte,
        lt,
    })
}

fn parse_bool(spec: &Value) -> Result<Plan, DslError> {
    let Some(obj) = spec.as_object() else {
        return Err(err(400, "parsing_exception", "[bool] must be an object"));
    };
    let mut must = Vec::new();
    let mut filter = Vec::new();
    let mut should = Vec::new();
    let mut must_not = Vec::new();
    for (key, v) in obj {
        match key.as_str() {
            "must" => must = clause_list(v, "must")?,
            "filter" => filter = clause_list(v, "filter")?,
            "should" => should = clause_list(v, "should")?,
            "must_not" => must_not = clause_list(v, "must_not")?,
            _ => {} // minimum_should_match/boost tolerated and ignored
        }
    }
    Ok(Plan::Bool {
        must,
        filter,
        should,
        must_not,
    })
}

/// A bool sub-clause is one object or an array of objects.
fn clause_list(v: &Value, key: &str) -> Result<Vec<Plan>, DslError> {
    match v {
        Value::Array(items) => items.iter().map(parse_query).collect(),
        Value::Object(_) => Ok(vec![parse_query(v)?]),
        Value::Null => Ok(Vec::new()),
        _ => Err(err(
            400,
            "parsing_exception",
            &format!("[bool] {key} clause must be an object or array"),
        )),
    }
}

#[cfg(test)]
#[path = "dsl_tests.rs"]
mod dsl_tests;
