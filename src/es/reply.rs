//! JSON reply envelopes for the ES HTTP transport: the [`Reply`]
//! wire carrier (status + content type + bytes) plus pure builders
//! for every response shape (index ack / doc write / search / bulk /
//! cat). The ES-shaped error envelope lives here too, so the
//! transport and the handlers share one error wire format.
//!
//! COMPAT: `_version` is always 1 and `_shards` pretends one
//! successful shard (single-node storage per index); `took` for
//! `_bulk` is reported as 0 (no per-op timing).

use serde_json::{json, Map, Value};

use super::exec::ExecResult;

/// One HTTP reply: status code, content type, serialized body.
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

/// 200-style JSON reply.
pub fn json(status: u16, v: Value) -> Reply {
    Reply {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&v).unwrap_or_default(),
    }
}

/// Plain-text reply (`_cat` endpoints, HEAD empties).
pub fn text(status: u16, s: String) -> Reply {
    Reply {
        status,
        content_type: "text/plain; charset=UTF-8",
        body: s.into_bytes(),
    }
}

/// Header-only reply (HEAD requests).
pub fn empty(status: u16) -> Reply {
    text(status, String::new())
}

/// ES error envelope: root_cause mirrors the top-level type/reason
/// and `status` repeats the HTTP code as an integer, exactly the
/// 8.x shape clients unpack.
pub fn error(status: u16, es_type: &str, reason: &str) -> Reply {
    json(
        status,
        json!({
            "error": {
                "root_cause": [{"type": es_type, "reason": reason}],
                "type": es_type,
                "reason": reason,
            },
            "status": status,
        }),
    )
}

/// Reason phrase for the status line (subset this frontend emits).
pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Request Entity Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Error",
    }
}

/// `PUT /{index}` acknowledgement.
pub fn ack_index(index: &str) -> Reply {
    json(
        200,
        json!({"acknowledged": true, "shards_acknowledged": true, "index": index}),
    )
}

/// `_doc` write result: 201/created when the id is new, 200/updated
/// on replace (`_version` always 1 -- COMPAT).
pub fn doc_write(index: &str, id: &str, created: bool) -> Reply {
    let result = if created { "created" } else { "updated" };
    json(
        if created { 201 } else { 200 },
        json!({
            "_index": index,
            "_id": id,
            "_version": 1,
            "result": result,
            "_shards": {"total": 1, "successful": 1, "failed": 0},
            "_seq_no": 0,
            "_primary_term": 1,
        }),
    )
}

/// `GET /{index}/_doc/{id}` hit; `source` is Null when `_source` was
/// disabled by the request (search) or the doc is read raw (get).
pub fn doc_found(index: &str, id: &str, source: &Value) -> Reply {
    json(
        200,
        json!({
            "_index": index,
            "_id": id,
            "_version": 1,
            "found": true,
            "_source": source,
        }),
    )
}

/// Miss body (`found:false`); the caller picked 404 already.
pub fn doc_missing(index: &str, id: &str) -> Reply {
    json(404, json!({"_index": index, "_id": id, "found": false}))
}

/// `DELETE /{index}/_doc/{id}`: "deleted" or "not_found" (both 200
/// at the ES wire, unlike the GET miss).
pub fn doc_deleted(index: &str, id: &str, deleted: bool) -> Reply {
    let result = if deleted { "deleted" } else { "not_found" };
    json(
        200,
        json!({
            "_index": index,
            "_id": id,
            "_version": 1,
            "result": result,
            "_shards": {"total": 1, "successful": 1, "failed": 0},
            "_seq_no": 0,
            "_primary_term": 1,
        }),
    )
}

/// `_search` reply envelope around an [`ExecResult`]: total is the
/// pre-window match count, `max_score`/`_score` follow the executor's
/// sort-first-key rule (null for field sorts).
pub fn search(index: &str, result: &ExecResult, took_ms: u128) -> Reply {
    let hits: Vec<Value> = result
        .hits
        .iter()
        .map(|h| {
            let mut o = Map::new();
            o.insert("_index".into(), json!(index));
            o.insert("_id".into(), json!(String::from_utf8_lossy(&h.docid)));
            o.insert(
                "_score".into(),
                h.score.map(|f| json!(f)).unwrap_or(Value::Null),
            );
            o.insert("_source".into(), h.source.clone());
            Value::Object(o)
        })
        .collect();
    json(
        200,
        json!({
            "took": took_ms,
            "timed_out": false,
            "_shards": {"total": 1, "successful": 1, "skipped": 0, "failed": 0},
            "hits": {
                "total": {"value": result.total, "relation": "eq"},
                "max_score": result.max_score.map(|f| json!(f)).unwrap_or(Value::Null),
                "hits": hits,
            },
        }),
    )
}

