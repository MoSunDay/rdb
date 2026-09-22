//! `_bulk` NDJSON framing + execution: action/source line pairs
//! (`index|create|delete|update`) routed op-by-op through the write
//! path -- each op latches and fsyncs independently, mirroring the
//! RESP FT.ADD granularity. COMPAT decisions: `update` is rejected
//! per-item (400, errors:true); a `delete` miss reports
//! result:"not_found" WITHOUT flipping the top-level `errors` flag
//! (ES marks only real failures); ids may be omitted for
//! index/create (server-generated).

use serde_json::{json, Value};

use crate::state::Shared;

use super::reply::{self, Reply};
use super::write;

/// One parsed action line: action name, optional `_index` override
/// and optional `_id` ("" on index/create = auto-generate).
#[derive(Debug)]
pub struct BulkOp {
    pub action: String,
    pub index: String,
    pub id: String,
}

const MAX_ID_LEN: usize = 512;
const MAX_INDEX_LEN: usize = 255;

/// Split an NDJSON body into (action, source) pairs. Blank lines are
/// skipped; `delete` takes no source; every other action must be
/// followed by one source line. Errors carry the 1-based line number.
pub fn parse_ndjson(body: &[u8]) -> Result<Vec<(BulkOp, Option<Vec<u8>>)>, String> {
    let text = std::str::from_utf8(body).map_err(|_| "bulk body must be UTF-8".to_string())?;
    let mut out = Vec::new();
    let mut pending: Option<BulkOp> = None;
    for (i, raw) in text.split('\n').enumerate() {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        match pending.take() {
            None => {
                let op = parse_action_line(line.trim())
                    .map_err(|e| format!("line {}: {e}", i + 1))?;
                if op.action == "delete" {
                    out.push((op, None));
                } else {
                    pending = Some(op);
                }
            }
            Some(op) => out.push((op, Some(line.as_bytes().to_vec()))),
        }
    }
    if let Some(op) = pending {
        return Err(format!("missing source line for '{}' action", op.action));
    }
    Ok(out)
}

/// `{"<action>": {"_index"?: s, "_id"?: s}}` -> [`BulkOp`].
fn parse_action_line(line: &str) -> Result<BulkOp, String> {
    let v: Value = serde_json::from_str(line).map_err(|_| "malformed JSON action line".to_string())?;
    let Some(obj) = v.as_object() else {
        return Err("action line must be a JSON object".to_string());
    };
    if obj.len() != 1 {
        return Err("action line must carry exactly one action".to_string());
    }
    let (action, meta) = obj.iter().next().unwrap();
    if !matches!(action.as_str(), "index" | "create" | "delete" | "update") {
        return Err(format!("unknown bulk action '{action}'"));
    }
    let Some(meta) = meta.as_object() else {
        return Err(format!("action '{action}' metadata must be an object"));
    };
    let str_field = |k: &str| -> Result<String, String> {
        match meta.get(k) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(format!("action '{action}' field [{k}] must be a string")),
        }
    };
    let index = str_field("_index")?;
    let id = str_field("_id")?;
    if id.is_empty() && (action == "delete" || action == "update") {
        return Err(format!("action '{action}' requires an _id"));
    }
    Ok(BulkOp { action: action.clone(), index, id })
}

