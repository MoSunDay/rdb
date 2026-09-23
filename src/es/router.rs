//! ES route table: `Request` (decoded segments) -> handler -> `Reply`.
//! System paths (`_bulk`, `_cluster/health`, ...) match BEFORE the
//! `/{index}` forms so an index literally named `_bulk` stays
//! unreachable (ES reserves the underscore namespace). Every
//! index-carrying route runs [`write::slot_check`] FIRST so a
//! remote-slot request fails fast with the routing error instead of
//! touching the local store. Known path + wrong method is a 405.

use std::time::Instant;

use serde_json::json;

use crate::state::Shared;

use super::bulk;
use super::http::Request;
use super::mapping;
use super::misc;
use super::reply::{self, Reply};
use super::write;

/// Max index name length (ES default 255 bytes).
const MAX_INDEX_LEN: usize = 255;
/// Max document id length (ES default 512 bytes).
const MAX_ID_LEN: usize = 512;

/// Dispatch one request. Infallible: every failure path (missing
/// index, bad DSL, store error) is already a `Reply`.
pub async fn route(shared: &Shared, req: &Request) -> Reply {
    let method = req.method.as_str();
    let segs: Vec<&str> = req.path.iter().map(String::as_str).collect();

    // -- underscore-system paths first --
    match segs.as_slice() {
        [] => {
            return match method {
                "GET" => misc::root(shared),
                _ => method_not_allowed(),
            };
        }
        ["_cluster", "health"] => {
            return match method {
                "GET" => misc::health(shared),
                _ => method_not_allowed(),
            };
        }
        ["_cat", "indices"] => {
            return match method {
                "GET" => misc::cat_indices(shared),
                _ => method_not_allowed(),
            };
        }
        ["_bulk"] => {
            return match method {
                "POST" => bulk::run_bulk(shared, "", &req.body).await,
                _ => method_not_allowed(),
            };
        }
        [index, "_bulk"] => {
            return match method {
                "POST" => {
                    if let Some(rep) = check_index(index) {
                        return rep;
                    }
                    bulk::run_bulk(shared, index, &req.body).await
                }
                _ => method_not_allowed(),
            };
        }
        _ => {}
    }

    // -- /{index} forms: reject remote slots before any store access --
    let index = segs[0];
    if let Some(rep) = check_index(index) {
        return rep;
    }
    if let Err(e) = write::slot_check(shared, index) {
        return e.reply();
    }
    match segs.len() {
        1 => route_index_root(shared, method, index, req).await,
        2 => route_index_action(shared, method, index, segs[1], req).await,
        3 if segs[1] == "_doc" => {
            let id = segs[2];
            if let Some(rep) = check_id(id) {
                return rep;
            }
            route_doc(shared, method, index, id, req).await
        }
        _ => no_handler(method, &segs),
    }
}

/// `PUT|GET|HEAD|DELETE /{index}` (create / mappings / exists / drop).
async fn route_index_root(shared: &Shared, method: &str, index: &str, req: &Request) -> Reply {
    match method {
        "PUT" => match write::create_index(shared, index, &req.body).await {
            Ok(_) => reply::ack_index(index),
            Err(e) => e.reply(),
        },
        "GET" => match write::index_meta(shared, index) {
            Ok(Some(meta)) => reply::json(200, index_get_body(index, &meta)),
            Ok(None) => not_found_index(index),
            Err(e) => e.reply(),
        },
        "HEAD" => match write::index_meta(shared, index) {
            Ok(Some(_)) => reply::empty(200),
            Ok(None) => reply::empty(404),
            Err(e) => e.reply(),
        },
        "DELETE" => match write::drop_index(shared, index).await {
            Ok(true) => reply::ack_index(index),
            Ok(false) => not_found_index(index),
            Err(e) => e.reply(),
        },
        _ => method_not_allowed(),
    }
}

