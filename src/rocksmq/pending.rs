//! `POST /pending` for the RocksMQ HTTP front (WP4): read-only
//! visibility into a consumer group's PEL. A thin, side-effect-free
//! passthrough -- the handler dispatches `XPENDING <stream> <group>`
//! (the engine's own summary path, `src/lite/pending.rs::summarize`)
//! and re-renders it as JSON, so the HTTP and RESP faces can never
//! disagree on the pending口径: same total, same min/max pending ids,
//! same per-consumer distribution.

use serde_json::json;

use crate::state::Shared;

use super::api::{bad_request, channel_stream, run, server_error, HttpReply, MAX_GROUP_BYTES};
use super::query::Query;
use super::respv::{self, Value};

/// `POST /pending?channel=NAME&group=G`: XPENDING summary. No PEL
/// mutation, no group creation; an unknown group answers 404
/// (XPENDING's NOGROUP, the same probe `/ack` uses).
pub async fn pending(shared: &Shared, query: &Query) -> HttpReply {
    let (Some(channel), Some(group)) = (query.get("channel"), query.get("group")) else {
        return bad_request("missing 'channel' or 'group' query parameter");
    };
    if channel.is_empty() || group.is_empty() {
        return bad_request("empty 'channel' or 'group' query parameter");
    }
    if group.len() > MAX_GROUP_BYTES {
        return bad_request("group name too long");
    }
    let stream = match channel_stream(channel) {
        Ok(s) => s,
        Err(_) => return bad_request("invalid channel name"),
    };
    let out = run(shared, &[b"XPENDING", stream.as_bytes(), group.as_bytes()]).await;
    if out.starts_with(b"-NOGROUP") {
        return HttpReply::text(404, "no such consumer group");
    }
    match respv::parse(&out) {
        Ok(Value::Array(Some(items))) => match pending_json(channel, group, &items) {
            Some(reply) => reply,
            None => server_error("unexpected xpending summary reply"),
        },
        Ok(Value::Error(e)) => server_error(&String::from_utf8_lossy(&e)),
        _ => server_error("unexpected xpending reply"),
    }
}

/// XPENDING summary items (`[total, min|null, max|null, name, n, ...]`)
/// -> the JSON reply. Pure; `None` = unexpected shape (a 500, an
/// internal bug). An empty PEL is `[0, nil, nil]`: `min_id`/`max_id`
/// render as JSON null and `consumers` as `[]`, matching what the
/// RESP summary answers for an empty PEL.
fn pending_json(channel: &str, group: &str, items: &[Value]) -> Option<HttpReply> {
    let Value::Int(total) = items.first()? else {
        return None;
    };
    let id_of = |v: &Value| -> Option<String> { String::from_utf8(v.as_bulk()?.to_vec()).ok() };
    let min_id = id_of(items.get(1)?);
    let max_id = id_of(items.get(2)?);
    let mut consumers = Vec::new();
    let mut i = 3;
    while i < items.len() {
        let name = id_of(items.get(i)?)?;
        let Value::Int(n) = items.get(i + 1)? else {
            return None;
        };
        consumers.push(json!({"name": name, "pending": n}));
        i += 2;
    }
    // The consumer section is flat name/count pairs; a dangling item
    // means the reply is not a summary -- refuse instead of guessing.
    if i != items.len() {
        return None;
    }
    let body = json!({
        "channel": channel,
        "group": group,
        "pending": total,
        "min_id": min_id,
        "max_id": max_id,
        "consumers": consumers,
    });
    Some(HttpReply::json(200, body.to_string().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::Bulk(Some(s.as_bytes().to_vec()))
    }

    #[test]
    fn pending_json_shapes() {
        let empty = [Value::Int(0), Value::Bulk(None), Value::Bulk(None)];
        let r = pending_json("ch", "g", &empty).unwrap();
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["channel"], "ch");
        assert_eq!(v["group"], "g");
        assert_eq!(v["pending"], 0);
        assert!(v["min_id"].is_null());
        assert!(v["max_id"].is_null());
        assert_eq!(v["consumers"], serde_json::json!([]));

        let full = [
            Value::Int(3),
            bulk("1760000000000-0"),
            bulk("1760000000005-1"),
            bulk("http"),
            Value::Int(3),
        ];
        let v: serde_json::Value =
            serde_json::from_slice(&pending_json("ch", "g", &full).unwrap().body).unwrap();
        assert_eq!(v["pending"], 3);
        assert_eq!(v["min_id"], "1760000000000-0");
        assert_eq!(v["max_id"], "1760000000005-1");
        assert_eq!(
            v["consumers"],
            serde_json::json!([{"name": "http", "pending": 3}])
        );

        // dangling / non-integer shapes refuse (None -> 500)
        let dangling = [
            Value::Int(1),
            Value::Bulk(None),
            Value::Bulk(None),
            bulk("x"),
        ];
        assert!(pending_json("ch", "g", &dangling).is_none());
        let not_int = [bulk("3"), Value::Bulk(None), Value::Bulk(None)];
        assert!(pending_json("ch", "g", &not_int).is_none());
    }
}
