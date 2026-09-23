//! RocksMQ-style route handlers: `/produce`, `/consume`, `/ack`.
//!
//! Engine reuse: every handler builds an argv and runs it through
//! `command::dispatch` (XADD / XREADGROUP / XGROUP / XPENDING / XACK /
//! XREVRANGE), then decodes the single RESP reply via [`super::respv`].
//! That buys the full dispatch pipeline for free -- slot-prefix
//! computation, the panic net, the backup read-only gate and the
//! `rdb_command_latency` histogram (labels xadd/xreadgroup/...), so
//! this front adds NO new metric. Calling `src/lite/` handlers directly
//! would duplicate the prefix computation and skip both safety net and
//! metrics for the same amount of reply parsing.

use serde_json::json;

use crate::command::dispatch;
use crate::lite::{self, TopicName};
use crate::state::Shared;
use crate::tx::session::ConnState;

use super::query::Query;
use super::respv::{self, Value};
use super::MAX_BATCH;

/// HTTP-side group-name cap (Lite itself takes raw bytes; the HTTP
/// query surface stays bounded).
const MAX_GROUP_BYTES: usize = 256;

/// Consumer name group-mode consume registers deliveries under: the
/// HTTP front is one logical consumer.
pub const CONSUMER: &str = "http";

// ---- shared helpers ------------------------------------------------------

/// Run one command through the dispatch pipeline; returns its reply.
async fn run(shared: &Shared, argv: &[&[u8]]) -> Vec<u8> {
    let mut conn = ConnState::default();
    let mut out = Vec::new();
    let mut close = false;
    dispatch(
        shared,
        argv.iter().map(|a| a.to_vec()).collect(),
        &mut conn,
        &mut out,
        &mut close,
    )
    .await;
    out
}

/// `channel` query value -> full Lite stream name. A bare name maps to
/// its `q0` queue (the same default queue RESP XADD auto-pick and the
/// Kafka front's partition 0 use); `parent/child` passes through.
/// Invalid names (charset of `lite::valid_part`, at most one `/`)
/// fail -> 400.
pub(crate) fn channel_stream(channel: &str) -> Result<String, String> {
    match lite::parse_topic_name(channel.as_bytes()) {
        Ok(TopicName::Stream(..)) => Ok(channel.to_string()),
        Ok(TopicName::Parent(p)) => Ok(format!("{}/q0", String::from_utf8_lossy(&p))),
        Err(e) => Err(e),
    }
}

/// One HTTP response: status line + typed body bytes.
pub struct HttpReply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// 405 answers carry `Allow: POST`.
    pub allow_post: bool,
}

impl HttpReply {
    pub fn text(status: u16, msg: &str) -> HttpReply {
        HttpReply {
            status,
            content_type: "text/plain",
            body: msg.as_bytes().to_vec(),
            allow_post: false,
        }
    }

    pub fn json(status: u16, body: &[u8]) -> HttpReply {
        HttpReply {
            status,
            content_type: "application/json",
            body: body.to_vec(),
            allow_post: false,
        }
    }
}

fn bad_request(msg: &str) -> HttpReply {
    HttpReply::text(400, msg)
}

fn server_error(detail: &str) -> HttpReply {
    HttpReply::text(500, &format!("internal error: {detail}"))
}

/// One consumed message of the JSON reply.
struct Msg {
    id: String,
    body: Vec<u8>,
}

/// Decode one XREAD/XREADGROUP/XREVRANGE entry frame
/// (`[id, [f1, v1, ...]]`): the body is the `v` field (the 1-pair rule
/// the Kafka front's produce also writes); entries without a `v` field
/// (hand-written RESP entries) yield an empty body.
fn msg_of(frame: &Value) -> Option<Msg> {
    let arr = frame.as_array()?;
    let id = String::from_utf8(arr.first()?.as_bulk()?.to_vec()).ok()?;
    let fields = arr.get(1)?.as_array()?;
    let body = fields
        .chunks(2)
        .find(|c| c.first().is_some_and(|f| f.as_bulk() == Some(b"v")))
        .and_then(|c| c.get(1))
        .and_then(|v| v.as_bulk())
        .unwrap_or(&[])
        .to_vec();
    Some(Msg { id, body })
}

fn msgs_json(msgs: Vec<Msg>) -> HttpReply {
    let items: Vec<_> = msgs
        .iter()
        .map(|m| json!({"id": m.id, "body": base64(&m.body)}))
        .collect();
    HttpReply::json(200, json!({"msgs": items}).to_string().as_bytes())
}