/// `POST /{index}/_doc` and the `/{index}/_search|_count|_refresh`
/// actions.
async fn route_index_action(
    shared: &Shared,
    method: &str,
    index: &str,
    action: &str,
    req: &Request,
) -> Reply {
    match action {
        "_doc" => {
            // auto-id POST: a fresh id can never collide -> plain create
            match method {
                "POST" => {
                    let id = write::auto_id();
                    match write::put_doc(shared, index, &id, &req.body, false).await {
                        Ok(created) => reply::doc_write(index, &id, created),
                        Err(e) => e.reply(),
                    }
                }
                _ => method_not_allowed(),
            }
        }
        "_search" => match method {
            "POST" | "GET" => misc::search_endpoint(shared, index, &req.body, Instant::now()),
            _ => method_not_allowed(),
        },
        "_count" => match method {
            "POST" | "GET" => misc::count_endpoint(shared, index, &req.body),
            _ => method_not_allowed(),
        },
        "_refresh" => match method {
            "POST" | "GET" => misc::refresh(shared, index),
            _ => method_not_allowed(),
        },
        _ => no_handler(method, &[index, action]),
    }
}

/// `/{index}/_doc/{id}` CRUD.
async fn route_doc(shared: &Shared, method: &str, index: &str, id: &str, req: &Request) -> Reply {
    match method {
        "POST" | "PUT" => {
            let create_only = query_is(&req.query, "op_type", "create");
            match write::put_doc(shared, index, id, &req.body, create_only).await {
                Ok(created) => reply::doc_write(index, id, created),
                Err(e) => e.reply(),
            }
        }
        "GET" => match write::get_doc(shared, index, id) {
            Ok(Some((_, rec))) => {
                let source = serde_json::from_slice(&rec.doc).unwrap_or(serde_json::Value::Null);
                reply::doc_found(index, id, &source)
            }
            Ok(None) => reply::doc_missing(index, id),
            Err(e) => e.reply(),
        },
        "HEAD" => match write::get_doc(shared, index, id) {
            Ok(Some(_)) => reply::empty(200),
            Ok(None) => reply::empty(404),
            Err(e) => e.reply(),
        },
        "DELETE" => match write::del_doc(shared, index, id).await {
            Ok(Some(())) => reply::doc_deleted(index, id, true),
            Ok(None) => reply::doc_deleted(index, id, false),
            Err(e) => e.reply(),
        },
        _ => method_not_allowed(),
    }
}

/// `GET /{index}` body: the ES index view (mappings from the stored
/// schema, single-shard settings).
fn index_get_body(index: &str, meta: &crate::search::index_codec::IndexMeta) -> serde_json::Value {
    json!({
        index: {
            "aliases": {},
            "mappings": mapping::mappings_json(&meta.fields),
            "settings": {"index": {"number_of_shards": "1", "number_of_replicas": "0"}},
        }
    })
}

fn not_found_index(index: &str) -> Reply {
    reply::error(
        404,
        "index_not_found_exception",
        &format!("no such index [{index}]"),
    )
}

fn method_not_allowed() -> Reply {
    reply::error(405, "method_not_allowed_exception", "incorrect HTTP method")
}

fn no_handler(method: &str, segs: &[&str]) -> Reply {
    reply::error(
        404,
        "no_handler_found_exception",
        &format!("no handler found for [{} /{}]", method, segs.join("/")),
    )
}

/// Index names: no '/' (post-decode), 255 bytes max. (Empty segments
/// cannot occur -- the parser drops them.)
fn check_index(index: &str) -> Option<Reply> {
    if index.contains('/') || index.len() > MAX_INDEX_LEN {
        return Some(reply::error(
            400,
            "invalid_index_name_exception",
            &format!("invalid index name [{index}]"),
        ));
    }
    None
}

/// Document ids: no '/', 512 bytes max.
fn check_id(id: &str) -> Option<Reply> {
    if id.contains('/') || id.len() > MAX_ID_LEN {
        return Some(reply::error(
            400,
            "illegal_argument_exception",
            &format!("invalid document id [{id}]"),
        ));
    }
    None
}

/// Bare `?name=value` check (no percent-decoding; the flags this
/// router reads are plain ASCII).
fn query_is(query: &str, name: &str, value: &str) -> bool {
    query
        .split('&')
        .any(|pair| pair == format!("{name}={value}"))
}
