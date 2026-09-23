//! Kafka per-connection loop: read one int32-length-prefixed frame,
//! parse the request header, dispatch by api key, write the framed
//! response back (correlation id echoed; flexible versions get the
//! tagged-field header tail).
//!
//! Error policy: malformed frames / oversized lengths / unsupported
//! versions of non-ApiVersions APIs end the connection (the broker
//! behavior -- there is no version-independent error body for those);
//! ApiVersions itself always answers, so every client bootstrap works.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::kafka::coordinator::{self, CoordRuntime};
use crate::kafka::errors;
use crate::kafka::fetch;
use crate::kafka::frame::{parse_req_header, put_i32, put_resp_header, Reader};
use crate::kafka::handshake;
use crate::kafka::{
    api_flexible, api_name, api_supported, API_KEY_API_VERSIONS, API_KEY_DESCRIBE_GROUPS,
    API_KEY_FETCH, API_KEY_FIND_COORDINATOR, API_KEY_HEARTBEAT, API_KEY_JOIN_GROUP,
    API_KEY_LEAVE_GROUP, API_KEY_LIST_OFFSETS, API_KEY_METADATA, API_KEY_OFFSET_COMMIT,
    API_KEY_OFFSET_FETCH, API_KEY_PRODUCE, API_KEY_SYNC_GROUP,
};
use crate::kafka::{offsets_commit, offsets_query, produce};
use crate::monitor;
use crate::state::Shared;

/// Largest request frame accepted (16 MiB: broker-side headroom over
/// the ~1 MiB librdkafka default batches, 6x tighter than Kafka's
/// 100 MiB socket cap so one connection cannot pin 100 MiB).
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Idle-read cutoff (Kafka's connections.max.idle.ms default 10min).
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Built-in connection cap when `kafka_max_connections` is 0.
const DEFAULT_MAX_CONNS: i64 = 4096;

/// Live kafka connections (process-wide; the front has one listener).
static LIVE_CONNS: AtomicI64 = AtomicI64::new(0);

/// Admission guard: counts one connection from accept to close, so
/// every early return (peer close, malformed frame, cap eviction)
/// releases its slot.
struct ConnGuard;

impl ConnGuard {
    /// `None` = at/over the cap (the socket is dropped = closed).
    fn enter(cap: i64) -> Option<ConnGuard> {
        if LIVE_CONNS.fetch_add(1, Ordering::AcqRel) >= cap {
            LIVE_CONNS.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(ConnGuard)
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        LIVE_CONNS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Drive one connection until it errors or the peer closes. Frame
/// buffers are REUSED across the loop (grown on demand, never freed
/// per frame) and the idle-read cutoff closes silent connections
/// (Kafka's connections.max.idle.ms behavior).
pub async fn handle_conn(sock: TcpStream, shared: Arc<Shared>, coord: Arc<CoordRuntime>) {
    let cap = if shared.conf.kafka_max_connections > 0 {
        shared.conf.kafka_max_connections
    } else {
        DEFAULT_MAX_CONNS
    };
    let Some(_guard) = ConnGuard::enter(cap) else {
        eprintln!("[kafka] connection refused: at cap {cap}");
        return;
    };
    let ad = handshake::advertise(
        &shared.conf.kafka_bind,
        &shared.conf.kafka_advertised_host,
        shared.conf.kafka_advertised_port,
    );
    let peer_host = sock
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let (mut rd, mut wr) = sock.into_split();
    let mut payload: Vec<u8> = Vec::new();
    loop {
        let mut len_buf = [0u8; 4];
        match tokio::time::timeout(IDLE_TIMEOUT, rd.read_exact(&mut len_buf)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                if e.kind() != std::io::ErrorKind::UnexpectedEof {
                    eprintln!("[kafka] read frame length failed: {e}");
                }
                return;
            }
            Err(_) => return, // idle cutoff
        }
        let len = i32::from_be_bytes(len_buf);
        if len < 0 || len as usize > MAX_FRAME_BYTES {
            eprintln!("[kafka] dropping connection: frame length {len}");
            return;
        }
        // Reuse the buffer: resize keeps capacity, so steady-state
        // frames never allocate.
        payload.resize(len as usize, 0);
        match tokio::time::timeout(IDLE_TIMEOUT, rd.read_exact(&mut payload)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                eprintln!("[kafka] read frame body failed: {e}");
                return;
            }
            Err(_) => return, // idle cutoff mid-frame
        }
        let reply = match process(&payload, &shared, &ad, &coord, &peer_host).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[kafka] closing connection: {e}");
                return;
            }
        };
        // `None` = the api promised NO response bytes (Produce acks=0):
        // skip the frame and keep reading requests.
        let Some(reply) = reply else {
            continue;
        };
        let mut out = Vec::with_capacity(reply.len() + 4);
        put_i32(&mut out, reply.len() as i32);
        out.extend_from_slice(&reply);
        if let Err(e) = wr.write_all(&out).await {
            eprintln!("[kafka] write reply failed: {e}");
            return;
        }
        if let Err(e) = wr.flush().await {
            eprintln!("[kafka] flush reply failed: {e}");
            return;
        }
    }
}