/// Hand-rolled standard base64 (A-Za-z0-9+/ with padding): no crate
/// added for one encoder.
pub(crate) fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let mut n = (chunk[0] as u32) << 16;
        if let Some(b) = chunk.get(1) {
            n |= (*b as u32) << 8;
        }
        if let Some(b) = chunk.get(2) {
            n |= *b as u32;
        }
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// `n` query value -> batch size (default 1; 0 or > MAX_BATCH invalid).
fn batch_n(raw: Option<&str>) -> Result<usize, String> {
    match raw {
        None => Ok(1),
        Some(raw) => {
            let n: usize = raw
                .parse()
                .map_err(|_| format!("invalid 'n' parameter '{raw}'"))?;
            if n == 0 || n > MAX_BATCH {
                return Err(format!("'n' must be 1..={MAX_BATCH}, got {n}"));
            }
            Ok(n)
        }
    }
}

// ---- /produce ------------------------------------------------------------

/// `POST /produce?channel=NAME` body=payload: XADD with the single
/// field pair ("v", body) -- byte-identical to the Kafka front's
/// keyless/headerless produce rule, so both fronts read each other's
/// messages. 200 body = the message id (`<ms>-<seq>`).
pub async fn produce(shared: &Shared, query: &Query, body: &[u8]) -> HttpReply {
    let Some(channel) = query.get("channel") else {
        return bad_request("missing 'channel' query parameter");
    };
    if channel.is_empty() {
        return bad_request("empty 'channel' query parameter");
    }
    if body.is_empty() {
        return bad_request("empty body: nothing to produce");
    }
    let stream = match channel_stream(channel) {
        Ok(s) => s,
        Err(_) => return bad_request("invalid channel name"),
    };
    let out = run(shared, &[b"XADD", stream.as_bytes(), b"*", b"v", body]).await;
    match respv::parse(&out) {
        Ok(Value::Bulk(Some(id))) => HttpReply::text(200, &String::from_utf8_lossy(&id)),
        Ok(Value::Error(e)) => server_error(&String::from_utf8_lossy(&e)),
        _ => server_error("unexpected xadd reply"),
    }
}

// ---- /consume ------------------------------------------------------------

/// Group mode: XREADGROUP `>` under consumer "http"; a missing group is
/// auto-created AT THE STREAM HEAD (`XGROUP CREATE ... 0`), so the
/// first group consume sees every retained entry (Kafka
/// auto.offset.reset=earliest). A missing stream stays invisible
/// (empty array, nothing created).
async fn consume_group(shared: &Shared, stream: &str, group: &str, n: usize) -> HttpReply {
    let n_str = n.to_string();
    let argv: [&[u8]; 9] = [
        b"XREADGROUP",
        b"GROUP",
        group.as_bytes(),
        CONSUMER.as_bytes(),
        b"COUNT",
        n_str.as_bytes(),
        b"STREAMS",
        stream.as_bytes(),
        b">",
    ];
    let mut out = run(shared, &argv).await;
    if out.starts_with(b"-NOGROUP") {
        // RocksMQ has no create-group endpoint: first consume creates it.
        let created = run(
            shared,
            &[
                b"XGROUP",
                b"CREATE",
                stream.as_bytes(),
                group.as_bytes(),
                b"0-0",
            ],
        )
        .await;
        if created.starts_with(b"-ERR The XGROUP subcommand requires the key to exist") {
            return msgs_json(Vec::new()); // stream missing: empty by contract
        }
        // +OK, or BUSYGROUP when a concurrent request won the race.
        out = run(shared, &argv).await;
        if out.starts_with(b"-NOGROUP") {
            return server_error("consumer group disappeared during consume");
        }
    }
    match respv::parse(&out) {
        Ok(Value::Array(None)) => msgs_json(Vec::new()), // nil: nothing new
        Ok(Value::Array(Some(list))) => {
            let msgs = list
                .first()
                .and_then(|pair| pair.as_array())
                .and_then(|p| p.get(1))
                .and_then(|entries| entries.as_array())
                .map(|entries| entries.iter().filter_map(msg_of).collect())
                .unwrap_or_default();
            msgs_json(msgs)
        }
        Ok(Value::Error(e)) => server_error(&String::from_utf8_lossy(&e)),
        _ => server_error("unexpected xreadgroup reply"),
    }
}

