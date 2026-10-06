//! The waiting half of the RocksMQ HTTP front (WP4): `wait_ms`
//! long-poll consume, split out of `api.rs` wholesale so that file
//! keeps its route-skeleton budget.
//!
//! Waiting is NOT re-implemented here. `wait_ms` maps to a `BLOCK`
//! option on the very `command::dispatch` argv the non-waiting paths
//! already build, so the whole park/wake machinery is inherited
//! verbatim from `src/lite/park_wait.rs` (the XREAD/XREADGROUP-shared
//! wait loop): ONE waiter registered under the stream's meta key
//! BEFORE the final read, parks sliced with the budget recomputed per
//! wake, and a wake-and-revalidate loop that closes the lost-notify
//! window against an XADD committing between scan and registration.
//! A delayed produce does not wake readers at write time (a staged
//! row is not an entry yet); the due sweep's exchange does the
//! notifying, so a parked consumer wakes exactly when the delayed
//! message becomes consumable.

use serde_json::json;

use crate::state::Shared;

use super::api::{
    bad_request, channel_stream, run, server_error, HttpReply, CONSUMER, MAX_GROUP_BYTES,
};
use super::query::Query;
use super::respv::{self, Value};
use super::MAX_BATCH;

/// `wait_ms` ceiling: longer asks clamp to it (a long poll should
/// stay under common upstream gateway idle timeouts, 75s/120s class).
pub const MAX_WAIT_MS: u64 = 60_000;

// ---- shared helpers (moved from api.rs with the consume flow) ------------

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

/// `wait_ms` query value -> park budget (absent/`0` = none: the reply
/// bytes are then identical to the no-wait behavior; non-numeric or
/// negative -> 400 like every other parameter; > MAX_WAIT_MS clamps).
fn wait_ms(raw: Option<&str>) -> Result<u64, String> {
    let Some(raw) = raw else {
        return Ok(0);
    };
    let ms: u64 = raw
        .parse()
        .map_err(|_| format!("invalid 'wait_ms' parameter '{raw}'"))?;
    Ok(ms.min(MAX_WAIT_MS))
}

/// Entry list out of one `[stream, entries]` reply pair (XREAD and
/// XREADGROUP share the shape; empty streams are left out entirely).
fn entries_of(list: &[Value]) -> Vec<Msg> {
    list.first()
        .and_then(|pair| pair.as_array())
        .and_then(|p| p.get(1))
        .and_then(|entries| entries.as_array())
        .map(|entries| entries.iter().filter_map(msg_of).collect())
        .unwrap_or_default()
}

// ---- /consume (wait_ms long poll) ----------------------------------------

/// `POST /consume?channel=NAME[&group=G][&n=K][&wait_ms=MS]`: group
/// mode XREADGROUP `>` / no-group tail pull, both able to park up to
/// `wait_ms` for the first message instead of answering empty at once.
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
    let wait = match wait_ms(query.get("wait_ms")) {
        Ok(w) => w,
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
            consume_group(shared, &stream, g, n, wait).await
        }
        _ => consume_tail(shared, &stream, n, wait).await, // absent/empty group: tail pull
    }
}

/// Park up to `wait` ms for the first entry appended after now
/// (`XREAD BLOCK ... STREAMS <stream> $` -- the same park loop every
/// blocking Lite read uses). `Some(msgs)` = data landed (or the budget
/// was interrupted by a wake with data), `None` = budget elapsed with
/// nothing visible. On a MISSING stream `$` resolves to the minimum
/// id, so the very first append (or a delayed row's due exchange)
/// wakes it; XREAD itself never mutates, so callers re-read through
/// their own path afterwards.
async fn park_stream(shared: &Shared, stream: &str, n: usize, wait: u64) -> Option<Vec<Msg>> {
    let (n_str, wait_str) = (n.to_string(), wait.to_string());
    let argv: [&[u8]; 8] = [
        b"XREAD",
        b"BLOCK",
        wait_str.as_bytes(),
        b"COUNT",
        n_str.as_bytes(),
        b"STREAMS",
        stream.as_bytes(),
        b"$",
    ];
    let out = run(shared, &argv).await;
    match respv::parse(&out) {
        Ok(Value::Array(None)) => None, // budget elapsed: empty, not an error
        Ok(Value::Array(Some(list))) => Some(entries_of(&list)),
        // An error reply means the park never legitimately started
        // (engine-side); treat it as "nothing landed" so the callers'
        // own read path surfaces any real problem.
        _ => None,
    }
}