/// One request frame -> one response frame (header + body); `Ok(None)`
/// = no response frame at all (Produce with acks=0, the spec behavior).
async fn process(
    payload: &[u8],
    shared: &Shared,
    ad: &(String, i32),
    coord: &Arc<CoordRuntime>,
    peer_host: &str,
) -> Result<Option<Vec<u8>>, String> {
    let started = Instant::now();
    // The header's own shape depends on the requested api version, so
    // peek key/version before choosing the flexible tail.
    let mut probe = Reader::new(payload);
    let api_key = probe.i16().ok_or("missing api_key")?;
    let api_version = probe.i16().ok_or("missing api_version")?;
    let flexible = api_flexible(api_key, api_version);
    let (header, mut body) =
        parse_req_header(payload, flexible).ok_or("malformed request header")?;
    let api_label = api_name(api_key);
    // Long-poll time the FETCH handler spent parked (0 for every
    // other api): subtracted from the latency observation so the
    // histogram measures handler work, not the client's max_wait.
    let mut parked_ms: u64 = 0;
    let body_opt = match api_key {
        API_KEY_API_VERSIONS => {
            if api_supported(api_key, api_version) {
                Some(handshake::api_versions_body(api_version, errors::NONE))
            } else {
                // Spec fallback: v0 body + the full supported list, so
                // the client can renegotiate from the error reply.
                Some(handshake::api_versions_body(0, errors::UNSUPPORTED_VERSION))
            }
        }
        API_KEY_METADATA => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            Some(handshake::handle_metadata(
                &mut body,
                api_version,
                shared,
                ad,
            )?)
        }
        API_KEY_PRODUCE => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            // None = acks=0: the broker sends NO response frame.
            produce::handle_produce(&mut body, api_version, shared).await?
        }
        API_KEY_LIST_OFFSETS => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            Some(offsets_query::handle_list_offsets(
                &mut body,
                api_version,
                shared,
            )?)
        }
        API_KEY_FETCH => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            let (body, parked) = fetch::handle_fetch(&mut body, api_version, shared).await?;
            parked_ms = parked;
            Some(body)
        }
        API_KEY_OFFSET_COMMIT => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            Some(offsets_commit::handle_offset_commit(&mut body, api_version, shared, coord).await?)
        }
        API_KEY_OFFSET_FETCH => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            Some(offsets_commit::handle_offset_fetch(
                &mut body,
                api_version,
                shared,
            )?)
        }
        API_KEY_FIND_COORDINATOR => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            Some(coordinator::api::handle_find_coordinator(
                &mut body,
                api_version,
                ad,
            )?)
        }
        API_KEY_JOIN_GROUP => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            Some(
                coordinator::group_api::handle_join_group(
                    &mut body,
                    api_version,
                    coord,
                    shared,
                    header.client_id.as_deref(),
                    peer_host,
                )
                .await?,
            )
        }
        API_KEY_HEARTBEAT => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            Some(coordinator::api::handle_heartbeat(&mut body, api_version, coord).await?)
        }
        API_KEY_LEAVE_GROUP => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            Some(coordinator::api::handle_leave_group(
                &mut body,
                api_version,
                coord,
            )?)
        }
        API_KEY_SYNC_GROUP => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            Some(coordinator::group_api::handle_sync_group(&mut body, api_version, coord).await?)
        }
        API_KEY_DESCRIBE_GROUPS => {
            if !api_supported(api_key, api_version) {
                return Err(format!("{} v{} unsupported", api_label, api_version));
            }
            Some(coordinator::api::handle_describe_groups(
                &mut body,
                api_version,
                coord,
            )?)
        }
        _ => Some(handshake::api_versions_body(0, errors::UNSUPPORTED_VERSION)),
    };
    let Some(body_bytes) = body_opt else {
        // acks=0 Produce promised NO response frame -- still observed
        // (fire-and-forget traffic counts toward the histogram too).
        monitor::observe_kafka_latency(
            &shared.monitor,
            api_label,
            started.elapsed().as_secs_f64() * 1000.0,
        );
        return Ok(None);
    };
    // A flexible response header only exists for flexible AND supported
    // versions (the v0 error fallback is a v0 response) -- EXCEPT
    // ApiVersions, which per KIP-511 always answers with the classic
    // v0 header (correlation id only) so pre-negotiation clients can
    // parse the error code; librdkafka hardcodes the same exception.
    let mut out = Vec::with_capacity(body_bytes.len() + 8);
    put_resp_header(
        &mut out,
        header.correlation_id,
        flexible && api_supported(api_key, api_version) && api_key != API_KEY_API_VERSIONS,
    );
    out.extend_from_slice(&body_bytes);
    monitor::observe_kafka_latency(
        &shared.monitor,
        api_label,
        (started.elapsed().as_secs_f64() * 1000.0) - parked_ms as f64,
    );
    Ok(Some(out))
}
