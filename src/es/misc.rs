//! ES meta endpoints: `/`, `/_cluster/health`, `/_cat/indices`,
//! `/{index}/_refresh`, `/{index}/_count` and the `_search` flow
//! glue (index lookup -> DSL parse -> executor -> reply envelope).
//! All read-only except the refresh no-op.

use std::time::Instant;

use serde_json::json;

use crate::ds::codec::{classify, decode_data_key, decode_envelope, Classification, KIND_SEARCH_META};
use crate::hash;
use crate::search::ft_index;
use crate::state::Shared;
use crate::store::ops;
use crate::utils;

use super::dsl;
use super::exec;
use super::reply::{self, Reply};
use super::write;

/// `GET /`: node identity. Version string advertises the compat
/// surface, not the storage version.
pub fn root(shared: &Shared) -> Reply {
    reply::json(
        200,
        json!({
            "name": shared.conf.bind,
            "cluster_name": "rdb",
            "version": {"number": "8.11.0-rdb", "build_flavor": "default"},
            "tagline": "You Know, for Search",
        }),
    )
}

/// `GET /_cluster/health`: status is green only once the topology
/// reports the cluster ready (raft membership settled), yellow
/// otherwise -- never red: the local data plane always serves reads.
pub fn health(shared: &Shared) -> Reply {
    let topo = shared
        .topology
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reply::json(
        200,
        json!({
            "cluster_name": "rdb",
            "status": if topo.cluster_ready { "green" } else { "yellow" },
            "timed_out": false,
            "number_of_nodes": topo.stable_addrs.len().max(1),
            "active_primary_shards": 0,
        }),
    )
}

/// `POST /{index}/_refresh`: validates the index exists, then acks.
/// No-op by design -- reads see the last fsync, so there is nothing
/// to refresh (COMPAT).
pub fn refresh(shared: &Shared, index: &str) -> Reply {
    match write::index_meta(shared, index) {
        Ok(Some(_)) => reply::refresh(),
        Ok(None) => reply::error(
            404,
            "index_not_found_exception",
            &format!("no such index [{index}]"),
        ),
        Err(e) => e.reply(),
    }
}

/// `GET /_cat/indices`: one physical scan over the whole local
/// keyspace keeping only search-meta roots. The scan stops after
/// [`MAX_INDEXES`] indexes (a full walk has no upper bound).
pub fn cat_indices(shared: &Shared) -> Reply {
    let mut rows: Vec<(String, String, u64)> = Vec::new();
    let _ = ops::for_each_from(&shared.store, b"", false, &mut |k, v| {
        // physical layout: "<slot-digits>/" ++ record; anything else
        // (no slash, raw string, other family) is not an index root.
        let Some(sep) = k.iter().position(|&b| b == b'/') else {
            return true;
        };
        if classify(&k[sep + 1..]) != Classification::Typed(KIND_SEARCH_META) {
            return true;
        }
        let Some((_kind, index, _suffix)) = decode_data_key(k, sep + 1) else {
            return true;
        };
        let (expire_ms, payload) = decode_envelope(v);
        let docs = ft_index::decode_index_meta(expire_ms, payload)
            .map(|m| m.num_docs)
            .unwrap_or(0);
        let name = String::from_utf8_lossy(&index).into_owned();
        let uuid = utils::md5_with40(&name);
        rows.push((name, uuid, docs));
        rows.len() < MAX_INDEXES
    });
    rows.sort_by(|a, b| a.0.cmp(&b.0)); // ES lists indices by name
    reply::text(200, cat_table(&rows))
}

const MAX_INDEXES: usize = 10_000;

/// Pure table formatter (header always shown, ES `_cat` style).
fn cat_table(rows: &[(String, String, u64)]) -> String {
    let mut out =
        String::from("health status index uuid pri rep docs.count store.size\n");
    for (name, uuid, docs) in rows {
        out.push_str(&format!("green open {name} {uuid} 1 0 {docs} 0b\n"));
    }
    out
}

/// Shared `/_count` + `/_search` prelude: the index meta, or the 404
/// / error reply a missing index must produce.
fn meta_or_404(shared: &Shared, index: &str) -> Result<crate::search::index_codec::IndexMeta, Reply> {
    match write::index_meta(shared, index) {
        Ok(Some(meta)) => Ok(meta),
        Ok(None) => Err(reply::error(
            404,
            "index_not_found_exception",
            &format!("no such index [{index}]"),
        )),
        Err(e) => Err(e.reply()),
    }
}

/// `POST|GET /{index}/_count`.
pub fn count_endpoint(shared: &Shared, index: &str, body: &[u8]) -> Reply {
    let meta = match meta_or_404(shared, index) {
        Ok(m) => m,
        Err(rep) => return rep,
    };
    let plan = match dsl::parse_search(body) {
        Ok(p) => p,
        Err(e) => return reply::error(e.status, &e.es_type, &e.reason),
    };
    let (_slot, prefix) = hash::slot_with_prefix(index.as_bytes());
    match exec::count(&shared.store, &prefix, index.as_bytes(), &meta, &plan) {
        Ok(n) => reply::count(n),
        Err(e) => reply::error(500, "search_phase_execution_exception", &format!("phase[query]: {e}")),
    }
}

/// `POST|GET /{index}/_search`: index lookup, DSL parse, execution,
/// envelope; `took_start` is when the request entered the router so
/// `took` covers parsing too (ES measures handler entry to reply).
pub fn search_endpoint(shared: &Shared, index: &str, body: &[u8], took_start: Instant) -> Reply {
    let meta = match meta_or_404(shared, index) {
        Ok(m) => m,
        Err(rep) => return rep,
    };
    let plan = match dsl::parse_search(body) {
        Ok(p) => p,
        Err(e) => return reply::error(e.status, &e.es_type, &e.reason),
    };
    let (_slot, prefix) = hash::slot_with_prefix(index.as_bytes());
    match exec::execute(&shared.store, &prefix, index.as_bytes(), &meta, &plan) {
        Ok(result) => reply::search(index, &result, took_start.elapsed().as_millis()),
        Err(e) => reply::error(500, "search_phase_execution_exception", &format!("phase[query]: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cat_table_format() {
        let t = cat_table(&[("b".into(), "u2".into(), 7), ("a".into(), "u1".into(), 0)]);
        assert_eq!(
            t,
            "health status index uuid pri rep docs.count store.size\n\
             green open b u2 1 0 7 0b\n\
             green open a u1 1 0 0 0b\n"
        );
    }
}