/// Group mode: XREADGROUP `>` under consumer "http"; a missing group is
/// auto-created AT THE STREAM HEAD (`XGROUP CREATE ... 0-0`), so the
/// first group consume sees every retained entry (Kafka
/// auto.offset.reset=earliest). A missing stream stays invisible
/// (empty array, nothing created). `wait` > 0 adds `BLOCK <wait>`: the
/// dispatch parks on the WaitHub until an entry lands (or the delayed
/// sweep exchanges one due) and only then re-reads; expiry answers the
/// same empty `{"msgs":[]}` as a dry non-blocking read. A missing
/// STREAM has no group to park under, so with a wait budget the
/// request parks on the stream's meta key instead ([`park_stream`])
/// and runs the group flow once the first append has landed.
async fn consume_group(
    shared: &Shared,
    stream: &str,
    group: &str,
    n: usize,
    wait: u64,
) -> HttpReply {
    let (n_str, wait_str) = (n.to_string(), wait.to_string());
    let mut argv: Vec<&[u8]> = vec![
        b"XREADGROUP",
        b"GROUP",
        group.as_bytes(),
        CONSUMER.as_bytes(),
        b"COUNT",
        n_str.as_bytes(),
    ];
    if wait > 0 {
        argv.extend_from_slice(&[b"BLOCK", wait_str.as_bytes()]);
    }
    argv.extend_from_slice(&[b"STREAMS", stream.as_bytes(), b">"]);
    let mut out = run(shared, &argv).await;
    if out.starts_with(b"-NOGROUP") {
        // RocksMQ has no create-group endpoint: first consume creates it.
        let mut created = run(
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
        if created.starts_with(b"-ERR The XGROUP subcommand requires the key to exist") && wait > 0
        {
            // Stream missing: park for the FIRST append (which creates
            // the stream), then retry the create+read once -- the group
            // must still start at the head, PEL registration and all.
            if park_stream(shared, stream, n, wait).await.is_none() {
                return msgs_json(Vec::new()); // budget elapsed, still no stream
            }
            created = run(
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
        }
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
        Ok(Value::Array(None)) => msgs_json(Vec::new()), // nil: timed out or nothing new
        Ok(Value::Array(Some(list))) => msgs_json(entries_of(&list)),
        Ok(Value::Error(e)) => server_error(&String::from_utf8_lossy(&e)),
        _ => server_error("unexpected xreadgroup reply"),
    }
}

/// No-group mode: an independent pull of the LATEST `n` entries
/// (XREVRANGE tail scan, reversed back to ascending order). No
/// progress is recorded anywhere: repeats and misses are both possible
/// (documented divergence -- real RocksMQ tracks per-consumer cursors).
/// With `wait` > 0 and a dry buffer the request then parks via
/// `XREAD BLOCK` on the `$` position (the same park loop XREADGROUP
/// uses): `$` snapshots last_id NOW, so only appends after this moment
/// can wake it -- anything retained was already answered above.
async fn consume_tail(shared: &Shared, stream: &str, n: usize, wait: u64) -> HttpReply {
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
    let msgs: Vec<Msg> = match respv::parse(&out) {
        Ok(Value::Array(Some(entries))) => {
            let mut msgs: Vec<Msg> = entries.iter().filter_map(msg_of).collect();
            msgs.reverse(); // XREVRANGE answers newest-first
            msgs
        }
        Ok(Value::Array(None)) => Vec::new(),
        Ok(Value::Error(e)) => return server_error(&String::from_utf8_lossy(&e)),
        _ => return server_error("unexpected xrevrange reply"),
    };
    if !msgs.is_empty() || wait == 0 {
        return msgs_json(msgs); // buffer has data, or no budget: current behavior
    }
    // Dry buffer with a budget: park for the first append after now.
    let msgs = park_stream(shared, stream, n, wait)
        .await
        .unwrap_or_default();
    msgs_json(msgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_ms_parse_and_clamp() {
        assert_eq!(wait_ms(None).unwrap(), 0);
        assert_eq!(wait_ms(Some("0")).unwrap(), 0);
        assert_eq!(wait_ms(Some("500")).unwrap(), 500);
        assert_eq!(wait_ms(Some("60000")).unwrap(), 60000);
        // oversize clamps instead of erroring (lenient by contract)
        assert_eq!(
            wait_ms(Some(&MAX_WAIT_MS.to_string())).unwrap(),
            MAX_WAIT_MS
        );
        assert_eq!(
            wait_ms(Some(&(MAX_WAIT_MS + 1).to_string())).unwrap(),
            MAX_WAIT_MS
        );
        assert!(wait_ms(Some("-1")).is_err());
        assert!(wait_ms(Some("x")).is_err());
        assert!(wait_ms(Some("")).is_err());
        assert!(wait_ms(Some("1.5")).is_err());
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
}