/// `_count` reply.
pub fn count(n: usize) -> Reply {
    json(200, json!({"count": n}))
}

/// `_refresh` reply -- a no-op acknowledgement (reads are realtime).
pub fn refresh() -> Reply {
    json(
        200,
        json!({"_shards": {"total": 1, "successful": 1, "failed": 0}}),
    )
}

/// `_bulk` reply: `took` fixed at 0 (COMPAT: no per-op timing).
pub fn bulk(errors: bool, items: Vec<Value>) -> Reply {
    json(200, json!({"took": 0, "errors": errors, "items": items}))
}

/// One `_bulk` item entry, keyed by the action name; `result` on
/// success ("created"/"updated"/"deleted"/"not_found"), `error` on
/// failure (an ES error object, not the full envelope).
pub fn bulk_item(
    action: &str,
    index: &str,
    id: &str,
    status: u16,
    result: Option<&str>,
    error: Option<Value>,
) -> Value {
    let mut inner = Map::new();
    inner.insert("_index".into(), json!(index));
    inner.insert("_id".into(), json!(id));
    inner.insert("status".into(), json!(status));
    if let Some(r) = result {
        inner.insert("result".into(), json!(r));
    }
    if let Some(e) = error {
        inner.insert("error".into(), e);
    }
    json!({ action: Value::Object(inner) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_envelope_shape() {
        let r = error(404, "index_not_found_exception", "no such index [i]");
        assert_eq!(r.status, 404);
        assert_eq!(r.content_type, "application/json");
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let e = &v["error"];
        assert_eq!(e["type"], "index_not_found_exception");
        assert_eq!(e["root_cause"][0]["type"], "index_not_found_exception");
        assert_eq!(e["root_cause"][0]["reason"], "no such index [i]");
        assert_eq!(v["status"], 404);
    }

    #[test]
    fn reason_phrases() {
        assert_eq!(reason(200), "OK");
        assert_eq!(reason(201), "Created");
        assert_eq!(reason(409), "Conflict");
        assert_eq!(reason(413), "Request Entity Too Large");
        assert_eq!(reason(501), "Not Implemented");
        assert_eq!(reason(599), "Error"); // unknown falls back
    }

    #[test]
    fn doc_shapes() {
        let created = doc_write("i", "1", true);
        assert_eq!(created.status, 201);
        let v: Value = serde_json::from_slice(&created.body).unwrap();
        assert_eq!(v["result"], "created");
        assert_eq!(v["_version"], 1);
        assert_eq!(v["_shards"]["successful"], 1);
        let updated = doc_write("i", "1", false);
        assert_eq!(updated.status, 200);
        let v: Value = serde_json::from_slice(&updated.body).unwrap();
        assert_eq!(v["result"], "updated");

        let miss = doc_missing("i", "1");
        assert_eq!(miss.status, 404);
        let v: Value = serde_json::from_slice(&miss.body).unwrap();
        assert_eq!(v["found"], false);

        let gone = doc_deleted("i", "1", false);
        assert_eq!(gone.status, 200);
        let v: Value = serde_json::from_slice(&gone.body).unwrap();
        assert_eq!(v["result"], "not_found");
    }

    #[test]
    fn bulk_item_variants() {
        let ok = bulk_item("index", "i", "1", 201, Some("created"), None);
        assert_eq!(ok["index"]["status"], 201);
        assert_eq!(ok["index"]["result"], "created");
        assert!(ok["index"].get("error").is_none());
        let bad = bulk_item(
            "update",
            "i",
            "1",
            400,
            None,
            Some(json!({"type": "action_request_validation_exception"})),
        );
        assert_eq!(
            bad["update"]["error"]["type"],
            "action_request_validation_exception"
        );
    }
}