/// Run one `_bulk` request against `default_index` (the URL's index;
/// per-op `_index` wins, both empty -> per-item validation error).
/// Never fails the whole request: per-op problems become item errors
/// and only flip the top-level `errors` flag.
pub async fn run_bulk(shared: &Shared, default_index: &str, body: &[u8]) -> Reply {
    let ops = match parse_ndjson(body) {
        Ok(ops) => ops,
        Err(e) => return reply::error(400, "illegal_argument_exception", &e),
    };
    let mut items = Vec::with_capacity(ops.len());
    let mut errors = false;
    for (op, source) in ops {
        let index = if op.index.is_empty() {
            default_index.to_string()
        } else {
            op.index.clone()
        };
        if !valid_name(&index, MAX_INDEX_LEN) {
            errors = true;
            items.push(item_err(&op.action, &index, &op.id, "invalid_index_name_exception", "missing or invalid index name"));
            continue;
        }
        match op.action.as_str() {
            "update" => {
                errors = true;
                items.push(item_err(
                    &op.action, &index, &op.id,
                    "action_request_validation_exception",
                    "update actions are not supported in this ES subset",
                ));
            }
            "delete" => match write::del_doc(shared, &index, &op.id).await {
                Ok(Some(())) => items.push(reply::bulk_item(&op.action, &index, &op.id, 200, Some("deleted"), None)),
                // delete-miss: result not_found, NOT an errors:true item
                Ok(None) => items.push(reply::bulk_item(&op.action, &index, &op.id, 404, Some("not_found"), None)),
                Err(e) => {
                    errors = true;
                    items.push(item_err(&op.action, &index, &op.id, &e.es_type, &e.reason));
                }
            },
            act => {
                let Some(src) = source else {
                    continue; // parser guarantees a source for index/create
                };
                if !valid_name(&op.id, MAX_ID_LEN) || op.id.contains('/') {
                    errors = true;
                    items.push(item_err(&op.action, &index, &op.id, "illegal_argument_exception", "invalid document id"));
                    continue;
                }
                let (id, create_only) = if op.id.is_empty() {
                    (write::auto_id(), false)
                } else {
                    (op.id.clone(), act == "create")
                };
                match write::put_doc(shared, &index, &id, &src, create_only).await {
                    Ok(true) => items.push(reply::bulk_item(&op.action, &index, &id, 201, Some("created"), None)),
                    Ok(false) => items.push(reply::bulk_item(&op.action, &index, &id, 200, Some("updated"), None)),
                    Err(e) => {
                        errors = true;
                        items.push(item_err(&op.action, &index, &id, &e.es_type, &e.reason));
                    }
                }
            }
        }
    }
    reply::bulk(errors, items)
}

/// Names are non-empty (when required), slash-free and length-capped.
fn valid_name(s: &str, max: usize) -> bool {
    !s.is_empty() && !s.contains('/') && s.len() <= max
}

fn item_err(action: &str, index: &str, id: &str, es_type: &str, reason: &str) -> Value {
    reply::bulk_item(
        action,
        index,
        id,
        400,
        None,
        Some(json!({"type": es_type, "reason": reason})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_blank_lines_and_crlf() {
        let body = b"\r\n{\"index\":{\"_index\":\"i\",\"_id\":\"1\"}}\n\n{\"a\":1}\r\n{\"delete\":{\"_index\":\"i\",\"_id\":\"2\"}}\n";
        let ops = parse_ndjson(body).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].0.action, "index");
        assert_eq!(ops[0].0.index, "i");
        assert_eq!(ops[1].0.action, "delete");
        assert!(ops[1].1.is_none());
        assert_eq!(ops[0].1.as_deref(), Some(&br#"{"a":1}"#[..]));
    }

    #[test]
    fn auto_id_index_without_id() {
        let body = b"{\"index\":{}}\n{\"a\":1}\n";
        let ops = parse_ndjson(body).unwrap();
        assert_eq!(ops[0].0.id, "");
        assert_eq!(ops[0].0.index, "");
    }

    #[test]
    fn missing_source_is_reported_with_action() {
        let err = parse_ndjson(b"{\"index\":{\"_id\":\"1\"}}\n").unwrap_err();
        assert_eq!(err, "missing source line for 'index' action");
    }

    #[test]
    fn bad_json_and_unknown_action_carry_line_numbers() {
        let err = parse_ndjson(b"{\"a\":1,\"b\":2}\n").unwrap_err();
        assert_eq!(err, "line 1: action line must carry exactly one action");
        let err = parse_ndjson(b"{\"index\":{\"_id\":\"1\"}}\n{}\n{\"frobnicate\":{}}\n{\"x\":1}\n")
            .unwrap_err();
        assert_eq!(err, "line 3: unknown bulk action 'frobnicate'");
        let err = parse_ndjson(b"{\"delete\":{\"_index\":\"i\"}}\n").unwrap_err();
        assert!(err.contains("requires an _id"), "{err}");
    }
}
