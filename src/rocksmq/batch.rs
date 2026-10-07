//! Batch routes for the RocksMQ HTTP front (P3 #10/#11): `POST
//! /produce_batch` and `POST /ack_batch`. The request body is a JSON
//! array whose elements are EXACTLY the existing single-route payloads
//! re-encoded as JSON objects (`channel`/`delay_ms`/`body` for
//! produce, `channel`/`group`/`id` for ack -- `api.rs`'s shapes); the
//! reply is a JSON array of per-item results in request order.
//!
//! Per-item semantics are NOT reimplemented here: every element is
//! rebuilt into the single route's `Query` (+ body bytes) and run
//! through the SAME handler (`api::produce` / `api::ack`), so behavior
//! is byte-identical by construction -- same validation order, same
//! 400/404 texts, same 200 bodies. Items are independent: one failing
//! element is reported in place and never aborts the rest. Elements
//! run sequentially (request order = XADD order, so batch produce ids
//! ascend), each through its own `command::dispatch` round like a
//! single request would.
//!
//! Item result shape: `{"status": <single-route status>, "body":
//! "<single-route body text>"}` -- the single route's reply verbatim,
//! re-homed in JSON. The batch reply itself is 200 whenever the array
//! parsed (per-item failures live in the elements); only a non-array
//! body, a non-object element or an over-[`MAX_BATCH`] array is a
//! whole-request 400.

use serde_json::{json, Value as J};

use crate::state::Shared;

use super::api::{self, bad_request, HttpReply};
use super::query::Query;
use super::MAX_BATCH;

/// `POST /produce_batch` body = `[{"channel": NAME, "delay_ms": MS?,
/// "body": B64}, ...]` (`delay_ms` may be a JSON number or string;
/// `body` is standard base64, the byte-faithful encoding of the single
/// route's raw body -- the same alphabet `/consume` replies with, so a
/// consumed message re-produces byte-exactly). An absent `body` is the
/// empty body, which the single handler itself refuses.
pub async fn produce_batch(shared: &Shared, body: &[u8]) -> HttpReply {
    let items = match items_of(body) {
        Ok(v) => v,
        Err(e) => return bad_request(&e),
    };
    let mut out = Vec::with_capacity(items.len());
    for item in &items {
        let r = match produce_item(item) {
            Ok((query, payload)) => api::produce(shared, &query, &payload).await,
            Err(e) => bad_request(&e),
        };
        out.push(item_json(r));
    }
    HttpReply::json(200, json!(out).to_string().as_bytes())
}

/// `POST /ack_batch` body = `[{"channel": NAME, "group": G, "id":
/// "<ms>-<seq>"}, ...]`: N independent XACKs (idempotent per item, the
/// single route's contract).
pub async fn ack_batch(shared: &Shared, body: &[u8]) -> HttpReply {
    let items = match items_of(body) {
        Ok(v) => v,
        Err(e) => return bad_request(&e),
    };
    let mut out = Vec::with_capacity(items.len());
    for item in &items {
        let r = match query_of(item, &["channel", "group", "id"]) {
            Ok(query) => api::ack(shared, &query).await,
            Err(e) => bad_request(&e),
        };
        out.push(item_json(r));
    }
    HttpReply::json(200, json!(out).to_string().as_bytes())
}

// ---- shared plumbing ------------------------------------------------------

/// One single-route reply as a per-item JSON result.
fn item_json(reply: HttpReply) -> J {
    json!({
        "status": reply.status,
        "body": String::from_utf8_lossy(&reply.body),
    })
}