/// No-group mode: an independent pull of the LATEST `n` entries
/// (XREVRANGE tail scan, reversed back to ascending order). No
/// progress is recorded anywhere: repeats and misses are both possible
/// (documented divergence -- real RocksMQ tracks per-consumer cursors).
async fn consume_tail(shared: &Shared, stream: &str, n: usize) -> HttpReply {
    let n_str = n.to_string();
    let argv: [&[u8]; 6] = [
        b"XREVRANGE",
        stream.as_bytes(),
        b"+",
        b"-",
        b"COUNT",
        n_str.as_bytes(),
    ];
    let out = run(shared, &argv).await;
    match respv::parse(&out) {
        Ok(Value::Array(Some(entries))) => {
            let mut msgs: Vec<Msg> = entries.iter().filter_map(msg_of).collect();
            msgs.reverse(); // XREVRANGE answers newest-first
            msgs_json(msgs)
        }
        Ok(Value::Array(None)) => msgs_json(Vec::new()),
        Ok(Value::Error(e)) => server_error(&String::from_utf8_lossy(&e)),
        _ => server_error("unexpected xrevrange reply"),
    }
}

/// `POST /consume?channel=NAME[&group=G][&n=K]`.
pub async fn consume(shared: &Shared, query: &Query) -> HttpReply {
    let Some(channel) = query.get("channel") else {
        return bad_request("missing 'channel' query parameter");
    };
    if channel.is_empty() {
        return bad_request("empty 'channel' query parameter");
    }
    let n = match batch_n(query.get("n")) {
        Ok(n) => n,
        Err(e) => return bad_request(&e),
    };
    let stream = match channel_stream(channel) {
        Ok(s) => s,
        Err(_) => return bad_request("invalid channel name"),
    };
    match query.get("group") {
        Some(g) if !g.is_empty() => {
            if g.len() > MAX_GROUP_BYTES {
                return bad_request("group name too long");
            }
            consume_group(shared, &stream, g, n).await
        }
        _ => consume_tail(shared, &stream, n).await, // absent/empty group: tail pull
    }
}

// ---- /ack ----------------------------------------------------------------

/// `POST /ack?channel=NAME&group=G&id=<ms>-<seq>`: XACK. Idempotent --
/// an id outside the PEL still answers 200 (XACK itself would count
/// 0). A missing group answers 404: XACK cannot tell (it replies :0
/// for unknown groups too), so the group is probed with a summary
/// XPENDING (its NOGROUP path) first.
pub async fn ack(shared: &Shared, query: &Query) -> HttpReply {
    let (Some(channel), Some(group), Some(id)) =
        (query.get("channel"), query.get("group"), query.get("id"))
    else {
        return bad_request("missing 'channel', 'group' or 'id' query parameter");
    };
    if channel.is_empty() || group.is_empty() || id.is_empty() {
        return bad_request("empty 'channel', 'group' or 'id' query parameter");
    }
    if group.len() > MAX_GROUP_BYTES {
        return bad_request("group name too long");
    }
    let stream = match channel_stream(channel) {
        Ok(s) => s,
        Err(_) => return bad_request("invalid channel name"),
    };
    let probe = run(shared, &[b"XPENDING", stream.as_bytes(), group.as_bytes()]).await;
    if probe.starts_with(b"-NOGROUP") {
        return HttpReply::text(404, "no such consumer group");
    }
    let out = run(
        shared,
        &[b"XACK", stream.as_bytes(), group.as_bytes(), id.as_bytes()],
    )
    .await;
    match respv::parse(&out) {
        Ok(Value::Int(_)) => HttpReply::text(200, "ok"),
        Ok(Value::Error(e)) => {
            let text = String::from_utf8_lossy(&e).to_string();
            if text.contains("Invalid stream ID") {
                bad_request("invalid message id")
            } else {
                server_error(&text)
            }
        }
        _ => server_error("unexpected xack reply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_maps_bare_names_to_q0() {
        assert_eq!(channel_stream("ch1").unwrap(), "ch1/q0");
        assert_eq!(channel_stream("parent/child").unwrap(), "parent/child");
        assert!(channel_stream("a/b/c").is_err());
        assert!(channel_stream("").is_err());
        assert!(channel_stream("bad name").is_err());
        assert!(channel_stream("a/").is_err());
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn batch_bounds() {
        assert_eq!(batch_n(None).unwrap(), 1);
        assert_eq!(batch_n(Some("7")).unwrap(), 7);
        assert_eq!(batch_n(Some(&MAX_BATCH.to_string())).unwrap(), MAX_BATCH);
        assert!(batch_n(Some("0")).is_err());
        assert!(batch_n(Some("101")).is_err());
        assert!(batch_n(Some("x")).is_err());
    }
}
