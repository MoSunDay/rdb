//! RocksMQ-style route handlers: `/produce`, `/ack` -- plus the
//! shared plumbing (`run`, `channel_stream`, `HttpReply`) the split
//! handler modules (`consume_wait.rs`: long-poll `/consume`;
//! `pending.rs`: `/pending`) are built on.
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

use crate::command::dispatch;
use crate::lite::{self, TopicName};
use crate::state::Shared;
use crate::tx::session::ConnState;

use super::query::Query;
use super::respv::{self, Value};

/// HTTP-side group-name cap (Lite itself takes raw bytes; the HTTP
/// query surface stays bounded).
pub(crate) const MAX_GROUP_BYTES: usize = 256;

/// Consumer name group-mode consume registers deliveries under: the
/// HTTP front is one logical consumer.
pub const CONSUMER: &str = "http";

/// `delay_ms` ceiling (365 days): far past any scheduling horizon,
/// and low enough that `now + delay_ms` can never approach the
/// engine's overflow refusal. Beyond it is a 400, not a clamp
/// (unlike `wait_ms`: a mistyped year count is a client bug).
pub const MAX_DELAY_MS: u64 = 31_536_000_000;

// ---- shared helpers ------------------------------------------------------

/// Run one command through the dispatch pipeline; returns its reply.
pub(crate) async fn run(shared: &Shared, argv: &[&[u8]]) -> Vec<u8> {
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
/// Invalid names (charset of `lite::valid_part`) fail -> 400. Channel
/// names stay TWO-part at most: nested names are an engine-internal
/// form (the `<stream>/dlq` dead-letter targets), not this surface.
pub(crate) fn channel_stream(channel: &str) -> Result<String, String> {
    if channel.bytes().filter(|&b| b == b'/').count() > 1 {
        return Err(format!("ERR invalid channel name '{channel}'"));
    }
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

pub(crate) fn bad_request(msg: &str) -> HttpReply {
    HttpReply::text(400, msg)
}

pub(crate) fn server_error(detail: &str) -> HttpReply {
    HttpReply::text(500, &format!("internal error: {detail}"))
}

// ---- /produce ------------------------------------------------------------

/// `delay_ms` query value -> staged-delay milliseconds for the XADD
/// `DELAY` option (absent/`0` = plain produce, byte-identical argv to
/// the no-parameter form; non-numeric/negative/beyond
/// [`MAX_DELAY_MS`] -> 400). Pure parse: the delay semantics
/// (staging row, due visibility, fresh id at exchange) all live in
/// `src/lite/delay.rs`.
fn delay_ms(raw: Option<&str>) -> Result<u64, String> {
    let Some(raw) = raw else {
        return Ok(0);
    };
    let ms: u64 = raw
        .parse()
        .map_err(|_| format!("invalid 'delay_ms' parameter '{raw}'"))?;
    if ms > MAX_DELAY_MS {
        return Err(format!("'delay_ms' must be 0..={MAX_DELAY_MS}, got {ms}"));
    }
    Ok(ms)
}

/// `POST /produce?channel=NAME[&delay_ms=MS]` body=payload: XADD with
/// the single field pair ("v", body) -- byte-identical to the Kafka
/// front's keyless/headerless produce rule, so both fronts read each
/// other's messages. 200 body = the message id (`<ms>-<seq>`); with
/// `delay_ms > 0` that id is the XADD-time RESERVATION token (the due
/// exchange allocates a fresh id when the message becomes visible --
/// see `features/mq-lite.md`).
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
    let delay = match delay_ms(query.get("delay_ms")) {
        Ok(ms) => ms,
        Err(e) => return bad_request(&e),
    };
    let stream = match channel_stream(channel) {
        Ok(s) => s,
        Err(_) => return bad_request("invalid channel name"),
    };
    // delay == 0 keeps the argv byte-identical to the plain form, so
    // the no-parameter behavior cannot drift.
    let out = if delay == 0 {
        run(shared, &[b"XADD", stream.as_bytes(), b"*", b"v", body]).await
    } else {
        let d = delay.to_string();
        run(
            shared,
            &[
                b"XADD",
                stream.as_bytes(),
                b"*",
                b"DELAY",
                d.as_bytes(),
                b"v",
                body,
            ],
        )
        .await
    };
    match respv::parse(&out) {
        Ok(Value::Bulk(Some(id))) => HttpReply::text(200, &String::from_utf8_lossy(&id)),
        Ok(Value::Error(e)) => server_error(&String::from_utf8_lossy(&e)),
        _ => server_error("unexpected xadd reply"),
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
    fn delay_ms_bounds() {
        assert_eq!(delay_ms(None).unwrap(), 0);
        assert_eq!(delay_ms(Some("0")).unwrap(), 0);
        assert_eq!(delay_ms(Some("500")).unwrap(), 500);
        assert_eq!(
            delay_ms(Some(&MAX_DELAY_MS.to_string())).unwrap(),
            MAX_DELAY_MS
        );
        assert!(delay_ms(Some(&(MAX_DELAY_MS + 1).to_string())).is_err());
        assert!(delay_ms(Some("-1")).is_err());
        assert!(delay_ms(Some("x")).is_err());
        assert!(delay_ms(Some("")).is_err());
    }
}