/// Request body -> item objects; a non-array body or an over-cap array
/// fails the whole request (400), everything else is per-item.
fn items_of(body: &[u8]) -> Result<Vec<J>, String> {
    let parsed: J = serde_json::from_slice(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    let items = parsed
        .as_array()
        .ok_or_else(|| "batch body must be a JSON array".to_string())?;
    if items.len() > MAX_BATCH {
        return Err(format!("batch accepts at most {MAX_BATCH} items"));
    }
    Ok(items.clone())
}

/// One produce element -> the single route's (query, body bytes). Only
/// shape-level problems fail here (non-string channel, bad base64);
/// every semantic check (empty body, channel charset, `delay_ms`
/// bounds) is left to `api::produce` so its errors surface verbatim.
fn produce_item(item: &J) -> Result<(Query, Vec<u8>), String> {
    let mut query = query_of(item, &["channel"])?;
    if let Some(v) = item.get("delay_ms") {
        let raw = match v {
            J::Number(n) => n.to_string(),
            J::String(s) => s.clone(),
            J::Null => String::new(),
            _ => return Err("'delay_ms' must be a number or string".to_string()),
        };
        if !raw.is_empty() {
            query.push("delay_ms", raw);
        }
    }
    let payload = match item.get("body") {
        None | Some(J::Null) => Vec::new(),
        Some(J::String(s)) => b64_decode(s).ok_or_else(|| "invalid base64 'body'".to_string())?,
        Some(_) => return Err("'body' must be a base64 string".to_string()),
    };
    Ok((query, payload))
}

/// Element -> `Query` holding the string fields the single route reads
/// (absent fields stay absent: the single handler answers its own
/// "missing parameter" error).
fn query_of(item: &J, keys: &[&str]) -> Result<Query, String> {
    let obj = item
        .as_object()
        .ok_or_else(|| "batch item must be a JSON object".to_string())?;
    let mut pairs = Vec::new();
    for key in keys {
        match obj.get(*key) {
            None | Some(J::Null) => {}
            Some(J::String(s)) => pairs.push(((*key).to_string(), s.clone())),
            Some(_) => return Err(format!("'{key}' must be a string")),
        }
    }
    Ok(Query::of(pairs))
}

/// Hand-rolled standard base64 decoder (the twin of
/// `consume_wait::base64`; no crate for one decoder). Strict: standard
/// alphabet plus `=` padding only, length a multiple of 4, padding
/// confined to the last two positions, trailing bits zero (canonical).
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    if b.is_empty() {
        return Some(Vec::new());
    }
    if !b.len().is_multiple_of(4) || b.len() < 4 {
        return None;
    }
    // `=` is not in the alphabet, so interior padding fails the lookup.
    let (pad, tail) = match b[b.len() - 1] {
        b'=' if b[b.len() - 2] == b'=' => (2, &b[..b.len() - 2]),
        b'=' => (1, &b[..b.len() - 1]),
        _ => (0, b),
    };
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for &c in tail {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // Canonical padding leaves 2 (one '=') or 4 (two '=') zero bits.
    if pad > 0 && acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::consume_wait::base64;
    use super::*;

    #[test]
    fn base64_roundtrip_known_vectors() {
        for v in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            b"\x00\xff\x10",
        ] {
            assert_eq!(b64_decode(&base64(v)).unwrap(), v.to_vec());
        }
        assert_eq!(b64_decode("Zm9v").unwrap(), b"foo");
        // strictness: bad alphabet, bad length, interior padding
        assert!(b64_decode("Zm9*").is_none());
        assert!(b64_decode("Zm9vY").is_none());
        assert!(b64_decode("Zm9=").is_none()); // 1 leftover char
        assert!(b64_decode("Z===").is_none()); // 3 pads
        assert!(b64_decode("Zm9vYg=d2F2ZQ==").is_none());
    }

    #[tokio::test]
    async fn produce_item_shapes() {
        // happy path: channel + numeric delay_ms + base64 body
        let item = json!({"channel": "ch", "delay_ms": 5, "body": "Zm9v"});
        let (q, body) = produce_item(&item).unwrap();
        assert_eq!(q.get("channel"), Some("ch"));
        assert_eq!(q.get("delay_ms"), Some("5"));
        assert_eq!(body, b"foo");
        // absent optional fields = the plain single-route request
        let (q, body) = produce_item(&json!({"channel": "ch"})).unwrap();
        assert_eq!(q.get("delay_ms"), None);
        assert!(body.is_empty());
        // null delay_ms = absent; null body = empty
        let (q, _) =
            produce_item(&json!({"channel": "c", "delay_ms": null, "body": null})).unwrap();
        assert_eq!(q.get("delay_ms"), None);
        // shape errors surface as per-item 400 texts
        assert!(produce_item(&json!({"channel": 5})).is_err());
        assert!(produce_item(&json!({"channel": "c", "body": "!!"})).is_err());
        assert!(produce_item(&json!({"channel": "c", "body": 7})).is_err());
        assert!(produce_item(&json!({"channel": "c", "delay_ms": true})).is_err());
        assert!(produce_item(&json!("str")).is_err());
    }

    #[test]
    fn items_of_shapes() {
        assert!(items_of(b"[]").unwrap().is_empty());
        assert_eq!(items_of(b"[{\"channel\":\"a\"}]").unwrap().len(), 1);
        assert!(items_of(b"{}").is_err());
        assert!(items_of(b"not json").is_err());
        let big = format!("[{}]", vec!["1"; MAX_BATCH + 1].join(","));
        assert!(items_of(big.as_bytes()).is_err());
        let full = format!("[{}]", vec!["1"; MAX_BATCH].join(","));
        assert_eq!(items_of(full.as_bytes()).unwrap().len(), MAX_BATCH);
    }
}
